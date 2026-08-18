//! Packs: verticals expressed as data.
//!
//! The engine knows nothing about crypto, SaaS or open source. A pack declares which
//! targets it recognises, where to fetch, how to pull facts out, and which thresholds
//! turn those facts into a verdict. Adding a vertical is adding a TOML file.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Packs shipped in the binary, so a fresh install researches something immediately
/// without a separate download step.
const BUILTIN: &[(&str, &str)] = &[
    ("oss", include_str!("../packs/oss.toml")),
    ("crypto", include_str!("../packs/crypto.toml")),
    ("saas", include_str!("../packs/saas.toml")),
    ("aivideo", include_str!("../packs/aivideo.toml")),
    ("probe", include_str!("../packs/probe.toml")),
    ("topic", include_str!("../packs/topic.toml")),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Api,
    Html,
    /// Delegated to `chrome-devtools-axi` — pilotage does not embed a browser.
    Browser,
    /// Submits a job to a vendor API and times it to completion.
    ///
    /// The only source kind that *measures* rather than reads. A vendor's own latency
    /// cannot be looked up anywhere — it has to be provoked — so a probe POSTs a real
    /// request, polls until the job finishes, and synthesises a document describing
    /// what happened. Probes cost the vendor's credits, so they are always
    /// `auth_required` and normally `depth = "deep"`.
    Probe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Depth {
    #[default]
    Quick,
    Deep,
}

impl std::str::FromStr for Depth {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "quick" => Ok(Depth::Quick),
            "deep" => Ok(Depth::Deep),
            _ => bail!("depth must be 'quick' or 'deep', got '{s}'"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Selector {
    /// CSS selector to run against markup. Absent means the selector works on the
    /// document's plain text instead, which is how browser snapshots — a rendered
    /// accessibility tree, not HTML — are read.
    #[serde(default)]
    pub css: Option<String>,
    /// Attribute to read; absent means the element's text.
    #[serde(default)]
    pub attr: Option<String>,
    /// Collect every match into an array instead of taking the first.
    #[serde(default)]
    pub all: bool,
    /// Optional narrowing regex applied to the selected text; capture group 1 wins
    /// when present, otherwise the whole match.
    #[serde(default)]
    pub regex: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LlmExtract {
    /// Logical tier, resolved to a concrete provider at call time.
    #[serde(default = "d_model")]
    pub model: String,
    pub fields: Vec<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    /// Hard cap on characters sent to the model. The single largest cost lever in
    /// the whole system.
    #[serde(default = "d_max_chars")]
    pub max_chars: usize,
}

/// How a probe waits for an asynchronous job to finish.
#[derive(Debug, Clone, Deserialize)]
pub struct Poll {
    /// Status endpoint, with `{id}` substituted from the submit response.
    pub url: String,
    /// Where the job id lives in the submit response. Absent means the status URL
    /// needs no id.
    #[serde(default)]
    pub id_path: Option<String>,
    /// Where the job state lives in each status response.
    pub status_path: String,
    /// States that mean finished, and finished-badly. Anything else keeps polling.
    pub done: Vec<String>,
    #[serde(default)]
    pub failed: Vec<String>,
    #[serde(default = "d_poll_interval")]
    pub interval: String,
    /// Hard cap on the whole probe. A vendor that never finishes must not hang the run.
    #[serde(default = "d_poll_timeout")]
    pub timeout: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Source {
    pub id: String,
    #[serde(default)]
    pub kind: Kind,
    pub url: String,
    /// HTTP method. Only probes need anything other than GET.
    #[serde(default = "d_method")]
    pub method: String,
    /// Request body, as a raw string with `{var}` templating.
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default = "d_content_type")]
    pub content_type: String,
    /// Present on a probe whose job completes asynchronously.
    #[serde(default)]
    pub poll: Option<Poll>,
    #[serde(default = "d_ttl")]
    pub ttl: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Environment variable holding a credential. Absent variable is not an error —
    /// the source is skipped and reported as degraded.
    #[serde(default)]
    pub auth_env: Option<String>,
    #[serde(default = "d_auth_header")]
    pub auth_header: String,
    #[serde(default = "d_auth_format")]
    pub auth_format: String,
    /// When true, a missing credential disables the source instead of falling back
    /// to an anonymous request. Most public APIs serve unauthenticated traffic at a
    /// lower rate limit, so this stays false unless the endpoint truly requires a key.
    #[serde(default)]
    pub auth_required: bool,
    /// A source that may legitimately be absent for a given target — a protocol with
    /// no CoinGecko listing, a company with no /pricing page. Its failure is reported
    /// as degradation rather than as a problem with the run.
    #[serde(default)]
    pub optional: bool,
    /// Minimum depth at which this source runs.
    #[serde(default)]
    pub depth: Depth,
    /// Relative importance when the budget forces sources to be dropped.
    #[serde(default = "d_weight")]
    pub weight: f64,
    /// Rung 1: JSON field map, `fact_key = "<json path>"`.
    #[serde(default)]
    pub map: HashMap<String, String>,
    /// Rung 2: CSS/regex extraction from markup.
    #[serde(default)]
    pub select: HashMap<String, Selector>,
    /// Rung 3: cheap-model extraction, only for what the rungs above could not reach.
    #[serde(default)]
    pub extract: Option<LlmExtract>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Metric {
    pub id: String,
    /// Fact key this metric reads.
    pub fact: String,
    /// Optional reduction applied to the fact value.
    #[serde(default)]
    pub agg: Option<String>,
    /// Regex filter applied to array elements before aggregating.
    ///
    /// This is what turns a raw list into an answerable question: not "how many
    /// incidents" but "how many incidents mentioned latency", not "how many comments"
    /// but "how many complained about price". The filter is a property of the metric,
    /// so one fetched list serves several questions.
    #[serde(default)]
    pub filter: Option<String>,
    /// Divisor fact for `agg = "ratio"`.
    ///
    /// This is what makes prices comparable across vendors that sell different units:
    /// one sells credits, another minutes, another API calls. The model extracts both
    /// numbers as they are written; pilotage does the division. Arithmetic belongs
    /// here, not in a prompt.
    #[serde(default)]
    pub over: Option<String>,
    /// Window applied to an array fact before aggregating: positive takes the first
    /// N elements, negative the last N. This is what turns a full TVL history into
    /// "the last 30 days".
    #[serde(default)]
    pub take: Option<i64>,
    #[serde(default)]
    pub warn: Option<String>,
    #[serde(default)]
    pub fail: Option<String>,
    #[serde(default = "d_weight")]
    pub weight: f64,
    /// A failing critical metric forces the overall verdict to `risk` regardless of
    /// how healthy everything else looks.
    #[serde(default)]
    pub critical: bool,
    /// Always show this metric's value, even when it passes.
    ///
    /// The compact view lists healthy metrics by name only, which is right for almost
    /// everything — but some values are context you need in order to trust the rest.
    /// "Which Wikipedia article did this query actually match?" is worthless as a name
    /// and decisive as a value.
    #[serde(default)]
    pub pin: bool,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub unit: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Pack {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Target patterns this pack claims, most specific first.
    #[serde(default)]
    pub detect: Vec<String>,
    /// Static variables merged into every URL template.
    #[serde(default)]
    pub vars: HashMap<String, String>,
    /// Relative difference above which two sources are considered to disagree.
    #[serde(default = "d_tolerance")]
    pub tolerance: f64,
    #[serde(default)]
    pub source: Vec<Source>,
    #[serde(default)]
    pub metric: Vec<Metric>,
}

fn d_ttl() -> String {
    "1h".into()
}
fn d_auth_header() -> String {
    "Authorization".into()
}
fn d_auth_format() -> String {
    "Bearer {}".into()
}
fn d_model() -> String {
    "flash".into()
}
fn d_max_chars() -> usize {
    12_000
}
fn d_weight() -> f64 {
    1.0
}
fn d_tolerance() -> f64 {
    0.05
}
fn d_method() -> String {
    "GET".into()
}
fn d_content_type() -> String {
    "application/json".into()
}
fn d_poll_interval() -> String {
    "5s".into()
}
fn d_poll_timeout() -> String {
    "5m".into()
}

impl Pack {
    pub fn parse(text: &str) -> Result<Pack> {
        toml::from_str(text).context("pack is not valid TOML")
    }

    pub fn load_file(path: &Path) -> Result<Pack> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading pack {}", path.display()))?;
        Pack::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    /// Does this pack recognise the target, and with which variables?
    pub fn detect_target(&self, target: &str) -> Option<HashMap<String, String>> {
        let normalized = normalize_target(target);
        for pattern in &self.detect {
            if let Some(vars) = match_pattern(pattern, &normalized) {
                return Some(self.finish_vars(vars, target, &normalized));
            }
        }
        None
    }

    /// Variables for a run. When the user forces a pack with `--pack`, any variable
    /// its patterns expect but the target did not supply falls back to the target
    /// itself — so `pilotage research aave --pack crypto` works without the user
    /// having to know the pack writes `{slug}`.
    pub fn vars_for(&self, target: &str, forced: bool) -> Option<HashMap<String, String>> {
        if let Some(v) = self.detect_target(target) {
            return Some(v);
        }
        if !forced {
            return None;
        }
        let normalized = normalize_target(target);
        let mut vars = HashMap::new();
        for pattern in &self.detect {
            for name in pattern_var_names(pattern) {
                vars.entry(name).or_insert_with(|| normalized.clone());
            }
        }
        Some(self.finish_vars(vars, target, &normalized))
    }

    /// Merges pack constants, target identity and date windows into the captured
    /// variables. Captured names win — a pack that writes `{domain}` in its pattern
    /// means that capture, not the derived host.
    fn finish_vars(
        &self,
        mut vars: HashMap<String, String>,
        target: &str,
        normalized: &str,
    ) -> HashMap<String, String> {
        for (k, v) in &self.vars {
            vars.entry(k.clone()).or_insert_with(|| v.clone());
        }
        vars.entry("target".into())
            .or_insert_with(|| target.to_string());
        vars.entry("slug".into())
            .or_insert_with(|| normalized.to_string());
        if let Some(host) = normalized.split('/').next()
            && host.contains('.')
        {
            vars.entry("host".into())
                .or_insert_with(|| host.to_string());
            vars.entry("domain".into())
                .or_insert_with(|| host.to_string());
            // `{brand}` is the host without its TLD. App stores, review sites and
            // discussion search all key on a product name rather than a domain: on
            // Hacker News "heygen" finds 2856 comments and "heygen.com" finds 6.
            if let Some((brand, _)) = host.rsplit_once('.') {
                vars.entry("brand".into())
                    .or_insert_with(|| brand.to_string());
            }
        }
        for (k, v) in crate::util::date_vars(crate::util::now()) {
            vars.entry(k).or_insert(v);
        }

        // Free-text targets ("machine learning") cannot go into a URL as-is, and some
        // APIs want underscores rather than spaces. Every variable gets both spellings
        // so a pack picks the one its endpoint expects instead of the engine guessing.
        let spelled: Vec<(String, String)> = vars
            .iter()
            .filter(|(k, _)| !k.ends_with("_enc") && !k.ends_with("_us"))
            .flat_map(|(k, v)| {
                [
                    (format!("{k}_enc"), crate::util::percent_encode(v)),
                    (format!("{k}_us"), v.replace(' ', "_")),
                ]
            })
            .collect();
        for (k, v) in spelled {
            vars.entry(k).or_insert(v);
        }
        vars
    }

    /// Static checks that catch the mistakes pack authors actually make: duplicate
    /// ids, thresholds that do not parse, and metrics pointing at facts no source
    /// produces — the last of which would otherwise show up as a silently
    /// unscoreable metric at runtime.
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();

        if self.name.trim().is_empty() {
            issues.push("pack has an empty name".into());
        }
        if self.detect.is_empty() {
            issues.push("pack has no `detect` patterns, so it can never be selected".into());
        }

        let mut seen = std::collections::HashSet::new();
        for s in &self.source {
            if !seen.insert(&s.id) {
                issues.push(format!("duplicate source id '{}'", s.id));
            }
            if let Err(e) = crate::util::parse_duration(&s.ttl) {
                issues.push(format!("source '{}': {e}", s.id));
            }
            // `api` and `probe` both yield JSON and are read with a `map`; `html` and
            // `browser` yield documents and are read with selectors.
            let json_kind = matches!(s.kind, Kind::Api | Kind::Probe);
            if json_kind && !s.select.is_empty() {
                issues.push(format!(
                    "source '{}' is kind={:?} but declares CSS selectors",
                    s.id, s.kind
                ));
            }
            if !json_kind && !s.map.is_empty() {
                issues.push(format!(
                    "source '{}' is kind={:?} but declares a JSON map",
                    s.id, s.kind
                ));
            }

            if s.kind == Kind::Probe {
                // A probe spends the vendor's credits on every run. Requiring a
                // credential is not a formality: it is the thing that stops a market
                // scan from quietly generating billable video.
                if s.auth_env.is_none() {
                    issues.push(format!(
                        "probe '{}' has no auth_env — a probe must be credential-gated",
                        s.id
                    ));
                } else if !s.auth_required {
                    issues.push(format!(
                        "probe '{}' must set auth_required = true, or it would run unauthenticated",
                        s.id
                    ));
                }
                if let Some(p) = &s.poll {
                    if p.done.is_empty() {
                        issues.push(format!(
                            "probe '{}': poll has no `done` states, so it could never finish",
                            s.id
                        ));
                    }
                    for (label, expr) in [("interval", &p.interval), ("timeout", &p.timeout)] {
                        if let Err(e) = crate::util::parse_duration(expr) {
                            issues.push(format!("probe '{}': poll {label}: {e}", s.id));
                        }
                    }
                    if let (Ok(i), Ok(t)) = (
                        crate::util::parse_duration(&p.interval),
                        crate::util::parse_duration(&p.timeout),
                    ) && i > t
                    {
                        issues.push(format!(
                            "probe '{}': poll interval ({}) exceeds its timeout ({})",
                            s.id, p.interval, p.timeout
                        ));
                    }
                    if p.url.contains("{id}") && p.id_path.is_none() {
                        issues.push(format!(
                            "probe '{}': poll url uses {{id}} but no id_path says where to find it",
                            s.id
                        ));
                    }
                }
            }

            let method = s.method.to_ascii_uppercase();
            if !["GET", "POST", "PUT"].contains(&method.as_str()) {
                issues.push(format!(
                    "source '{}': unsupported method '{}'",
                    s.id, s.method
                ));
            }
            if s.body.is_some() && method == "GET" {
                issues.push(format!("source '{}' declares a body but uses GET", s.id));
            }
            for (key, sel) in &s.select {
                if let Some(re) = &sel.regex
                    && let Err(e) = regex::Regex::new(re)
                {
                    issues.push(format!(
                        "source '{}': selector '{key}': bad regex: {e}",
                        s.id
                    ));
                }
                if sel.css.is_none() && sel.regex.is_none() {
                    issues.push(format!(
                        "source '{}': selector '{key}' has neither a css selector nor a regex",
                        s.id
                    ));
                }
                if s.kind == Kind::Html && sel.css.is_none() {
                    issues.push(format!(
                        "source '{}': selector '{key}' needs a css selector for an html source",
                        s.id
                    ));
                }
            }
        }

        let produced: std::collections::HashSet<&str> = self
            .source
            .iter()
            .flat_map(|s| {
                s.map
                    .keys()
                    .chain(s.select.keys())
                    .map(String::as_str)
                    .chain(
                        s.extract
                            .iter()
                            .flat_map(|e| e.fields.iter().map(String::as_str)),
                    )
            })
            .collect();

        let mut seen_m = std::collections::HashSet::new();
        for m in &self.metric {
            if !seen_m.insert(&m.id) {
                issues.push(format!("duplicate metric id '{}'", m.id));
            }
            if !produced.contains(m.fact.as_str()) {
                issues.push(format!(
                    "metric '{}' reads fact '{}' which no source produces",
                    m.id, m.fact
                ));
            }
            if let Some(agg) = &m.agg
                && !VALID_AGGS.contains(&agg.as_str())
            {
                issues.push(format!(
                    "metric '{}': unknown agg '{agg}' (expected one of {})",
                    m.id,
                    VALID_AGGS.join(", ")
                ));
            }
            for (label, expr) in [("warn", &m.warn), ("fail", &m.fail)] {
                if let Some(e) = expr
                    && let Err(err) = Cond::parse(e)
                {
                    issues.push(format!("metric '{}': {label} threshold: {err}", m.id));
                }
            }
            if m.warn.is_none() && m.fail.is_none() {
                issues.push(format!(
                    "metric '{}' has neither a warn nor a fail threshold, so it can never affect the verdict",
                    m.id
                ));
            }
            if let Some(f) = &m.filter {
                if let Err(e) = regex::Regex::new(f) {
                    issues.push(format!("metric '{}': bad filter regex: {e}", m.id));
                }
                if m.agg.is_none() {
                    issues.push(format!(
                        "metric '{}' has a filter but no agg, so the filter changes nothing",
                        m.id
                    ));
                }
            }
            match (&m.agg, &m.over) {
                (Some(a), None) if a == "ratio" => issues.push(format!(
                    "metric '{}': agg = \"ratio\" needs an `over` fact to divide by",
                    m.id
                )),
                (agg, Some(over)) => {
                    if agg.as_deref() != Some("ratio") {
                        issues.push(format!(
                            "metric '{}' declares `over` but its agg is not \"ratio\"",
                            m.id
                        ));
                    }
                    if !produced.contains(over.as_str()) {
                        issues.push(format!(
                            "metric '{}' divides by fact '{over}' which no source produces",
                            m.id
                        ));
                    }
                }
                _ => {}
            }
        }

        for p in &self.detect {
            if let Err(e) = pattern_regex(p) {
                issues.push(format!("detect pattern '{p}': {e}"));
            }
        }

        issues
    }
}

pub const VALID_AGGS: &[&str] = &[
    "count",
    "sum",
    "avg",
    "max",
    "min",
    "first",
    "days_since",
    "pct_change",
    "unique",
    "ratio",
];

/// Where user packs live. Anything here is loaded at runtime, so a new vertical needs
/// no recompile.
pub fn user_pack_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PILOTAGE_HOME") {
        return PathBuf::from(dir).join("packs");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".pilotage").join("packs")
}

/// Built-in packs, then user packs. A user pack with the same name wins, which is how
/// a shipped pack gets patched without waiting for a release.
pub fn load_all() -> Result<Vec<Pack>> {
    let mut by_name: HashMap<String, Pack> = HashMap::new();
    for (name, text) in BUILTIN {
        let p = Pack::parse(text).with_context(|| format!("builtin pack '{name}' is broken"))?;
        by_name.insert(p.name.clone(), p);
    }
    let dir = user_pack_dir();
    if dir.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            let p = Pack::load_file(&path)?;
            by_name.insert(p.name.clone(), p);
        }
    }
    let mut packs: Vec<Pack> = by_name.into_values().collect();
    packs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(packs)
}

