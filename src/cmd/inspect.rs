//! `report` and `source` — reading back what a run did.
//!
//! Both work entirely from stored runs. Re-rendering, inspecting a body, or opening
//! an HTML report never costs a fetch.

use super::{ReportArgs, SourceCmd, parse_fields};
use crate::cache::{Cache, RunRow};
use crate::exit;
use crate::research::RunResult;
use crate::toon::Toon;
use anyhow::{Context, Result};

/// Resolves a run id, defaulting to the most recent run so `pilotage report` alone
/// does the obvious thing.
fn resolve_run(cache: &Cache, id: Option<&str>) -> Result<Option<RunRow>> {
    match id {
        Some(id) => cache.get_run(id),
        None => Ok(cache.recent_runs(1)?.into_iter().next()),
    }
}

pub fn report(cache: &Cache, args: ReportArgs) -> Result<i32> {
    let fields = parse_fields(args.fields.as_deref())?;
    let Some(row) = resolve_run(cache, args.run.as_deref())? else {
        let mut t = Toon::new();
        t.empty("runs");
        t.next(&["pilotage research <target>".to_string()]);
        t.print();
        // No runs yet is a legitimate state, not a failure.
        return Ok(exit::OK);
    };
    let run = RunResult::from_row(&row)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&run)?);
        return Ok(exit::OK);
    }

    if args.html {
        let path = args
            .out
            .unwrap_or_else(|| std::env::temp_dir().join(format!("pilotage-{}.html", run.id)));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&path, crate::report::render(&run))
            .with_context(|| format!("writing {}", path.display()))?;

        let mut t = Toon::new();
        t.tagged("report", &run.id, path.display().to_string());
        t.kv("verdict", run.scored.verdict.as_str());
        t.next(&[
            format!("lavish-axi {}", path.display()),
            format!("open {}", path.display()),
        ]);
        t.print();
        return Ok(exit::OK);
    }

    let mut t = Toon::new();
    t.kv("target", &run.target);
    t.kv("age", crate::util::age(crate::util::now() - run.created_at));
    super::research::render(&mut t, &run, &fields, args.full);

    let mut next = vec![format!("pilotage report {} --html", run.id)];
    if !args.full {
        next.push(format!("pilotage report {} --full", run.id));
    }
    next.push(format!("pilotage source list --run {}", run.id));
    t.next(&next);
    t.print();
    Ok(exit::OK)
}

pub fn source(cache: &Cache, cmd: SourceCmd) -> Result<i32> {
    match cmd {
        SourceCmd::List { run } => {
            let Some(row) = resolve_run(cache, run.as_deref())? else {
                let mut t = Toon::new();
                t.empty("runs");
                t.next(&["pilotage research <target>".to_string()]);
                t.print();
                return Ok(exit::OK);
            };
            let run = RunResult::from_row(&row)?;

            let rows: Vec<Vec<String>> = run
                .sources
                .iter()
                .map(|s| {
                    vec![
                        s.id.clone(),
                        if s.status == 0 {
                            "-".into()
                        } else {
                            s.status.to_string()
                        },
                        s.facts.to_string(),
                        format!("{}ms", s.ms),
                        super::bytes(s.bytes as i64),
                        if s.from_cache { "cached" } else { "live" }.to_string(),
                    ]
                })
                .collect();

            let mut t = Toon::new();
            t.tagged("run", &run.id, &run.target);
            t.table(
                "sources",
                &["id", "status", "facts", "ms", "bytes", "origin"],
                &rows,
            );
            let failed: Vec<String> = run
                .sources
                .iter()
                .filter(|s| !s.ok)
                .map(|s| format!("{}({})", s.id, s.error.clone().unwrap_or_default()))
                .collect();
            t.list("failed", &failed);

            let first = run
                .sources
                .first()
                .map(|s| s.id.clone())
                .unwrap_or_default();
            t.next(&[format!("pilotage source get {first} --run {}", run.id)]);
            t.print();
            Ok(exit::OK)
        }

        SourceCmd::Get {
            id,
            run,
            full,
            head,
        } => {
            let Some(row) = resolve_run(cache, run.as_deref())? else {
                anyhow::bail!("no runs recorded yet — run `pilotage research <target>` first");
            };
            let run = RunResult::from_row(&row)?;
            let Some(src) = run.sources.iter().find(|s| s.id == id) else {
                anyhow::bail!(
                    "run {} has no source '{id}' (it read: {})",
                    run.id,
                    run.sources
                        .iter()
                        .map(|s| s.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            };

            // The browser transport stores under a distinct key; try both so
            // `source get` works regardless of how the source was fetched.
            let blob = cache
                .get_blob(&src.url)?
                .or(cache.get_blob(&format!("browser:{}", src.url))?);
            let Some(blob) = blob else {
                anyhow::bail!(
                    "the response for '{id}' is no longer cached (it may have been pruned)"
                );
            };

            let body = String::from_utf8_lossy(&blob.body);
            let mut t = Toon::new();
            t.tagged("source", &id, &src.url);
            t.kv("status", blob.status);
            t.kv("bytes", blob.body.len());
            t.kv(
                "age",
                crate::util::age(crate::util::now() - blob.fetched_at),
            );
            t.kv("hash", &blob.content_hash);
            t.print();

            if full {
                println!("{body}");
            } else {
                let total = body.lines().count();
                for line in body.lines().take(head) {
                    println!("{line}");
                }
                if total > head {
                    println!(
                        "... {} more line(s) — rerun with --full or --head <n>",
                        total - head
                    );
                }
            }
            Ok(exit::OK)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::RunRow;

    fn row(id: &str, created_at: i64) -> RunRow {
        RunRow {
            id: id.into(),
            target: "a/b".into(),
            pack: "oss".into(),
            created_at,
            duration_ms: 1,
            verdict: "healthy".into(),
            score: 1.0,
            coverage: 1.0,
            cost_usd: 0.0,
            payload: "{}".into(),
        }
    }

    #[test]
    fn no_run_id_resolves_to_the_most_recent_run() {
        let c = Cache::open_in_memory().unwrap();
        c.save_run(&row("r_old", 100)).unwrap();
        c.save_run(&row("r_new", 200)).unwrap();
        assert_eq!(resolve_run(&c, None).unwrap().unwrap().id, "r_new");
        assert_eq!(resolve_run(&c, Some("r_old")).unwrap().unwrap().id, "r_old");
    }

    #[test]
    fn an_empty_cache_resolves_to_nothing_rather_than_erroring() {
        let c = Cache::open_in_memory().unwrap();
        assert!(resolve_run(&c, None).unwrap().is_none());
        assert!(resolve_run(&c, Some("r_nope")).unwrap().is_none());
    }

    #[test]
    fn reporting_with_no_runs_is_a_clean_empty_state() {
        let c = Cache::open_in_memory().unwrap();
        let code = report(
            &c,
            ReportArgs {
                run: None,
                html: false,
                out: None,
                full: false,
                fields: None,
                json: false,
            },
        )
        .unwrap();
        assert_eq!(code, exit::OK);
    }

    #[test]
    fn an_unreadable_payload_is_reported_with_its_run_id() {
        let c = Cache::open_in_memory().unwrap();
        c.save_run(&row("r_bad", 1)).unwrap();
        let err = RunResult::from_row(&c.get_run("r_bad").unwrap().unwrap()).unwrap_err();
        assert!(err.to_string().contains("r_bad"), "{err}");
    }
}
