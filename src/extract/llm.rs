//! Rung 3: cheap-model extraction, and the budget that keeps it cheap.
//!
//! This is the only place pilotage spends money. The model is asked to *extract* and
//! nothing else — never to infer, estimate, judge or summarise. Anything it returns
//! that is not visibly present in the document is a bug, and the prompt says so.
//!
//! Results are cached against the content hash, so a page that has not changed is
//! never paid for twice.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::sync::Mutex;

/// Hard spending ceiling for a run.
///
/// Checked *before* each call using a conservative estimate, because a ceiling that
/// is only noticed after the money is gone is not a ceiling.
#[derive(Debug)]
pub struct Budget {
    limit: Option<f64>,
    spent: Mutex<f64>,
    blocked: Mutex<bool>,
}

impl Budget {
    pub fn new(limit: Option<f64>) -> Budget {
        Budget {
            limit,
            spent: Mutex::new(0.0),
            blocked: Mutex::new(false),
        }
    }

    pub fn unlimited() -> Budget {
        Budget::new(None)
    }

    pub fn limit(&self) -> Option<f64> {
        self.limit
    }

    pub fn spent(&self) -> f64 {
        *self.spent.lock().unwrap()
    }

    pub fn remaining(&self) -> Option<f64> {
        self.limit.map(|l| (l - self.spent()).max(0.0))
    }

    /// True when the ceiling stopped work at least once during the run. Surfaced in
    /// the output and in the exit code so a truncated run is never mistaken for a
    /// complete one.
    pub fn was_blocked(&self) -> bool {
        *self.blocked.lock().unwrap()
    }

    pub fn can_afford(&self, estimate: f64) -> bool {
        match self.limit {
            None => true,
            Some(limit) => {
                let ok = self.spent() + estimate <= limit;
                if !ok {
                    *self.blocked.lock().unwrap() = true;
                }
                ok
            }
        }
    }

    pub fn record(&self, usd: f64) {
        *self.spent.lock().unwrap() += usd;
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub in_tokens: u64,
    pub out_tokens: u64,
    pub cost_usd: f64,
    pub calls: u64,
}

impl Usage {
    pub fn merge(&mut self, other: Usage) {
        self.in_tokens += other.in_tokens;
        self.out_tokens += other.out_tokens;
        self.cost_usd += other.cost_usd;
        self.calls += other.calls;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Api {
    Gemini,
    /// OpenAI-compatible chat completions — what DeepSeek serves.
    OpenAiCompatible,
}

#[derive(Debug, Clone)]
struct Provider {
    api: Api,
    endpoint: String,
    model: String,
    key: String,
    /// USD per million tokens. Defaults are list prices at the time of writing and
    /// are overridable per tier, because provider pricing moves and a hardcoded
    /// number that silently goes stale would make every budget wrong.
    price_in: f64,
    price_out: f64,
}

pub struct Llm {
    http: reqwest::Client,
}

impl Llm {
    pub fn new() -> Result<Llm> {
        Ok(Llm {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .context("building the model HTTP client")?,
        })
    }

    /// Whether a tier is usable right now. A missing key is not an error — it means
    /// rung 3 is unavailable, the source degrades, and the run continues.
    pub fn available(&self, tier: &str) -> bool {
        provider(tier).is_ok()
    }

    pub fn describe(&self, tier: &str) -> String {
        match provider(tier) {
            Ok(p) => format!("{} ({})", p.model, tier),
            Err(_) => format!("{tier} (unconfigured)"),
        }
    }

    /// Extracts named fields from a document. Returns a JSON object containing
    /// exactly the requested keys, with `null` for anything the document does not
    /// state.
    pub async fn extract_fields(
        &self,
        tier: &str,
        instructions: Option<&str>,
        fields: &[String],
        content: &str,
        max_chars: usize,
        budget: &Budget,
    ) -> Result<(Value, Usage)> {
        let p = provider(tier)?;
        let doc = truncate_chars(content, max_chars);
        let prompt = build_prompt(instructions, fields, &doc);

        // Estimate before spending. Output is bounded by the field count, so a
        // generous flat allowance is accurate enough to protect the ceiling.
        let est_in = (prompt.len() / 4) as f64;
        let est_out = (fields.len() * 32).max(128) as f64;
        let estimate = (est_in * p.price_in + est_out * p.price_out) / 1_000_000.0;
        if !budget.can_afford(estimate) {
            bail!(
                "budget ceiling reached (spent ${:.4} of ${:.4}, this call needs about ${estimate:.4})",
                budget.spent(),
                budget.limit().unwrap_or(0.0)
            );
        }

        let (text, usage_in, usage_out) = match p.api {
            Api::Gemini => self.call_gemini(&p, &prompt).await?,
            Api::OpenAiCompatible => self.call_openai_compatible(&p, &prompt).await?,
        };

        let cost = (usage_in as f64 * p.price_in + usage_out as f64 * p.price_out) / 1_000_000.0;
        budget.record(cost);

        let parsed = parse_json_object(&text).with_context(|| {
            format!("model returned something that is not a JSON object: {text:.200}")
        })?;

        // Keep only what was asked for. A model that volunteers extra keys is
        // improvising, and improvisation is exactly what this rung must not do.
        let mut out = serde_json::Map::new();
        for f in fields {
            if let Some(v) = parsed.get(f)
                && !v.is_null()
            {
                out.insert(f.clone(), v.clone());
            }
        }

        Ok((
            Value::Object(out),
            Usage {
                in_tokens: usage_in,
                out_tokens: usage_out,
                cost_usd: cost,
                calls: 1,
            },
        ))
    }

    async fn call_gemini(&self, p: &Provider, prompt: &str) -> Result<(String, u64, u64)> {
        let url = format!("{}/models/{}:generateContent", p.endpoint, p.model);
        let body = serde_json::json!({
            "contents": [{ "parts": [{ "text": prompt }] }],
            "generationConfig": {
                "temperature": 0,
                "responseMimeType": "application/json"
            }
        });
        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", &p.key)
            .json(&body)
            .send()
            .await
            .context("calling the Gemini API")?;

        let status = resp.status();
        let text = resp.text().await.context("reading the Gemini response")?;
        if !status.is_success() {
            bail!(
                "Gemini returned HTTP {}: {}",
                status.as_u16(),
                truncate_chars(&text, 300)
            );
        }
        let v: Value = serde_json::from_str(&text).context("Gemini response was not JSON")?;
        let content = v["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Gemini response had no text part: {}",
                    truncate_chars(&text, 300)
                )
            })?
            .to_string();
        Ok((
            content,
            v["usageMetadata"]["promptTokenCount"].as_u64().unwrap_or(0),
            v["usageMetadata"]["candidatesTokenCount"]
                .as_u64()
                .unwrap_or(0),
        ))
    }