pub fn find(packs: &[Pack], name: &str) -> Option<usize> {
    packs.iter().position(|p| p.name == name)
}

/// Picks the pack whose `detect` patterns match the target. Ties break on pattern
/// specificity — a pattern with more literal characters is the better claim, so
/// `github.com/{o}/{r}` beats a bare `{o}/{r}`.
pub fn select_for<'a>(
    packs: &'a [Pack],
    target: &str,
) -> Option<(&'a Pack, HashMap<String, String>)> {
    let mut best: Option<(usize, &Pack, HashMap<String, String>)> = None;
    for p in packs {
        if let Some(vars) = p.detect_target(target) {
            let score = p
                .detect
                .iter()
                .filter(|pat| match_pattern(pat, &normalize_target(target)).is_some())
                .map(|pat| specificity(pat))
                .max()
                .unwrap_or(0);
            if best.as_ref().is_none_or(|(b, _, _)| score > *b) {
                best = Some((score, p, vars));
            }
        }
    }
    best.map(|(_, p, v)| (p, v))
}

/// How strong a claim a pattern makes on a target, measured in literal characters.
/// `github.com/{owner}/{repo}` outranks a bare `{owner}/{repo}`, so the specific pack
/// wins when both match.
fn specificity(pattern: &str) -> usize {
    if let Some(re) = pattern.strip_prefix("re:") {
        return re.len();
    }
    pattern.len() - pattern.matches('{').count() * 2
}

