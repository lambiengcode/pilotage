//! End-to-end tests that drive the real binary against a real socket.
//!
//! Each test gets a private `PILOTAGE_HOME`, so they run in parallel without
//! sharing a cache, and a private mock server, so nothing here touches the network.
//! Driving the compiled binary rather than calling the library means exit codes,
//! argument parsing and TOON output are all covered by the same assertions.

use pilotage::testserver::MockServer as Mock;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_pilotage");

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Env {
    home: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let home = std::env::temp_dir().join(format!(
            "pilotage-it-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(home.join("packs")).unwrap();
        Env { home }
    }

    fn write_pack(&self, name: &str, toml: &str) {
        std::fs::write(self.home.join("packs").join(format!("{name}.toml")), toml).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[])
    }

    fn run_with(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .env("PILOTAGE_HOME", &self.home)
            // The per-host courtesy delay is real behaviour, but it would add
            // seconds to a suite that talks only to itself.
            .env("PILOTAGE_HOST_INTERVAL_MS", "0");
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        cmd.output().expect("running the pilotage binary")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.home).ok();
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

/// Reads a `key[n]{cols}:` table out of TOON output as rows of cells.
fn toon_table(out: &str, key: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut inside = false;
    for line in out.lines() {
        if inside {
            if line.starts_with("  ") && !line.trim_start().is_empty() {
                rows.push(line.trim().split(',').map(str::to_string).collect());
                continue;
            }
            break;
        }
        if line.starts_with(&format!("{key}[")) && line.trim_end().ends_with(':') {
            inside = true;
        }
    }
    rows
}

fn toon_value(out: &str, key: &str) -> Option<String> {
    out.lines()
        .find(|l| l.starts_with(&format!("{key}: ")))
        .map(|l| l[key.len() + 2..].trim().to_string())
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn repo_routes() -> HashMap<String, (u16, String)> {
    HashMap::from([
        (
            "/repo".to_string(),
            (
                200,
                serde_json::json!({
                    "stargazers_count": 4200,
                    "archived": false,
                    "license": {"spdx_id": "MIT"},
                    "pushed_at": "2026-08-10T00:00:00Z"
                })
                .to_string(),
            ),
        ),
        (
            "/commits".to_string(),
            (
                200,
                serde_json::json!([
                    {"commit": {"author": {"email": "a@x.com"}}},
                    {"commit": {"author": {"email": "b@x.com"}}},
                    {"commit": {"author": {"email": "c@x.com"}}},
                    {"commit": {"author": {"email": "a@x.com"}}}
                ])
                .to_string(),
            ),
        ),
    ])
}

/// A pack pointed at the mock. Written per-test so the port is baked in.
fn test_pack(mock: &Mock) -> String {
    format!(
        r#"
name = "t"
description = "integration fixture"
detect = ["test:{{name}}"]

[[source]]
id = "repo"
kind = "api"
url = "{repo}"
ttl = "1h"
weight = 3.0
[source.map]
stars = ".stargazers_count"
archived = ".archived"
license = ".license.spdx_id"
pushed_at = ".pushed_at"

[[source]]
id = "commits"
kind = "api"
url = "{commits}"
ttl = "1h"
weight = 2.0
[source.map]
commits_90d = "."
authors = ".[].commit.author.email"

[[source]]
id = "optional_missing"
kind = "api"
url = "{missing}"
ttl = "1h"
weight = 0.5
optional = true
[source.map]
never = ".never"

[[metric]]
id = "archived"
fact = "archived"
fail = "== 1"
critical = true
weight = 3.0

[[metric]]
id = "stars"
fact = "stars"
warn = "< 100"
weight = 1.0

[[metric]]
id = "license"
fact = "license"
fail = "is none"
weight = 2.0

[[metric]]
id = "commits_90d"
fact = "commits_90d"
agg = "count"
warn = "< 10"
fail = "< 3"
weight = 3.0

[[metric]]
id = "bus_factor"
fact = "authors"
agg = "unique"
warn = "< 3"
fail = "< 2"
weight = 2.0

[[metric]]
id = "never"
fact = "never"
warn = "is none"
weight = 0.5
"#,
        repo = mock.url("/repo"),
        commits = mock.url("/commits"),
        missing = mock.url("/gone"),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn a_full_run_produces_a_verdict_with_scored_signals() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("full");
    env.write_pack("t", &test_pack(&mock));

    let out = env.run(&["research", "test:demo", "--full"]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let s = stdout(&out);

    assert_eq!(toon_value(&s, "verdict").as_deref(), Some("healthy"));

    let signals: HashMap<String, Vec<String>> = toon_table(&s, "signals")
        .into_iter()
        .map(|r| (r[0].clone(), r))
        .collect();

    // Every rung-1 path resolved, and each aggregation did its job.
    assert_eq!(signals["stars"][1], "4200");
    assert_eq!(signals["license"][1], "MIT");
    assert_eq!(signals["commits_90d"][1], "4", "count over the array");
    assert_eq!(
        signals["bus_factor"][1], "3",
        "unique authors, not total commits"
    );
    assert_eq!(signals["archived"][2], "ok");
    // 4 commits trips `warn = "< 10"` but not `fail = "< 3"`.
    assert_eq!(signals["commits_90d"][2], "warn");
}

#[test]
fn an_optional_source_that_404s_degrades_without_failing_the_run() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("degrade");
    env.write_pack("t", &test_pack(&mock));

    let out = env.run(&["research", "test:demo"]);
    assert_eq!(
        code(&out),
        0,
        "a missing optional source must not fail the run"
    );
    let s = stdout(&out);
    assert!(
        s.contains("optional_missing(404)"),
        "degradation must be named:\n{s}"
    );
    // Optional, so it is not flagged as a required-source failure.
    assert!(!s.contains("optional_missing(404) required"), "{s}");
    assert!(s.contains("2/3 sources"), "{s}");
}

#[test]
fn a_metric_whose_source_never_produced_a_fact_is_unknown_not_a_warning() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("unknown");
    env.write_pack("t", &test_pack(&mock));

    let s = stdout(&env.run(&["research", "test:demo", "--full"]));
    let signals: HashMap<String, Vec<String>> = toon_table(&s, "signals")
        .into_iter()
        .map(|r| (r[0].clone(), r))
        .collect();
    // `never` comes from a source that 404'd, so nothing was ever looked at.
    // Firing its `is none` threshold here would be a warning about data no one read.
    assert_eq!(signals["never"][2], "unknown");
}

#[test]
fn a_run_where_every_source_fails_exits_no_data() {
    let mock = Mock::start(HashMap::new()); // every path 404s
    let env = Env::new("nodata");
    env.write_pack("t", &test_pack(&mock));

    let out = env.run(&["research", "test:demo"]);
    assert_eq!(code(&out), 3, "expected NO_DATA\nstdout: {}", stdout(&out));
}

#[test]
fn the_second_run_is_served_from_cache_and_touches_no_socket() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("cache");
    env.write_pack("t", &test_pack(&mock));

    env.run(&["research", "test:demo"]);
    let after_first = mock.hits();
    assert!(after_first >= 3, "the first run must actually fetch");

    let s = stdout(&env.run(&["research", "test:demo"]));
    assert_eq!(
        mock.hits(),
        after_first,
        "a warm run must issue no new requests"
    );
    assert!(
        s.contains("cached"),
        "the run must say it used the cache:\n{s}"
    );
}

#[test]
fn no_cache_forces_a_refetch() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("nocache");
    env.write_pack("t", &test_pack(&mock));

    env.run(&["research", "test:demo"]);
    let after_first = mock.hits();
    env.run(&["research", "test:demo", "--no-cache"]);
    assert!(
        mock.hits() > after_first,
        "--no-cache must bypass the cache"
    );
}