    async fn call_openai_compatible(
        &self,
        p: &Provider,
        prompt: &str,
    ) -> Result<(String, u64, u64)> {
        let body = serde_json::json!({
            "model": p.model,
            "temperature": 0,
            "response_format": { "type": "json_object" },
            "messages": [{ "role": "user", "content": prompt }]
        });
        let resp = self
            .http
            .post(format!("{}/chat/completions", p.endpoint))
            .bearer_auth(&p.key)
            .json(&body)
            .send()
            .await
            .context("calling the chat completions API")?;

        let status = resp.status();
        let text = resp.text().await.context("reading the model response")?;
        if !status.is_success() {
            bail!(
                "model returned HTTP {}: {}",
                status.as_u16(),
                truncate_chars(&text, 300)
            );
        }
        let v: Value = serde_json::from_str(&text).context("model response was not JSON")?;
        let content = v["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "model response had no content: {}",
                    truncate_chars(&text, 300)
                )
            })?
            .to_string();
        Ok((
            content,
            v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        ))
    }
}

/// Resolves a logical tier to a configured provider.
///
/// `flash` is the bulk extraction tier: cheapest per token, and extraction is an easy
/// task. `long` is for documents that do not fit comfortably — whitepapers, filings —
/// where long context at low cost matters more than raw capability.
fn provider(tier: &str) -> Result<Provider> {
    // A pack asks for a capability tier, not a vendor. When the preferred vendor has
    // no key configured, falling back beats degrading: the task is extraction, and
    // every one of these does it.
    let tier = match tier {
        "flash" if first_env(&["GEMINI_API_KEY", "GOOGLE_API_KEY"]).is_none() => {
            if first_env(&["OPENAI_API_KEY"]).is_some() {
                "mini"
            } else {
                "flash"
            }
        }
        t => t,
    };
    match tier {
        "flash" => {
            let key = first_env(&["GEMINI_API_KEY", "GOOGLE_API_KEY"])
                .ok_or_else(|| anyhow::anyhow!("GEMINI_API_KEY is not set"))?;
            Ok(Provider {
                api: Api::Gemini,
                endpoint: env_or(
                    "PILOTAGE_GEMINI_ENDPOINT",
                    "https://generativelanguage.googleapis.com/v1beta",
                ),
                model: env_or("PILOTAGE_GEMINI_MODEL", "gemini-flash-latest"),
                key,
                price_in: env_f64("PILOTAGE_PRICE_FLASH_IN", 0.10),
                price_out: env_f64("PILOTAGE_PRICE_FLASH_OUT", 0.40),
            })
        }
        "long" => {
            let key = first_env(&["DEEPSEEK_API_KEY"])
                .ok_or_else(|| anyhow::anyhow!("DEEPSEEK_API_KEY is not set"))?;
            Ok(Provider {
                api: Api::OpenAiCompatible,
                endpoint: env_or("PILOTAGE_DEEPSEEK_ENDPOINT", "https://api.deepseek.com"),
                model: env_or("PILOTAGE_DEEPSEEK_MODEL", "deepseek-chat"),
                key,
                price_in: env_f64("PILOTAGE_PRICE_LONG_IN", 0.28),
                price_out: env_f64("PILOTAGE_PRICE_LONG_OUT", 0.42),
            })
        }
        // Not a distinct capability tier so much as a distinct *key* tier: an OpenAI
        // key is the one most people already have, and extraction is easy enough that
        // any small model does it. Packs that ask for `flash` fall back to this when
        // no Gemini key is configured.
        "mini" => {
            let key = first_env(&["OPENAI_API_KEY"])
                .ok_or_else(|| anyhow::anyhow!("OPENAI_API_KEY is not set"))?;
            Ok(Provider {
                api: Api::OpenAiCompatible,
                endpoint: env_or("PILOTAGE_OPENAI_ENDPOINT", "https://api.openai.com/v1"),
                model: env_or("PILOTAGE_OPENAI_MODEL", "gpt-5-mini"),
                key,
                price_in: env_f64("PILOTAGE_PRICE_MINI_IN", 0.25),
                price_out: env_f64("PILOTAGE_PRICE_MINI_OUT", 2.00),
            })
        }
        other => bail!("unknown model tier '{other}' (expected 'flash', 'mini' or 'long')"),
    }
}