/// Variable names a pattern expects, used to fill in defaults for a forced pack.
fn pattern_var_names(pattern: &str) -> Vec<String> {
    if let Some(re) = pattern.strip_prefix("re:") {
        return regex::Regex::new(re)
            .map(|r| r.capture_names().flatten().map(String::from).collect())
            .unwrap_or_default();
    }
    let mut names = Vec::new();
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}').map(|i| i + open) else {
            break;
        };
        names.push(rest[open + 1..close].to_string());
        rest = &rest[close + 1..];
    }
    names
}

/// Strips the parts of a target that carry no meaning, so `https://github.com/a/b/`
/// and `github.com/a/b` are the same thing.
pub fn normalize_target(t: &str) -> String {
    let t = t.trim();
    let t = t
        .strip_prefix("https://")
        .or_else(|| t.strip_prefix("http://"))
        .unwrap_or(t);
    let t = t.strip_prefix("www.").unwrap_or(t);
    t.trim_end_matches('/').to_string()
}

fn pattern_regex(pattern: &str) -> Result<regex::Regex> {
    // Escape hatch: some targets (a bare hostname, but not a bare word) cannot be
    // expressed with `{var}` segments. Rather than grow a pattern mini-language,
    // `re:` hands the pack author a raw regex with named groups.
    if let Some(raw) = pattern.strip_prefix("re:") {
        return regex::Regex::new(raw).context("raw `re:` pattern did not compile");
    }
    let mut re = String::from("^");
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        re.push_str(&regex::escape(&rest[..open]));
        let close = rest[open..]
            .find('}')
            .ok_or_else(|| anyhow::anyhow!("unclosed '{{' in pattern"))?
            + open;
        let name = &rest[open + 1..close];
        if name.is_empty() {
            bail!("empty variable name in pattern");
        }
        re.push_str(&format!("(?P<{name}>[^/]+)"));
        rest = &rest[close + 1..];
    }
    // A trailing `/*` means "and anything below", which is how doc trees are matched.
    if let Some(head) = rest.strip_suffix("/*") {
        re.push_str(&regex::escape(head));
        re.push_str("(?:/.*)?");
    } else {
        re.push_str(&regex::escape(rest));
    }
    re.push('$');
    regex::Regex::new(&re).context("pattern did not compile")
}

