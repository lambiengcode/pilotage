//! Three-tier SQLite cache. One file, no server.
//!
//! * **L1 blobs** — raw responses, expired by per-source TTL.
//! * **L2 facts** — extraction results keyed by `(content_hash, extractor_version)`.
//!   Never expires: the same bytes put through the same extractor cannot produce a
//!   different answer, which is what makes re-running research on unchanged pages
//!   free even when rung 3 was involved.
//! * **L3 runs** — complete run payloads, so `compare` and `watch` diff stored
//!   results instead of researching again.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const SCHEMA_VERSION: i64 = 1;

// ponytail: one connection behind a mutex. Every query here is a keyed lookup on an
// indexed column, so contention is nanoseconds against fetches measured in
// milliseconds. Move to a pool if pilotage ever grows a long-running server mode.
pub struct Cache {
    conn: Mutex<Connection>,
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Blob {
    pub url: String,
    pub status: u16,
    pub body: Vec<u8>,
    pub content_hash: String,
    pub fetched_at: i64,
}

impl Blob {
    pub fn age(&self, now: i64) -> i64 {
        (now - self.fetched_at).max(0)
    }
    pub fn is_fresh(&self, now: i64, ttl: i64) -> bool {
        self.age(now) < ttl
    }
}

#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: String,
    pub target: String,
    pub pack: String,
    pub created_at: i64,
    pub duration_ms: i64,
    pub verdict: String,
    pub score: f64,
    pub coverage: f64,
    pub cost_usd: f64,
    pub payload: String,
}

#[derive(Debug, Clone)]
pub struct Watch {
    pub target: String,
    pub pack: String,
    pub added_at: i64,
    pub last_run: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub blobs: i64,
    pub blob_bytes: i64,
    pub facts: i64,
    pub runs: i64,
    pub watches: i64,
    pub db_bytes: i64,
}

/// Cache location. `PILOTAGE_HOME` exists so tests and benchmarks get a private
/// database instead of trampling the user's.
pub fn default_path() -> PathBuf {
    if let Ok(dir) = std::env::var("PILOTAGE_HOME") {
        return PathBuf::from(dir).join("cache.sqlite");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".pilotage").join("cache.sqlite")
}

impl Cache {
    pub fn open_default() -> Result<Cache> {
        Cache::open(&default_path())
    }

