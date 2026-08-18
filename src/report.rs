//! Self-contained HTML report for a stored run.
//!
//! One file, no external assets, no network. Written for a human to read after the
//! agent has already acted on the TOON — and for `lavish-axi` to open when a person
//! wants to annotate it.

use crate::research::RunResult;
use crate::score::Status;

pub fn render(run: &RunResult) -> String {
    let verdict = run.scored.verdict.as_str();
    let mut s = String::with_capacity(16_384);

    s.push_str(&format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{target} — pilotage {id}</title>
<style>{css}</style>
</head><body>
<main>
<header>
  <div class="eyebrow">pilotage · {pack} · {depth}</div>
  <h1>{target}</h1>
  <div class="verdict v-{verdict}">
    <span class="badge">{verdict}</span>
    <span class="score">{score:.0}<span class="pct">/100</span></span>
  </div>
  <dl class="meta">
    <div><dt>run</dt><dd>{id}</dd></div>
    <div><dt>when</dt><dd>{when}</dd></div>
    <div><dt>duration</dt><dd>{duration} ms</dd></div>
    <div><dt>sources</dt><dd>{ok}/{total} ok · {hits} cached</dd></div>
    <div><dt>coverage</dt><dd>{coverage:.0}%</dd></div>
    <div><dt>confidence</dt><dd>{confidence:.0}%</dd></div>
    <div><dt>cost</dt><dd>${cost:.4}</dd></div>
  </dl>
</header>
"#,
        target = esc(&run.target),
        id = esc(&run.id),
        pack = esc(&run.pack),
        depth = esc(&run.depth),
        verdict = verdict,
        score = run.scored.score * 100.0,
        when = crate::util::iso_datetime(run.created_at),
        duration = run.duration_ms,
        ok = run.ok_sources(),
        total = run.sources.len(),
        hits = run.cache_hits(),
        coverage = run.scored.coverage * 100.0,
        confidence = run.scored.confidence * 100.0,
        cost = run.cost_usd,
        css = CSS,
    ));

    // -- Signals -------------------------------------------------------------
    s.push_str("<section><h2>Signals</h2>");
    if run.scored.signals.is_empty() {
        s.push_str("<p class=\"empty\">No metrics defined for this pack.</p>");
    } else {
        s.push_str("<table><thead><tr><th>metric</th><th>value</th><th>status</th><th>weight</th><th>source</th></tr></thead><tbody>");
        let mut signals: Vec<_> = run.scored.signals.iter().collect();
        // Worst first: the reason someone opened this report is near the top.
        signals.sort_by_key(|s| match s.status {
            Status::Fail => 0,
            Status::Warn => 1,
            Status::Unknown => 2,
            Status::Ok => 3,
        });
        for sig in signals {
            s.push_str(&format!(
                "<tr class=\"s-{st}\"><td><span class=\"dot\"></span>{label}{crit}</td><td class=\"num\">{value}</td><td><span class=\"tag\">{st}</span></td><td class=\"num dim\">{w}</td><td class=\"dim\">{src}</td></tr>",
                st = sig.status.as_str(),
                label = esc(&sig.label),
                crit = if sig.critical { " <span class=\"crit\">critical</span>" } else { "" },
                value = esc(&sig.display),
                w = fmt_num(sig.weight),
                src = match (&sig.source, &sig.source_url) {
                    (Some(id), Some(url)) => format!("<a href=\"{}\">{}</a>", esc(url), esc(id)),
                    (Some(id), None) => esc(id),
                    _ => "—".into(),
                }
            ));
        }
        s.push_str("</tbody></table>");
    }
    s.push_str("</section>");

    // -- Contradictions ------------------------------------------------------
    if !run.contradictions.is_empty() {
        s.push_str("<section><h2>Contradictions</h2><p class=\"note\">Two sources disagree. Pilotage reports the disagreement rather than picking a winner.</p><table><thead><tr><th>key</th><th>source</th><th>value</th><th>source</th><th>value</th></tr></thead><tbody>");
        for c in &run.contradictions {
            s.push_str(&format!(
                "<tr><td>{k}</td><td class=\"dim\">{a}</td><td class=\"num\">{av}</td><td class=\"dim\">{b}</td><td class=\"num\">{bv}</td></tr>",
                k = esc(&c.key),
                a = esc(&c.a_source),
                av = esc(&c.a_value),
                b = esc(&c.b_source),
                bv = esc(&c.b_value)
            ));
        }
        s.push_str("</tbody></table></section>");
    }

    // -- Sources -------------------------------------------------------------
    s.push_str("<section><h2>Sources</h2><table><thead><tr><th>id</th><th>status</th><th>facts</th><th>ms</th><th>bytes</th><th>url</th></tr></thead><tbody>");
    for src in &run.sources {
        s.push_str(&format!(
            "<tr class=\"s-{cls}\"><td><span class=\"dot\"></span>{id}</td><td>{status}{cached}</td><td class=\"num\">{facts}</td><td class=\"num dim\">{ms}</td><td class=\"num dim\">{bytes}</td><td class=\"url\"><a href=\"{url}\">{url}</a></td></tr>",
            cls = if src.ok { "ok" } else { "fail" },
            id = esc(&src.id),
            status = if src.status == 0 { "—".to_string() } else { src.status.to_string() },
            cached = if src.from_cache { " <span class=\"tag\">cached</span>" } else { "" },
            facts = src.facts,
            ms = src.ms,
            bytes = src.bytes,
            url = esc(&src.url)
        ));
    }
    s.push_str("</tbody></table></section>");

    // -- Degraded ------------------------------------------------------------
    if !run.degraded.is_empty() {
        s.push_str("<section><h2>Degraded</h2><p class=\"note\">Everything that did not go to plan. Nothing is dropped silently.</p><ul class=\"degraded\">");
        for d in &run.degraded {
            s.push_str(&format!("<li>{}</li>", esc(d)));
        }
        s.push_str("</ul></section>");
    }

    // -- Facts ---------------------------------------------------------------
    s.push_str("<section><h2>Facts</h2><p class=\"note\">Every value with where it came from and how it was obtained.</p><table><thead><tr><th>key</th><th>value</th><th>method</th><th>conf</th><th>source</th></tr></thead><tbody>");
    let mut facts: Vec<_> = run.facts.iter().collect();
    facts.sort_by(|a, b| a.key.cmp(&b.key));
    for f in facts {
        s.push_str(&format!(
            "<tr><td>{k}</td><td class=\"num\">{v}</td><td><span class=\"tag m-{m}\">{m}</span></td><td class=\"num dim\">{c:.0}%</td><td class=\"dim\"><a href=\"{u}\">{sid}</a></td></tr>",
            k = esc(&f.key),
            v = esc(&crate::extract::llm::truncate_chars(&f.as_display(), 120)),
            m = f.method.as_str(),
            c = f.confidence * 100.0,
            u = esc(&f.source_url),
            sid = esc(&f.source_id)
        ));
    }
    s.push_str("</tbody></table></section>");

    s.push_str(&format!(
        "<footer>generated by pilotage {v} · {calls} model call(s), {tokens} tokens, ${cost:.4}{replay}</footer></main></body></html>",
        v = env!("CARGO_PKG_VERSION"),
        calls = run.llm_calls,
        tokens = run.llm_tokens,
        cost = run.cost_usd,
        replay = if run.replay { " · replay mode" } else { "" }
    ));
    s
}

