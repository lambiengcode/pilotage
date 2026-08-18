//! Concurrent, cache-backed, well-behaved I/O.
//!
//! This is where Rust earns its place: thirty sources are in flight at once inside a
//! single process, so a research run costs one wall-clock fetch rather than thirty.
//! Everything is funnelled through here so that caching, rate limiting, robots and
//! replay mode cannot be bypassed by a new source type.

use crate::cache::Cache;
use crate::pack::{Kind, Source};
use anyhow::{Context, Result, bail};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::Instant;

const DEFAULT_CONCURRENCY: usize = 16;
const DEFAULT_TIMEOUT_SECS: u64 = 20;
/// Minimum gap between two requests to the same host. Politeness, and it keeps
/// pilotage under the rate limits of the free APIs the packs rely on.
const DEFAULT_HOST_INTERVAL_MS: u64 = 200;
const MAX_ATTEMPTS: u32 = 3;
const USER_AGENT: &str = concat!("pilotage/", env!("CARGO_PKG_VERSION"), " (+https://axi.md)");

#[derive(Debug, Clone)]
pub struct Fetched {
    pub url: String,
    pub status: u16,
    pub body: Vec<u8>,
    pub content_hash: String,
    pub fetched_at: i64,
    pub from_cache: bool,
}

impl Fetched {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

pub struct Fetcher {
    client: reqwest::Client,
    cache: Arc<Cache>,
    sem: Arc<Semaphore>,
    last_hit: Mutex<HashMap<String, Instant>>,
    robots: Mutex<HashMap<String, Arc<Robots>>>,
    host_interval: Duration,
    replay: bool,
    respect_robots: bool,
}

impl Fetcher {
    pub fn new(cache: Arc<Cache>) -> Result<Fetcher> {
        let concurrency = env_usize("PILOTAGE_CONCURRENCY", DEFAULT_CONCURRENCY).max(1);
        let timeout = env_usize("PILOTAGE_TIMEOUT_SECS", DEFAULT_TIMEOUT_SECS as usize) as u64;
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(timeout))
            .build()
            .context("building the HTTP client")?;
        Ok(Fetcher {
            client,
            cache,
            sem: Arc::new(Semaphore::new(concurrency)),
            last_hit: Mutex::new(HashMap::new()),
            robots: Mutex::new(HashMap::new()),
            host_interval: Duration::from_millis(env_usize(
                "PILOTAGE_HOST_INTERVAL_MS",
                DEFAULT_HOST_INTERVAL_MS as usize,
            ) as u64),
            // Replay serves the cache and refuses to reach the network. Tests and
            // benchmarks run the entire pipeline under it, deterministically.
            replay: std::env::var("PILOTAGE_REPLAY").is_ok_and(|v| v == "1" || v == "true"),
            respect_robots: !std::env::var("PILOTAGE_IGNORE_ROBOTS")
                .is_ok_and(|v| v == "1" || v == "true"),
        })
    }

    pub fn is_replay(&self) -> bool {
        self.replay
    }