    pub fn open(path: &Path) -> Result<Cache> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating cache directory {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening cache at {}", path.display()))?;
        // WAL keeps a long research run's writes from blocking a concurrent read,
        // which matters because several pilotage invocations often overlap.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        let cache = Cache {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
        };
        cache.migrate()?;
        Ok(cache)
    }

    pub fn open_in_memory() -> Result<Cache> {
        let conn = Connection::open_in_memory()?;
        let cache = Cache {
            conn: Mutex::new(conn),
            path: PathBuf::from(":memory:"),
        };
        cache.migrate()?;
        Ok(cache)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS blobs (
                url          TEXT PRIMARY KEY,
                status       INTEGER NOT NULL,
                body         BLOB NOT NULL,
                content_hash TEXT NOT NULL,
                fetched_at   INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS blobs_hash ON blobs(content_hash);

            CREATE TABLE IF NOT EXISTS facts (
                content_hash      TEXT NOT NULL,
                extractor_version TEXT NOT NULL,
                payload           TEXT NOT NULL,
                created_at        INTEGER NOT NULL,
                PRIMARY KEY (content_hash, extractor_version)
            );

            CREATE TABLE IF NOT EXISTS runs (
                id          TEXT PRIMARY KEY,
                target      TEXT NOT NULL,
                pack        TEXT NOT NULL,
                created_at  INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                verdict     TEXT NOT NULL,
                score       REAL NOT NULL,
                coverage    REAL NOT NULL,
                cost_usd    REAL NOT NULL,
                payload     TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS runs_target ON runs(target, pack, created_at DESC);
            CREATE INDEX IF NOT EXISTS runs_created ON runs(created_at DESC);

            CREATE TABLE IF NOT EXISTS watches (
                target   TEXT NOT NULL,
                pack     TEXT NOT NULL,
                added_at INTEGER NOT NULL,
                last_run TEXT,
                PRIMARY KEY (target, pack)
            );
            "#,
        )?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    // -- L1 ------------------------------------------------------------------

    /// Returns the stored response regardless of age. Freshness is the caller's
    /// decision because it depends on the source's TTL, and because replay mode
    /// deliberately serves stale entries.
    pub fn get_blob(&self, url: &str) -> Result<Option<Blob>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT url, status, body, content_hash, fetched_at FROM blobs WHERE url = ?1",
                params![url],
                |r| {
                    Ok(Blob {
                        url: r.get(0)?,
                        status: r.get::<_, i64>(1)? as u16,
                        body: r.get(2)?,
                        content_hash: r.get(3)?,
                        fetched_at: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn put_blob(&self, url: &str, status: u16, body: &[u8], fetched_at: i64) -> Result<String> {
        let hash = crate::util::content_hash(body);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO blobs (url, status, body, content_hash, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(url) DO UPDATE SET
               status = excluded.status,
               body = excluded.body,
               content_hash = excluded.content_hash,
               fetched_at = excluded.fetched_at",
            params![url, status as i64, body, hash, fetched_at],
        )?;
        Ok(hash)
    }

    // -- L2 ------------------------------------------------------------------

    pub fn get_facts(
        &self,
        content_hash: &str,
        version: &str,
    ) -> Result<Option<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM facts WHERE content_hash = ?1 AND extractor_version = ?2",
                params![content_hash, version],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match payload {
            Some(p) => Some(serde_json::from_str(&p)?),
            None => None,
        })
    }

    pub fn put_facts(
        &self,
        content_hash: &str,
        version: &str,
        payload: &serde_json::Value,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO facts (content_hash, extractor_version, payload, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                content_hash,
                version,
                serde_json::to_string(payload)?,
                crate::util::now()
            ],
        )?;
        Ok(())
    }

    // -- L3 ------------------------------------------------------------------

    pub fn save_run(&self, run: &RunRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO runs
               (id, target, pack, created_at, duration_ms, verdict, score, coverage, cost_usd, payload)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                run.id, run.target, run.pack, run.created_at, run.duration_ms,
                run.verdict, run.score, run.coverage, run.cost_usd, run.payload
            ],
        )?;
        Ok(())
    }

    pub fn get_run(&self, id: &str) -> Result<Option<RunRow>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT id,target,pack,created_at,duration_ms,verdict,score,coverage,cost_usd,payload
                 FROM runs WHERE id = ?1",
                params![id],
                run_row,
            )
            .optional()?;
        Ok(row)
    }

    /// Most recent run for a target, used by `watch` to compute deltas.
    pub fn last_run_for(&self, target: &str, pack: &str) -> Result<Option<RunRow>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT id,target,pack,created_at,duration_ms,verdict,score,coverage,cost_usd,payload
                 FROM runs WHERE target = ?1 AND pack = ?2 ORDER BY created_at DESC LIMIT 1",
                params![target, pack],
                run_row,
            )
            .optional()?;
        Ok(row)
    }

    /// The run before `before_id` for the same target — the other half of a diff.
    pub fn previous_run(&self, run: &RunRow) -> Result<Option<RunRow>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT id,target,pack,created_at,duration_ms,verdict,score,coverage,cost_usd,payload
                 FROM runs WHERE target = ?1 AND pack = ?2 AND created_at < ?3
                 ORDER BY created_at DESC LIMIT 1",
                params![run.target, run.pack, run.created_at],
                run_row,
            )
            .optional()?;
        Ok(row)
    }

    pub fn recent_runs(&self, limit: usize) -> Result<Vec<RunRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id,target,pack,created_at,duration_ms,verdict,score,coverage,cost_usd,payload
             FROM runs ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], run_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // -- Watches -------------------------------------------------------------

    /// Idempotent by primary key: adding the same watch twice is one watch.
    pub fn watch_add(&self, target: &str, pack: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO watches (target, pack, added_at) VALUES (?1, ?2, ?3)",
            params![target, pack, crate::util::now()],
        )?;
        Ok(changed > 0)
    }

    pub fn watch_remove(&self, target: &str, pack: Option<&str>) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = match pack {
            Some(p) => conn.execute(
                "DELETE FROM watches WHERE target = ?1 AND pack = ?2",
                params![target, p],
            )?,
            None => conn.execute("DELETE FROM watches WHERE target = ?1", params![target])?,
        };
        Ok(n)
    }

    pub fn watch_list(&self) -> Result<Vec<Watch>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT target, pack, added_at, last_run FROM watches ORDER BY added_at")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Watch {
                    target: r.get(0)?,
                    pack: r.get(1)?,
                    added_at: r.get(2)?,
                    last_run: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn watch_touch(&self, target: &str, pack: &str, run_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE watches SET last_run = ?3 WHERE target = ?1 AND pack = ?2",
            params![target, pack, run_id],
        )?;
        Ok(())
    }

    // -- Housekeeping --------------------------------------------------------

    pub fn stats(&self) -> Result<Stats> {
        let conn = self.conn.lock().unwrap();
        let mut s = Stats {
            blobs: conn.query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))?,
            blob_bytes: conn.query_row(
                "SELECT COALESCE(SUM(LENGTH(body)),0) FROM blobs",
                [],
                |r| r.get(0),
            )?,
            facts: conn.query_row("SELECT COUNT(*) FROM facts", [], |r| r.get(0))?,
            runs: conn.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))?,
            watches: conn.query_row("SELECT COUNT(*) FROM watches", [], |r| r.get(0))?,
            db_bytes: 0,
        };
        s.db_bytes = std::fs::metadata(&self.path)
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        Ok(s)
    }

    /// Drops expired blobs and runs past their retention window. L2 facts survive —
    /// they are small, immutable, and the expensive thing to recompute.
    pub fn prune(&self, blob_max_age: i64, run_max_age: i64) -> Result<(usize, usize)> {
        let now = crate::util::now();
        let conn = self.conn.lock().unwrap();
        // Inclusive, so a max age of zero means "everything" rather than "everything
        // written before this second".
        let blobs = conn.execute(
            "DELETE FROM blobs WHERE fetched_at <= ?1",
            params![now - blob_max_age],
        )?;
        let runs = conn.execute(
            "DELETE FROM runs WHERE created_at <= ?1",
            params![now - run_max_age],
        )?;
        Ok((blobs, runs))
    }
}

