//! The extraction ladder, and the cache that makes it cheap.
//!
//! Rungs run in order and stop paying as early as possible:
//!
//! 1. JSON field map — free, instant
//! 2. CSS / regex selectors — free, instant
//! 3. cheap-model extraction — only for fields the rungs above could not reach
//!
//! Results are keyed by `(content_hash, extractor_version)`. Identical bytes through
//! an identical extractor cannot produce a different answer, so that entry never
//! expires — which is what makes re-running research on an unchanged page free even
//! when a model was involved the first time.

pub mod det;
pub mod llm;

pub use llm::{Budget, Llm, Usage};

use crate::cache::Cache;
use crate::fact::{Fact, Method};
use crate::fetch::Fetched;
use crate::pack::{Kind, Source};
use anyhow::{Context, Result};
use serde_json::Value;

/// Bumped when extraction semantics change in a way that invalidates every cached
/// result, independently of any individual pack edit.
const EXTRACTOR_EPOCH: &str = "1";

#[derive(Debug, Default)]
pub struct Extraction {
    pub facts: Vec<Fact>,
    pub usage: Usage,
    /// Anything the caller must be told about: a rung that could not run, a model
    /// call that failed. These become `degraded` entries in the output — a skipped
    /// rung is never silent.
    pub notes: Vec<String>,
    pub from_cache: bool,
}

/// Identity of a source's extraction rules. Any change to a path, selector or model
/// field produces a new version, which invalidates exactly the affected cache
/// entries and leaves every other source's cached work intact.
pub fn source_version(src: &Source) -> String {
    let mut parts = vec![
        format!("epoch={EXTRACTOR_EPOCH}"),
        format!("kind={:?}", src.kind),
    ];

    let mut map: Vec<_> = src.map.iter().collect();
    map.sort();
    for (k, v) in map {
        parts.push(format!("map:{k}={v}"));
    }

    let mut sel: Vec<_> = src.select.iter().collect();
    sel.sort_by_key(|(k, _)| *k);
    for (k, s) in sel {
        parts.push(format!(
            "sel:{k}=css({});attr({});all({});re({})",
            s.css.as_deref().unwrap_or(""),
            s.attr.as_deref().unwrap_or(""),
            s.all,
            s.regex.as_deref().unwrap_or("")
        ));
    }

    // A probe's request shape is part of its identity: change the body and you have
    // measured a different thing, so the cached measurement must not be reused.
    parts.push(format!("method={}", src.method.to_ascii_uppercase()));
    if let Some(b) = &src.body {
        parts.push(format!("body={}", crate::util::content_hash(b.as_bytes())));
    }
    if let Some(p) = &src.poll {
        parts.push(format!(
            "poll:url({});id({});status({});done({});failed({})",
            p.url,
            p.id_path.as_deref().unwrap_or(""),
            p.status_path,
            p.done.join("|"),
            p.failed.join("|")
        ));
    }

    if let Some(e) = &src.extract {
        let mut fields = e.fields.clone();
        fields.sort();
        parts.push(format!(
            "llm:model({});fields({});max({});prompt({})",
            e.model,
            fields.join("|"),
            e.max_chars,
            e.prompt.as_deref().unwrap_or("")
        ));
    }

    crate::util::content_hash(parts.join("\n").as_bytes())
}

