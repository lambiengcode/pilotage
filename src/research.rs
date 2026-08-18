//! The research run: select a pack, fetch everything at once, extract, score, store.
//!
//! Fetching is concurrent because it is the only part that waits on the world.
//! Extraction is sequential and ordered by source weight, which costs nothing (it is
//! CPU work on already-downloaded bytes) and buys an exact budget policy: when the
//! ceiling is reached, it is always the least important source that goes unextracted.

use crate::cache::{Cache, RunRow};
use crate::extract::{self, Budget, Llm, Usage};
use crate::fact::{Contradiction, Fact};
use crate::fetch::Fetcher;
use crate::pack::{Depth, Pack, Source};
use crate::score::{self, Scored};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Options {
    pub target: String,
    pub pack: Option<String>,
    pub depth: Depth,
    pub budget: Option<f64>,
    pub no_cache: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            target: String::new(),
            pack: None,
            depth: Depth::Quick,
            budget: None,
            no_cache: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceResult {
    pub id: String,
    pub url: String,
    pub status: u16,
    pub ok: bool,
    pub from_cache: bool,
    pub facts: usize,
    pub ms: u64,
    pub bytes: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub id: String,
    pub target: String,
    pub pack: String,
    pub created_at: i64,
    pub duration_ms: u64,
    pub depth: String,
    pub scored: Scored,
    pub sources: Vec<SourceResult>,
    pub contradictions: Vec<Contradiction>,
    /// Everything that did not go to plan, named. Never a silent omission.
    pub degraded: Vec<String>,
    pub facts: Vec<Fact>,
    pub cost_usd: f64,
    pub llm_calls: u64,
    pub llm_tokens: u64,
    /// True when the cost ceiling stopped work before the run finished.
    pub budget_blocked: bool,
    pub replay: bool,
}

impl RunResult {
    pub fn cache_hits(&self) -> usize {
        self.sources.iter().filter(|s| s.from_cache).count()
    }
    pub fn ok_sources(&self) -> usize {
        self.sources.iter().filter(|s| s.ok).count()
    }
    pub fn to_row(&self) -> Result<RunRow> {
        Ok(RunRow {
            id: self.id.clone(),
            target: self.target.clone(),
            pack: self.pack.clone(),
            created_at: self.created_at,
            duration_ms: self.duration_ms as i64,
            verdict: self.scored.verdict.as_str().to_string(),
            score: self.scored.score,
            coverage: self.scored.coverage,
            cost_usd: self.cost_usd,
            payload: serde_json::to_string(self)?,
        })
    }
    pub fn from_row(row: &RunRow) -> Result<RunResult> {
        serde_json::from_str(&row.payload)
            .with_context(|| format!("run {} has an unreadable payload", row.id))
    }
}

/// Chooses the pack for a target. An explicit `--pack` wins; otherwise the pack whose
/// detect patterns make the most specific claim.
pub fn resolve_pack<'a>(
    packs: &'a [Pack],
    target: &str,
    forced: Option<&str>,
) -> Result<(&'a Pack, std::collections::HashMap<String, String>)> {
    match forced {
        Some(name) => {
            let idx = crate::pack::find(packs, name).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown pack '{name}' (available: {})",
                    packs
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            let pack = &packs[idx];
            let vars = pack.vars_for(target, true).ok_or_else(|| {
                anyhow::anyhow!("pack '{name}' could not derive variables for '{target}'")
            })?;
            Ok((pack, vars))
        }
        None => crate::pack::select_for(packs, target).ok_or_else(|| {
            anyhow::anyhow!(
                "no pack recognises '{target}'. Force one with --pack <{}>",
                packs
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join("|")
            )
        }),
    }
}