fn first_env(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// The extraction prompt. Every line of it exists to stop the model from being
/// helpful in ways that would put unsourced claims into a research report.
pub fn build_prompt(instructions: Option<&str>, fields: &[String], doc: &str) -> String {
    let mut p = String::new();
    p.push_str("Extract data from the DOCUMENT below.\n\n");
    if let Some(i) = instructions {
        p.push_str(i);
        p.push_str("\n\n");
    }
    p.push_str("Rules:\n");
    p.push_str("- Return ONLY a JSON object with exactly these keys: ");
    p.push_str(&fields.join(", "));
    p.push_str(".\n");
    p.push_str("- Every value must be stated explicitly in the DOCUMENT. Copy it, do not rephrase numbers.\n");
    p.push_str("- Use null for anything the DOCUMENT does not state. Do not infer, estimate, or use outside knowledge.\n");
    p.push_str("- Numeric fields must be plain JSON numbers with no currency symbols or units.\n");
    p.push_str("- Boolean fields must be true or false.\n\n");
    p.push_str("DOCUMENT:\n");
    p.push_str(doc);
    p
}

/// Models wrap JSON in code fences even when told not to. Unwrap, then parse.
pub fn parse_json_object(text: &str) -> Result<Value> {
    let t = text.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .map(|s| s.trim_start())
        .unwrap_or(t);
    let t = t.strip_suffix("```").map(str::trim_end).unwrap_or(t);

    if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(t) {
        return Ok(v);
    }
    // Last resort: the first balanced object in the text.
    let start = t
        .find('{')
        .context("no JSON object found in the model output")?;
    let end = t
        .rfind('}')
        .context("no JSON object found in the model output")?;
    if end <= start {
        bail!("no JSON object found in the model output");
    }
    let v: Value =
        serde_json::from_str(&t[start..=end]).context("model output did not parse as JSON")?;
    if !v.is_object() {
        bail!("model output was not a JSON object");
    }
    Ok(v)
}

/// Character-safe truncation. Slicing a UTF-8 string at an arbitrary byte offset
/// panics, and page content is full of multi-byte characters.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Strips markup down to readable text before it is sent to a model. Roughly a 5–10x
/// reduction on a real marketing page, which is a 5–10x reduction in what rung 3
/// costs.
pub fn html_to_text(html: &str) -> String {
    let doc = scraper::Html::parse_document(html);
    let mut out = String::new();
    if let Ok(sel) = scraper::Selector::parse(
        "body p, body h1, body h2, body h3, body h4, body li, body td, body th, body button, body a, body span",
    ) {
        for el in doc.select(&sel) {
            let text = el.text().collect::<String>();
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if text.is_empty() || text.len() > 400 {
                continue;
            }
            out.push_str(&text);
            out.push('\n');
        }
    }
    if out.trim().is_empty() {
        // Nothing matched — fall back to whole-document text rather than sending
        // an empty prompt and paying for a guaranteed null.
        let text = doc.root_element().text().collect::<String>();
        return text.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    dedupe_lines(&out)
}

/// Navigation and footers repeat the same strings many times over. Dropping repeats
/// costs nothing and removes a surprising share of a page's tokens.
fn dedupe_lines(s: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    s.lines()
        .filter(|l| seen.insert(l.to_string()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unlimited_budget_affords_anything() {
        let b = Budget::unlimited();
        assert!(b.can_afford(1_000.0));
        assert!(!b.was_blocked());
        assert_eq!(b.remaining(), None);
    }

    #[test]
    fn a_ceiling_blocks_before_spending_not_after() {
        let b = Budget::new(Some(0.05));
        assert!(b.can_afford(0.04));
        b.record(0.04);
        assert!(!b.can_afford(0.02));
        assert!(b.was_blocked(), "blocking must be visible to the caller");
        assert_eq!(b.spent(), 0.04);
        assert!((b.remaining().unwrap() - 0.01).abs() < 1e-9);
    }

    #[test]
    fn remaining_never_goes_negative() {
        let b = Budget::new(Some(0.01));
        b.record(0.05);
        assert_eq!(b.remaining(), Some(0.0));
    }

    #[test]
    fn unknown_tiers_are_rejected() {
        assert!(provider("gpt-9").is_err());
    }

    #[test]
    fn plain_json_parses() {
        let v = parse_json_object(r#"{"a": 1}"#).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn fenced_json_parses_because_models_keep_doing_that() {
        let v = parse_json_object("```json\n{\"a\": 1}\n```").unwrap();
        assert_eq!(v["a"], 1);
        let v = parse_json_object("```\n{\"a\": 2}\n```").unwrap();
        assert_eq!(v["a"], 2);
    }

    #[test]
    fn json_embedded_in_prose_is_recovered() {
        let v = parse_json_object("Sure! Here you go:\n{\"a\": 3}\nHope that helps.").unwrap();
        assert_eq!(v["a"], 3);
    }

    #[test]
    fn non_objects_are_rejected() {
        for s in ["", "not json", "[1,2,3]", "42"] {
            assert!(parse_json_object(s).is_err(), "{s} should be rejected");
        }
    }

    #[test]
    fn the_prompt_forbids_inference_and_names_every_field() {
        let p = build_prompt(Some("Pricing page."), &["a".into(), "b".into()], "DOC");
        assert!(p.contains("exactly these keys: a, b"));
        assert!(p.contains("Do not infer"));
        assert!(p.contains("Pricing page."));
        assert!(p.ends_with("DOC"));
    }

    #[test]
    fn truncation_is_character_safe() {
        // Byte slicing here would panic; character slicing must not.
        let s = "héllo wörld ünicode";
        assert_eq!(truncate_chars(s, 5).chars().count(), 5);
        assert_eq!(truncate_chars(s, 500), s);
    }

    #[test]
    fn html_is_reduced_to_readable_text() {
        let html = r#"<html><head><style>.a{color:red}</style><script>var x=1;</script></head>
            <body><h1>Pricing</h1><p>Simple plans</p><li>$49/mo</li></body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Pricing"));
        assert!(text.contains("$49/mo"));
        assert!(
            !text.contains("var x"),
            "script content must not be billed for"
        );
        assert!(!text.contains("color:red"), "css must not be billed for");
    }

    #[test]
    fn repeated_nav_text_is_billed_for_once() {
        let html = "<body><a>Docs</a><a>Docs</a><a>Docs</a><a>Pricing</a></body>";
        assert_eq!(html_to_text(html), "Docs\nPricing");
    }
}