/// A side-by-side report over several runs.
///
/// Metrics are ordered by weight, so what the packs consider important is what a reader
/// sees first, and every cell is coloured by its status. The closing section names what
/// was *not* measured — a comparison that quietly omits a dimension is worse than one
/// that says the dimension is missing.
pub fn render_comparison(runs: &[RunResult], title: &str) -> String {
    let mut s = String::with_capacity(32_768);

    let when = runs
        .iter()
        .map(|r| r.created_at)
        .max()
        .unwrap_or_else(crate::util::now);
    let packs = {
        let mut p: Vec<&str> = runs.iter().map(|r| r.pack.as_str()).collect();
        p.sort_unstable();
        p.dedup();
        p.join(", ")
    };

    s.push_str(&format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} — pilotage comparison</title>
<style>{css}</style>
</head><body>
<main>
<header>
  <div class="eyebrow">pilotage comparison · {packs}</div>
  <h1>{title}</h1>
  <dl class="meta">
    <div><dt>subjects</dt><dd>{n}</dd></div>
    <div><dt>when</dt><dd>{when}</dd></div>
    <div><dt>total cost</dt><dd>${cost:.4}</dd></div>
  </dl>
</header>
<section class="cards">
"#,
        title = esc(title),
        packs = esc(&packs),
        n = runs.len(),
        when = crate::util::iso_datetime(when),
        cost = runs.iter().map(|r| r.cost_usd).sum::<f64>(),
        css = CSS,
    ));

    for r in runs {
        s.push_str(&format!(
            r#"<div class="card v-{v}">
  <div class="card-name">{target}</div>
  <div class="card-score">{score:.0}<span class="pct">/100</span></div>
  <span class="badge">{v}</span>
  <div class="card-meta">{ok}/{total} sources · {cov:.0}% coverage · {ms}ms</div>
</div>
"#,
            v = r.scored.verdict.as_str(),
            target = esc(&r.target),
            score = r.scored.score * 100.0,
            ok = r.ok_sources(),
            total = r.sources.len(),
            cov = r.scored.coverage * 100.0,
            ms = r.duration_ms,
        ));
    }
    s.push_str("</section>");

    // Union of metric ids, heaviest first — the packs' own weighting decides the
    // reading order rather than declaration order.
    let mut metrics: Vec<(&str, &str, f64)> = Vec::new();
    for r in runs {
        for sig in &r.scored.signals {
            if !metrics.iter().any(|(id, _, _)| *id == sig.id) {
                metrics.push((&sig.id, &sig.label, sig.weight));
            }
        }
    }
    metrics.sort_by(|a, b| b.2.total_cmp(&a.2));

    let head = |s: &mut String, caption: &str, note: &str| {
        s.push_str(&format!("<section><h2>{caption}</h2>"));
        if !note.is_empty() {
            s.push_str(&format!("<p class=\"note\">{note}</p>"));
        }
        s.push_str("<table><thead><tr><th>metric</th><th class=\"num dim\">w</th>");
        for r in runs {
            s.push_str(&format!("<th>{}</th>", esc(&short(&r.target))));
        }
        s.push_str("</tr></thead><tbody>");
    };

    let row = |s: &mut String, id: &str, label: &str, weight: f64| {
        s.push_str(&format!(
            "<tr><td>{}</td><td class=\"num dim\">{}</td>",
            esc(label),
            fmt_num(weight)
        ));
        for r in runs {
            match r.scored.signals.iter().find(|x| x.id == id) {
                Some(sig) => s.push_str(&format!(
                    "<td class=\"cell s-{st}\"><span class=\"dot\"></span>{v}</td>",
                    st = sig.status.as_str(),
                    v = esc(&crate::extract::llm::truncate_chars(&sig.display, 70))
                )),
                None => s.push_str("<td class=\"dim\">—</td>"),
            }
        }
        s.push_str("</tr>");
    };

    let concerns: Vec<&(&str, &str, f64)> = metrics
        .iter()
        .filter(|(id, _, _)| {
            runs.iter().any(|r| {
                r.scored
                    .signals
                    .iter()
                    .any(|x| x.id == *id && matches!(x.status, Status::Fail | Status::Warn))
            })
        })
        .collect();

    if !concerns.is_empty() {
        head(
            &mut s,
            "Where they differ",
            "Metrics on which at least one subject warns or fails. This is the short list.",
        );
        for (id, label, w) in &concerns {
            row(&mut s, id, label, *w);
        }
        s.push_str("</tbody></table></section>");
    }

    head(&mut s, "Every metric", "");
    for (id, label, w) in &metrics {
        row(&mut s, id, label, *w);
    }
    s.push_str("</tbody></table></section>");

    // -- What was not measured ----------------------------------------------
    let mut gaps: Vec<String> = Vec::new();
    for (id, label, _) in &metrics {
        let unknown: Vec<&str> = runs
            .iter()
            .filter(|r| {
                r.scored
                    .signals
                    .iter()
                    .any(|x| x.id == *id && x.status == Status::Unknown)
            })
            .map(|r| r.target.as_str())
            .collect();
        if unknown.len() == runs.len() {
            gaps.push(format!(
                "<strong>{}</strong> — not measured for any subject",
                esc(label)
            ));
        } else if !unknown.is_empty() {
            gaps.push(format!(
                "<strong>{}</strong> — not measured for {}",
                esc(label),
                esc(&unknown.join(", "))
            ));
        }
    }

    let mut degraded: Vec<String> = Vec::new();
    for r in runs {
        for d in &r.degraded {
            degraded.push(format!("{}: {}", esc(&short(&r.target)), esc(d)));
        }
    }

    if !gaps.is_empty() || !degraded.is_empty() {
        s.push_str("<section><h2>Not measured</h2><p class=\"note\">Every gap, named. A comparison that quietly omits a dimension is worse than one that says the dimension is missing.</p>");
        if !gaps.is_empty() {
            s.push_str("<ul class=\"degraded\">");
            for g in &gaps {
                s.push_str(&format!("<li>{g}</li>"));
            }
            s.push_str("</ul>");
        }
        if !degraded.is_empty() {
            s.push_str("<h2>Degraded</h2><ul class=\"degraded\">");
            for d in &degraded {
                s.push_str(&format!("<li>{d}</li>"));
            }
            s.push_str("</ul>");
        }
        s.push_str("</section>");
    }

    // -- Provenance ----------------------------------------------------------
    s.push_str("<section><h2>Sources</h2><p class=\"note\">Where every number above came from.</p><table><thead><tr><th>subject</th><th>source</th><th>status</th><th class=\"num\">facts</th><th class=\"num\">bytes</th><th>url</th></tr></thead><tbody>");
    for r in runs {
        for src in &r.sources {
            s.push_str(&format!(
                "<tr class=\"s-{cls}\"><td class=\"dim\">{t}</td><td><span class=\"dot\"></span>{id}</td><td>{status}{cached}</td><td class=\"num dim\">{facts}</td><td class=\"num dim\">{bytes}</td><td class=\"url\"><a href=\"{url}\">{url}</a></td></tr>",
                cls = if src.ok { "ok" } else { "fail" },
                t = esc(&short(&r.target)),
                id = esc(&src.id),
                status = if src.status == 0 { "—".to_string() } else { src.status.to_string() },
                cached = if src.from_cache { " <span class=\"tag\">cached</span>" } else { "" },
                facts = src.facts,
                bytes = src.bytes,
                url = esc(&src.url),
            ));
        }
    }
    s.push_str("</tbody></table></section>");

    s.push_str(&format!(
        "<footer>generated by pilotage {v} · runs {ids} · {calls} model call(s), ${cost:.4}</footer></main></body></html>",
        v = env!("CARGO_PKG_VERSION"),
        ids = runs.iter().map(|r| r.id.as_str()).collect::<Vec<_>>().join(", "),
        calls = runs.iter().map(|r| r.llm_calls).sum::<u64>(),
        cost = runs.iter().map(|r| r.cost_usd).sum::<f64>(),
    ));
    s
}

