//! `research` and `compare`.

use super::{CompareArgs, ResearchArgs, parse_fields, signal_cell};
use crate::cache::Cache;
use crate::exit;
use crate::research::{self, Options, RunResult};
use crate::score::Status;
use crate::toon::Toon;
use anyhow::{Context, Result};
use std::str::FromStr;
use std::sync::Arc;

pub async fn research(cache: Arc<Cache>, args: ResearchArgs) -> Result<i32> {
    let fields = parse_fields(args.fields.as_deref())?;
    let packs = crate::pack::load_all()?;
    let opts = Options {
        target: args.target.clone(),
        pack: args.pack.clone(),
        depth: crate::pack::Depth::from_str(&args.depth)?,
        budget: args.budget,
        no_cache: args.no_cache,
    };

    let run = research::run(&packs, cache.clone(), &opts).await?;
    research::save(&cache, &run)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&run)?);
    } else {
        let mut t = Toon::new();
        render(&mut t, &run, &fields, args.full);

        let mut next = Vec::new();
        if !args.full && run.scored.signals.iter().any(|s| s.status == Status::Ok) {
            next.push(format!("pilotage report {} --full", run.id));
        }
        next.push(format!("pilotage report {} --html", run.id));
        if run.sources.iter().any(|s| !s.ok) {
            next.push(format!("pilotage source list --run {}", run.id));
        }
        if opts.depth == crate::pack::Depth::Quick {
            next.push(format!(
                "pilotage research {} --depth deep",
                shell_quote(&run.target)
            ));
        }
        next.push(format!("pilotage watch add {}", shell_quote(&run.target)));
        t.next(&next);
        t.print();
    }

    if let Some(path) = &args.html {
        std::fs::write(path, crate::report::render(&run))
            .with_context(|| format!("writing {}", path.display()))?;
        eprintln!("report: {}", path.display());
    }

    Ok(exit_code(&run))
}

/// Exit codes carry the outcome so a caller can branch without parsing text.
pub fn exit_code(run: &RunResult) -> i32 {
    if run.ok_sources() == 0 {
        exit::NO_DATA
    } else if run.budget_blocked {
        exit::BUDGET
    } else {
        exit::OK
    }
}

/// The default view is the actionable one: what is wrong, plus the names of what is
/// fine. `--full` opens it up. Healthy metrics are the bulk of a run and the least
/// interesting part of it, so they cost one line between them instead of one each.
pub fn render(t: &mut Toon, run: &RunResult, fields: &[String], full: bool) {
    let cols: Vec<&str> = fields.iter().map(String::as_str).collect();

    t.tagged(
        "run",
        &run.id,
        format!(
            "{}, {}/{} sources, {}ms, ${:.4}{}",
            run.pack,
            run.ok_sources(),
            run.sources.len(),
            run.duration_ms,
            run.cost_usd,
            if run.cache_hits() > 0 {
                format!(", {} cached", run.cache_hits())
            } else {
                String::new()
            }
        ),
    );
    t.kv("verdict", run.scored.verdict.as_str());
    t.kv("score", format!("{:.0}/100", run.scored.score * 100.0));
    t.kv("coverage", format!("{:.0}%", run.scored.coverage * 100.0));
    t.kv(
        "confidence",
        format!("{:.0}%", run.scored.confidence * 100.0),
    );

    let mut shown: Vec<&crate::score::Signal> = run
        .scored
        .signals
        .iter()
        .filter(|s| full || s.pin || s.status != Status::Ok)
        .collect();
    // Worst first — the whole point of the default view.
    shown.sort_by_key(|s| match s.status {
        Status::Fail => 0,
        Status::Warn => 1,
        Status::Unknown => 2,
        Status::Ok => 3,
    });

    let rows: Vec<Vec<String>> = shown
        .iter()
        .map(|s| cols.iter().map(|c| signal_cell(s, c)).collect())
        .collect();
    t.table(if full { "signals" } else { "issues" }, &cols, &rows);

    if !full {
        let ok: Vec<String> = run
            .scored
            .signals
            .iter()
            .filter(|s| s.status == Status::Ok && !s.pin)
            .map(|s| s.id.clone())
            .collect();
        t.list("ok", &ok);
    }

    let contradictions: Vec<Vec<String>> = run
        .contradictions
        .iter()
        .map(|c| {
            vec![
                c.key.clone(),
                format!("{}={}", c.a_source, c.a_value),
                format!("{}={}", c.b_source, c.b_value),
            ]
        })
        .collect();
    if !contradictions.is_empty() {
        t.table("contradictions", &["key", "a", "b"], &contradictions);
    }

    // Never a silent cap: anything skipped, failed or unauthenticated is named.
    t.list("degraded", &run.degraded);

    if run.budget_blocked {
        t.kv(
            "budget",
            format!(
                "ceiling reached: spent ${:.4}, some model extraction was skipped",
                run.cost_usd
            ),
        );
    }
    if run.replay {
        t.kv("mode", "replay (cache only, no network)");
    }
}