pub async fn run(packs: &[Pack], cache: Arc<Cache>, opts: &Options) -> Result<RunResult> {
    let started = std::time::Instant::now();
    let (pack, vars) = resolve_pack(packs, &opts.target, opts.pack.as_deref())?;

    let fetcher = Fetcher::new(cache.clone())?;
    let llm = Llm::new()?;
    let budget = Budget::new(opts.budget);

    let selected: Vec<&Source> = pack
        .source
        .iter()
        .filter(|s| s.depth <= opts.depth)
        .collect();

    if selected.is_empty() {
        bail!(
            "pack '{}' has no sources at depth {:?}",
            pack.name,
            opts.depth
        );
    }

    let mut degraded: Vec<String> = Vec::new();
    let mut unauthenticated: std::collections::BTreeMap<String, Vec<String>> = Default::default();

    // -- Fetch, all at once --------------------------------------------------
    let fetches = selected.iter().map(|src| {
        let url = crate::util::template(&src.url, &vars);
        let mut headers = src.headers.clone();
        let mut auth_missing = false;
        if let Some(env_key) = &src.auth_env {
            match std::env::var(env_key) {
                Ok(v) if !v.trim().is_empty() => {
                    headers.insert(
                        src.auth_header.clone(),
                        src.auth_format.replace("{}", v.trim()),
                    );
                }
                // A missing optional credential is not an error: most of these APIs
                // serve anonymous requests at a lower rate limit.
                _ => auth_missing = true,
            }
        }
        // The credential gate has to be enforced *here*, before the request is built.
        // A `kind = "probe"` source spends the vendor's credits on submission, so
        // "skip it and report why" must mean the socket is never opened — checking
        // after the fetch would bill the user for a source that was then discarded.
        let gated = auth_missing && src.auth_required;
        let missing_key = src.auth_env.clone().unwrap_or_else(|| "?".into());

        let ttl = crate::util::parse_duration(&src.ttl).unwrap_or(3_600);
        let ttl = if opts.no_cache { 0 } else { ttl };
        let fetcher = &fetcher;
        async move {
            if gated {
                return (
                    src.id.clone(),
                    url,
                    Err(anyhow::anyhow!("{missing_key} not set")),
                    0,
                    auth_missing,
                );
            }
            let t = std::time::Instant::now();
            let res = fetcher.get(&url, ttl, &headers, src).await;
            (
                src.id.clone(),
                url,
                res,
                t.elapsed().as_millis() as u64,
                auth_missing,
            )
        }
    });

    let fetched = futures::future::join_all(fetches).await;

    // -- Extract, most important source first --------------------------------
    // Sequential on purpose: this is CPU work on bytes already in hand, and running
    // it in weight order makes the budget policy exact rather than a race.
    let mut order: Vec<usize> = (0..selected.len()).collect();
    order.sort_by(|a, b| selected[*b].weight.total_cmp(&selected[*a].weight));

    let mut source_results: Vec<SourceResult> = Vec::with_capacity(selected.len());
    let mut facts: Vec<Fact> = Vec::new();
    let mut usage = Usage::default();
    let mut ran: HashSet<String> = HashSet::new();

    for i in order {
        let src = selected[i];
        let (id, url, result, ms, auth_missing) = &fetched[i];

        if *auth_missing {
            if src.auth_required {
                degraded.push(format!(
                    "{id}(auth: {} not set)",
                    src.auth_env.as_deref().unwrap_or("?")
                ));
                source_results.push(SourceResult {
                    id: id.clone(),
                    url: url.clone(),
                    status: 0,
                    ok: false,
                    from_cache: false,
                    facts: 0,
                    ms: *ms,
                    bytes: 0,
                    error: Some(format!(
                        "{} is not set",
                        src.auth_env.as_deref().unwrap_or("?")
                    )),
                });
                continue;
            }
            // Collapsed rather than repeated: one missing token typically affects
            // every source in the pack, and saying so five times is five times the
            // tokens for the same fact.
            unauthenticated
                .entry(src.auth_env.clone().unwrap_or_else(|| "?".into()))
                .or_default()
                .push(id.clone());
        }

        let doc = match result {
            Ok(d) => d,
            Err(e) => {
                degraded.push(format!("{id}({}){}", short_error(e), required_marker(src)));
                source_results.push(SourceResult {
                    id: id.clone(),
                    url: url.clone(),
                    status: 0,
                    ok: false,
                    from_cache: false,
                    facts: 0,
                    ms: *ms,
                    bytes: 0,
                    error: Some(e.to_string()),
                });
                continue;
            }
        };

        if !doc.is_success() {
            degraded.push(format!("{id}({}){}", doc.status, required_marker(src)));
            source_results.push(SourceResult {
                id: id.clone(),
                url: url.clone(),
                status: doc.status,
                ok: false,
                from_cache: doc.from_cache,
                facts: 0,
                ms: *ms,
                bytes: doc.body.len(),
                error: Some(format!("HTTP {}", doc.status)),
            });
            continue;
        }

        match extract::extract_source(src, doc, &cache, &llm, &budget).await {
            Ok(ex) => {
                ran.insert(id.clone());
                usage.merge(ex.usage);
                degraded.extend(ex.notes);
                let n = ex.facts.len();
                facts.extend(ex.facts);
                source_results.push(SourceResult {
                    id: id.clone(),
                    url: url.clone(),
                    status: doc.status,
                    ok: true,
                    from_cache: doc.from_cache,
                    facts: n,
                    ms: *ms,
                    bytes: doc.body.len(),
                    error: None,
                });
            }
            Err(e) => {
                degraded.push(format!("{id}(extract: {})", short_error(&e)));
                source_results.push(SourceResult {
                    id: id.clone(),
                    url: url.clone(),
                    status: doc.status,
                    ok: false,
                    from_cache: doc.from_cache,
                    facts: 0,
                    ms: *ms,
                    bytes: doc.body.len(),
                    error: Some(e.to_string()),
                });
            }
        }
    }

    for (env_key, ids) in unauthenticated {
        degraded.push(format!(
            "unauthenticated({env_key} unset, {} source(s) at the anonymous rate limit)",
            ids.len()
        ));
    }

    // Restore declaration order so output is stable regardless of weights.
    source_results.sort_by_key(|r| {
        selected
            .iter()
            .position(|s| s.id == r.id)
            .unwrap_or(usize::MAX)
    });

    let contradictions = crate::fact::find_contradictions(&facts, pack.tolerance);
    let scored = score::score(pack, &facts, &ran);

    Ok(RunResult {
        id: crate::util::new_run_id(),
        target: opts.target.clone(),
        pack: pack.name.clone(),
        created_at: crate::util::now(),
        duration_ms: started.elapsed().as_millis() as u64,
        depth: format!("{:?}", opts.depth).to_lowercase(),
        scored,
        sources: source_results,
        contradictions,
        degraded,
        facts,
        cost_usd: usage.cost_usd,
        llm_calls: usage.calls,
        llm_tokens: usage.in_tokens + usage.out_tokens,
        budget_blocked: budget.was_blocked(),
        replay: fetcher.is_replay(),
    })
}