    /// Fetches a source, serving a fresh cache entry when one exists.
    ///
    /// The source's `kind` decides the transport. `Browser` shells out to
    /// `chrome-devtools-axi` rather than embedding a browser engine; `Probe` submits a
    /// job and times it. Each caches under a distinct key so a rendered snapshot or a
    /// probe result never collides with the raw HTTP body for the same URL.
    pub async fn get(
        &self,
        url: &str,
        ttl: i64,
        headers: &HashMap<String, String>,
        src: &Source,
    ) -> Result<Fetched> {
        let kind = src.kind;
        let key = cache_key(url, kind);
        let now = crate::util::now();

        if let Some(blob) = self.cache.get_blob(&key)?
            && (self.replay || blob.is_fresh(now, ttl))
        {
            return Ok(Fetched {
                url: url.to_string(),
                status: blob.status,
                body: blob.body,
                content_hash: blob.content_hash,
                fetched_at: blob.fetched_at,
                from_cache: true,
            });
        }

        if self.replay {
            bail!("replay miss: {key} is not in the cache (record it with a live run first)");
        }

        let (status, body) = match kind {
            Kind::Browser => self.fetch_via_browser(url).await?,
            // robots.txt governs crawling, so it is enforced for the kinds that
            // crawl: markup and rendered pages. A `kind = "api"` source is a
            // documented interface the pack author named and the user invoked
            // deliberately — treating that as crawling would block public APIs
            // (CoinGecko's disallows /api/) that are meant to be called by clients.
            Kind::Html => {
                if self.respect_robots && !self.robots_allows(url).await {
                    bail!("robots.txt disallows {url}");
                }
                self.request(url, headers, "GET", None, &src.content_type)
                    .await?
            }
            Kind::Api => {
                self.request(
                    url,
                    headers,
                    &src.method,
                    src.body.as_deref(),
                    &src.content_type,
                )
                .await?
            }
            Kind::Probe => self.run_probe(url, headers, src).await?,
        };

        let fetched_at = crate::util::now();
        let content_hash = self.cache.put_blob(&key, status, &body, fetched_at)?;
        Ok(Fetched {
            url: url.to_string(),
            status,
            body,
            content_hash,
            fetched_at,
            from_cache: false,
        })
    }

    async fn request(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
        method: &str,
        body: Option<&str>,
        content_type: &str,
    ) -> Result<(u16, Vec<u8>)> {
        let mut hmap = HeaderMap::new();
        for (k, v) in headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .with_context(|| format!("invalid header name '{k}'"))?;
            let value = HeaderValue::from_str(v)
                .with_context(|| format!("invalid value for header '{k}'"))?;
            hmap.insert(name, value);
        }