pub async fn compare(cache: Arc<Cache>, args: CompareArgs) -> Result<i32> {
    let packs = crate::pack::load_all()?;
    let depth = crate::pack::Depth::from_str(&args.depth)?;
    // A shared ceiling split evenly, so `--budget` means the same thing here as it
    // does for a single run: the most this invocation may cost in total.
    let per_target = args.budget.map(|b| b / args.targets.len() as f64);

    let mut runs: Vec<RunResult> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    // Sequential on purpose: comparing eight competitors should not mean eight
    // simultaneous bursts at the same handful of APIs.
    for target in &args.targets {
        let opts = Options {
            target: target.clone(),
            pack: args.pack.clone(),
            depth,
            budget: per_target,
            no_cache: false,
        };
        match research::run(&packs, cache.clone(), &opts).await {
            Ok(run) => {
                research::save(&cache, &run)?;
                runs.push(run);
            }
            Err(e) => failures.push(format!("{target}({})", first_line(&e.to_string()))),
        }
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&runs)?);
        return Ok(if runs.is_empty() {
            exit::NO_DATA
        } else {
            exit::OK
        });
    }

    let mut t = Toon::new();
    let summary: Vec<Vec<String>> = runs
        .iter()
        .map(|r| {
            vec![
                r.target.clone(),
                r.pack.clone(),
                r.scored.verdict.as_str().to_string(),
                format!("{:.0}", r.scored.score * 100.0),
                format!("{:.0}%", r.scored.coverage * 100.0),
                format!("${:.4}", r.cost_usd),
            ]
        })
        .collect();
    t.table(
        "compare",
        &["target", "pack", "verdict", "score", "coverage", "cost"],
        &summary,
    );

    // Pivot: one row per metric, one column per target. Comparing eight numbers
    // across four competitors is the entire reason this command exists.
    if !runs.is_empty() {
        let mut metric_ids: Vec<String> = Vec::new();
        for r in &runs {
            for s in &r.scored.signals {
                if !metric_ids.contains(&s.id) {
                    metric_ids.push(s.id.clone());
                }
            }
        }

        let mut cols: Vec<String> = vec!["metric".into()];
        cols.extend(runs.iter().map(|r| short_target(&r.target)));
        let col_refs: Vec<&str> = cols.iter().map(String::as_str).collect();

        let rows: Vec<Vec<String>> = metric_ids
            .iter()
            .map(|id| {
                let mut row = vec![id.clone()];
                for r in &runs {
                    row.push(match r.scored.signals.iter().find(|s| &s.id == id) {
                        Some(s) if s.status == Status::Unknown => "-".into(),
                        Some(s) => s.display.clone(),
                        None => "-".into(),
                    });
                }
                row
            })
            .collect();
        t.table("metrics", &col_refs, &rows);

        let status_rows: Vec<Vec<String>> = metric_ids
            .iter()
            .filter(|id| {
                runs.iter().any(|r| {
                    r.scored
                        .signals
                        .iter()
                        .any(|s| &&s.id == id && matches!(s.status, Status::Fail | Status::Warn))
                })
            })
            .map(|id| {
                let mut row = vec![id.clone()];
                for r in &runs {
                    row.push(
                        r.scored
                            .signals
                            .iter()
                            .find(|s| &s.id == id)
                            .map(|s| s.status.as_str().to_string())
                            .unwrap_or_else(|| "-".into()),
                    );
                }
                row
            })
            .collect();
        if !status_rows.is_empty() {
            t.table("concerns", &col_refs, &status_rows);
        }
    }

    let mut degraded = failures.clone();
    for r in &runs {
        degraded.extend(
            r.degraded
                .iter()
                .map(|d| format!("{}: {d}", short_target(&r.target))),
        );
    }
    t.list("degraded", &degraded);

    if let Some(path) = &args.html {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let title = args.title.clone().unwrap_or_else(|| {
            runs.iter()
                .map(|r| short_target(&r.target))
                .collect::<Vec<_>>()
                .join(" vs ")
        });
        std::fs::write(path, crate::report::render_comparison(&runs, &title))
            .with_context(|| format!("writing {}", path.display()))?;
        t.tagged("report", "comparison", path.display().to_string());
    }

    let mut next: Vec<String> = runs
        .iter()
        .take(2)
        .map(|r| format!("pilotage report {} --html", r.id))
        .collect();
    if args.html.is_none() {
        next.push("pilotage compare <targets> --html report.html".to_string());
    }
    if let Some(best) = runs
        .iter()
        .max_by(|a, b| a.scored.score.total_cmp(&b.scored.score))
    {
        next.push(format!(
            "pilotage research {} --full",
            shell_quote(&best.target)
        ));
    }
    t.next(&next);
    t.print();

    Ok(if runs.is_empty() {
        exit::NO_DATA
    } else if runs.iter().any(|r| r.budget_blocked) {
        exit::BUDGET
    } else {
        exit::OK
    })
}

/// Column headers are a per-row cost in a pivot table, so targets are shortened to
/// the part that distinguishes them.
fn short_target(t: &str) -> String {
    let n = crate::pack::normalize_target(t);
    n.rsplit('/').next().unwrap_or(&n).to_string()
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).to_string()
}

