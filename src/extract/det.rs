//! Rungs 1 and 2 of the extraction ladder: JSON field maps and CSS/regex selectors.
//!
//! Both are free and instant. Between them they cover roughly nine in ten sources,
//! which is the only reason the model rung stays affordable.

use crate::pack::Selector;
use anyhow::{Result, bail};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Rung 1 — JSON paths
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Field(String),
    Index(usize),
    /// `[]` — map the remaining path across every element.
    Iter,
    /// `{}` — the object's keys as an array. Some APIs carry the interesting list in
    /// the keys of a map rather than in an array (DefiLlama's per-chain TVL, for
    /// one), and counting them is otherwise impossible.
    Keys,
}

/// A deliberately small subset of JSONPath: `.a.b`, `.a[0]`, `.a[].b`, and `.` for
/// the whole document. Everything the packs need, nothing that needs a grammar.
fn parse_path(path: &str) -> Result<Vec<Seg>> {
    let p = path.trim();
    if p == "." {
        return Ok(Vec::new());
    }
    let Some(mut rest) = p.strip_prefix('.') else {
        bail!("json path '{path}' must start with '.'");
    };

    let mut segs = Vec::new();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('[') {
            let Some(end) = after.find(']') else {
                bail!("json path '{path}' has an unclosed '['");
            };
            let inner = &after[..end];
            segs.push(if inner.is_empty() {
                Seg::Iter
            } else {
                Seg::Index(inner.parse().map_err(|_| {
                    anyhow::anyhow!("json path '{path}': '{inner}' is not an index")
                })?)
            });
            rest = after[end + 1..]
                .strip_prefix('.')
                .unwrap_or(&after[end + 1..]);
            continue;
        }
        if let Some(after) = rest.strip_prefix("{}") {
            segs.push(Seg::Keys);
            rest = after.strip_prefix('.').unwrap_or(after);
            continue;
        }
        let end = rest.find(['.', '[', '{']).unwrap_or(rest.len());
        let name = &rest[..end];
        if name.is_empty() {
            bail!("json path '{path}' has an empty field name");
        }
        segs.push(Seg::Field(name.to_string()));
        rest = rest[end..].strip_prefix('.').unwrap_or(&rest[end..]);
    }
    Ok(segs)
}

fn eval(value: &Value, segs: &[Seg]) -> Option<Value> {
    let Some(seg) = segs.first() else {
        return Some(value.clone());
    };
    let tail = &segs[1..];
    match seg {
        Seg::Field(f) => value.get(f).and_then(|v| eval(v, tail)),
        Seg::Index(i) => value.as_array()?.get(*i).and_then(|v| eval(v, tail)),
        Seg::Keys => {
            let obj = value.as_object()?;
            let keys: Vec<Value> = obj.keys().map(|k| Value::String(k.clone())).collect();
            eval(&Value::Array(keys), tail)
        }
        Seg::Iter => {
            let items = value.as_array()?;
            // Elements the tail cannot reach are skipped rather than becoming nulls;
            // a partially-populated array is more useful than one full of holes.
            Some(Value::Array(
                items.iter().filter_map(|v| eval(v, tail)).collect(),
            ))
        }
    }
}

/// Reads one fact out of a JSON document. `None` means "not present", which is a
/// legitimate answer and distinct from an error.
pub fn json_extract(doc: &Value, path: &str) -> Result<Option<Value>> {
    let segs = parse_path(path)?;
    Ok(eval(doc, &segs).filter(|v| !v.is_null()))
}

// ---------------------------------------------------------------------------
// Rung 2 — selectors
// ---------------------------------------------------------------------------