/// Runs the ladder for one fetched document.
pub async fn extract_source(
    src: &Source,
    fetched: &Fetched,
    cache: &Cache,
    llm: &Llm,
    budget: &Budget,
) -> Result<Extraction> {
    let version = source_version(src);

    if let Some(payload) = cache.get_facts(&fetched.content_hash, &version)? {
        return Ok(Extraction {
            facts: decode_facts(&payload, src, fetched),
            from_cache: true,
            ..Default::default()
        });
    }

    let mut out = Extraction::default();
    let body = fetched.text();

    // -- Rungs 1 and 2 -------------------------------------------------------
    match src.kind {
        // A probe's synthesised document and an API's response are both JSON read
        // with a field map — the probe rung needs no extraction machinery of its own.
        Kind::Api | Kind::Probe => {
            let doc: Value = serde_json::from_slice(&fetched.body).with_context(|| {
                format!("source '{}' returned something that is not JSON", src.id)
            })?;
            for (key, path) in &src.map {
                match det::json_extract(&doc, path) {
                    Ok(Some(v)) => out.facts.push(fact(key, v, src, fetched, Method::Api)),
                    Ok(None) => {}
                    Err(e) => out.notes.push(format!("{}:{key} path error: {e}", src.id)),
                }
            }
        }
        Kind::Html | Kind::Browser => {
            for (key, sel) in &src.select {
                match det::select_extract(&body, sel) {
                    Ok(Some(v)) => out.facts.push(fact(key, v, src, fetched, Method::Parse)),
                    Ok(None) => {}
                    Err(e) => out
                        .notes
                        .push(format!("{}:{key} selector error: {e}", src.id)),
                }
            }
        }
    }

    // -- Rung 3 --------------------------------------------------------------
    // `complete` decides whether this extraction is safe to cache forever. A rung
    // that was skipped for a missing key or an exhausted budget must NOT be frozen
    // into the cache, or configuring the key later would never take effect.
    let mut complete = true;

    if let Some(cfg) = &src.extract {
        let already: std::collections::HashSet<&str> =
            out.facts.iter().map(|f| f.key.as_str()).collect();
        let wanted: Vec<String> = cfg
            .fields
            .iter()
            .filter(|f| !already.contains(f.as_str()))
            .cloned()
            .collect();

        if wanted.is_empty() {
            // Selectors already reached everything — the cheapest possible outcome.
        } else if !llm.available(&cfg.model) {
            complete = false;
            out.notes.push(format!(
                "{}: model extraction skipped, tier '{}' is unconfigured ({} field(s) unresolved)",
                src.id,
                cfg.model,
                wanted.len()
            ));
        } else {
            let content = match src.kind {
                Kind::Html => llm::html_to_text(&body),
                _ => body.clone(),
            };
            match llm
                .extract_fields(
                    &cfg.model,
                    cfg.prompt.as_deref(),
                    &wanted,
                    &content,
                    cfg.max_chars,
                    budget,
                )
                .await
            {
                Ok((value, usage)) => {
                    out.usage.merge(usage);
                    if let Value::Object(obj) = value {
                        for (k, v) in obj {
                            out.facts.push(fact(&k, v, src, fetched, Method::Llm));
                        }
                    }
                }
                Err(e) => {
                    complete = false;
                    // Bounded to one short line: a provider error body can be a
                    // multi-line JSON blob, and pasting it whole into the output would
                    // cost the caller more tokens than the extraction would have.
                    out.notes.push(format!(
                        "{}: model extraction failed: {}",
                        src.id,
                        one_line(&e)
                    ));
                }
            }
        }
    }

    if complete {
        cache.put_facts(&fetched.content_hash, &version, &encode_facts(&out.facts))?;
    }
    Ok(out)
}

/// Collapses an error chain to one bounded, credential-free line for `degraded`.
///
/// Providers echo the offending key back in their 401 bodies — masked, but a masked key
/// is still a credential fragment, and `degraded` ends up in reports that get shared.
/// Anything key-shaped is redacted before it can travel.
fn one_line(e: &anyhow::Error) -> String {
    let text = e
        .chain()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(": ");
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let redacted = redact_secrets(&flat);
    llm::truncate_chars(&redacted, 140)
}

/// Replaces anything that looks like an API key with a placeholder.
fn redact_secrets(s: &str) -> String {
    // Common prefixes, plus any long unbroken run of key-ish characters.
    let patterns = [
        r"\b(?:sk|pk|rk|api|key|tok|ghp|gho|xox[a-z])[-_][A-Za-z0-9_\-*]{8,}",
        r"\b[A-Za-z0-9_\-]{32,}\b",
    ];
    let mut out = s.to_string();
    for p in patterns {
        if let Ok(re) = regex::Regex::new(p) {
            out = re.replace_all(&out, "[redacted]").into_owned();
        }
    }
    out
}

fn fact(key: &str, value: Value, src: &Source, fetched: &Fetched, method: Method) -> Fact {
    Fact::new(
        key,
        value,
        &src.id,
        &fetched.url,
        fetched.fetched_at,
        method,
    )
}

fn encode_facts(facts: &[Fact]) -> Value {
    Value::Array(
        facts
            .iter()
            .map(|f| serde_json::json!({ "k": f.key, "v": f.value, "m": f.method }))
            .collect(),
    )
}