pub fn match_pattern(pattern: &str, target: &str) -> Option<HashMap<String, String>> {
    let re = pattern_regex(pattern).ok()?;
    let caps = re.captures(target)?;
    let mut vars = HashMap::new();
    for name in re.capture_names().flatten() {
        if let Some(m) = caps.name(name) {
            vars.insert(name.to_string(), m.as_str().to_string());
        }
    }
    Some(vars)
}

// ---------------------------------------------------------------------------
// Threshold expressions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    Num(f64),
    Str(String),
    Missing,
}

/// One side of a `warn = "< 10"` threshold.
#[derive(Debug, Clone)]
pub struct Cond {
    pub op: Op,
    pub rhs: Operand,
}

impl Cond {
    pub fn parse(s: &str) -> Result<Cond> {
        let s = s.trim();
        if s.is_empty() {
            bail!("empty threshold expression");
        }
        // `is none` / `is empty` read better than `== none` in a pack file.
        if let Some(rest) = s.strip_prefix("is ") {
            return match rest.trim() {
                "none" | "empty" | "missing" => Ok(Cond {
                    op: Op::Eq,
                    rhs: Operand::Missing,
                }),
                other => bail!("expected 'is none' or 'is empty', got 'is {other}'"),
            };
        }
        let (op, rest) = if let Some(r) = s.strip_prefix("<=") {
            (Op::Le, r)
        } else if let Some(r) = s.strip_prefix(">=") {
            (Op::Ge, r)
        } else if let Some(r) = s.strip_prefix("==") {
            (Op::Eq, r)
        } else if let Some(r) = s.strip_prefix("!=") {
            (Op::Ne, r)
        } else if let Some(r) = s.strip_prefix('<') {
            (Op::Lt, r)
        } else if let Some(r) = s.strip_prefix('>') {
            (Op::Gt, r)
        } else {
            bail!("'{s}' does not start with a comparison operator (<, <=, >, >=, ==, !=)");
        };

        let rest = rest.trim();
        if rest.is_empty() {
            bail!("'{s}' has an operator but no value");
        }
        let rhs = if rest == "none" || rest == "empty" || rest == "missing" {
            Operand::Missing
        } else if let Ok(n) = rest.parse::<f64>() {
            Operand::Num(n)
        } else {
            Operand::Str(rest.trim_matches(['"', '\'']).to_string())
        };
        Ok(Cond { op, rhs })
    }