#[test]
fn replay_mode_reproduces_a_run_with_the_server_switched_off() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("replay");
    env.write_pack("t", &test_pack(&mock));

    let live = stdout(&env.run(&["research", "test:demo", "--full"]));
    mock.stop();

    let replayed = env.run_with(
        &["research", "test:demo", "--full"],
        &[("PILOTAGE_REPLAY", "1")],
    );
    assert_eq!(code(&replayed), 0, "stderr: {}", stderr(&replayed));
    let r = stdout(&replayed);

    assert!(
        r.contains("mode: replay"),
        "replay must announce itself:\n{r}"
    );
    assert_eq!(
        toon_table(&r, "signals"),
        toon_table(&live, "signals"),
        "replay must reproduce the live signals exactly"
    );
}

#[test]
fn replay_without_a_recording_fails_loudly_rather_than_returning_nothing() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("replaymiss");
    env.write_pack("t", &test_pack(&mock));

    let out = env.run_with(&["research", "test:demo"], &[("PILOTAGE_REPLAY", "1")]);
    assert_eq!(
        code(&out),
        3,
        "an unrecorded replay is NO_DATA, not a silent empty run"
    );
    assert!(stdout(&out).contains("replay miss") || stderr(&out).contains("replay miss"));
}

#[test]
fn sources_that_disagree_are_reported_rather_than_reconciled() {
    let mut routes = repo_routes();
    routes.insert(
        "/second".to_string(),
        (200, serde_json::json!({"stargazers_count": 11}).to_string()),
    );
    let mock = Mock::start(routes);
    let env = Env::new("contradiction");

    let pack = format!(
        "{}\n[[source]]\nid = \"second\"\nkind = \"api\"\nurl = \"{}\"\nttl = \"1h\"\nweight = 1.0\n[source.map]\nstars = \".stargazers_count\"\n",
        test_pack(&mock),
        mock.url("/second")
    );
    env.write_pack("t", &pack);

    let s = stdout(&env.run(&["research", "test:demo"]));
    let rows = toon_table(&s, "contradictions");
    assert_eq!(rows.len(), 1, "expected one contradiction in:\n{s}");
    assert_eq!(rows[0][0], "stars");
    assert!(
        rows[0][1].contains("4200") || rows[0][2].contains("4200"),
        "{:?}",
        rows[0]
    );
    assert!(
        rows[0][1].contains("11") || rows[0][2].contains("11"),
        "{:?}",
        rows[0]
    );
}