/// Applies a pack selector to a document.
///
/// With a `css` selector this parses markup; without one it treats the input as plain
/// text and runs the regex over it, which is how rendered browser snapshots are read.
pub fn select_extract(body: &str, sel: &Selector) -> Result<Option<Value>> {
    let re = match &sel.regex {
        Some(r) => Some(
            regex::Regex::new(r)
                .map_err(|e| anyhow::anyhow!("selector regex '{r}' did not compile: {e}"))?,
        ),
        None => None,
    };

    let Some(css) = &sel.css else {
        let Some(re) = re else {
            bail!("selector has neither a css selector nor a regex");
        };
        return Ok(collect(
            re.find_iter(body)
                .map(|m| narrow(&re, m.as_str()))
                .collect(),
            sel.all,
        ));
    };

    let doc = scraper::Html::parse_document(body);
    let parsed = scraper::Selector::parse(css)
        .map_err(|e| anyhow::anyhow!("css selector '{css}' did not parse: {e:?}"))?;

    let mut values = Vec::new();
    for el in doc.select(&parsed) {
        let raw = match &sel.attr {
            Some(a) => el.value().attr(a).map(str::to_string),
            None => Some(el.text().collect::<String>()),
        };
        let Some(raw) = raw else { continue };
        let text = normalize_ws(&raw);
        if text.is_empty() {
            continue;
        }
        match &re {
            // A regex that does not match means this element was not the one we
            // wanted — skip it and keep looking, rather than recording noise.
            Some(re) => match re.find(&text) {
                Some(m) => values.push(narrow(re, m.as_str())),
                None => continue,
            },
            None => values.push(text),
        }
        if !sel.all {
            break;
        }
    }
    Ok(collect(values, sel.all))
}

/// When the regex has a capture group, the group is the interesting part.
fn narrow(re: &regex::Regex, text: &str) -> String {
    match re.captures(text) {
        Some(c) => c
            .get(1)
            .or_else(|| c.get(0))
            .map(|m| m.as_str().to_string())
            .unwrap_or_else(|| text.to_string()),
        None => text.to_string(),
    }
}

fn collect(values: Vec<String>, all: bool) -> Option<Value> {
    if values.is_empty() {
        return None;
    }
    Some(if all {
        Value::Array(values.into_iter().map(Value::String).collect())
    } else {
        Value::String(values.into_iter().next().unwrap())
    })
}