    /// Evaluates against a computed metric value. A missing value only satisfies an
    /// explicit `is none` style check; ordinary comparisons against absent data are
    /// false, and the metric is reported as unknown rather than quietly passing.
    pub fn eval(&self, value: Option<&MetricValue>) -> bool {
        match (value, &self.rhs) {
            (None, Operand::Missing) => self.op == Op::Eq,
            (Some(MetricValue::Missing), Operand::Missing) => self.op == Op::Eq,
            (Some(_), Operand::Missing) => self.op == Op::Ne,
            (None, _) => false,
            (Some(MetricValue::Missing), _) => false,
            (Some(MetricValue::Num(a)), Operand::Num(b)) => cmp_num(*a, self.op, *b),
            (Some(MetricValue::Num(a)), Operand::Str(b)) => match b.parse::<f64>() {
                Ok(b) => cmp_num(*a, self.op, b),
                Err(_) => matches!(self.op, Op::Ne),
            },
            (Some(MetricValue::Str(a)), Operand::Str(b)) => match self.op {
                Op::Eq => a.eq_ignore_ascii_case(b),
                Op::Ne => !a.eq_ignore_ascii_case(b),
                _ => match (a.parse::<f64>(), b.parse::<f64>()) {
                    (Ok(x), Ok(y)) => cmp_num(x, self.op, y),
                    _ => false,
                },
            },
            (Some(MetricValue::Str(a)), Operand::Num(b)) => {
                match crate::fact::parse_loose_number(a) {
                    Some(x) => cmp_num(x, self.op, *b),
                    None => matches!(self.op, Op::Ne),
                }
            }
        }
    }
}