#[test]
fn the_default_view_hides_healthy_signals_and_full_shows_them() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("views");
    env.write_pack("t", &test_pack(&mock));

    let brief = stdout(&env.run(&["research", "test:demo"]));
    let full = stdout(&env.run(&["research", "test:demo", "--full"]));

    assert!(brief.contains("issues["), "{brief}");
    assert!(brief.contains("ok["), "{brief}");
    assert!(full.contains("signals["), "{full}");
    assert!(
        full.len() > brief.len(),
        "the compact view must actually be smaller: {} vs {}",
        brief.len(),
        full.len()
    );
}

#[test]
fn a_stored_run_can_be_replayed_reported_and_inspected_without_refetching() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("stored");
    env.write_pack("t", &test_pack(&mock));

    let s = stdout(&env.run(&["research", "test:demo"]));
    let run_id = s
        .lines()
        .next()
        .unwrap()
        .split(['[', ']'])
        .nth(1)
        .unwrap()
        .to_string();
    mock.stop();

    let report = env.run(&["report", &run_id]);
    assert_eq!(code(&report), 0, "{}", stderr(&report));
    assert!(stdout(&report).contains("verdict:"));

    let sources = env.run(&["source", "list", "--run", &run_id]);
    assert_eq!(code(&sources), 0);
    let rows = toon_table(&stdout(&sources), "sources");
    assert_eq!(
        rows.len(),
        3,
        "every attempted source is listed, including the failure"
    );

    let body = env.run(&["source", "get", "repo", "--run", &run_id, "--full"]);
    assert_eq!(code(&body), 0, "{}", stderr(&body));
    assert!(
        stdout(&body).contains("stargazers_count"),
        "the raw body must be retrievable"
    );

    let html_path = env.home.join("r.html");
    let html = env.run(&[
        "report",
        &run_id,
        "--html",
        "--out",
        html_path.to_str().unwrap(),
    ]);
    assert_eq!(code(&html), 0, "{}", stderr(&html));
    let doc = std::fs::read_to_string(&html_path).unwrap();
    assert!(doc.starts_with("<!doctype html>") && doc.contains("</html>"));
    assert!(
        !doc.contains("<script"),
        "the report must not carry scripts"
    );
}

#[test]
fn json_output_round_trips_through_a_parser() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("json");
    env.write_pack("t", &test_pack(&mock));

    let out = env.run(&["research", "test:demo", "--json"]);
    assert_eq!(code(&out), 0);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(v["pack"], "t");
    assert_eq!(v["scored"]["verdict"], "healthy");
    assert!(v["facts"].as_array().unwrap().len() >= 5);
}