        // A request with a body is not safely retryable in general, but every body
        // pilotage sends is an idempotent job submission described by a pack, and the
        // alternative to retrying a 429 is losing the source entirely.
        let mut last_err = None;
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                // Exponential backoff. Deliberately blunt: the alternative is a
                // scheduler, and these are one-shot CLI runs.
                tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))).await;
            }
            let _permit = self.sem.acquire().await.expect("semaphore is never closed");
            self.throttle(url).await;

            let mut req = match method.to_ascii_uppercase().as_str() {
                "POST" => self.client.post(url),
                "PUT" => self.client.put(url),
                _ => self.client.get(url),
            }
            .headers(hmap.clone());
            if let Some(b) = body {
                req = req.header("Content-Type", content_type).body(b.to_string());
            }

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    // 429 and 5xx are worth another attempt; 4xx is a real answer and
                    // gets cached so a missing optional source is not re-requested.
                    if status == 429 || (500..600).contains(&status) {
                        last_err = Some(anyhow::anyhow!("HTTP {status}"));
                        continue;
                    }
                    let body = resp.bytes().await.context("reading response body")?;
                    return Ok((status, body.to_vec()));
                }
                Err(e) => last_err = Some(anyhow::Error::new(e)),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("request failed")))
            .with_context(|| format!("fetching {url} after {MAX_ATTEMPTS} attempts"))
    }

    /// Submits a job and times it to completion.
    ///
    /// A vendor's own latency is not published anywhere — it has to be provoked. This
    /// POSTs a real request, polls until the job reaches a terminal state, and returns
    /// a synthesised JSON document that the ordinary rung-1 `map` then reads. Reusing
    /// the field-map rung means a probe needs no new extraction machinery.
    ///
    /// A probe that times out or fails still returns HTTP 200 with `probe.ok = false`:
    /// "this vendor did not finish in five minutes" is a measurement, not an error, and
    /// it belongs in the comparison rather than in the degraded list.
    async fn run_probe(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
        src: &Source,
    ) -> Result<(u16, Vec<u8>)> {
        let started = Instant::now();

        let (submit_status, submit_bytes) = self
            .request(
                url,
                headers,
                &src.method,
                src.body.as_deref(),
                &src.content_type,
            )
            .await?;
        let submit_ms = started.elapsed().as_millis() as u64;

        // A rejected submission is a real failure of the source, not a slow vendor:
        // surface the status so the run degrades and says why.
        if !(200..300).contains(&submit_status) {
            return Ok((submit_status, submit_bytes));
        }

        let submit: serde_json::Value =
            serde_json::from_slice(&submit_bytes).unwrap_or(serde_json::Value::Null);

        let Some(poll) = &src.poll else {
            // Synchronous vendor: the submit round trip *is* the latency.
            return Ok((
                200,
                probe_doc(
                    true,
                    submit_ms,
                    submit_ms,
                    0,
                    "completed",
                    None,
                    &submit,
                    &submit,
                ),
            ));
        };

        let job_id = match &poll.id_path {
            Some(path) => match crate::extract::det::json_extract(&submit, path) {
                Ok(Some(v)) => match v {
                    serde_json::Value::String(s) => s,
                    other => other.to_string().trim_matches('"').to_string(),
                },
                _ => {
                    return Ok((
                        200,
                        probe_doc(
                            false,
                            submit_ms,
                            submit_ms,
                            0,
                            "no-job-id",
                            Some(&format!("submit response had nothing at {path}")),
                            &submit,
                            &submit,
                        ),
                    ));
                }
            },
            None => String::new(),
        };

        let interval = Duration::from_secs(
            crate::util::parse_duration(&poll.interval)
                .unwrap_or(5)
                .max(1) as u64,
        );
        let timeout = Duration::from_secs(
            crate::util::parse_duration(&poll.timeout)
                .unwrap_or(300)
                .max(1) as u64,
        );

        let mut vars = HashMap::new();
        vars.insert("id".to_string(), job_id);
        let poll_url = crate::util::template(&poll.url, &vars);

        let mut polls = 0u64;
        let mut last = submit.clone();
        loop {
            if started.elapsed() >= timeout {
                return Ok((
                    200,
                    probe_doc(
                        false,
                        submit_ms,
                        started.elapsed().as_millis() as u64,
                        polls,
                        "timeout",
                        Some(&format!("still unfinished after {}", poll.timeout)),
                        &submit,
                        &last,
                    ),
                ));
            }
            tokio::time::sleep(interval).await;
            polls += 1;

            let (status, bytes) = match self
                .request(&poll_url, headers, "GET", None, &src.content_type)
                .await
            {
                Ok(v) => v,
                // A single failed poll is transient; keep waiting until the timeout
                // rather than throwing away a job that is probably still running.
                Err(_) => continue,
            };
            if !(200..300).contains(&status) {
                continue;
            }
            last = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);

            let state = match crate::extract::det::json_extract(&last, &poll.status_path) {
                Ok(Some(serde_json::Value::String(s))) => s,
                Ok(Some(other)) => other.to_string().trim_matches('"').to_string(),
                _ => continue,
            };

            let matches_any = |list: &[String]| list.iter().any(|s| s.eq_ignore_ascii_case(&state));
            if matches_any(&poll.done) {
                return Ok((
                    200,
                    probe_doc(
                        true,
                        submit_ms,
                        started.elapsed().as_millis() as u64,
                        polls,
                        &state,
                        None,
                        &submit,
                        &last,
                    ),
                ));
            }
            if matches_any(&poll.failed) {
                return Ok((
                    200,
                    probe_doc(
                        false,
                        submit_ms,
                        started.elapsed().as_millis() as u64,
                        polls,
                        &state,
                        Some("vendor reported the job failed"),
                        &submit,
                        &last,
                    ),
                ));
            }
        }
    }

    /// Client-rendered pages return an empty shell over plain HTTP. `chrome-devtools-axi`
    /// already solves that and is already agent-ergonomic, so pilotage drives it
    /// instead of shipping a second browser stack.
    async fn fetch_via_browser(&self, url: &str) -> Result<(u16, Vec<u8>)> {
        let bin =
            std::env::var("PILOTAGE_BROWSER_BIN").unwrap_or_else(|_| "chrome-devtools-axi".into());
        let _permit = self.sem.acquire().await.expect("semaphore is never closed");

        let open = tokio::process::Command::new(&bin)
            .arg("open")
            .arg(url)
            .output()
            .await
            .with_context(|| {
                format!("running `{bin} open` — install it with `npm i -g chrome-devtools-axi`")
            })?;
        if !open.status.success() {
            bail!(
                "`{bin} open {url}` failed: {}",
                String::from_utf8_lossy(&open.stderr).trim()
            );
        }
        let snap = tokio::process::Command::new(&bin)
            .arg("snapshot")
            .output()
            .await
            .with_context(|| format!("running `{bin} snapshot`"))?;
        if !snap.status.success() {
            bail!(
                "`{bin} snapshot` failed: {}",
                String::from_utf8_lossy(&snap.stderr).trim()
            );
        }
        Ok((200, snap.stdout))
    }

    /// Enforces a minimum gap between requests to the same host.
    async fn throttle(&self, url: &str) {
        let Some(host) = host_of(url) else { return };
        let wait = {
            let mut map = self.last_hit.lock().await;
            let now = Instant::now();
            let wait = map
                .get(&host)
                .map(|prev| self.host_interval.saturating_sub(now.duration_since(*prev)))
                .unwrap_or_default();
            map.insert(host, now + wait);
            wait
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    async fn robots_allows(&self, url: &str) -> bool {
        let Some(host) = host_of(url) else {
            return true;
        };
        let scheme = if url.starts_with("http://") {
            "http"
        } else {
            "https"
        };

        let cached = self.robots.lock().await.get(&host).cloned();
        let rules = match cached {
            Some(r) => r,
            None => {
                let robots_url = format!("{scheme}://{host}/robots.txt");
                let text = match self.client.get(&robots_url).send().await {
                    Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
                    // No robots.txt, or it is unreachable: the standard reading is
                    // "everything is allowed".
                    _ => String::new(),
                };
                let parsed = Arc::new(Robots::parse(&text));
                self.robots.lock().await.insert(host, parsed.clone());
                parsed
            }
        };
        rules.allows(path_of(url))
    }
}

fn cache_key(url: &str, kind: Kind) -> String {
    match kind {
        Kind::Browser => format!("browser:{url}"),
        Kind::Probe => format!("probe:{url}"),
        _ => url.to_string(),
    }
}

/// The document a probe produces. Deliberately shaped so a pack reads it with the
/// ordinary rung-1 field map: `.probe.latency_s`, `.probe.ok`, and so on.
#[allow(clippy::too_many_arguments)]
fn probe_doc(
    ok: bool,
    submit_ms: u64,
    latency_ms: u64,
    polls: u64,
    status: &str,
    error: Option<&str>,
    submit: &serde_json::Value,
    final_response: &serde_json::Value,
) -> Vec<u8> {
    serde_json::json!({
        "probe": {
            "ok": ok,
            "submit_ms": submit_ms,
            "latency_ms": latency_ms,
            "latency_s": (latency_ms as f64) / 1000.0,
            "polls": polls,
            "status": status,
            "error": error,
        },
        "submit": submit,
        "final": final_response,
    })
    .to_string()
    .into_bytes()
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.split('@').next_back()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_lowercase())
    }
}