fn cmp_num(a: f64, op: Op, b: f64) -> bool {
    match op {
        Op::Lt => a < b,
        Op::Le => a <= b,
        Op::Gt => a > b,
        Op::Ge => a >= b,
        Op::Eq => (a - b).abs() < f64::EPSILON * a.abs().max(b.abs()).max(1.0),
        Op::Ne => (a - b).abs() >= f64::EPSILON * a.abs().max(b.abs()).max(1.0),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricValue {
    Num(f64),
    Str(String),
    Missing,
}

impl std::fmt::Display for MetricValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetricValue::Num(n) => {
                if n.fract().abs() < 1e-9 && n.abs() < 1e15 {
                    write!(f, "{}", *n as i64)
                } else {
                    write!(f, "{n:.2}")
                }
            }
            MetricValue::Str(s) => write!(f, "{s}"),
            MetricValue::Missing => write!(f, "none"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_packs_all_parse_and_validate() {
        for (name, text) in BUILTIN {
            let p =
                Pack::parse(text).unwrap_or_else(|e| panic!("pack {name} failed to parse: {e}"));
            let issues = p.validate();
            assert!(issues.is_empty(), "pack {name} has issues: {issues:#?}");
        }
    }

    #[test]
    fn detect_extracts_variables() {
        let vars = match_pattern("github.com/{owner}/{repo}", "github.com/rust-lang/rust").unwrap();
        assert_eq!(vars["owner"], "rust-lang");
        assert_eq!(vars["repo"], "rust");
    }

    #[test]
    fn detect_is_anchored_and_rejects_extra_segments() {
        assert!(match_pattern("github.com/{owner}/{repo}", "github.com/a/b/c").is_none());
        assert!(match_pattern("github.com/{owner}/{repo}", "x.com/a/b").is_none());
    }

    #[test]
    fn trailing_wildcard_matches_subtrees() {
        assert!(match_pattern("docs.x.com/*", "docs.x.com").is_some());
        assert!(match_pattern("docs.x.com/*", "docs.x.com/a/b/c").is_some());
    }

    #[test]
    fn normalization_makes_url_forms_equivalent() {
        assert_eq!(
            normalize_target("https://github.com/a/b/"),
            "github.com/a/b"
        );
        assert_eq!(normalize_target("http://www.x.com"), "x.com");
        assert_eq!(normalize_target("  a/b  "), "a/b");
    }

    #[test]
    fn more_literal_patterns_win_the_tie() {
        let packs = load_all().unwrap();
        let (pack, vars) = select_for(&packs, "https://github.com/rust-lang/rust").unwrap();
        assert_eq!(pack.name, "oss");
        assert_eq!(vars["owner"], "rust-lang");
        assert_eq!(vars["repo"], "rust");
        assert_eq!(vars["slug"], "github.com/rust-lang/rust");
    }

    #[test]
    fn unknown_targets_select_nothing_rather_than_guessing() {
        let packs = load_all().unwrap();
        assert!(select_for(&packs, "not a real target at all").is_none());
    }

    #[test]
    fn thresholds_parse_every_supported_form() {
        for s in [
            "< 10",
            "<=10",
            "> 0.5",
            ">= 1",
            "== none",
            "!= none",
            "is none",
            "== \"MIT\"",
        ] {
            Cond::parse(s).unwrap_or_else(|e| panic!("{s} failed: {e}"));
        }
    }

    #[test]
    fn thresholds_reject_malformed_input() {
        for s in ["", "10", "~ 5", "<", "is banana"] {
            assert!(Cond::parse(s).is_err(), "{s} should not parse");
        }
    }

    #[test]
    fn numeric_comparison() {
        let c = Cond::parse("< 10").unwrap();
        assert!(c.eval(Some(&MetricValue::Num(3.0))));
        assert!(!c.eval(Some(&MetricValue::Num(30.0))));
    }

    #[test]
    fn missing_data_never_silently_passes_a_comparison() {
        let c = Cond::parse("> 100").unwrap();
        assert!(!c.eval(None));
        assert!(!c.eval(Some(&MetricValue::Missing)));
    }

    #[test]
    fn is_none_matches_only_absence() {
        let c = Cond::parse("is none").unwrap();
        assert!(c.eval(None));
        assert!(c.eval(Some(&MetricValue::Missing)));
        assert!(!c.eval(Some(&MetricValue::Num(1.0))));
    }

    #[test]
    fn ne_none_means_present() {
        let c = Cond::parse("!= none").unwrap();
        assert!(c.eval(Some(&MetricValue::Num(1.0))));
        assert!(!c.eval(None));
    }

    #[test]
    fn string_equality_is_case_insensitive() {
        let c = Cond::parse("== \"MIT\"").unwrap();
        assert!(c.eval(Some(&MetricValue::Str("mit".into()))));
        assert!(!c.eval(Some(&MetricValue::Str("GPL-3.0".into()))));
    }

    #[test]
    fn scraped_strings_compare_numerically_when_they_can() {
        let c = Cond::parse("< 10").unwrap();
        assert!(c.eval(Some(&MetricValue::Str("$5".into()))));
    }

    #[test]
    fn validation_catches_a_metric_pointing_at_nothing() {
        let p = Pack::parse(
            r#"
name = "t"
detect = ["{a}/{b}"]
[[source]]
id = "s"
url = "https://x/{a}"
map = { stars = ".stars" }
[[metric]]
id = "m"
fact = "forks"
warn = "< 1"
"#,
        )
        .unwrap();
        let issues = p.validate();
        assert!(
            issues.iter().any(|i| i.contains("no source produces")),
            "{issues:?}"
        );
    }

    #[test]
    fn validation_catches_duplicate_ids_and_bad_thresholds() {
        let p = Pack::parse(
            r#"
name = "t"
detect = ["{a}/{b}"]
[[source]]
id = "s"
url = "https://x"
map = { k = ".k" }
[[source]]
id = "s"
url = "https://y"
map = { k = ".k" }
[[metric]]
id = "m"
fact = "k"
warn = "~~ 3"
"#,
        )
        .unwrap();
        let issues = p.validate();
        assert!(
            issues.iter().any(|i| i.contains("duplicate source id")),
            "{issues:?}"
        );
        assert!(
            issues.iter().any(|i| i.contains("warn threshold")),
            "{issues:?}"
        );
    }

    /// A probe that could run without a credential would let a market scan quietly
    /// spend a vendor's credits. Validation refuses to load one.
    #[test]
    fn validation_refuses_an_ungated_probe() {
        let p = Pack::parse(
            r#"
name = "t"
detect = ["{a}"]
[[source]]
id = "p"
kind = "probe"
url = "https://x/submit"
method = "POST"
body = "{}"
[source.map]
latency = ".probe.latency_s"
[[metric]]
id = "m"
fact = "latency"
warn = "> 60"
"#,
        )
        .unwrap();
        let issues = p.validate();
        assert!(
            issues.iter().any(|i| i.contains("credential-gated")),
            "{issues:?}"
        );
    }

    #[test]
    fn validation_requires_auth_required_on_a_probe() {
        let p = Pack::parse(
            r#"
name = "t"
detect = ["{a}"]
[[source]]
id = "p"
kind = "probe"
url = "https://x/submit"
method = "POST"
auth_env = "SOME_KEY"
[source.map]
latency = ".probe.latency_s"
[[metric]]
id = "m"
fact = "latency"
warn = "> 60"
"#,
        )
        .unwrap();
        assert!(
            p.validate().iter().any(|i| i.contains("auth_required")),
            "{:?}",
            p.validate()
        );
    }

    #[test]
    fn validation_catches_an_unfinishable_or_misconfigured_poll() {
        let base = r#"
name = "t"
detect = ["{a}"]
[[source]]
id = "p"
kind = "probe"
url = "https://x/submit"
method = "POST"
auth_env = "K"
auth_required = true
[source.poll]
url = "https://x/status/{id}"
status_path = ".status"
done = []
interval = "10m"
timeout = "1m"
[source.map]
latency = ".probe.latency_s"
[[metric]]
id = "m"
fact = "latency"
warn = "> 60"
"#;
        let issues = Pack::parse(base).unwrap().validate();
        assert!(
            issues.iter().any(|i| i.contains("no `done` states")),
            "{issues:?}"
        );
        assert!(
            issues.iter().any(|i| i.contains("exceeds its timeout")),
            "{issues:?}"
        );
        assert!(
            issues.iter().any(|i| i.contains("no id_path")),
            "{issues:?}"
        );
    }

    #[test]
    fn validation_rejects_a_body_on_a_get() {
        let p = Pack::parse(
            r#"
name = "t"
detect = ["{a}"]
[[source]]
id = "s"
url = "https://x"
body = "{}"
[source.map]
k = ".k"
[[metric]]
id = "m"
fact = "k"
warn = "< 1"
"#,
        )
        .unwrap();
        assert!(p.validate().iter().any(|i| i.contains("body but uses GET")));
    }

    #[test]
    fn validation_catches_a_ratio_without_a_divisor_and_a_divisor_without_a_ratio() {
        let missing_over = Pack::parse(
            "name='t'\ndetect=['{a}']\n[[source]]\nid='s'\nurl='https://x'\n[source.map]\nv='.v'\n\
             [[metric]]\nid='m'\nfact='v'\nagg='ratio'\nwarn='> 1'\n",
        )
        .unwrap();
        assert!(
            missing_over
                .validate()
                .iter()
                .any(|i| i.contains("needs an `over` fact")),
            "{:?}",
            missing_over.validate()
        );

        let stray_over = Pack::parse(
            "name='t'\ndetect=['{a}']\n[[source]]\nid='s'\nurl='https://x'\n[source.map]\nv='.v'\nw='.w'\n\
             [[metric]]\nid='m'\nfact='v'\nover='w'\nagg='sum'\nwarn='> 1'\n",
        )
        .unwrap();
        assert!(
            stray_over
                .validate()
                .iter()
                .any(|i| i.contains("agg is not")),
            "{:?}",
            stray_over.validate()
        );

        let ghost_divisor = Pack::parse(
            "name='t'\ndetect=['{a}']\n[[source]]\nid='s'\nurl='https://x'\n[source.map]\nv='.v'\n\
             [[metric]]\nid='m'\nfact='v'\nover='ghost'\nagg='ratio'\nwarn='> 1'\n",
        )
        .unwrap();
        assert!(
            ghost_divisor
                .validate()
                .iter()
                .any(|i| i.contains("divides by fact 'ghost'")),
            "{:?}",
            ghost_divisor.validate()
        );
    }

    #[test]
    fn a_probe_reads_json_so_selectors_are_rejected_on_it() {
        let p = Pack::parse(
            r#"
name = "t"
detect = ["{a}"]
[[source]]
id = "p"
kind = "probe"
url = "https://x"
method = "POST"
auth_env = "K"
auth_required = true
[source.select.bad]
css = "title"
[[metric]]
id = "m"
fact = "bad"
warn = "is none"
"#,
        )
        .unwrap();
        assert!(
            p.validate()
                .iter()
                .any(|i| i.contains("declares CSS selectors")),
            "{:?}",
            p.validate()
        );
    }

    #[test]
    fn brand_is_derived_from_a_host_and_encoded_forms_are_offered() {
        let packs = load_all().unwrap();
        let (_, vars) = select_for(&packs, "heygen.com").unwrap();
        assert_eq!(vars["host"], "heygen.com");
        assert_eq!(vars["brand"], "heygen");
        assert_eq!(vars["brand_enc"], "heygen");
    }

    #[test]
    fn a_free_text_target_gets_url_safe_spellings() {
        let packs = load_all().unwrap();
        let (pack, vars) = select_for(&packs, "topic:Go (programming language)").unwrap();
        assert_eq!(pack.name, "topic");
        assert_eq!(vars["q"], "Go (programming language)");
        assert_eq!(vars["q_enc"], "Go%20%28programming%20language%29");
        assert_eq!(vars["q_us"], "Go_(programming_language)");
    }

    #[test]
    fn validation_catches_a_bad_filter_and_a_filter_with_nothing_to_filter() {
        let bad = Pack::parse(
            "name='t'\ndetect=['{a}']\n[[source]]\nid='s'\nurl='https://x'\n[source.map]\nv='.v'\n\
             [[metric]]\nid='m'\nfact='v'\nagg='count'\nfilter='(unclosed'\nwarn='> 1'\n",
        )
        .unwrap();
        assert!(
            bad.validate()
                .iter()
                .any(|i| i.contains("bad filter regex")),
            "{:?}",
            bad.validate()
        );

        let pointless = Pack::parse(
            "name='t'\ndetect=['{a}']\n[[source]]\nid='s'\nurl='https://x'\n[source.map]\nv='.v'\n\
             [[metric]]\nid='m'\nfact='v'\nfilter='x'\nwarn='> 1'\n",
        )
        .unwrap();
        assert!(
            pointless
                .validate()
                .iter()
                .any(|i| i.contains("filter but no agg")),
            "{:?}",
            pointless.validate()
        );
    }

    #[test]
    fn validation_flags_a_metric_that_can_never_fire() {
        let p = Pack::parse(
            r#"
name = "t"
detect = ["{a}/{b}"]
[[source]]
id = "s"
url = "https://x"
map = { k = ".k" }
[[metric]]
id = "m"
fact = "k"
"#,
        )
        .unwrap();
        assert!(
            p.validate()
                .iter()
                .any(|i| i.contains("neither a warn nor a fail"))
        );
    }

    #[test]
    fn metric_values_render_without_pointless_decimals() {
        assert_eq!(MetricValue::Num(42.0).to_string(), "42");
        assert_eq!(MetricValue::Num(1.5).to_string(), "1.50");
        assert_eq!(MetricValue::Missing.to_string(), "none");
    }
}