#[test]
fn compare_pivots_targets_into_one_table() {
    let mut routes = repo_routes();
    routes.insert(
        "/repo2".to_string(),
        (
            200,
            serde_json::json!({"stargazers_count": 7, "archived": true}).to_string(),
        ),
    );
    let mock = Mock::start(routes);
    let env = Env::new("compare");

    // Two detect patterns so one pack serves both targets from different URLs.
    let pack = test_pack(&mock).replace(
        r#"detect = ["test:{name}"]"#,
        r#"detect = ["test:{name}"]
vars = {}"#,
    );
    env.write_pack("t", &pack);

    let out = env.run(&["compare", "test:a", "test:b"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let s = stdout(&out);
    assert_eq!(toon_table(&s, "compare").len(), 2);
    let metrics = toon_table(&s, "metrics");
    assert!(!metrics.is_empty(), "a pivot table must be produced:\n{s}");
    // One row per metric, one column per target plus the metric name.
    assert!(metrics.iter().all(|r| r.len() == 3), "{metrics:?}");
}

#[test]
fn watches_are_idempotent_and_report_what_moved() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("watch");
    env.write_pack("t", &test_pack(&mock));

    assert!(stdout(&env.run(&["watch", "add", "test:demo"])).contains("status: added"));
    assert!(
        stdout(&env.run(&["watch", "add", "test:demo"])).contains("already watched"),
        "adding twice must not create a second watch"
    );
    assert_eq!(
        toon_table(&stdout(&env.run(&["watch", "list"])), "watches").len(),
        1
    );

    // First check has no history to compare against.
    let first = stdout(&env.run(&["watch", "check"]));
    assert_eq!(toon_table(&first, "checked")[0][3], "new");
    assert!(first.contains("changed[0]: none"));

    let second = stdout(&env.run(&["watch", "check"]));
    assert_eq!(
        toon_table(&second, "checked")[0][3],
        "0",
        "nothing changed:\n{second}"
    );

    assert!(stdout(&env.run(&["watch", "rm", "test:demo"])).contains("removed: 1"));
    assert!(stdout(&env.run(&["watch", "list"])).contains("watches[0]: none"));
}

#[test]
fn a_watch_reports_a_verdict_change_between_runs() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("watchdiff");
    env.write_pack("t", &test_pack(&mock));

    env.run(&["watch", "add", "test:demo"]);
    env.run(&["watch", "check"]);

    // Rewrite the pack so the same target now scores differently, then rerun with a
    // cold cache. Changing the thresholds stands in for the world changing.
    // Replacing the fail threshold (not adding a second one) keeps the TOML valid.
    env.write_pack(
        "t",
        &test_pack(&mock).replace(r#"fail = "< 3""#, r#"fail = "< 500""#),
    );
    let out = stdout(&env.run(&["watch", "check"]));

    let changed = out.lines().find(|l| l.starts_with("changed[")).unwrap();
    assert!(
        !changed.contains("none"),
        "a verdict move must be reported:\n{out}"
    );
    assert!(out.contains("verdict"), "{out}");
}

#[test]
fn bare_invocation_shows_live_data_rather_than_help() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("dashboard");
    env.write_pack("t", &test_pack(&mock));
    env.run(&["research", "test:demo"]);

    let out = env.run(&[]);
    assert_eq!(code(&out), 0);
    let s = stdout(&out);
    assert!(s.starts_with("pilotage: "), "{s}");
    assert!(s.contains("test:demo"), "recent runs must be listed:\n{s}");
    assert!(
        !s.contains("Usage:"),
        "a bare invocation must not print help:\n{s}"
    );
    assert!(
        s.contains("next: "),
        "contextual disclosure is mandatory:\n{s}"
    );
}

#[test]
fn empty_states_are_explicit_rather_than_blank() {
    let env = Env::new("empty");
    let s = stdout(&env.run(&[]));
    assert!(s.contains("recent[0]: none"), "{s}");
    assert!(s.contains("watch[0]: none"), "{s}");
    assert!(stdout(&env.run(&["report"])).contains("runs[0]: none"));
    assert!(stdout(&env.run(&["watch", "check"])).contains("watches[0]: none"));
}

#[test]
fn the_ambient_hook_is_a_single_compact_block() {
    let env = Env::new("ambient");
    let out = env.run(&["ambient"]);
    assert_eq!(code(&out), 0);
    let s = stdout(&out);
    assert!(s.contains("bin: "));
    assert!(s.contains("description: "));
    assert!(s.contains("help["));
    assert!(
        s.lines().count() < 15,
        "ambient output must stay small:\n{s}"
    );
}