/// Column headers cost width in a wide table, so subjects are shortened to the part
/// that distinguishes them.
fn short(target: &str) -> String {
    let n = crate::pack::normalize_target(target);
    n.rsplit('/').next().unwrap_or(&n).to_string()
}

fn fmt_num(n: f64) -> String {
    if n.fract().abs() < 1e-9 {
        format!("{}", n as i64)
    } else {
        format!("{n:.1}")
    }
}

/// Escapes text for HTML. Applied to every interpolated value without exception —
/// source URLs and scraped page titles are attacker-influenced input.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const CSS: &str = r#"
:root{--bg:#fbfbfa;--panel:#fff;--ink:#16181d;--dim:#6b7280;--line:#e5e7eb;
--ok:#15803d;--warn:#b45309;--fail:#b91c1c;--unknown:#6b7280;--accent:#1d4ed8;}
@media (prefers-color-scheme:dark){:root{--bg:#0d0f13;--panel:#14171d;--ink:#e8eaed;
--dim:#9aa3b2;--line:#262b35;--ok:#4ade80;--warn:#fbbf24;--fail:#f87171;--unknown:#8b93a1;--accent:#7aa2f7;}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--ink);
font:15px/1.55 ui-sans-serif,-apple-system,"Segoe UI",Roboto,sans-serif;}
main{max-width:1080px;margin:0 auto;padding:40px 24px 80px}
header{margin-bottom:40px}
.eyebrow{font-size:12px;letter-spacing:.08em;text-transform:uppercase;color:var(--dim)}
h1{font-size:30px;margin:6px 0 18px;font-weight:650;word-break:break-word}
h2{font-size:13px;letter-spacing:.08em;text-transform:uppercase;color:var(--dim);
margin:36px 0 12px;font-weight:600}
.verdict{display:flex;align-items:baseline;gap:16px;margin-bottom:22px}
.badge{display:inline-block;padding:5px 13px;border-radius:999px;font-weight:650;
font-size:13px;letter-spacing:.02em;border:1px solid currentColor}
.score{font-size:34px;font-weight:680;font-variant-numeric:tabular-nums}
.pct{font-size:15px;color:var(--dim);font-weight:400}
.v-healthy .badge,.v-healthy .score{color:var(--ok)}
.v-caution .badge,.v-caution .score{color:var(--warn)}
.v-risk .badge,.v-risk .score{color:var(--fail)}
.v-insufficient-data .badge,.v-insufficient-data .score{color:var(--unknown)}
.meta{display:flex;flex-wrap:wrap;gap:10px 28px;margin:0}
.meta div{margin:0}
.meta dt{font-size:11px;text-transform:uppercase;letter-spacing:.06em;color:var(--dim)}
.meta dd{margin:2px 0 0;font-variant-numeric:tabular-nums;font-weight:550}
section{background:var(--panel);border:1px solid var(--line);border-radius:12px;
padding:4px 20px 18px;margin-bottom:20px;overflow-x:auto}
table{width:100%;border-collapse:collapse;font-size:14px;min-width:560px}
th{text-align:left;font-size:11px;text-transform:uppercase;letter-spacing:.06em;
color:var(--dim);font-weight:600;padding:8px 10px;border-bottom:1px solid var(--line)}
td{padding:9px 10px;border-bottom:1px solid var(--line);vertical-align:top}
tr:last-child td{border-bottom:none}
.num{font-variant-numeric:tabular-nums}
.dim{color:var(--dim)}
.url{max-width:340px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
a{color:var(--accent);text-decoration:none}
a:hover{text-decoration:underline}
.dot{display:inline-block;width:7px;height:7px;border-radius:50%;
margin-right:9px;vertical-align:middle;background:var(--unknown)}
.s-ok .dot{background:var(--ok)}
.s-warn .dot{background:var(--warn)}
.s-fail .dot{background:var(--fail)}
.tag{display:inline-block;padding:1px 7px;border-radius:5px;font-size:11px;
background:var(--bg);border:1px solid var(--line);color:var(--dim)}
.s-fail .tag{color:var(--fail)}
.s-warn .tag{color:var(--warn)}
.s-ok .tag{color:var(--ok)}
.m-llm{color:var(--warn)}
.crit{font-size:10px;text-transform:uppercase;letter-spacing:.06em;color:var(--fail);
border:1px solid currentColor;border-radius:4px;padding:0 4px;margin-left:6px}
.note{color:var(--dim);font-size:13px;margin:4px 0 14px}
.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(200px,100%),1fr));
gap:14px;background:none;border:none;padding:0;margin-bottom:24px}
.card{background:var(--panel);border:1px solid var(--line);border-radius:12px;
padding:16px 18px;min-width:0}
.card-name{font-weight:600;font-size:15px;word-break:break-word;margin-bottom:8px}
.card-score{font-size:28px;font-weight:680;font-variant-numeric:tabular-nums;line-height:1}
.card .badge{margin-top:10px;font-size:11px;padding:3px 10px}
.card-meta{margin-top:10px;font-size:12px;color:var(--dim)}
.v-healthy .card-score,.v-healthy .badge{color:var(--ok)}
.v-caution .card-score,.v-caution .badge{color:var(--warn)}
.v-risk .card-score,.v-risk .badge{color:var(--fail)}
.v-insufficient-data .card-score,.v-insufficient-data .badge{color:var(--unknown)}
.cell{font-variant-numeric:tabular-nums}
strong{font-weight:600;color:var(--ink)}
.empty{color:var(--dim);font-size:14px;padding:8px 0 14px}
.degraded{margin:0 0 12px;padding-left:20px;color:var(--dim);font-size:14px}
.degraded li{margin:3px 0}
footer{margin-top:32px;color:var(--dim);font-size:12px;text-align:center}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::MetricValue;
    use crate::score::{Scored, Signal, Verdict};

    fn run() -> RunResult {
        RunResult {
            id: "r_abc123".into(),
            target: "github.com/a/b".into(),
            pack: "oss".into(),
            created_at: 1_700_000_000,
            duration_ms: 412,
            depth: "quick".into(),
            scored: Scored {
                signals: vec![
                    Signal {
                        id: "commits_90d".into(),
                        label: "commits (90d)".into(),
                        value: MetricValue::Num(2.0),
                        display: "2".into(),
                        status: Status::Fail,
                        weight: 3.0,
                        confidence: 1.0,
                        critical: false,
                        pin: false,
                        source: Some("gh_commits".into()),
                        source_url: Some("https://api.github.com/x".into()),
                    },
                    Signal {
                        id: "stars".into(),
                        label: "stars".into(),
                        value: MetricValue::Num(900.0),
                        display: "900".into(),
                        status: Status::Ok,
                        weight: 0.5,
                        confidence: 1.0,
                        critical: false,
                        pin: false,
                        source: Some("gh_repo".into()),
                        source_url: Some("https://api.github.com/y".into()),
                    },
                ],
                verdict: Verdict::Caution,
                score: 0.62,
                coverage: 0.8,
                confidence: 1.0,
            },
            sources: vec![crate::research::SourceResult {
                id: "gh_repo".into(),
                url: "https://api.github.com/repos/a/b".into(),
                status: 200,
                ok: true,
                from_cache: true,
                facts: 9,
                ms: 3,
                bytes: 4096,
                error: None,
            }],
            contradictions: vec![],
            degraded: vec!["coingecko(404)".into()],
            facts: vec![crate::fact::Fact::new(
                "stars",
                serde_json::json!(900),
                "gh_repo",
                "https://api.github.com/repos/a/b",
                1_700_000_000,
                crate::fact::Method::Api,
            )],
            cost_usd: 0.0031,
            llm_calls: 1,
            llm_tokens: 2400,
            budget_blocked: false,
            replay: false,
        }
    }

    #[test]
    fn report_is_a_complete_self_contained_document() {
        let html = render(&run());
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.trim_end().ends_with("</html>"));
        // The report must open offline: no scripts, no remote stylesheets, no fonts.
        for forbidden in ["<script", "src=", "@import", "<link"] {
            assert!(
                !html.contains(forbidden),
                "report must not contain `{forbidden}`"
            );
        }
    }

    #[test]
    fn report_contains_the_verdict_signals_and_provenance() {
        let html = render(&run());
        assert!(html.contains("caution"));
        assert!(html.contains("commits (90d)"));
        assert!(
            html.contains("coingecko(404)"),
            "degraded entries must be visible"
        );
        assert!(html.contains("api.github.com/repos/a/b"));
    }

    #[test]
    fn worst_signals_are_listed_first() {
        let html = render(&run());
        let fail_at = html.find("commits (90d)").unwrap();
        let ok_at = html.rfind(">stars<").unwrap();
        assert!(
            fail_at < ok_at,
            "failing signals must sort above healthy ones"
        );
    }

    #[test]
    fn scraped_content_cannot_inject_markup() {
        let mut r = run();
        r.target = "<script>alert(1)</script>".into();
        r.degraded = vec!["<img onerror=x>".into()];
        let html = render(&r);
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(!html.contains("<img onerror=x>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    /// Two runs where one has a metric the other lacks, and one metric nobody could
    /// measure — the shape a real comparison always has.
    fn comparison_runs() -> Vec<RunResult> {
        let mut a = run();
        a.target = "heygen.com".into();
        a.scored.signals.push(Signal {
            id: "cost_per_minute".into(),
            label: "$ per minute of video".into(),
            value: MetricValue::Missing,
            display: "none".into(),
            status: Status::Unknown,
            weight: 3.0,
            confidence: 0.0,
            critical: false,
            pin: false,
            source: None,
            source_url: None,
        });

        let mut b = run();
        b.id = "r_def456".into();
        b.target = "synthesia.io".into();
        b.scored.verdict = Verdict::Healthy;
        b.scored.score = 0.91;
        b.scored.signals = vec![
            Signal {
                id: "commits_90d".into(),
                label: "commits (90d)".into(),
                value: MetricValue::Num(80.0),
                display: "80".into(),
                status: Status::Ok,
                weight: 3.0,
                confidence: 1.0,
                critical: false,
                pin: false,
                source: Some("gh_commits".into()),
                source_url: Some("https://api.github.com/x".into()),
            },
            Signal {
                id: "cost_per_minute".into(),
                label: "$ per minute of video".into(),
                value: MetricValue::Missing,
                display: "none".into(),
                status: Status::Unknown,
                weight: 3.0,
                confidence: 0.0,
                critical: false,
                pin: false,
                source: None,
                source_url: None,
            },
        ];
        b.degraded = vec!["trust(404)".into()];
        vec![a, b]
    }

    #[test]
    fn a_comparison_is_a_complete_self_contained_document() {
        let html = render_comparison(&comparison_runs(), "HeyGen vs alternatives");
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.trim_end().ends_with("</html>"));
        for forbidden in ["<script", "src=", "@import", "<link"] {
            assert!(!html.contains(forbidden), "must not contain `{forbidden}`");
        }
        assert!(html.contains("HeyGen vs alternatives"));
    }

    #[test]
    fn a_comparison_gives_every_subject_a_card_and_a_column() {
        let html = render_comparison(&comparison_runs(), "t");
        assert!(html.contains("heygen.com") && html.contains("synthesia.io"));
        assert_eq!(html.matches("class=\"card v-").count(), 2);
    }

    #[test]
    fn metrics_are_ordered_by_weight_so_the_important_ones_read_first() {
        let html = render_comparison(&comparison_runs(), "t");
        let body = html.split("Every metric").nth(1).unwrap();
        let heavy = body.find("commits (90d)").unwrap();
        let light = body.find(">stars<").unwrap();
        assert!(heavy < light, "weight 3 must sort above weight 0.5");
    }

    #[test]
    fn a_metric_one_subject_lacks_renders_as_a_dash_not_a_wrong_value() {
        // `stars` exists only on the first run.
        let html = render_comparison(&comparison_runs(), "t");
        assert!(html.contains("—"), "missing cells must be explicit");
    }

    #[test]
    fn unmeasured_dimensions_are_named_rather_than_omitted() {
        let html = render_comparison(&comparison_runs(), "t");
        let gaps = html.split("Not measured").nth(1).unwrap();
        assert!(
            gaps.contains("$ per minute of video") && gaps.contains("not measured for any subject"),
            "a dimension nobody could measure must be called out"
        );
        assert!(
            gaps.contains("trust(404)"),
            "degradation must survive into the report"
        );
    }

    #[test]
    fn a_comparison_carries_provenance_for_every_subject() {
        let html = render_comparison(&comparison_runs(), "t");
        let sources = html.split(">Sources<").nth(1).unwrap();
        assert!(sources.contains("api.github.com/repos/a/b"));
    }

    #[test]
    fn a_comparison_cannot_be_injected_through_a_scraped_title() {
        let mut runs = comparison_runs();
        runs[0].target = "<script>alert(1)</script>".into();
        let html = render_comparison(&runs, "<img onerror=x>");
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(!html.contains("<img onerror=x>"));
    }

    #[test]
    fn escaping_covers_every_dangerous_character() {
        assert_eq!(
            esc(r#"<a href="x">&'"#),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }
}
