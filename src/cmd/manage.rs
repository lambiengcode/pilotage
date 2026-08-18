//! `watch`, `pack`, `cache` and `setup` — the commands that manage pilotage itself.

use super::{CacheCmd, PackCmd, SetupCmd, WatchCmd};
use crate::cache::Cache;
use crate::exit;
use crate::research::{self, Options, RunResult};
use crate::toon::Toon;
use anyhow::{Context, Result};
use std::str::FromStr;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// watch
// ---------------------------------------------------------------------------

pub async fn watch(cache: Arc<Cache>, cmd: WatchCmd) -> Result<i32> {
    match cmd {
        WatchCmd::Add { target, pack } => {
            let packs = crate::pack::load_all()?;
            // Resolve now so a target that no pack understands fails here, loudly,
            // rather than silently doing nothing on every future `watch check`.
            let (resolved, _) = research::resolve_pack(&packs, &target, pack.as_deref())?;
            let added = cache.watch_add(&target, &resolved.name)?;

            let mut t = Toon::new();
            t.kv("watch", &target);
            t.kv("pack", &resolved.name);
            // Idempotent: adding twice is one watch, and says so.
            t.kv("status", if added { "added" } else { "already watched" });
            t.kv("watches", cache.watch_list()?.len());
            t.next(&[
                "pilotage watch check".to_string(),
                "pilotage watch list".to_string(),
            ]);
            t.print();
            Ok(exit::OK)
        }

        WatchCmd::Rm { target, pack } => {
            let removed = cache.watch_remove(&target, pack.as_deref())?;
            let mut t = Toon::new();
            t.kv("removed", removed);
            t.kv("watches", cache.watch_list()?.len());
            if removed == 0 {
                t.kv("note", format!("'{target}' was not being watched"));
            }
            t.next(&["pilotage watch list".to_string()]);
            t.print();
            Ok(exit::OK)
        }

        WatchCmd::List => {
            let watches = cache.watch_list()?;
            let now = crate::util::now();
            let rows: Vec<Vec<String>> = watches
                .iter()
                .map(|w| {
                    let last = cache.last_run_for(&w.target, &w.pack).ok().flatten();
                    vec![
                        w.target.clone(),
                        w.pack.clone(),
                        last.as_ref()
                            .map(|r| r.verdict.clone())
                            .unwrap_or_else(|| "never-run".into()),
                        last.as_ref()
                            .map(|r| format!("{:.0}", r.score * 100.0))
                            .unwrap_or_else(|| "-".into()),
                        last.as_ref()
                            .map(|r| crate::util::age(now - r.created_at))
                            .unwrap_or_else(|| "-".into()),
                    ]
                })
                .collect();

            let mut t = Toon::new();
            t.table(
                "watches",
                &["target", "pack", "verdict", "score", "age"],
                &rows,
            );
            t.next(&if watches.is_empty() {
                vec!["pilotage watch add <target>".to_string()]
            } else {
                vec!["pilotage watch check".to_string()]
            });
            t.print();
            Ok(exit::OK)
        }

        WatchCmd::Check { depth, budget } => {
            let watches = cache.watch_list()?;
            if watches.is_empty() {
                let mut t = Toon::new();
                t.empty("watches");
                t.next(&["pilotage watch add <target>".to_string()]);
                t.print();
                return Ok(exit::OK);
            }

            let packs = crate::pack::load_all()?;
            let depth = crate::pack::Depth::from_str(&depth)?;
            let per_target = budget.map(|b| b / watches.len() as f64);

            let mut rows: Vec<Vec<String>> = Vec::new();
            let mut moved: Vec<String> = Vec::new();
            let mut failures: Vec<String> = Vec::new();
            let mut blocked = false;

            for w in &watches {
                // The previous run is captured before the new one is saved, so the
                // diff is against history rather than against itself.
                let previous = cache
                    .last_run_for(&w.target, &w.pack)?
                    .and_then(|r| RunResult::from_row(&r).ok());

                let opts = Options {
                    target: w.target.clone(),
                    pack: Some(w.pack.clone()),
                    depth,
                    budget: per_target,
                    no_cache: false,
                };
                match research::run(&packs, cache.clone(), &opts).await {
                    Ok(run) => {
                        research::save(&cache, &run)?;
                        cache.watch_touch(&w.target, &w.pack, &run.id)?;
                        blocked |= run.budget_blocked;

                        let (delta, changes) = diff(previous.as_ref(), &run);
                        if !changes.is_empty() {
                            moved.extend(changes);
                        }
                        rows.push(vec![
                            w.target.clone(),
                            run.scored.verdict.as_str().to_string(),
                            format!("{:.0}", run.scored.score * 100.0),
                            delta,
                            run.id.clone(),
                        ]);
                    }
                    Err(e) => failures.push(format!(
                        "{}({})",
                        w.target,
                        e.to_string().lines().next().unwrap_or_default()
                    )),
                }
            }

            let mut t = Toon::new();
            t.table(
                "checked",
                &["target", "verdict", "score", "delta", "run"],
                &rows,
            );
            t.list("changed", &moved);
            t.list("failed", &failures);
            t.next(&["pilotage watch list".to_string(), "pilotage".to_string()]);
            t.print();

            Ok(if rows.is_empty() {
                exit::NO_DATA
            } else if blocked {
                exit::BUDGET
            } else {
                exit::OK
            })
        }
    }
}

