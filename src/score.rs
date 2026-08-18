//! Facts in, verdict out.
//!
//! The one distinction that matters here: a metric whose source never ran is
//! **unknown**, while a metric whose source ran and found nothing is **missing**.
//! Conflating them either invents confidence pilotage does not have, or fires a
//! warning about data nobody looked for.

use crate::fact::Fact;
use crate::pack::{Cond, Metric, MetricValue, Pack};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
    /// No source that could produce this metric ran.
    Unknown,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Unknown => "unknown",
        }
    }
    fn points(self) -> f64 {
        match self {
            Status::Ok => 1.0,
            Status::Warn => 0.5,
            Status::Fail | Status::Unknown => 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    Healthy,
    Caution,
    Risk,
    /// Too little of the metric weight could be evaluated to say anything.
    InsufficientData,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Healthy => "healthy",
            Verdict::Caution => "caution",
            Verdict::Risk => "risk",
            Verdict::InsufficientData => "insufficient-data",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub id: String,
    pub label: String,
    pub value: MetricValue,
    pub display: String,
    pub status: Status,
    pub weight: f64,
    pub confidence: f64,
    pub critical: bool,
    /// Show the value even when this signal is fine. See `Metric::pin`.
    #[serde(default)]
    pub pin: bool,
    pub source: Option<String>,
    pub source_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scored {
    pub signals: Vec<Signal>,
    pub verdict: Verdict,
    pub score: f64,
    pub coverage: f64,
    pub confidence: f64,
}

/// Coverage below this and no verdict is honest — pilotage says so instead of
/// scoring whatever happened to arrive.
const MIN_COVERAGE: f64 = 0.4;
const HEALTHY_AT: f64 = 0.75;
const CAUTION_AT: f64 = 0.5;

pub fn score(pack: &Pack, facts: &[Fact], ran_sources: &HashSet<String>) -> Scored {
    // Which sources could have produced each fact key, so an absent fact can be
    // told apart from an absent source.
    let mut producers: HashMap<&str, Vec<&str>> = HashMap::new();
    for s in &pack.source {
        for key in s
            .map
            .keys()
            .chain(s.select.keys())
            .chain(s.extract.iter().flat_map(|e| e.fields.iter()))
        {
            producers
                .entry(key.as_str())
                .or_default()
                .push(s.id.as_str());
        }
    }

    let mut by_key: HashMap<&str, Vec<&Fact>> = HashMap::new();
    for f in facts {
        by_key.entry(f.key.as_str()).or_default().push(f);
    }

    let signals: Vec<Signal> = pack
        .metric
        .iter()
        .map(|m| evaluate(m, &by_key, &producers, ran_sources))
        .collect();

    let total_weight: f64 = signals.iter().map(|s| s.weight).sum();
    let known: Vec<&Signal> = signals
        .iter()
        .filter(|s| s.status != Status::Unknown)
        .collect();
    let known_weight: f64 = known.iter().map(|s| s.weight).sum();

    let coverage = if total_weight > 0.0 {
        known_weight / total_weight
    } else {
        0.0
    };
    let score = if known_weight > 0.0 {
        known
            .iter()
            .map(|s| s.weight * s.status.points())
            .sum::<f64>()
            / known_weight
    } else {
        0.0
    };
    let confidence = if known_weight > 0.0 {
        known.iter().map(|s| s.weight * s.confidence).sum::<f64>() / known_weight
    } else {
        0.0
    };

    let critical_failure = signals
        .iter()
        .any(|s| s.critical && s.status == Status::Fail);
    let verdict = if coverage < MIN_COVERAGE {
        Verdict::InsufficientData
    } else if critical_failure {
        // A repository being archived is not something a good star count offsets.
        Verdict::Risk
    } else if score >= HEALTHY_AT {
        Verdict::Healthy
    } else if score >= CAUTION_AT {
        Verdict::Caution
    } else {
        Verdict::Risk
    };

    Scored {
        signals,
        verdict,
        score,
        coverage,
        confidence,
    }
}

fn evaluate(
    m: &Metric,
    by_key: &HashMap<&str, Vec<&Fact>>,
    producers: &HashMap<&str, Vec<&str>>,
    ran: &HashSet<String>,
) -> Signal {
    let label = m.label.clone().unwrap_or_else(|| m.id.clone());
    let candidates = by_key.get(m.fact.as_str());

    // Highest-confidence fact wins when several sources produced the same key. The
    // disagreement itself is reported separately as a contradiction.
    let best = candidates.and_then(|fs| {
        fs.iter()
            .max_by(|a, b| a.confidence.total_cmp(&b.confidence))
            .copied()
    });

    let any_producer_ran = producers
        .get(m.fact.as_str())
        .is_some_and(|ids| ids.iter().any(|id| ran.contains(*id)));

    if best.is_none() && !any_producer_ran {
        return Signal {
            id: m.id.clone(),
            label,
            value: MetricValue::Missing,
            display: "unknown".into(),
            status: Status::Unknown,
            weight: m.weight,
            confidence: 0.0,
            critical: m.critical,
            pin: m.pin,
            source: None,
            source_url: None,
        };
    }

    // A counting metric whose source ran and produced nothing counted zero. Leaving it
    // "unknown" would report "we did not look" about a page we did read — and for
    // "how many prices are published?", zero is the answer, not an absence.
    if best.is_none()
        && any_producer_ran
        && matches!(m.agg.as_deref(), Some("count") | Some("unique"))
    {
        let value = MetricValue::Num(0.0);
        let status = classify(m, &value);
        return Signal {
            id: m.id.clone(),
            label,
            display: display(&value, m.unit.as_deref()),
            value,
            status,
            weight: m.weight,
            confidence: crate::fact::Method::Parse.confidence(),
            critical: m.critical,
            pin: m.pin,
            source: None,
            source_url: None,
        };
    }

    let value = match best {
        Some(f) => match (m.agg.as_deref(), &m.over) {
            // A ratio needs a second fact, so it is resolved here rather than in
            // `apply`, which only ever sees one. Division by a missing or zero
            // denominator is missing data, not infinity.
            (Some("ratio"), Some(over)) => {
                let denominator = by_key
                    .get(over.as_str())
                    .and_then(|fs| {
                        fs.iter()
                            .max_by(|a, b| a.confidence.total_cmp(&b.confidence))
                            .copied()
                    })
                    .and_then(|d| d.as_f64());
                match (f.as_f64(), denominator) {
                    (Some(n), Some(d)) if d.abs() > f64::EPSILON => MetricValue::Num(n / d),
                    _ => MetricValue::Missing,
                }
            }
            _ => apply(f, m),
        },
        None => MetricValue::Missing,
    };

    let status = classify(m, &value);

    Signal {
        id: m.id.clone(),
        label,
        display: display(&value, m.unit.as_deref()),
        value,
        status,
        weight: m.weight,
        confidence: best.map(|f| f.confidence).unwrap_or(0.0),
        critical: m.critical,
        pin: m.pin,
        source: best.map(|f| f.source_id.clone()),
        source_url: best.map(|f| f.source_url.clone()),
    }
}

fn classify(m: &Metric, value: &MetricValue) -> Status {
    let fired = |expr: &Option<String>| {
        expr.as_deref()
            .and_then(|e| Cond::parse(e).ok())
            .is_some_and(|c| c.eval(Some(value)))
    };
    // Fail is checked first: thresholds usually overlap (`warn = "< 10"`,
    // `fail = "< 3"`) and the worse verdict must win.
    if fired(&m.fail) {
        Status::Fail
    } else if fired(&m.warn) {
        Status::Warn
    } else if *value == MetricValue::Missing {
        // Nothing was found and no threshold cared. Scoring this as healthy would
        // reward emptiness, so it stays unknown.
        Status::Unknown
    } else {
        Status::Ok
    }
}

/// Reduces a fact to a single comparable value.
fn apply(f: &Fact, m: &Metric) -> MetricValue {
    // `filter` runs before `take` and before the aggregation: narrowing the list is
    // what makes the count mean something specific.
    let filtered = match (&f.value, &m.filter) {
        (serde_json::Value::Array(items), Some(pattern)) => match regex::Regex::new(pattern) {
            Ok(re) => serde_json::Value::Array(
                items
                    .iter()
                    .filter(|v| {
                        let text = match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        re.is_match(&text)
                    })
                    .cloned()
                    .collect(),
            ),
            // A pack with a broken filter must not silently report the unfiltered
            // count as if it were the filtered one.
            Err(_) => serde_json::Value::Null,
        },
        _ => f.value.clone(),
    };

    let value = match (&filtered, m.take) {
        (serde_json::Value::Array(items), Some(n)) => {
            let len = items.len();
            let slice = if n >= 0 {
                &items[..(n as usize).min(len)]
            } else {
                &items[len.saturating_sub(n.unsigned_abs() as usize)..]
            };
            serde_json::Value::Array(slice.to_vec())
        }
        _ => filtered.clone(),
    };

    let numbers = || -> Vec<f64> {
        match &value {
            serde_json::Value::Array(items) => items
                .iter()
                .filter_map(|v| match v {
                    serde_json::Value::Number(n) => n.as_f64(),
                    serde_json::Value::String(s) => crate::fact::parse_loose_number(s),
                    serde_json::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    };

    let Some(agg) = m.agg.as_deref() else {
        return scalar(&value);
    };

    match agg {
        "count" => MetricValue::Num(match &value {
            serde_json::Value::Array(a) => a.len() as f64,
            serde_json::Value::Null => 0.0,
            _ => 1.0,
        }),
        "unique" => MetricValue::Num(match &value {
            serde_json::Value::Array(a) => a
                .iter()
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect::<HashSet<_>>()
                .len() as f64,
            serde_json::Value::Null => 0.0,
            _ => 1.0,
        }),
        "sum" => MetricValue::Num(numbers().iter().sum()),
        "avg" => {
            let n = numbers();
            if n.is_empty() {
                MetricValue::Missing
            } else {
                MetricValue::Num(n.iter().sum::<f64>() / n.len() as f64)
            }
        }
        "max" => numbers()
            .into_iter()
            .reduce(f64::max)
            .map(MetricValue::Num)
            .unwrap_or(MetricValue::Missing),
        "min" => numbers()
            .into_iter()
            .reduce(f64::min)
            .map(MetricValue::Num)
            .unwrap_or(MetricValue::Missing),
        "first" => match &value {
            serde_json::Value::Array(a) => a.first().map(scalar).unwrap_or(MetricValue::Missing),
            other => scalar(other),
        },
        "days_since" => {
            let ts = match &value {
                serde_json::Value::String(s) => crate::util::parse_timestamp(s),
                serde_json::Value::Number(n) => n.as_f64().map(|f| f as i64),
                _ => None,
            };
            ts.map(|t| MetricValue::Num(crate::util::days_since(t)))
                .unwrap_or(MetricValue::Missing)
        }
        "pct_change" => {
            let n = numbers();
            match (n.first(), n.last()) {
                (Some(first), Some(last)) if n.len() >= 2 && first.abs() > f64::EPSILON => {
                    MetricValue::Num((last - first) / first.abs() * 100.0)
                }
                _ => MetricValue::Missing,
            }
        }
        _ => scalar(&value),
    }
}

fn scalar(v: &serde_json::Value) -> MetricValue {
    match v {
        serde_json::Value::Number(n) => n
            .as_f64()
            .map(MetricValue::Num)
            .unwrap_or(MetricValue::Missing),
        serde_json::Value::Bool(b) => MetricValue::Num(if *b { 1.0 } else { 0.0 }),
        serde_json::Value::String(s) if s.trim().is_empty() => MetricValue::Missing,
        serde_json::Value::String(s) => MetricValue::Str(s.clone()),
        serde_json::Value::Null => MetricValue::Missing,
        serde_json::Value::Array(a) if a.is_empty() => MetricValue::Missing,
        serde_json::Value::Array(a) => MetricValue::Num(a.len() as f64),
        serde_json::Value::Object(_) => MetricValue::Missing,
    }
}

/// Compact rendering. Output width is a running cost for the caller, so a market cap
/// is `$1.2B`, not fifteen digits.
pub fn display(v: &MetricValue, unit: Option<&str>) -> String {
    match v {
        MetricValue::Missing => "none".into(),
        MetricValue::Str(s) => crate::extract::llm::truncate_chars(s, 60),
        MetricValue::Num(n) => match unit {
            Some("usd") => format!("${}", humanize(*n)),
            Some("%") => format!("{}%", round(*n)),
            Some(u) => format!("{}{u}", round(*n)),
            None => round(*n),
        },
    }
}

fn round(n: f64) -> String {
    if n.fract().abs() < 1e-9 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n:.1}")
    }
}

fn humanize(n: f64) -> String {
    let a = n.abs();
    let (scaled, suffix) = if a >= 1e9 {
        (n / 1e9, "B")
    } else if a >= 1e6 {
        (n / 1e6, "M")
    } else if a >= 1e3 {
        (n / 1e3, "K")
    } else {
        return round(n);
    };
    format!("{scaled:.2}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::Method;
    use serde_json::json;

    fn pack(metrics: &str) -> Pack {
        Pack::parse(&format!(
            "name='t'\ndetect=['{{a}}']\n[[source]]\nid='s'\nurl='https://x'\n[source.map]\nv = '.v'\nw = '.w'\n{metrics}"
        ))
        .unwrap()
    }

    fn f(key: &str, v: serde_json::Value) -> Fact {
        Fact::new(key, v, "s", "https://x", crate::util::now(), Method::Api)
    }

    fn ran() -> HashSet<String> {
        HashSet::from(["s".to_string()])
    }

    #[test]
    fn a_source_that_never_ran_leaves_the_metric_unknown() {
        let p = pack("[[metric]]\nid='m'\nfact='v'\nfail='is none'\n");
        let s = score(&p, &[], &HashSet::new());
        assert_eq!(s.signals[0].status, Status::Unknown);
        assert_eq!(s.verdict, Verdict::InsufficientData);
        assert_eq!(s.coverage, 0.0);
    }

    #[test]
    fn a_source_that_ran_and_found_nothing_fires_is_none() {
        let p = pack("[[metric]]\nid='m'\nfact='v'\nfail='is none'\n");
        let s = score(&p, &[], &ran());
        assert_eq!(s.signals[0].status, Status::Fail);
        assert_eq!(s.coverage, 1.0);
    }

    #[test]
    fn fail_wins_over_warn_when_thresholds_overlap() {
        let p = pack("[[metric]]\nid='m'\nfact='v'\nwarn='< 10'\nfail='< 3'\n");
        let s = score(&p, &[f("v", json!(1))], &ran());
        assert_eq!(s.signals[0].status, Status::Fail);
        let s = score(&p, &[f("v", json!(5))], &ran());
        assert_eq!(s.signals[0].status, Status::Warn);
        let s = score(&p, &[f("v", json!(50))], &ran());
        assert_eq!(s.signals[0].status, Status::Ok);
    }

    #[test]
    fn a_critical_failure_overrides_an_otherwise_healthy_score() {
        let p = pack(
            "[[metric]]\nid='archived'\nfact='v'\nfail='== 1'\ncritical=true\nweight=1.0\n\
             [[metric]]\nid='stars'\nfact='w'\nwarn='< 10'\nweight=9.0\n",
        );
        let s = score(&p, &[f("v", json!(true)), f("w", json!(10_000))], &ran());
        assert!(
            s.score > HEALTHY_AT,
            "the rest of the pack looks great: {}",
            s.score
        );
        assert_eq!(s.verdict, Verdict::Risk);
    }

    #[test]
    fn thin_coverage_refuses_to_produce_a_verdict() {
        let p = pack(
            "[[metric]]\nid='a'\nfact='v'\nwarn='< 1'\nweight=1.0\n\
             [[metric]]\nid='b'\nfact='missing_fact'\nwarn='< 1'\nweight=9.0\n",
        );
        // `missing_fact` has no producer at all, so it can never be known.
        let s = score(&p, &[f("v", json!(100))], &ran());
        assert!(s.coverage < MIN_COVERAGE);
        assert_eq!(s.verdict, Verdict::InsufficientData);
    }

    #[test]
    fn verdict_bands_follow_the_weighted_score() {
        let p = pack("[[metric]]\nid='m'\nfact='v'\nwarn='< 10'\nfail='< 3'\n");
        assert_eq!(
            score(&p, &[f("v", json!(99))], &ran()).verdict,
            Verdict::Healthy
        );
        assert_eq!(
            score(&p, &[f("v", json!(5))], &ran()).verdict,
            Verdict::Caution
        );
        assert_eq!(
            score(&p, &[f("v", json!(1))], &ran()).verdict,
            Verdict::Risk
        );
    }

    #[test]
    fn the_highest_confidence_fact_wins_when_sources_disagree() {
        let p = pack("[[metric]]\nid='m'\nfact='v'\nwarn='< 10'\n");
        let scraped = Fact::new("v", json!(1), "b", "https://b", 0, Method::Llm);
        let from_api = Fact::new("v", json!(100), "a", "https://a", 0, Method::Api);
        let s = score(&p, &[scraped, from_api], &ran());
        assert_eq!(s.signals[0].status, Status::Ok);
        assert_eq!(s.signals[0].source.as_deref(), Some("a"));
    }

    #[test]
    fn count_and_unique_measure_different_things() {
        let p = pack(
            "[[metric]]\nid='c'\nfact='v'\nagg='count'\nwarn='< 1'\n\
             [[metric]]\nid='u'\nfact='v'\nagg='unique'\nwarn='< 1'\n",
        );
        let s = score(&p, &[f("v", json!(["a", "a", "b"]))], &ran());
        assert_eq!(s.signals[0].value, MetricValue::Num(3.0));
        assert_eq!(s.signals[1].value, MetricValue::Num(2.0));
    }

    #[test]
    fn numeric_aggregations() {
        for (agg, expected) in [("sum", 60.0), ("avg", 20.0), ("max", 30.0), ("min", 10.0)] {
            let p = pack(&format!(
                "[[metric]]\nid='m'\nfact='v'\nagg='{agg}'\nwarn='< 0'\n"
            ));
            let s = score(&p, &[f("v", json!([10, 20, 30]))], &ran());
            assert_eq!(s.signals[0].value, MetricValue::Num(expected), "{agg}");
        }
    }

    #[test]
    fn take_windows_a_series_before_aggregating() {
        let p = pack("[[metric]]\nid='m'\nfact='v'\ntake=-3\nagg='sum'\nwarn='< 0'\n");
        let s = score(&p, &[f("v", json!([1, 1, 1, 10, 20, 30]))], &ran());
        assert_eq!(s.signals[0].value, MetricValue::Num(60.0));

        let p = pack("[[metric]]\nid='m'\nfact='v'\ntake=2\nagg='sum'\nwarn='< 0'\n");
        let s = score(&p, &[f("v", json!([1, 1, 1, 10, 20, 30]))], &ran());
        assert_eq!(s.signals[0].value, MetricValue::Num(2.0));
    }

    #[test]
    fn take_larger_than_the_series_is_not_a_panic() {
        let p = pack("[[metric]]\nid='m'\nfact='v'\ntake=-100\nagg='sum'\nwarn='< 0'\n");
        let s = score(&p, &[f("v", json!([1, 2]))], &ran());
        assert_eq!(s.signals[0].value, MetricValue::Num(3.0));
    }

    #[test]
    fn pct_change_reads_first_against_last() {
        let p = pack("[[metric]]\nid='m'\nfact='v'\nagg='pct_change'\nfail='< -50'\n");
        let s = score(&p, &[f("v", json!([100.0, 90.0, 40.0]))], &ran());
        assert_eq!(s.signals[0].value, MetricValue::Num(-60.0));
        assert_eq!(s.signals[0].status, Status::Fail);
    }

    #[test]
    fn pct_change_needs_two_points_and_a_nonzero_base() {
        let p = pack("[[metric]]\nid='m'\nfact='v'\nagg='pct_change'\nwarn='< 0'\n");
        assert_eq!(
            score(&p, &[f("v", json!([5.0]))], &ran()).signals[0].value,
            MetricValue::Missing
        );
        assert_eq!(
            score(&p, &[f("v", json!([0.0, 5.0]))], &ran()).signals[0].value,
            MetricValue::Missing
        );
    }

    #[test]
    fn days_since_accepts_both_iso_strings_and_epochs() {
        let p = pack("[[metric]]\nid='m'\nfact='v'\nagg='days_since'\nwarn='> 5'\n");
        let ten_days_ago = crate::util::now() - 10 * 86_400;
        for v in [
            json!(crate::util::iso_datetime(ten_days_ago)),
            json!(ten_days_ago),
        ] {
            let s = score(&p, &[f("v", v)], &ran());
            match s.signals[0].value {
                MetricValue::Num(n) => assert!((n - 10.0).abs() < 0.01, "got {n}"),
                ref other => panic!("expected a number, got {other:?}"),
            }
            assert_eq!(s.signals[0].status, Status::Warn);
        }
    }

    #[test]
    fn an_unparseable_timestamp_is_missing_not_zero_days() {
        let p = pack("[[metric]]\nid='m'\nfact='v'\nagg='days_since'\nwarn='> 5'\n");
        let s = score(&p, &[f("v", json!("whenever"))], &ran());
        assert_eq!(s.signals[0].value, MetricValue::Missing);
    }

    #[test]
    fn booleans_score_as_one_and_zero() {
        let p = pack("[[metric]]\nid='m'\nfact='v'\nfail='== 1'\n");
        assert_eq!(
            score(&p, &[f("v", json!(true))], &ran()).signals[0].status,
            Status::Fail
        );
        assert_eq!(
            score(&p, &[f("v", json!(false))], &ran()).signals[0].status,
            Status::Ok
        );
    }

    #[test]
    fn found_but_uninteresting_data_is_ok_while_absent_data_stays_unknown() {
        let p = pack("[[metric]]\nid='m'\nfact='v'\nwarn='< 10'\n");
        assert_eq!(
            score(&p, &[f("v", json!(50))], &ran()).signals[0].status,
            Status::Ok
        );
        // Nothing found and no threshold asked about absence: must not score as ok.
        assert_eq!(score(&p, &[], &ran()).signals[0].status, Status::Unknown);
    }

    #[test]
    fn confidence_is_carried_from_the_facts_that_were_used() {
        let p = pack("[[metric]]\nid='m'\nfact='v'\nwarn='< 10'\n");
        let s = score(
            &p,
            &[Fact::new("v", json!(50), "s", "u", 0, Method::Llm)],
            &ran(),
        );
        assert_eq!(s.signals[0].confidence, Method::Llm.confidence());
        assert_eq!(s.confidence, Method::Llm.confidence());
    }

    #[test]
    fn ratio_divides_two_facts_so_vendors_become_comparable() {
        // $29/month for 15 minutes is $1.93/minute; the model extracted both numbers
        // as written and pilotage did the arithmetic.
        let p =
            pack("[[metric]]\nid='cpm'\nfact='v'\nover='w'\nagg='ratio'\nwarn='> 2'\nfail='> 5'\n");
        let s = score(&p, &[f("v", json!(29.0)), f("w", json!(15.0))], &ran());
        match s.signals[0].value {
            MetricValue::Num(n) => assert!((n - 1.9333).abs() < 0.001, "got {n}"),
            ref other => panic!("expected a number, got {other:?}"),
        }
        assert_eq!(s.signals[0].status, Status::Ok);
    }

    #[test]
    fn a_ratio_threshold_still_fires() {
        let p =
            pack("[[metric]]\nid='cpm'\nfact='v'\nover='w'\nagg='ratio'\nwarn='> 2'\nfail='> 5'\n");
        // $99 for 10 minutes is $9.90/minute.
        let s = score(&p, &[f("v", json!(99.0)), f("w", json!(10.0))], &ran());
        assert_eq!(s.signals[0].status, Status::Fail);
    }

    #[test]
    fn a_ratio_with_a_missing_or_zero_divisor_is_missing_not_infinity() {
        let p = pack("[[metric]]\nid='cpm'\nfact='v'\nover='w'\nagg='ratio'\nwarn='> 2'\n");
        // Quote-only pricing: a price with no stated allowance.
        let s = score(&p, &[f("v", json!(29.0))], &ran());
        assert_eq!(s.signals[0].value, MetricValue::Missing);
        assert_eq!(s.signals[0].status, Status::Unknown);

        let s = score(&p, &[f("v", json!(29.0)), f("w", json!(0.0))], &ran());
        assert_eq!(s.signals[0].value, MetricValue::Missing);
    }

    #[test]
    fn a_ratio_uses_the_highest_confidence_divisor() {
        let p = pack("[[metric]]\nid='cpm'\nfact='v'\nover='w'\nagg='ratio'\nwarn='> 100'\n");
        let guessed = Fact::new("w", json!(1.0), "b", "u", 0, Method::Llm);
        let stated = Fact::new("w", json!(10.0), "a", "u", 0, Method::Api);
        let s = score(&p, &[f("v", json!(100.0)), guessed, stated], &ran());
        assert_eq!(s.signals[0].value, MetricValue::Num(10.0));
    }

    #[test]
    fn a_filter_narrows_a_list_before_counting_it() {
        // The question is not "how many incidents" but "how many were about latency".
        let p = pack(
            "[[metric]]\nid='lat'\nfact='v'\nagg='count'\nfilter='(?i)(latency|slow|degrad)'\nwarn='> 1'\n",
        );
        let incidents = json!([
            "Elevated video translate latency and failure",
            "Slow down/Limited Capabilities",
            "Scheduled maintenance",
            "Degraded Service",
            "Billing page copy fix"
        ]);
        let s = score(&p, &[f("v", incidents)], &ran());
        assert_eq!(s.signals[0].value, MetricValue::Num(3.0));
        assert_eq!(s.signals[0].status, Status::Warn);
    }

    #[test]
    fn a_filter_matching_nothing_counts_zero_not_everything() {
        let p = pack("[[metric]]\nid='m'\nfact='v'\nagg='count'\nfilter='(?i)zzz'\nwarn='> 0'\n");
        let s = score(&p, &[f("v", json!(["a", "b"]))], &ran());
        assert_eq!(s.signals[0].value, MetricValue::Num(0.0));
    }

    #[test]
    fn a_broken_filter_regex_yields_missing_rather_than_the_unfiltered_count() {
        // Reporting 5 when the author asked for "5 matching X" would be a lie.
        let p = pack("[[metric]]\nid='m'\nfact='v'\nagg='count'\nfilter='(unclosed'\nwarn='> 0'\n");
        let s = score(&p, &[f("v", json!(["a", "b", "c", "d", "e"]))], &ran());
        assert_ne!(s.signals[0].value, MetricValue::Num(5.0));
    }

    #[test]
    fn a_filter_composes_with_take_and_aggregation() {
        let p = pack(
            "[[metric]]\nid='m'\nfact='v'\nagg='count'\nfilter='(?i)major'\ntake=-2\nwarn='> 5'\n",
        );
        // Filter first (4 majors), then the last 2 of those.
        let s = score(
            &p,
            &[f(
                "v",
                json!(["major a", "minor", "major b", "major c", "none", "major d"]),
            )],
            &ran(),
        );
        assert_eq!(s.signals[0].value, MetricValue::Num(2.0));
    }

    #[test]
    fn a_pinned_metric_carries_its_flag_onto_the_signal() {
        let p = pack("[[metric]]\nid='m'\nfact='v'\nwarn='is none'\npin=true\n");
        let s = score(&p, &[f("v", json!("Go (programming language)"))], &ran());
        assert!(s.signals[0].pin, "pin must survive onto the signal");
        assert_eq!(s.signals[0].status, Status::Ok);
    }

    #[test]
    fn large_numbers_render_compactly() {
        assert_eq!(
            display(&MetricValue::Num(1_234_567_890.0), Some("usd")),
            "$1.23B"
        );
        assert_eq!(
            display(&MetricValue::Num(4_500_000.0), Some("usd")),
            "$4.50M"
        );
        assert_eq!(display(&MetricValue::Num(950.0), Some("usd")), "$950");
        assert_eq!(display(&MetricValue::Num(-42.0), Some("%")), "-42%");
        assert_eq!(display(&MetricValue::Num(90.0), Some("d")), "90d");
        assert_eq!(display(&MetricValue::Missing, Some("usd")), "none");
    }

    #[test]
    fn long_strings_are_truncated_so_one_field_cannot_dominate_the_output() {
        let long = "x".repeat(500);
        assert_eq!(display(&MetricValue::Str(long), None).len(), 60);
    }

    #[test]
    fn an_empty_pack_produces_insufficient_data_rather_than_dividing_by_zero() {
        let p = pack("");
        let s = score(&p, &[], &ran());
        assert_eq!(s.verdict, Verdict::InsufficientData);
        assert_eq!(s.score, 0.0);
        assert!(s.signals.is_empty());
    }
}