#[test]
fn every_command_ends_with_a_next_suggestion() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("disclosure");
    env.write_pack("t", &test_pack(&mock));
    env.run(&["research", "test:demo"]);

    for args in [
        vec![],
        vec!["research", "test:demo"],
        vec!["report"],
        vec!["source", "list"],
        vec!["watch", "list"],
        vec!["pack", "list"],
        vec!["cache", "stats"],
    ] {
        let s = stdout(&env.run(&args));
        assert!(s.contains("next: "), "`{args:?}` gave no next step:\n{s}");
    }
}

#[test]
fn a_user_pack_overrides_a_builtin_of_the_same_name() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("override");
    env.write_pack(
        "oss",
        &test_pack(&mock).replace(r#"name = "t""#, r#"name = "oss""#),
    );

    let s = stdout(&env.run(&["pack", "show", "oss"]));
    assert!(
        s.contains("integration fixture"),
        "the user pack must win:\n{s}"
    );
    assert_eq!(toon_table(&s, "sources").len(), 3);
}

#[test]
fn shipped_packs_validate_and_a_broken_pack_does_not() {
    let env = Env::new("validate");
    let out = env.run(&["pack", "validate"]);
    assert_eq!(code(&out), 0, "{}", stdout(&out));
    assert!(stdout(&out).contains("issues[0]: none"));

    env.write_pack(
        "broken",
        "name='broken'\ndetect=['{a}/{b}']\n[[source]]\nid='s'\nurl='https://x'\n[source.map]\nk='.k'\n[[metric]]\nid='m'\nfact='ghost'\nwarn='< 1'\n",
    );
    let out = env.run(&["pack", "validate"]);
    assert_eq!(code(&out), 1, "a broken pack must exit USAGE");
    assert!(
        stdout(&out).contains("no source produces"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn misuse_is_rejected_with_a_usage_exit_code_and_a_useful_message() {
    let env = Env::new("misuse");

    let out = env.run(&["research", "not a real target"]);
    assert_eq!(code(&out), 1);
    assert!(stderr(&out).contains("--pack"), "{}", stderr(&out));

    let out = env.run(&["research", "test:x", "--pack", "nope"]);
    assert_eq!(code(&out), 1);
    assert!(
        stderr(&out).contains("oss"),
        "the real pack names must be listed"
    );

    let out = env.run(&["research", "github.com/a/b", "--fields", "metric,valu"]);
    assert_eq!(code(&out), 1);
    assert!(stderr(&out).contains("unknown field"), "{}", stderr(&out));

    let out = env.run(&["research", "github.com/a/b", "--depth", "sideways"]);
    assert_eq!(code(&out), 1);
    assert!(stderr(&out).contains("quick"), "{}", stderr(&out));
}

#[test]
fn custom_fields_change_the_columns_returned() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("fields");
    env.write_pack("t", &test_pack(&mock));

    let s = stdout(&env.run(&[
        "research",
        "test:demo",
        "--fields",
        "metric,status,conf,source",
    ]));
    assert!(
        s.contains("issues[") && s.contains("{metric,status,conf,source}:"),
        "{s}"
    );
}

#[test]
fn cache_prune_drops_blobs_but_keeps_extracted_facts() {
    let mock = Mock::start(repo_routes());
    let env = Env::new("prune");
    env.write_pack("t", &test_pack(&mock));
    env.run(&["research", "test:demo"]);

    let before = stdout(&env.run(&["cache", "stats"]));
    assert!(before.contains("extractions: 2"), "{before}");

    let pruned = stdout(&env.run(&["cache", "prune", "--blobs", "0s", "--runs", "0s"]));
    assert!(pruned.contains("pruned_blobs: 3"), "{pruned}");

    let after = stdout(&env.run(&["cache", "stats"]));
    assert!(after.contains("blobs: 0"), "{after}");
    assert!(
        after.contains("extractions: 2"),
        "extraction results are the expensive thing — pruning must keep them:\n{after}"
    );
}

#[test]
fn the_hook_installer_is_idempotent_and_never_clobbers_existing_settings() {
    let env = Env::new("hooks");
    let settings = env.home.join("settings.json");
    std::fs::write(&settings, r#"{"theme":"dark"}"#).unwrap();
    let sp = settings.to_str().unwrap();

    // Printing must not write.
    let out = env.run_with(&["setup", "hooks"], &[("PILOTAGE_SETTINGS_PATH", sp)]);
    assert_eq!(code(&out), 0);
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        r#"{"theme":"dark"}"#
    );

    for _ in 0..2 {
        let out = env.run_with(
            &["setup", "hooks", "--install"],
            &[("PILOTAGE_SETTINGS_PATH", sp)],
        );
        assert_eq!(code(&out), 0, "{}", stderr(&out));
    }
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(v["theme"], "dark", "unrelated settings must survive");
    assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Probes — the only source kind that measures rather than reads
// ---------------------------------------------------------------------------

/// A probe pack pointed at the mock: POST to submit, poll until a terminal state.
fn probe_pack(mock: &Mock, interval: &str, timeout: &str) -> String {
    format!(
        r#"
name = "pr"
detect = ["probe:{{name}}"]

[[source]]
id = "vendor"
kind = "probe"
url = "{submit}"
method = "POST"
ttl = "1d"
depth = "deep"
weight = 1.0
auth_env = "FAKE_VENDOR_KEY"
auth_header = "X-Api-Key"
auth_format = "{{}}"
auth_required = true
body = '{{"script":"probe"}}'
[source.poll]
url = "{status}"
id_path = ".data.job_id"
status_path = ".data.status"
done = ["completed"]
failed = ["failed"]
interval = "{interval}"
timeout = "{timeout}"
[source.map]
latency_s = ".probe.latency_s"
ok = ".probe.ok"
status = ".probe.status"
polls = ".probe.polls"

[[metric]]
id = "latency"
fact = "latency_s"
warn = "> 60"
fail = "> 180"
weight = 3.0
unit = "s"

[[metric]]
id = "completed"
fact = "ok"
fail = "== 0"
weight = 2.0
"#,
        submit = mock.url("/submit"),
        status = mock.url("/status"),
    )
}

fn probe_routes() -> HashMap<String, (u16, String)> {
    HashMap::from([(
        "/submit".to_string(),
        (
            200,
            serde_json::json!({"data": {"job_id": "job-1"}}).to_string(),
        ),
    )])
}

fn status(state: &str) -> (u16, String) {
    (
        200,
        serde_json::json!({"data": {"status": state}}).to_string(),
    )
}

#[test]
fn a_probe_submits_polls_to_completion_and_reports_measured_latency() {
    let mock = Mock::start(probe_routes());
    // Two "processing" polls before completion, so the loop is genuinely exercised.
    mock.sequence(
        "/status",
        vec![
            status("processing"),
            status("processing"),
            status("completed"),
        ],
    );
    let env = Env::new("probe");
    env.write_pack("pr", &probe_pack(&mock, "1s", "30s"));

    let out = env.run_with(
        &["research", "probe:x", "--depth", "deep", "--full"],
        &[("FAKE_VENDOR_KEY", "secret")],
    );
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let s = stdout(&out);

    let signals: HashMap<String, Vec<String>> = toon_table(&s, "signals")
        .into_iter()
        .map(|r| (r[0].clone(), r))
        .collect();

    assert_eq!(
        signals["completed"][2], "ok",
        "the probe must report success:\n{s}"
    );

    // Three polls at a 1s interval: the measurement must reflect real elapsed time,
    // not a constant.
    let latency: f64 = signals["latency"][1].trim_end_matches('s').parse().unwrap();
    assert!(
        (2.5..8.0).contains(&latency),
        "expected roughly 3s of measured latency, got {latency}"
    );
    assert_eq!(signals["latency"][2], "ok");

    let polls = env.run(&["report", "--json"]);
    let run: serde_json::Value = serde_json::from_slice(&polls.stdout).unwrap();
    let facts: HashMap<&str, &serde_json::Value> = run["facts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["key"].as_str().unwrap(), &f["value"]))
        .collect();
    assert_eq!(facts["polls"], 3);
    assert_eq!(facts["status"], "completed");
}

#[test]
fn a_probe_without_its_credential_is_skipped_rather_than_run() {
    // The whole point of gating: a run with no key must not reach the vendor at all.
    let mock = Mock::start(probe_routes());
    mock.sequence("/status", vec![status("completed")]);
    let env = Env::new("probegate");
    env.write_pack("pr", &probe_pack(&mock, "1s", "30s"));

    let out = env.run(&["research", "probe:x", "--depth", "deep"]);
    assert_eq!(mock.hits(), 0, "an ungated probe must never be submitted");
    assert_eq!(code(&out), 3, "no sources ran, so NO_DATA");
    assert!(
        stdout(&out).contains("FAKE_VENDOR_KEY not set"),
        "the missing key must be named:\n{}",
        stdout(&out)
    );
}

#[test]
fn a_vendor_that_never_finishes_is_a_measurement_not_an_error() {
    let mock = Mock::start(probe_routes());
    mock.sequence("/status", vec![status("processing")]);
    let env = Env::new("probetimeout");
    env.write_pack("pr", &probe_pack(&mock, "1s", "3s"));

    let out = env.run_with(
        &["research", "probe:x", "--depth", "deep", "--full"],
        &[("FAKE_VENDOR_KEY", "secret")],
    );
    // "This vendor did not finish in time" belongs in the comparison, not in the
    // degraded list — so the run succeeds and the metric fails.
    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let s = stdout(&out);
    let signals: HashMap<String, Vec<String>> = toon_table(&s, "signals")
        .into_iter()
        .map(|r| (r[0].clone(), r))
        .collect();
    assert_eq!(signals["completed"][2], "fail", "{s}");

    let json = env.run(&["report", "--json"]);
    let run: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let status_fact = run["facts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["key"] == "status")
        .unwrap();
    assert_eq!(status_fact["value"], "timeout");
}

#[test]
fn a_probe_result_is_cached_because_measuring_again_costs_real_money() {
    let mock = Mock::start(probe_routes());
    mock.sequence("/status", vec![status("completed")]);
    let env = Env::new("probecache");
    env.write_pack("pr", &probe_pack(&mock, "1s", "30s"));

    env.run_with(
        &["research", "probe:x", "--depth", "deep"],
        &[("FAKE_VENDOR_KEY", "secret")],
    );
    let after_first = mock.hits();
    assert!(after_first >= 2, "submit plus at least one poll");

    let s = stdout(&env.run_with(
        &["research", "probe:x", "--depth", "deep"],
        &[("FAKE_VENDOR_KEY", "secret")],
    ));
    assert_eq!(
        mock.hits(),
        after_first,
        "a second run must reuse the measurement, not re-submit a billable job"
    );
    assert!(s.contains("cached"), "{s}");
}

#[test]
fn a_rejected_submission_degrades_the_source_and_names_the_status() {
    // 401 from the vendor is a real failure of the source, unlike a slow job.
    let mut routes = probe_routes();
    routes.insert(
        "/submit".to_string(),
        (401, r#"{"error":"bad key"}"#.to_string()),
    );
    let mock = Mock::start(routes);
    let env = Env::new("probereject");
    env.write_pack("pr", &probe_pack(&mock, "1s", "30s"));

    let out = env.run_with(
        &["research", "probe:x", "--depth", "deep"],
        &[("FAKE_VENDOR_KEY", "wrong")],
    );
    assert_eq!(code(&out), 3);
    assert!(stdout(&out).contains("vendor(401)"), "{}", stdout(&out));
}

#[test]
fn probes_do_not_run_at_quick_depth() {
    let mock = Mock::start(probe_routes());
    mock.sequence("/status", vec![status("completed")]);
    let env = Env::new("probedepth");
    env.write_pack("pr", &probe_pack(&mock, "1s", "30s"));

    env.run_with(&["research", "probe:x"], &[("FAKE_VENDOR_KEY", "secret")]);
    assert_eq!(
        mock.hits(),
        0,
        "a quick scan must never spend vendor credits"
    );
}

#[test]
fn the_shipped_probe_pack_is_never_selected_by_accident() {
    let env = Env::new("probeauto");
    // A bare hostname must reach the reading packs, never the spending one.
    for target in ["heygen.com", "github.com/a/b", "stripe.com"] {
        let out = env.run(&["research", target, "--json"]);
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
            assert_ne!(v["pack"], "probe", "{target} auto-selected the probe pack");
        }
    }
}

#[test]
fn output_stays_compact_as_the_pack_grows() {
    // The premise of the whole tool: what comes back to the caller is bounded by the
    // number of problems, not by how much was read.
    let mock = Mock::start(repo_routes());
    let env = Env::new("compact");
    env.write_pack("t", &test_pack(&mock));

    let s = stdout(&env.run(&["research", "test:demo"]));
    let bytes_read: usize = 4200; // the fixture bodies, roughly
    assert!(
        s.len() < bytes_read,
        "returned {} bytes for {} read",
        s.len(),
        bytes_read
    );
    assert!(
        s.lines().count() < 20,
        "default output must stay scannable:\n{s}"
    );
}