/// What moved since the previous run. Returns the score delta and one line per
/// metric whose status changed — the entire reason to keep a watch.
fn diff(previous: Option<&RunResult>, current: &RunResult) -> (String, Vec<String>) {
    let Some(prev) = previous else {
        return ("new".into(), Vec::new());
    };

    let points = (current.scored.score - prev.scored.score) * 100.0;
    let delta = if points.abs() < 0.5 {
        "0".to_string()
    } else {
        format!("{points:+.0}")
    };

    let mut changes = Vec::new();
    if prev.scored.verdict != current.scored.verdict {
        changes.push(format!(
            "{}: verdict {} -> {}",
            current.target,
            prev.scored.verdict.as_str(),
            current.scored.verdict.as_str()
        ));
    }
    for now in &current.scored.signals {
        if let Some(before) = prev.scored.signals.iter().find(|s| s.id == now.id)
            && before.status != now.status
        {
            changes.push(format!(
                "{}: {} {} -> {} ({} -> {})",
                current.target,
                now.id,
                before.status.as_str(),
                now.status.as_str(),
                before.display,
                now.display
            ));
        }
    }
    (delta, changes)
}

// ---------------------------------------------------------------------------
// pack
// ---------------------------------------------------------------------------

pub fn pack(cmd: PackCmd) -> Result<i32> {
    match cmd {
        PackCmd::List => {
            let packs = crate::pack::load_all()?;
            let rows: Vec<Vec<String>> = packs
                .iter()
                .map(|p| {
                    vec![
                        p.name.clone(),
                        p.source.len().to_string(),
                        p.metric.len().to_string(),
                        p.detect.join(" "),
                    ]
                })
                .collect();
            let mut t = Toon::new();
            t.table("packs", &["name", "sources", "metrics", "detects"], &rows);
            t.kv(
                "user_packs",
                crate::pack::user_pack_dir().display().to_string(),
            );
            t.next(&[
                "pilotage pack show <name>".to_string(),
                "pilotage pack validate".to_string(),
            ]);
            t.print();
            Ok(exit::OK)
        }

        PackCmd::Show { name } => {
            let packs = crate::pack::load_all()?;
            let Some(idx) = crate::pack::find(&packs, &name) else {
                anyhow::bail!(
                    "unknown pack '{name}' (available: {})",
                    packs
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            };
            let p = &packs[idx];

            let mut t = Toon::new();
            t.kv("pack", &p.name);
            t.kv("description", &p.description);
            t.list("detect", &p.detect);
            t.kv("tolerance", p.tolerance);

            let srows: Vec<Vec<String>> = p
                .source
                .iter()
                .map(|s| {
                    vec![
                        s.id.clone(),
                        format!("{:?}", s.kind).to_lowercase(),
                        format!("{:?}", s.depth).to_lowercase(),
                        s.ttl.clone(),
                        super::trim_num(s.weight),
                        if s.optional { "optional" } else { "required" }.to_string(),
                        (s.map.len()
                            + s.select.len()
                            + s.extract.as_ref().map(|e| e.fields.len()).unwrap_or(0))
                        .to_string(),
                    ]
                })
                .collect();
            t.table(
                "sources",
                &["id", "kind", "depth", "ttl", "w", "req", "facts"],
                &srows,
            );

            let mrows: Vec<Vec<String>> = p
                .metric
                .iter()
                .map(|m| {
                    vec![
                        m.id.clone(),
                        m.fact.clone(),
                        m.agg.clone().unwrap_or_else(|| "-".into()),
                        m.warn.clone().unwrap_or_else(|| "-".into()),
                        m.fail.clone().unwrap_or_else(|| "-".into()),
                        super::trim_num(m.weight),
                        if m.critical { "yes" } else { "" }.to_string(),
                    ]
                })
                .collect();
            t.table(
                "metrics",
                &["id", "fact", "agg", "warn", "fail", "w", "critical"],
                &mrows,
            );

            let issues = p.validate();
            t.list("issues", &issues);
            t.next(&[format!("pilotage research <target> --pack {}", p.name)]);
            t.print();
            Ok(exit::OK)
        }

        PackCmd::Validate { file } => {
            let packs = match &file {
                Some(path) => vec![crate::pack::Pack::load_file(path)?],
                None => crate::pack::load_all()?,
            };
            let mut rows: Vec<Vec<String>> = Vec::new();
            let mut all_issues: Vec<String> = Vec::new();
            for p in &packs {
                let issues = p.validate();
                rows.push(vec![
                    p.name.clone(),
                    if issues.is_empty() { "ok" } else { "issues" }.to_string(),
                    issues.len().to_string(),
                ]);
                all_issues.extend(issues.iter().map(|i| format!("{}: {i}", p.name)));
            }

            let mut t = Toon::new();
            t.table("validated", &["pack", "status", "issues"], &rows);
            t.list("issues", &all_issues);
            t.print();
            // A broken pack is a usage error: it cannot produce a valid run.
            Ok(if all_issues.is_empty() {
                exit::OK
            } else {
                exit::USAGE
            })
        }
    }
}

// ---------------------------------------------------------------------------
// cache
// ---------------------------------------------------------------------------

pub fn cache_cmd(cache: &Cache, cmd: CacheCmd) -> Result<i32> {
    match cmd {
        CacheCmd::Stats => {
            let s = cache.stats()?;
            let mut t = Toon::new();
            t.kv("path", cache.path().display().to_string());
            t.kv(
                "blobs",
                format!("{} ({})", s.blobs, super::bytes(s.blob_bytes)),
            );
            t.kv("extractions", s.facts);
            t.kv("runs", s.runs);
            t.kv("watches", s.watches);
            t.kv("size", super::bytes(s.db_bytes));
            t.next(&["pilotage cache prune".to_string()]);
            t.print();
            Ok(exit::OK)
        }
        CacheCmd::Prune { blobs, runs } => {
            let blob_age = crate::util::parse_duration(&blobs)?;
            let run_age = crate::util::parse_duration(&runs)?;
            let (b, r) = cache.prune(blob_age, run_age)?;
            let mut t = Toon::new();
            t.kv("pruned_blobs", b);
            t.kv("pruned_runs", r);
            // Extracted facts are immutable and cheap to keep, but expensive to
            // recompute — pruning them would throw away the money already spent.
            t.kv("kept", "extractions (immutable, never expire)");
            t.print();
            Ok(exit::OK)
        }
    }
}

// ---------------------------------------------------------------------------
// setup
// ---------------------------------------------------------------------------

const HOOK_COMMAND: &str = "pilotage ambient";

pub fn setup(cmd: SetupCmd) -> Result<i32> {
    let SetupCmd::Hooks { install } = cmd;

    let snippet = serde_json::json!({
        "hooks": {
            "SessionStart": [{
                "matcher": "startup",
                "hooks": [{ "type": "command", "command": HOOK_COMMAND }]
            }]
        }
    });

    if !install {
        let mut t = Toon::new();
        t.kv("hook", HOOK_COMMAND);
        t.kv("target", settings_path().display().to_string());
        t.kv("action", "printed only — rerun with --install to write it");
        t.print();
        println!("{}", serde_json::to_string_pretty(&snippet)?);
        return Ok(exit::OK);
    }

    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let existing = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".into());
    let mut settings: serde_json::Value = serde_json::from_str(&existing).with_context(|| {
        format!(
            "{} is not valid JSON — fix or move it first",
            path.display()
        )
    })?;
    if !settings.is_object() {
        anyhow::bail!("{} does not contain a JSON object", path.display());
    }

    let hooks = settings["hooks"]["SessionStart"].clone();
    let mut entries = hooks.as_array().cloned().unwrap_or_default();

    // Idempotent: installing twice leaves one hook.
    let already = entries.iter().any(|e| {
        e["hooks"]
            .as_array()
            .is_some_and(|hs| hs.iter().any(|h| h["command"] == HOOK_COMMAND))
    });

    if already {
        let mut t = Toon::new();
        t.kv("hook", HOOK_COMMAND);
        t.kv("status", "already installed");
        t.kv("settings", path.display().to_string());
        t.print();
        return Ok(exit::OK);
    }

    // Back up before touching a file pilotage does not own.
    if path.exists() {
        std::fs::copy(&path, path.with_extension("json.bak"))
            .with_context(|| format!("backing up {}", path.display()))?;
    }

    entries.push(serde_json::json!({
        "matcher": "startup",
        "hooks": [{ "type": "command", "command": HOOK_COMMAND }]
    }));
    settings["hooks"]["SessionStart"] = serde_json::Value::Array(entries);
    std::fs::write(&path, serde_json::to_string_pretty(&settings)?)
        .with_context(|| format!("writing {}", path.display()))?;

    let mut t = Toon::new();
    t.kv("hook", HOOK_COMMAND);
    t.kv("status", "installed");
    t.kv("settings", path.display().to_string());
    t.kv(
        "backup",
        path.with_extension("json.bak").display().to_string(),
    );
    t.next(&["pilotage ambient".to_string()]);
    t.print();
    Ok(exit::OK)
}

fn settings_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("PILOTAGE_SETTINGS_PATH") {
        return std::path::PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home)
        .join(".claude")
        .join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::MetricValue;
    use crate::score::{Scored, Signal, Status, Verdict};

    fn signal(id: &str, status: Status, display: &str) -> Signal {
        Signal {
            id: id.into(),
            label: id.into(),
            value: MetricValue::Str(display.into()),
            display: display.into(),
            status,
            weight: 1.0,
            confidence: 1.0,
            critical: false,
            pin: false,
            source: None,
            source_url: None,
        }
    }

    fn run(score: f64, verdict: Verdict, signals: Vec<Signal>) -> RunResult {
        RunResult {
            id: "r_x".into(),
            target: "a/b".into(),
            pack: "oss".into(),
            created_at: 0,
            duration_ms: 1,
            depth: "quick".into(),
            scored: Scored {
                signals,
                verdict,
                score,
                coverage: 1.0,
                confidence: 1.0,
            },
            sources: vec![],
            contradictions: vec![],
            degraded: vec![],
            facts: vec![],
            cost_usd: 0.0,
            llm_calls: 0,
            llm_tokens: 0,
            budget_blocked: false,
            replay: false,
        }
    }

    #[test]
    fn a_first_run_has_no_delta_to_report() {
        let (delta, changes) = diff(None, &run(0.9, Verdict::Healthy, vec![]));
        assert_eq!(delta, "new");
        assert!(changes.is_empty());
    }

    #[test]
    fn an_unchanged_target_reports_zero_and_says_nothing_else() {
        let before = run(0.9, Verdict::Healthy, vec![signal("a", Status::Ok, "1")]);
        let after = run(0.9, Verdict::Healthy, vec![signal("a", Status::Ok, "1")]);
        let (delta, changes) = diff(Some(&before), &after);
        assert_eq!(delta, "0");
        assert!(changes.is_empty());
    }

    #[test]
    fn a_score_move_is_signed_so_direction_is_obvious() {
        let before = run(0.90, Verdict::Healthy, vec![]);
        let after = run(0.60, Verdict::Caution, vec![]);
        assert_eq!(diff(Some(&before), &after).0, "-30");
        assert_eq!(diff(Some(&after), &before).0, "+30");
    }

    #[test]
    fn a_verdict_change_is_called_out_explicitly() {
        let before = run(0.9, Verdict::Healthy, vec![]);
        let after = run(0.4, Verdict::Risk, vec![]);
        let (_, changes) = diff(Some(&before), &after);
        assert_eq!(changes.len(), 1);
        assert!(
            changes[0].contains("verdict healthy -> risk"),
            "{changes:?}"
        );
    }

    #[test]
    fn each_metric_that_flipped_is_named_with_both_values() {
        let before = run(
            0.9,
            Verdict::Healthy,
            vec![signal("commits", Status::Ok, "50")],
        );
        let after = run(
            0.5,
            Verdict::Caution,
            vec![signal("commits", Status::Fail, "2")],
        );
        let (_, changes) = diff(Some(&before), &after);
        assert!(
            changes
                .iter()
                .any(|c| c.contains("commits ok -> fail (50 -> 2)")),
            "{changes:?}"
        );
    }

    #[test]
    fn a_metric_that_did_not_exist_before_is_not_reported_as_a_change() {
        let before = run(0.9, Verdict::Healthy, vec![]);
        let after = run(
            0.9,
            Verdict::Healthy,
            vec![signal("new_metric", Status::Fail, "0")],
        );
        let (_, changes) = diff(Some(&before), &after);
        assert!(changes.is_empty(), "{changes:?}");
    }

    #[test]
    fn validating_the_shipped_packs_succeeds() {
        assert_eq!(pack(PackCmd::Validate { file: None }).unwrap(), exit::OK);
    }

    #[test]
    fn validating_a_broken_pack_exits_usage() {
        let dir = std::env::temp_dir().join(format!("pilotage-pv-{}", crate::util::new_run_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.toml");
        std::fs::write(
            &path,
            "name='broken'\ndetect=['{a}/{b}']\n[[source]]\nid='s'\nurl='https://x'\n[source.map]\nk='.k'\n[[metric]]\nid='m'\nfact='nonexistent'\nwarn='< 1'\n",
        )
        .unwrap();
        assert_eq!(
            pack(PackCmd::Validate { file: Some(path) }).unwrap(),
            exit::USAGE
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