pub fn path_of(url: &str) -> &str {
    let rest = url.split("://").nth(1).unwrap_or(url);
    match rest.find('/') {
        Some(i) => &rest[i..],
        None => "/",
    }
}

/// A deliberately small robots.txt reader: the `User-agent` / `Disallow` / `Allow`
/// subset, longest-match-wins as the de-facto standard specifies.
///
/// Out of scope, because pilotage reads a handful of pack-declared pages rather than
/// crawling: crawl-delay, sitemaps, `*`/`$` path wildcards, and merging consecutive
/// `User-agent` lines into one group. Each of those omissions makes pilotage *more*
/// restrictive or equally so, never less.
#[derive(Debug, Default)]
pub struct Robots {
    rules: Vec<(bool, String)>,
}

impl Robots {
    pub fn parse(text: &str) -> Robots {
        let mut rules = Vec::new();
        let mut applies = false;

        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                // Each `User-agent` line opens a group; rules below it apply to us
                // only if that group names us or everyone.
                "user-agent" => {
                    applies = value == "*" || value.eq_ignore_ascii_case("pilotage");
                }
                "disallow" if applies => {
                    if !value.is_empty() {
                        rules.push((false, value.to_string()));
                    }
                }
                "allow" if applies && !value.is_empty() => {
                    rules.push((true, value.to_string()));
                }
                _ => {}
            }
        }
        Robots { rules }
    }

    pub fn allows(&self, path: &str) -> bool {
        let mut best: Option<(usize, bool)> = None;
        for (allow, prefix) in &self.rules {
            if path.starts_with(prefix.as_str()) {
                let len = prefix.len();
                if best.is_none_or(|(l, _)| len > l) {
                    best = Some((len, *allow));
                }
            }
        }
        best.map(|(_, allow)| allow).unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_extracted_from_every_url_shape() {
        assert_eq!(
            host_of("https://api.github.com/repos/a/b").unwrap(),
            "api.github.com"
        );
        assert_eq!(host_of("http://x.com").unwrap(), "x.com");
        assert_eq!(host_of("https://X.COM/a").unwrap(), "x.com");
        assert_eq!(host_of("https://user:pw@x.com/a").unwrap(), "x.com");
        assert_eq!(host_of("https://x.com?q=1").unwrap(), "x.com");
    }

    #[test]
    fn paths_default_to_root() {
        assert_eq!(path_of("https://x.com"), "/");
        assert_eq!(path_of("https://x.com/a/b?q=1"), "/a/b?q=1");
    }

    #[test]
    fn absent_robots_allows_everything() {
        assert!(Robots::parse("").allows("/anything"));
    }

    #[test]
    fn disallow_blocks_matching_prefixes_only() {
        let r = Robots::parse("User-agent: *\nDisallow: /private\n");
        assert!(!r.allows("/private/x"));
        assert!(r.allows("/public"));
    }

    #[test]
    fn longest_match_wins_so_allow_can_carve_out_an_exception() {
        let r = Robots::parse("User-agent: *\nDisallow: /a\nAllow: /a/public\n");
        assert!(!r.allows("/a/secret"));
        assert!(r.allows("/a/public/x"));
    }

    #[test]
    fn rules_for_other_agents_are_ignored() {
        let r = Robots::parse("User-agent: BadBot\nDisallow: /\n");
        assert!(r.allows("/anything"));
    }

    #[test]
    fn comments_and_blank_lines_do_not_confuse_the_parser() {
        let r = Robots::parse("# hello\n\nUser-agent: *  # us\nDisallow: /x # nope\n");
        assert!(!r.allows("/x"));
        assert!(r.allows("/y"));
    }

    #[test]
    fn empty_disallow_means_allow_all() {
        let r = Robots::parse("User-agent: *\nDisallow:\n");
        assert!(r.allows("/anything"));
    }

    #[test]
    fn browser_results_never_collide_with_http_results() {
        assert_ne!(
            cache_key("https://x.com", Kind::Html),
            cache_key("https://x.com", Kind::Browser)
        );
        assert_eq!(cache_key("https://x.com", Kind::Api), "https://x.com");
    }
}
