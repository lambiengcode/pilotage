//! Facts are the only thing that flows between fetching and scoring.
//!
//! Every fact carries where it came from and how it was obtained. That provenance is
//! not decoration: `method` drives confidence, and a fact whose source cannot be shown
//! is dropped rather than reported.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How a fact was obtained, cheapest rung first. This is the extraction ladder from
/// the plan, made explicit in the data so it survives into the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    /// Read straight out of a structured API response. Trusted.
    Api,
    /// Pulled out of markup with a selector or regex from the pack.
    Parse,
    /// Extracted by a cheap model because nothing deterministic could reach it.
    Llm,
}

impl Method {
    /// Base confidence by acquisition method. An API field is as good as the API; a
    /// selector can silently match the wrong node after a redesign; a model can be
    /// confidently wrong even when told only to extract.
    pub fn confidence(self) -> f64 {
        match self {
            Method::Api => 1.0,
            Method::Parse => 0.8,
            Method::Llm => 0.6,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Method::Api => "api",
            Method::Parse => "parse",
            Method::Llm => "llm",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub key: String,
    pub value: Value,
    pub source_id: String,
    pub source_url: String,
    pub fetched_at: i64,
    pub method: Method,
    pub confidence: f64,
}

impl Fact {
    pub fn new(
        key: impl Into<String>,
        value: Value,
        source_id: impl Into<String>,
        source_url: impl Into<String>,
        fetched_at: i64,
        method: Method,
    ) -> Self {
        Self {
            key: key.into(),
            value,
            source_id: source_id.into(),
            source_url: source_url.into(),
            fetched_at,
            method,
            confidence: method.confidence(),
        }
    }

    /// Numeric view of the value. Strings that look like numbers count — APIs return
    /// `"1234"` often enough that refusing to coerce would drop real data. Percentage
    /// and comma-grouped forms are handled because pricing and metric pages are full
    /// of them.
    pub fn as_f64(&self) -> Option<f64> {
        match &self.value {
            Value::Number(n) => n.as_f64(),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::String(s) => parse_loose_number(s),
            Value::Array(a) => Some(a.len() as f64),
            _ => None,
        }
    }

    pub fn as_display(&self) -> String {
        match &self.value {
            Value::String(s) => s.clone(),
            Value::Null => "none".into(),
            Value::Array(a) => format!("[{}]", a.len()),
            v => v.to_string(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match &self.value {
            Value::Null => true,
            Value::String(s) => s.trim().is_empty(),
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
            _ => false,
        }
    }
}

/// Tolerant number parsing for values scraped off pages: `$1,234.50`, `42%`, `1.2k`.
pub fn parse_loose_number(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let mut cleaned = String::with_capacity(t.len());
    let mut scale = 1.0;
    for (i, c) in t.char_indices() {
        match c {
            '0'..='9' | '.' | '-' => cleaned.push(c),
            '+' if cleaned.is_empty() => {}
            ',' | '$' | '€' | '£' | '%' | ' ' | '_' => {}
            'k' | 'K' if i > 0 => scale = 1e3,
            'm' | 'M' if i > 0 => scale = 1e6,
            'b' | 'B' if i > 0 => scale = 1e9,
            _ => return None,
        }
    }
    cleaned.parse::<f64>().ok().map(|n| n * scale)
}

/// Two sources reporting materially different values for the same key. Surfaced in
/// the output rather than resolved — disagreement between sources is a finding about
/// the target, not noise to be smoothed away.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contradiction {
    pub key: String,
    pub a_source: String,
    pub a_value: String,
    pub b_source: String,
    pub b_value: String,
}

/// Detects conflicting facts for the same key. Numeric values are compared with a
/// relative tolerance so that rounding between two APIs is not reported as a conflict.
pub fn find_contradictions(facts: &[Fact], tolerance: f64) -> Vec<Contradiction> {
    let mut out = Vec::new();
    for i in 0..facts.len() {
        for j in (i + 1)..facts.len() {
            let (a, b) = (&facts[i], &facts[j]);
            if a.key != b.key || a.source_id == b.source_id {
                continue;
            }
            let conflict = match (a.as_f64(), b.as_f64()) {
                (Some(x), Some(y)) => {
                    let scale = x.abs().max(y.abs()).max(f64::EPSILON);
                    (x - y).abs() / scale > tolerance
                }
                _ => a.as_display() != b.as_display(),
            };
            if conflict {
                out.push(Contradiction {
                    key: a.key.clone(),
                    a_source: a.source_id.clone(),
                    a_value: a.as_display(),
                    b_source: b.source_id.clone(),
                    b_value: b.as_display(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fact(key: &str, v: Value, src: &str) -> Fact {
        Fact::new(key, v, src, "https://x", 0, Method::Api)
    }

    #[test]
    fn confidence_follows_the_extraction_ladder() {
        assert!(Method::Api.confidence() > Method::Parse.confidence());
        assert!(Method::Parse.confidence() > Method::Llm.confidence());
    }

    #[test]
    fn loose_numbers_cover_what_real_pages_contain() {
        assert_eq!(parse_loose_number("1,234"), Some(1234.0));
        assert_eq!(parse_loose_number("$49.00"), Some(49.0));
        assert_eq!(parse_loose_number("-42%"), Some(-42.0));
        assert_eq!(parse_loose_number("1.2k"), Some(1200.0));
        assert_eq!(parse_loose_number("3B"), Some(3e9));
        assert_eq!(parse_loose_number("free"), None);
        assert_eq!(parse_loose_number(""), None);
    }

    #[test]
    fn arrays_count_as_their_length() {
        let f = fact("commits", json!([1, 2, 3]), "gh");
        assert_eq!(f.as_f64(), Some(3.0));
        assert_eq!(f.as_display(), "[3]");
    }

    #[test]
    fn rounding_between_sources_is_not_a_contradiction() {
        let facts = vec![
            fact("tvl", json!(1_000_000.0), "a"),
            fact("tvl", json!(1_000_500.0), "b"),
        ];
        assert!(find_contradictions(&facts, 0.05).is_empty());
    }

    #[test]
    fn real_disagreement_is_reported() {
        let facts = vec![
            fact("tvl", json!(1_000_000.0), "a"),
            fact("tvl", json!(400_000.0), "b"),
        ];
        let c = find_contradictions(&facts, 0.05);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].key, "tvl");
    }

    #[test]
    fn one_source_never_contradicts_itself() {
        let facts = vec![fact("tvl", json!(1.0), "a"), fact("tvl", json!(99.0), "a")];
        assert!(find_contradictions(&facts, 0.05).is_empty());
    }

    #[test]
    fn differing_strings_conflict() {
        let facts = vec![
            fact("license", json!("MIT"), "a"),
            fact("license", json!("GPL-3.0"), "b"),
        ];
        assert_eq!(find_contradictions(&facts, 0.05).len(), 1);
    }

    #[test]
    fn emptiness_is_recognised_across_shapes() {
        assert!(fact("k", json!(null), "a").is_empty());
        assert!(fact("k", json!("  "), "a").is_empty());
        assert!(fact("k", json!([]), "a").is_empty());
        assert!(!fact("k", json!(0), "a").is_empty());
    }
}