/// Persists a run so `report`, `compare` and `watch` can work from stored results
/// instead of researching again.
pub fn save(cache: &Cache, run: &RunResult) -> Result<()> {
    cache.save_run(&run.to_row()?)
}

/// Marks a failure the pack did not anticipate. An optional source going missing is
/// expected (no CoinGecko listing, no /pricing page); a required one failing means
/// the run is thinner than it should be, and that difference is worth one character.
fn required_marker(src: &Source) -> &'static str {
    if src.optional { "" } else { " required" }
}

/// Error text for a `degraded` entry: the root cause on one line.
///
/// The outer context frames name the URL, which the source id already implies —
/// the innermost cause is the part that says what actually went wrong.
fn short_error(e: &anyhow::Error) -> String {
    let root = e
        .chain()
        .next_back()
        .map(|c| c.to_string())
        .unwrap_or_else(|| e.to_string());
    let line = root.lines().next().unwrap_or(&root).trim();
    crate::extract::llm::truncate_chars(if line.is_empty() { &root } else { line }, 90)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::load_all;

    #[test]
    fn detection_picks_the_most_specific_pack() {
        let packs = load_all().unwrap();
        let (p, vars) = resolve_pack(&packs, "https://github.com/rust-lang/rust", None).unwrap();
        assert_eq!(p.name, "oss");
        assert_eq!(vars["owner"], "rust-lang");

        let (p, _) = resolve_pack(&packs, "stripe.com", None).unwrap();
        assert_eq!(p.name, "saas");

        let (p, vars) = resolve_pack(&packs, "defillama.com/protocol/aave", None).unwrap();
        assert_eq!(p.name, "crypto");
        assert_eq!(vars["slug"], "aave");
    }

    #[test]
    fn a_forced_pack_fills_in_the_variables_it_expects() {
        let packs = load_all().unwrap();
        // A bare protocol name matches no crypto pattern, but forcing the pack
        // should still work rather than making the user learn its variable names.
        let (p, vars) = resolve_pack(&packs, "aave", Some("crypto")).unwrap();
        assert_eq!(p.name, "crypto");
        assert_eq!(vars["slug"], "aave");
    }

    #[test]
    fn an_unknown_pack_name_lists_the_real_ones() {
        let packs = load_all().unwrap();
        let err = resolve_pack(&packs, "x", Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("oss") && err.contains("crypto") && err.contains("saas"),
            "{err}"
        );
    }

    #[test]
    fn an_unrecognised_target_says_how_to_force_a_pack() {
        let packs = load_all().unwrap();
        let err = resolve_pack(&packs, "just some words", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--pack"), "{err}");
    }

    #[test]
    fn date_windows_are_available_to_url_templates() {
        let packs = load_all().unwrap();
        let (_, vars) = resolve_pack(&packs, "github.com/a/b", None).unwrap();
        assert_eq!(vars["since_90d"].len(), 10);
        assert!(vars["since_90d"] < vars["today"]);
    }

    #[test]
    fn degraded_entries_show_the_root_cause_not_the_context_frame() {
        let e = anyhow::anyhow!("connection refused")
            .context("fetching https://api.example.com/a/b?x=1");
        assert_eq!(short_error(&e), "connection refused");
    }

    #[test]
    fn a_url_in_the_message_survives_intact() {
        // An earlier version split on ':' to strip context and mangled every URL.
        let e = anyhow::anyhow!("robots.txt disallows https://api.example.com/v3/coins");
        assert_eq!(
            short_error(&e),
            "robots.txt disallows https://api.example.com/v3/coins"
        );
    }

    #[test]
    fn errors_are_one_bounded_line() {
        let e = anyhow::anyhow!("{}\nsecond line", "y".repeat(500));
        let s = short_error(&e);
        assert!(s.chars().count() <= 90, "got {} chars", s.chars().count());
        assert!(!s.contains('\n'));
    }
}