fn run_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        id: r.get(0)?,
        target: r.get(1)?,
        pack: r.get(2)?,
        created_at: r.get(3)?,
        duration_ms: r.get(4)?,
        verdict: r.get(5)?,
        score: r.get(6)?,
        coverage: r.get(7)?,
        cost_usd: r.get(8)?,
        payload: r.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cache() -> Cache {
        Cache::open_in_memory().unwrap()
    }

    fn row(id: &str, target: &str, created_at: i64) -> RunRow {
        RunRow {
            id: id.into(),
            target: target.into(),
            pack: "oss".into(),
            created_at,
            duration_ms: 100,
            verdict: "healthy".into(),
            score: 0.9,
            coverage: 1.0,
            cost_usd: 0.0,
            payload: "{}".into(),
        }
    }

    #[test]
    fn blobs_round_trip_and_report_freshness() {
        let c = cache();
        let hash = c.put_blob("https://x/a", 200, b"hello", 1_000).unwrap();
        let b = c.get_blob("https://x/a").unwrap().unwrap();
        assert_eq!(b.body, b"hello");
        assert_eq!(b.status, 200);
        assert_eq!(b.content_hash, hash);
        assert!(b.is_fresh(1_500, 3_600));
        assert!(!b.is_fresh(9_000, 3_600));
    }

    #[test]
    fn refetching_a_url_replaces_rather_than_duplicates() {
        let c = cache();
        c.put_blob("https://x/a", 200, b"v1", 1_000).unwrap();
        c.put_blob("https://x/a", 200, b"v2", 2_000).unwrap();
        assert_eq!(c.get_blob("https://x/a").unwrap().unwrap().body, b"v2");
        assert_eq!(c.stats().unwrap().blobs, 1);
    }

    #[test]
    fn missing_blob_is_none_not_an_error() {
        assert!(cache().get_blob("https://nope").unwrap().is_none());
    }

    #[test]
    fn extraction_cache_is_keyed_on_content_and_extractor_version() {
        let c = cache();
        c.put_facts("h1", "v1", &json!({"a": 1})).unwrap();
        assert_eq!(c.get_facts("h1", "v1").unwrap().unwrap()["a"], 1);
        // Same content, newer extractor: a miss, which is exactly what forces
        // re-extraction after a rule is fixed.
        assert!(c.get_facts("h1", "v2").unwrap().is_none());
        // Different content, same extractor: also a miss.
        assert!(c.get_facts("h2", "v1").unwrap().is_none());
    }

    #[test]
    fn runs_are_retrievable_and_ordered_newest_first() {
        let c = cache();
        c.save_run(&row("r_1", "a/b", 100)).unwrap();
        c.save_run(&row("r_2", "a/b", 200)).unwrap();
        c.save_run(&row("r_3", "c/d", 300)).unwrap();

        assert_eq!(c.get_run("r_2").unwrap().unwrap().target, "a/b");
        let recent = c.recent_runs(10).unwrap();
        assert_eq!(
            recent.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["r_3", "r_2", "r_1"]
        );
        assert_eq!(c.recent_runs(2).unwrap().len(), 2);
    }

    #[test]
    fn previous_run_finds_the_other_half_of_a_diff() {
        let c = cache();
        c.save_run(&row("r_1", "a/b", 100)).unwrap();
        c.save_run(&row("r_2", "a/b", 200)).unwrap();
        c.save_run(&row("r_other", "z/z", 150)).unwrap();

        let latest = c.last_run_for("a/b", "oss").unwrap().unwrap();
        assert_eq!(latest.id, "r_2");
        let prev = c.previous_run(&latest).unwrap().unwrap();
        assert_eq!(prev.id, "r_1");
        // The oldest run has no predecessor rather than wrapping around.
        assert!(c.previous_run(&prev).unwrap().is_none());
    }

    #[test]
    fn watches_are_idempotent() {
        let c = cache();
        assert!(c.watch_add("a/b", "oss").unwrap());
        assert!(!c.watch_add("a/b", "oss").unwrap());
        assert_eq!(c.watch_list().unwrap().len(), 1);

        c.watch_touch("a/b", "oss", "r_9").unwrap();
        assert_eq!(c.watch_list().unwrap()[0].last_run.as_deref(), Some("r_9"));

        assert_eq!(c.watch_remove("a/b", Some("oss")).unwrap(), 1);
        assert!(c.watch_list().unwrap().is_empty());
    }

    #[test]
    fn removing_a_watch_without_a_pack_removes_every_pack() {
        let c = cache();
        c.watch_add("a/b", "oss").unwrap();
        c.watch_add("a/b", "saas").unwrap();
        assert_eq!(c.watch_remove("a/b", None).unwrap(), 2);
    }

    #[test]
    fn prune_expires_blobs_and_runs_but_keeps_facts() {
        let c = cache();
        let now = crate::util::now();
        c.put_blob("https://x/old", 200, b"x", now - 10_000)
            .unwrap();
        c.put_blob("https://x/new", 200, b"x", now).unwrap();
        c.save_run(&row("r_old", "a/b", now - 10_000)).unwrap();
        c.put_facts("h", "v1", &json!({"a": 1})).unwrap();

        let (blobs, runs) = c.prune(5_000, 5_000).unwrap();
        assert_eq!((blobs, runs), (1, 1));
        assert!(c.get_blob("https://x/new").unwrap().is_some());
        assert!(c.get_facts("h", "v1").unwrap().is_some());
    }

    #[test]
    fn stats_count_what_is_stored() {
        let c = cache();
        c.put_blob("https://x/a", 200, b"12345", 0).unwrap();
        c.put_facts("h", "v", &json!({})).unwrap();
        c.save_run(&row("r_1", "a/b", 0)).unwrap();
        c.watch_add("a/b", "oss").unwrap();
        let s = c.stats().unwrap();
        assert_eq!(
            (s.blobs, s.blob_bytes, s.facts, s.runs, s.watches),
            (1, 5, 1, 1, 1)
        );
    }

    #[test]
    fn reopening_an_existing_database_does_not_re_migrate_or_lose_data() {
        let dir = std::env::temp_dir().join(format!("pilotage-test-{}", crate::util::new_run_id()));
        let path = dir.join("cache.sqlite");
        {
            let c = Cache::open(&path).unwrap();
            c.put_blob("https://x/a", 200, b"persisted", 0).unwrap();
        }
        {
            let c = Cache::open(&path).unwrap();
            assert_eq!(
                c.get_blob("https://x/a").unwrap().unwrap().body,
                b"persisted"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