/// Rebuilds facts from a cache entry, restamping provenance with the current fetch.
/// The values are cached; where they came from and when is not.
fn decode_facts(payload: &Value, src: &Source, fetched: &Fetched) -> Vec<Fact> {
    payload
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let key = item.get("k")?.as_str()?;
                    let value = item.get("v")?.clone();
                    let method = serde_json::from_value::<Method>(item.get("m")?.clone()).ok()?;
                    Some(fact(key, value, src, fetched, method))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::{Pack, Selector};

    fn fetched(body: &str) -> Fetched {
        Fetched {
            url: "https://x.test/a".into(),
            status: 200,
            body: body.as_bytes().to_vec(),
            content_hash: crate::util::content_hash(body.as_bytes()),
            fetched_at: 1_700_000_000,
            from_cache: false,
        }
    }

    fn api_source(map: &[(&str, &str)]) -> Source {
        let toml = format!(
            "name='t'\ndetect=['{{a}}']\n[[source]]\nid='s'\nurl='https://x.test/a'\n[source.map]\n{}\n",
            map.iter()
                .map(|(k, v)| format!("{k} = '{v}'"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        Pack::parse(&toml).unwrap().source.pop().unwrap()
    }

    #[test]
    fn credentials_never_reach_the_degraded_list() {
        // Synthetic on purpose: a fixture must never carry a fragment of a real key,
        // because test files are the most public part of a repository.
        let e = anyhow::anyhow!(
            "model returned HTTP 401: Incorrect API key provided: sk-EXAMPLE-0000000000000000"
        );
        let line = one_line(&e);
        assert!(!line.contains("sk-EXAMPLE"), "key must be redacted: {line}");
        assert!(line.contains("[redacted]"), "{line}");
        assert!(line.contains("401"), "the useful part must survive: {line}");
    }

    #[test]
    fn redaction_leaves_ordinary_error_text_alone() {
        assert_eq!(
            redact_secrets("connection refused after 3 attempts"),
            "connection refused after 3 attempts"
        );
        assert_eq!(
            redact_secrets("robots.txt disallows /api/v3"),
            "robots.txt disallows /api/v3"
        );
    }

    #[test]
    fn degraded_lines_stay_bounded_and_single_line() {
        let e = anyhow::anyhow!("{}\nsecond line", "x".repeat(500));
        let line = one_line(&e);
        assert!(line.chars().count() <= 140);
        assert!(!line.contains('\n'));
    }

    #[tokio::test]
    async fn rung_one_reads_json_fields_at_no_cost() {
        let cache = Cache::open_in_memory().unwrap();
        let llm = Llm::new().unwrap();
        let budget = Budget::unlimited();
        let src = api_source(&[
            ("stars", ".stargazers_count"),
            ("license", ".license.spdx_id"),
        ]);

        let ex = extract_source(
            &src,
            &fetched(r#"{"stargazers_count": 42, "license": {"spdx_id": "MIT"}}"#),
            &cache,
            &llm,
            &budget,
        )
        .await
        .unwrap();

        assert_eq!(ex.facts.len(), 2);
        assert_eq!(ex.usage.cost_usd, 0.0);
        assert!(ex.notes.is_empty());
        let stars = ex.facts.iter().find(|f| f.key == "stars").unwrap();
        assert_eq!(stars.as_f64(), Some(42.0));
        assert_eq!(stars.method, Method::Api);
        assert_eq!(stars.source_url, "https://x.test/a");
    }

    #[tokio::test]
    async fn absent_fields_are_simply_not_produced() {
        let cache = Cache::open_in_memory().unwrap();
        let src = api_source(&[
            ("stars", ".stargazers_count"),
            ("license", ".license.spdx_id"),
        ]);
        let ex = extract_source(
            &src,
            &fetched(r#"{"stargazers_count": 1, "license": null}"#),
            &cache,
            &Llm::new().unwrap(),
            &Budget::unlimited(),
        )
        .await
        .unwrap();
        assert_eq!(ex.facts.len(), 1);
        assert_eq!(ex.facts[0].key, "stars");
    }

    #[tokio::test]
    async fn a_non_json_api_response_is_an_error_not_an_empty_result() {
        let cache = Cache::open_in_memory().unwrap();
        let src = api_source(&[("stars", ".stargazers_count")]);
        let err = extract_source(
            &src,
            &fetched("<html>rate limited</html>"),
            &cache,
            &Llm::new().unwrap(),
            &Budget::unlimited(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not JSON"));
    }

    #[tokio::test]
    async fn identical_bytes_are_served_from_the_extraction_cache() {
        let cache = Cache::open_in_memory().unwrap();
        let llm = Llm::new().unwrap();
        let budget = Budget::unlimited();
        let src = api_source(&[("stars", ".stargazers_count")]);
        let doc = fetched(r#"{"stargazers_count": 7}"#);

        let first = extract_source(&src, &doc, &cache, &llm, &budget)
            .await
            .unwrap();
        assert!(!first.from_cache);

        let second = extract_source(&src, &doc, &cache, &llm, &budget)
            .await
            .unwrap();
        assert!(second.from_cache);
        assert_eq!(second.facts.len(), 1);
        assert_eq!(second.facts[0].as_f64(), Some(7.0));
    }

    #[test]
    fn changing_a_rule_changes_the_version_and_invalidates_only_that_source() {
        let a = api_source(&[("stars", ".stargazers_count")]);
        let b = api_source(&[("stars", ".watchers_count")]);
        let a_again = api_source(&[("stars", ".stargazers_count")]);
        assert_ne!(source_version(&a), source_version(&b));
        assert_eq!(source_version(&a), source_version(&a_again));
    }

    #[test]
    fn version_is_stable_regardless_of_map_ordering() {
        let a = api_source(&[("x", ".x"), ("y", ".y")]);
        let b = api_source(&[("y", ".y"), ("x", ".x")]);
        assert_eq!(source_version(&a), source_version(&b));
    }

    #[test]
    fn changing_a_selector_changes_the_version() {
        let mut src = api_source(&[("k", ".k")]);
        let v1 = source_version(&src);
        src.select.insert(
            "title".into(),
            Selector {
                css: Some("title".into()),
                attr: None,
                all: false,
                regex: None,
            },
        );
        let v2 = source_version(&src);
        src.select.get_mut("title").unwrap().css = Some("h1".into());
        assert_ne!(v1, v2);
        assert_ne!(v2, source_version(&src));
    }

    #[tokio::test]
    async fn an_unconfigured_model_tier_degrades_loudly_and_is_not_cached() {
        // Freezing "the model was unavailable" into a permanent cache entry would
        // mean configuring a key later silently had no effect.
        let cache = Cache::open_in_memory().unwrap();
        let pack = Pack::parse(
            r#"
name='t'
detect=['{a}']
[[source]]
id='s'
kind='html'
url='https://x.test/a'
[source.select.title]
css = "title"
[source.extract]
model = "definitely-not-a-tier"
fields = ["pricing_model"]
"#,
        )
        .unwrap();
        let src = &pack.source[0];
        let doc = fetched("<html><head><title>Hi</title></head><body></body></html>");

        let ex = extract_source(
            src,
            &doc,
            &cache,
            &Llm::new().unwrap(),
            &Budget::unlimited(),
        )
        .await
        .unwrap();

        assert_eq!(ex.facts.len(), 1, "selector rung still produced its fact");
        assert!(
            ex.notes.iter().any(|n| n.contains("unconfigured")),
            "{:?}",
            ex.notes
        );
        assert!(
            cache
                .get_facts(&doc.content_hash, &source_version(src))
                .unwrap()
                .is_none(),
            "an incomplete extraction must not be cached"
        );
    }

    #[tokio::test]
    async fn selectors_reaching_every_field_means_the_model_is_never_called() {
        let cache = Cache::open_in_memory().unwrap();
        let pack = Pack::parse(
            r#"
name='t'
detect=['{a}']
[[source]]
id='s'
kind='html'
url='https://x.test/a'
[source.select.pricing_model]
css = "div.model"
[source.extract]
model = "definitely-not-a-tier"
fields = ["pricing_model"]
"#,
        )
        .unwrap();
        let ex = extract_source(
            &pack.source[0],
            &fetched("<html><body><div class='model'>per-seat</div></body></html>"),
            &cache,
            &Llm::new().unwrap(),
            &Budget::unlimited(),
        )
        .await
        .unwrap();

        assert_eq!(ex.facts.len(), 1);
        assert!(
            ex.notes.is_empty(),
            "no rung-3 note when rung 2 sufficed: {:?}",
            ex.notes
        );
    }
}