/// Targets go into `next:` suggestions that a human or agent may paste into a shell.
fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_alphanumeric() || "._-/:".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::MetricValue;
    use crate::score::{Scored, Signal, Verdict};

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
            source: Some("s".into()),
            source_url: Some("https://x".into()),
        }
    }

    fn run(signals: Vec<Signal>) -> RunResult {
        RunResult {
            id: "r_test1".into(),
            target: "github.com/a/b".into(),
            pack: "oss".into(),
            created_at: 0,
            duration_ms: 120,
            depth: "quick".into(),
            scored: Scored {
                signals,
                verdict: Verdict::Caution,
                score: 0.6,
                coverage: 0.9,
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

    fn render_to_string(r: &RunResult, full: bool) -> String {
        let mut t = Toon::new();
        render(&mut t, r, &parse_fields(None).unwrap(), full);
        t.render()
    }

    #[test]
    fn the_default_view_shows_problems_and_names_the_rest() {
        let out = render_to_string(
            &run(vec![
                signal("commits", Status::Fail, "2"),
                signal("stars", Status::Ok, "900"),
                signal("license", Status::Ok, "MIT"),
            ]),
            false,
        );
        assert!(out.contains("issues[1]{metric,value,status,w}:"));
        assert!(out.contains("commits,2,fail,1"));
        // Healthy metrics cost one line between them, not one line each.
        assert!(out.contains("ok[2]: stars,license"));
        assert!(!out.contains("stars,900"));
    }

    #[test]
    fn full_shows_every_signal_and_drops_the_ok_summary() {
        let out = render_to_string(
            &run(vec![
                signal("commits", Status::Fail, "2"),
                signal("stars", Status::Ok, "900"),
            ]),
            true,
        );
        assert!(out.contains("signals[2]{metric,value,status,w}:"));
        assert!(out.contains("stars,900,ok,1"));
        assert!(!out.contains("ok[1]"));
    }

    #[test]
    fn a_pinned_signal_shows_its_value_even_when_it_passes() {
        // The whole point: "which subject did this query actually match?" is useless as
        // a name in the ok list and decisive as a value.
        let mut pinned = signal("matched_article", Status::Ok, "Go (game)");
        pinned.pin = true;
        let out = render_to_string(
            &run(vec![pinned, signal("stars", Status::Ok, "900")]),
            false,
        );
        assert!(out.contains("matched_article,Go (game),ok"), "{out}");
        assert!(
            out.contains("ok[1]: stars"),
            "unpinned passes still collapse:\n{out}"
        );
    }

    #[test]
    fn issues_are_ordered_worst_first() {
        let out = render_to_string(
            &run(vec![
                signal("u", Status::Unknown, "-"),
                signal("w", Status::Warn, "5"),
                signal("f", Status::Fail, "1"),
            ]),
            false,
        );
        let body = out.split("issues").nth(1).unwrap();
        let (fi, wi, ui) = (
            body.find("f,1,fail").unwrap(),
            body.find("w,5,warn").unwrap(),
            body.find("u,-,unknown").unwrap(),
        );
        assert!(fi < wi && wi < ui, "expected fail < warn < unknown\n{body}");
    }

    #[test]
    fn a_clean_run_states_it_explicitly_rather_than_printing_nothing() {
        let out = render_to_string(&run(vec![signal("stars", Status::Ok, "900")]), false);
        assert!(out.contains("issues[0]: none"));
        assert!(out.contains("ok[1]: stars"));
    }

    #[test]
    fn degraded_is_always_present_even_when_empty() {
        let out = render_to_string(&run(vec![]), false);
        assert!(out.contains("degraded[0]: none"));
    }

    #[test]
    fn a_budget_ceiling_is_reported_and_changes_the_exit_code() {
        let mut r = run(vec![]);
        r.budget_blocked = true;
        r.sources = vec![crate::research::SourceResult {
            id: "s".into(),
            url: "u".into(),
            status: 200,
            ok: true,
            from_cache: false,
            facts: 1,
            ms: 1,
            bytes: 1,
            error: None,
        }];
        assert!(render_to_string(&r, false).contains("ceiling reached"));
        assert_eq!(exit_code(&r), exit::BUDGET);
    }

    #[test]
    fn a_run_where_every_source_failed_exits_no_data() {
        assert_eq!(exit_code(&run(vec![])), exit::NO_DATA);
    }

    #[test]
    fn custom_fields_change_the_columns() {
        let mut t = Toon::new();
        render(
            &mut t,
            &run(vec![signal("commits", Status::Fail, "2")]),
            &parse_fields(Some("metric,conf,source")).unwrap(),
            false,
        );
        assert!(t.render().contains("issues[1]{metric,conf,source}:"));
        assert!(t.render().contains("commits,100%,s"));
    }

    #[test]
    fn targets_shorten_to_the_distinguishing_part() {
        assert_eq!(short_target("https://github.com/rust-lang/rust"), "rust");
        assert_eq!(short_target("stripe.com"), "stripe.com");
    }

    #[test]
    fn suggested_commands_are_shell_safe() {
        assert_eq!(shell_quote("github.com/a/b"), "github.com/a/b");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
