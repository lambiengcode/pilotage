//! A small local HTTP server for tests and benchmarks.
//!
//! Both the integration suite and the benchmark need to measure pilotage against
//! something that answers like an API but is not the internet. Configurable latency
//! matters for the benchmark: on loopback every request is sub-millisecond, and a
//! fan-out measurement taken there would flatter the concurrency it is meant to
//! demonstrate.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type Routes = HashMap<String, (u16, String)>;
/// Paths whose response changes on each hit, the last entry repeating. Polling cannot
/// be tested without this: a job that is "processing" forever never completes, and a
/// job that is "completed" on the first poll never exercises the loop.
pub type Sequences = HashMap<String, Vec<(u16, String)>>;

#[derive(Default)]
struct State {
    routes: Routes,
    sequences: Sequences,
    counters: HashMap<String, usize>,
}

pub struct MockServer {
    port: u16,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
}

impl MockServer {
    pub fn start(routes: Routes) -> MockServer {
        MockServer::with_latency(routes, Duration::ZERO)
    }

    /// Registers a path that walks through `responses`, one per request.
    pub fn sequence(&self, path: &str, responses: Vec<(u16, String)>) {
        let mut st = self.state.lock().unwrap();
        st.sequences.insert(path.to_string(), responses);
        st.counters.remove(path);
    }

    /// `latency` is applied before each response, standing in for the round trip to a
    /// real API.
    pub fn with_latency(routes: Routes, latency: Duration) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let port = listener.local_addr().unwrap().port();
        listener
            .set_nonblocking(true)
            .expect("non-blocking listener so shutdown does not hang on accept");

        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(State {
            routes,
            ..Default::default()
        }));

        {
            let (hits, stop, state) = (hits.clone(), stop.clone(), state.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let (hits, state) = (hits.clone(), state.clone());
                            std::thread::spawn(move || serve(stream, &hits, &state, latency));
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => break,
                    }
                }
            });
        }
        MockServer {
            port,
            hits,
            stop,
            state,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Total requests served, so a test can assert a warm run reached no socket.
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn reset_hits(&self) {
        self.hits.store(0, Ordering::Relaxed);
    }

    /// Shuts down and waits for in-flight handlers, so a later run provably reaches
    /// no network.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(30));
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn serve(mut stream: TcpStream, hits: &AtomicUsize, state: &Mutex<State>, latency: Duration) {
    // On macOS a socket accepted from a non-blocking listener inherits O_NONBLOCK,
    // so this read would intermittently return WouldBlock and the handler would drop
    // the connection before answering. Each handler has its own thread; it can block.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

    let mut buf = [0u8; 8192];
    let Ok(n) = stream.read(&mut buf) else { return };
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();

    hits.fetch_add(1, Ordering::Relaxed);
    if !latency.is_zero() {
        std::thread::sleep(latency);
    }

    let (status, body) = {
        let mut st = state.lock().unwrap();
        match st.sequences.get(&path).cloned() {
            Some(seq) if !seq.is_empty() => {
                let n = st.counters.entry(path.clone()).or_insert(0);
                // Clamp rather than wrap: the last response is the terminal state, and
                // a poll loop that saw "completed" must keep seeing it.
                let response = seq[(*n).min(seq.len() - 1)].clone();
                *n += 1;
                response
            }
            _ => st
                .routes
                .get(&path)
                .cloned()
                .unwrap_or((404, r#"{"error":"not found"}"#.to_string())),
        }
    };

    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_configured_routes_and_404s_everything_else() {
        let s = MockServer::start(HashMap::from([(
            "/a".to_string(),
            (200, r#"{"ok":true}"#.to_string()),
        )]));
        let body = ureq_get(&s.url("/a"));
        assert!(body.contains("\"ok\":true"), "{body}");
        assert!(ureq_get(&s.url("/missing")).contains("not found"));
        assert_eq!(s.hits(), 2);
    }

    #[test]
    fn stops_on_request() {
        let s = MockServer::start(Routes::new());
        s.stop();
        assert!(
            std::net::TcpStream::connect(("127.0.0.1", s.port()))
                .and_then(|mut c| {
                    use std::io::Write;
                    c.write_all(b"GET / HTTP/1.1\r\n\r\n")?;
                    let mut buf = String::new();
                    std::io::Read::read_to_string(&mut c, &mut buf)?;
                    Ok(buf)
                })
                .map(|b| b.is_empty())
                .unwrap_or(true),
            "a stopped server must not answer"
        );
    }

    /// A one-request HTTP client, so the module's own tests need no async runtime.
    fn ureq_get(url: &str) -> String {
        let rest = url.trim_start_matches("http://");
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        let mut c = TcpStream::connect(host).unwrap();
        write!(
            c,
            "GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).unwrap();
        out
    }
}