/// Collapses the whitespace that markup is full of, so a scraped title compares
/// equal to the same title fetched from an API.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sel(css: Option<&str>, attr: Option<&str>, all: bool, regex: Option<&str>) -> Selector {
        Selector {
            css: css.map(String::from),
            attr: attr.map(String::from),
            all,
            regex: regex.map(String::from),
        }
    }

    #[test]
    fn root_path_returns_the_whole_document() {
        let d = json!([1, 2, 3]);
        assert_eq!(json_extract(&d, ".").unwrap().unwrap(), json!([1, 2, 3]));
    }

    #[test]
    fn nested_fields_and_indices() {
        let d = json!({"a": {"b": [10, 20]}});
        assert_eq!(json_extract(&d, ".a.b[1]").unwrap().unwrap(), json!(20));
        assert_eq!(json_extract(&d, ".a.b").unwrap().unwrap(), json!([10, 20]));
    }

    #[test]
    fn iteration_maps_the_tail_across_elements() {
        let d = json!({"hits": [{"points": 5}, {"points": 9}]});
        assert_eq!(
            json_extract(&d, ".hits[].points").unwrap().unwrap(),
            json!([5, 9])
        );
    }

    #[test]
    fn iteration_skips_elements_the_tail_cannot_reach() {
        let d = json!({"hits": [{"points": 5}, {"other": 1}, {"points": 9}]});
        assert_eq!(
            json_extract(&d, ".hits[].points").unwrap().unwrap(),
            json!([5, 9])
        );
    }

    #[test]
    fn missing_and_null_are_both_absent() {
        let d = json!({"license": null, "name": "x"});
        assert!(json_extract(&d, ".license").unwrap().is_none());
        assert!(json_extract(&d, ".nope").unwrap().is_none());
        // Descending through a null does not panic.
        assert!(json_extract(&d, ".license.spdx_id").unwrap().is_none());
    }

    #[test]
    fn zero_and_false_are_present_values_not_absences() {
        let d = json!({"count": 0, "archived": false});
        assert_eq!(json_extract(&d, ".count").unwrap().unwrap(), json!(0));
        assert_eq!(
            json_extract(&d, ".archived").unwrap().unwrap(),
            json!(false)
        );
    }

    #[test]
    fn object_keys_can_be_read_as_a_list() {
        // DefiLlama reports per-chain TVL as a map keyed by chain name, so the chain
        // list only exists as the key set.
        let d = json!({"currentChainTvls": {"Ethereum": 1, "Polygon": 2}});
        let v = json_extract(&d, ".currentChainTvls{}").unwrap().unwrap();
        let mut names: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["Ethereum", "Polygon"]);
    }

    #[test]
    fn keys_on_a_non_object_is_absent_rather_than_an_error() {
        let d = json!({"a": [1, 2]});
        assert!(json_extract(&d, ".a{}").unwrap().is_none());
    }

    #[test]
    fn malformed_paths_are_errors_not_silent_misses() {
        let d = json!({});
        for p in ["a.b", ".a[", ".a[x]", ".a..b"] {
            assert!(json_extract(&d, p).is_err(), "{p} should be an error");
        }
    }

    #[test]
    fn css_reads_text_and_attributes() {
        let html = r#"<html><head><title> My  Site </title>
            <meta name="description" content="Hello there"></head><body></body></html>"#;
        assert_eq!(
            select_extract(html, &sel(Some("title"), None, false, None))
                .unwrap()
                .unwrap(),
            json!("My Site")
        );
        assert_eq!(
            select_extract(
                html,
                &sel(
                    Some(r#"meta[name="description"]"#),
                    Some("content"),
                    false,
                    None
                )
            )
            .unwrap()
            .unwrap(),
            json!("Hello there")
        );
    }

    #[test]
    fn all_collects_every_match_while_default_takes_the_first() {
        let html = "<ul><li>a</li><li>b</li></ul>";
        assert_eq!(
            select_extract(html, &sel(Some("li"), None, true, None))
                .unwrap()
                .unwrap(),
            json!(["a", "b"])
        );
        assert_eq!(
            select_extract(html, &sel(Some("li"), None, false, None))
                .unwrap()
                .unwrap(),
            json!("a")
        );
    }

    #[test]
    fn a_regex_filters_out_elements_that_do_not_match() {
        let html = "<div class='price'>Contact us</div><div class='price'>$49/mo</div>";
        assert_eq!(
            select_extract(
                html,
                &sel(Some(".price"), None, true, Some(r"\$\s?[0-9][0-9,.]*"))
            )
            .unwrap()
            .unwrap(),
            json!(["$49"])
        );
    }

    #[test]
    fn a_capture_group_narrows_to_the_interesting_part() {
        let html = "<div>Version 4.2.1 released</div>";
        assert_eq!(
            select_extract(
                html,
                &sel(Some("div"), None, false, Some(r"Version ([0-9.]+)"))
            )
            .unwrap()
            .unwrap(),
            json!("4.2.1")
        );
    }

    #[test]
    fn no_match_is_absent_rather_than_empty_string() {
        let html = "<div>nothing here</div>";
        assert!(
            select_extract(html, &sel(Some("h1"), None, false, None))
                .unwrap()
                .is_none()
        );
        assert!(
            select_extract(html, &sel(Some("div"), None, true, Some(r"\$[0-9]+")))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn missing_attributes_skip_the_element() {
        let html = r#"<meta name="a"><meta name="b" content="found">"#;
        assert_eq!(
            select_extract(html, &sel(Some("meta"), Some("content"), false, None))
                .unwrap()
                .unwrap(),
            json!("found")
        );
    }

    #[test]
    fn text_mode_runs_the_regex_over_a_rendered_snapshot() {
        // Browser snapshots are an accessibility tree, not markup — no css available.
        let snapshot = "uid=1 button \"Start free trial\"\nuid=2 link \"Docs\"";
        assert_eq!(
            select_extract(
                snapshot,
                &sel(None, None, false, Some(r"(?i)(start free|get started)"))
            )
            .unwrap()
            .unwrap(),
            json!("Start free")
        );
    }

    #[test]
    fn a_selector_with_neither_css_nor_regex_is_an_error() {
        assert!(select_extract("x", &sel(None, None, false, None)).is_err());
    }

    #[test]
    fn a_bad_css_selector_is_an_error_not_a_silent_miss() {
        assert!(select_extract("<div/>", &sel(Some("<<<"), None, false, None)).is_err());
    }
}
