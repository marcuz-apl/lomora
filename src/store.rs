//! The only module in the crate that knows SQL. `routes.rs` and the benchmark tasks go
//! through `Db`, which owns the schema, the `runs` rows the history API reads back
//! oldest-first, and the `profile` singleton row.
//! One `rusqlite::Connection` behind a `tokio::sync::Mutex` — no connection pool: every
//! statement here is a single row write or a <60-row read, and the mutex is what keeps
//! rusqlite's `!Sync`-by-design `Connection` off the async executor's hands.

use std::path::Path;
use std::sync::Arc;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

/// `PartialEq` is for the tests below: whole-row `assert_eq!` prints a `Debug` diff that names
/// the field a shifted `params!` position landed in.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    /// The database identity, filled by reads (`runs_page`); `insert_run` ignores it and lets
    /// AUTOINCREMENT assign it. Exposed to the dashboard as the leading `id` key so the
    /// per-row delete can name exactly one row.
    pub id: i64,
    pub kind: String, pub timestamp: String, pub model: String, pub max_tokens: i64,
    pub ttft_ms: Option<f64>, pub total_s: f64, pub tokens: i64, pub tok_per_s: f64,
    pub concurrency: Option<i64>, pub base_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Profile { pub name: String, pub notes: String }

fn provider_name(base_url: &str) -> &'static str {
    if base_url.contains(":1234") { "LM Studio" }
    else if base_url.contains(":11434") { "Ollama" }
    else if base_url.contains(":4788") { "Moderado" }
    else if base_url.contains(":9931") { "llama.cpp" }
    else { "Custom" }
}

impl RunRow {
    /// A history entry carries the seven wire keys in insertion order, led by `id` — the
    /// database identity the dashboard's per-row delete targets; a (timestamp, model) pair
    /// would be ambiguous when one model is benchmarked twice in the same second. `kind`,
    /// `concurrency` and `base_url` are internal columns and never reach the wire. A `None`
    /// `ttft_ms` stays a present null key — stress rows answer `"ttft_ms": null`, not a
    /// missing field.
    pub fn to_history_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "timestamp": self.timestamp,
            "provider": self.base_url.as_deref().map(provider_name),
            "model": self.model,
            "max_tokens": self.max_tokens,
            "ttft_ms": self.ttft_ms,
            "total_s": self.total_s,
            "tokens": self.tokens,
            "tok_per_s": self.tok_per_s,
        })
    }
}

impl Profile {
    /// The factory default: the same strings the `profile` table declares as column defaults
    /// and `SCHEMA` seeds. Keep the three in sync.
    fn factory() -> Profile { Profile { name: "My workstation".into(), notes: String::new() } }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runs (
  id INTEGER PRIMARY KEY, kind TEXT NOT NULL CHECK (kind IN ('single','stress')),
  created_at TEXT NOT NULL, timestamp TEXT NOT NULL, model TEXT NOT NULL,
  max_tokens INTEGER NOT NULL, ttft_ms REAL, total_s REAL NOT NULL,
  tokens INTEGER NOT NULL, tok_per_s REAL NOT NULL, concurrency INTEGER, base_url TEXT);
CREATE TABLE IF NOT EXISTS profile (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  name TEXT NOT NULL DEFAULT 'My workstation', notes TEXT NOT NULL DEFAULT '');
INSERT OR IGNORE INTO profile (id, name, notes) VALUES (1, 'My workstation', '');
CREATE TABLE IF NOT EXISTS prompts (
  id INTEGER PRIMARY KEY, category TEXT NOT NULL, text TEXT NOT NULL,
  created_at TEXT NOT NULL);";

/// A `BEFORE UPDATE` trigger that aborts every write to the `profile` row while reads and the
/// seeded default keep working. Used only by `Db::profile_writes_fail_for_tests`.
const BREAK_PROFILE_UPDATES: &str = "
CREATE TRIGGER tests_block_profile_updates BEFORE UPDATE ON profile
BEGIN SELECT RAISE(ABORT, 'profile writes are blocked by the test'); END;";

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Db> {
        // The default path is `data/lomora.db`, and a fresh checkout has no `data/` folder.
        // Creating the parent here (not at the call site) covers every file path, explicit
        // `LOMORA_DB` values included; an empty parent (bare filename) needs nothing.
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("creating the database directory {}: {e}",
                                             parent.display()))?;
        }
        let conn = Connection::open(path)?;
        // Two handles over one file are a real configuration (the restart test builds two
        // apps on one path; a second `cargo run` does too), and without a timeout SQLite answers
        // a lock conflict immediately with SQLITE_BUSY instead of waiting for the holder.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // WAL is deliberately not enabled: this is one connection in one process, so there is no
        // concurrent reader to benefit from it, and it would drop `-wal`/`-shm` sidecar files
        // beside the user's `lomora.db` that nothing else in the project creates.
        conn.execute_batch(SCHEMA)?;
        // One-time normalization for tables written before the history stamp widened from
        // `%H:%M:%S` to `%Y-%m-%d %H:%M:%S`: the calendar date only ever lived in `created_at`
        // (the RFC 3339 local stamp written beside every row), so legacy rows get that date
        // prepended — the row's own time-of-day is kept verbatim, since `?3 is the caller's
        // timestamp, not created_at`. `length(timestamp) = 8` keeps the pass idempotent
        // (widened rows are 19 chars and never match), and the `length(created_at) >= 10`
        // guard leaves a truncated `created_at` alone rather than corrupting the stamp. Runs
        // on every open; on a fresh or already-widened table it is a no-op.
        conn.execute("UPDATE runs SET timestamp = substr(created_at, 1, 10) || ' ' || timestamp \
                      WHERE length(timestamp) = 8 AND length(created_at) >= 10", [])?;
        Ok(Db(Arc::new(Mutex::new(conn))))
    }

    /// An in-memory database with the schema applied: what `spawn_app` hands every test, so no
    /// test shares a row with another and none writes a `.db` file into the repo. Named for what
    /// it returns (a `Db`); the old `default_profile()` name promised a `Profile`.
    pub fn in_memory() -> Db {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch(SCHEMA).expect("schema");
        Db(Arc::new(Mutex::new(conn)))
    }

    /// Test seam for the swallowed-write bug: the real schema plus a trigger that makes every
    /// `profile` UPDATE fail, while reads and the seeded row keep working — which is exactly the
    /// shape of the bug (a failed write that answers with the stale row as a success). Through
    /// the public API the write path cannot otherwise fail: `open()` creates the tables and
    /// seeds `id = 1`, and nothing in the app drops them, so without this seam `POST /api/profile`
    /// would have no covering test for the mandated 200 `{"error": …}` envelope.
    pub fn profile_writes_fail_for_tests() -> Db {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch(&format!("{SCHEMA}{BREAK_PROFILE_UPDATES}")).expect("schema + trigger");
        Db(Arc::new(Mutex::new(conn)))
    }

    /// The read-side seam: a connection that is open but has no schema, so every read and write
    /// fails with `no such table`. Exists so `GET /api/profile`'s error envelope has a covering
    /// test — against the old `.unwrap_or(default)` the handler answered a success-shaped body
    /// here, which is how a busy database used to reach the UI as "My workstation".
    pub fn unreadable_for_tests() -> Db {
        Db(Arc::new(Mutex::new(Connection::open_in_memory().expect("in-memory sqlite"))))
    }

    pub async fn insert_run(&self, r: &RunRow) -> anyhow::Result<()> {
        let (kind, ts, model, mt, ttft, total, tok, tps, conc, base) = (
            r.kind.clone(), r.timestamp.clone(), r.model.clone(), r.max_tokens, r.ttft_ms,
            r.total_s, r.tokens, r.tok_per_s, r.concurrency, r.base_url.clone());
        let created = chrono::Local::now().to_rfc3339();
        let c = self.0.clone(); let g = c.lock().await;
        g.execute("INSERT INTO runs (kind, created_at, timestamp, model, max_tokens, ttft_ms, \
                   total_s, tokens, tok_per_s, concurrency, base_url) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![kind, created, ts, model, mt, ttft, total, tok, tps, conc, base])?;
        Ok(())
    }
    /// ORDER BY id DESC LIMIT n, then reversed: the API answers oldest-first and keeps that
    /// shape — the dashboard renders each page newest-first itself. Spec 5.1.
    pub async fn recent_runs(&self, limit: i64) -> anyhow::Result<Vec<RunRow>> {
        Ok(self.runs_page(limit, 0).await?.0)
    }

    /// One page of the runs table for the dashboard's history pager, plus the unfiltered row
    /// count so the UI can size its Prev/Next controls without a second endpoint. Same
    /// ordering rule as `recent_runs`: newest-first SQL, reversed to oldest-first on the way
    /// out, so page 0 is the newest `limit` rows presented oldest-first.
    pub async fn runs_page(&self, limit: i64, offset: i64) -> anyhow::Result<(Vec<RunRow>, i64)> {
        let c = self.0.clone(); let g = c.lock().await;
        let total: i64 = g.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))?;
        let mut stmt = g.prepare("SELECT id, kind, timestamp, model, max_tokens, ttft_ms, total_s, \
                   tokens, tok_per_s, concurrency, base_url FROM runs ORDER BY id DESC LIMIT ?1 OFFSET ?2")?;
        let rows = stmt.query_map(params![limit, offset], |row| Ok(RunRow {
            id: row.get(0)?, kind: row.get(1)?, timestamp: row.get(2)?, model: row.get(3)?,
            max_tokens: row.get(4)?, ttft_ms: row.get(5)?, total_s: row.get(6)?,
            tokens: row.get(7)?, tok_per_s: row.get(8)?, concurrency: row.get(9)?,
            base_url: row.get(10)?,
        }))?;
        let mut out = rows.collect::<Result<Vec<_>, _>>()?;
        out.reverse();
        Ok((out, total))
    }

    /// Removes exactly one run by its database id and reports how many rows went — 0 for an
    /// unknown id, which the handler answers as `{"deleted": 0}`, not a failure — together
    /// with the fresh row count so the dashboard can clamp its pager without a second request.
    pub async fn delete_run(&self, id: i64) -> anyhow::Result<(usize, i64)> {
        let c = self.0.clone(); let g = c.lock().await;
        let deleted = g.execute("DELETE FROM runs WHERE id = ?1", params![id])?;
        let total: i64 = g.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))?;
        Ok((deleted, total))
    }
    /// The stored profile. Errors propagate: this used to end in `.unwrap_or(default)`, which
    /// fires for *any* `query_row` failure, so a transient `SQLITE_BUSY` made `GET /api/profile`
    /// report "My workstation", the UI loaded that into the form, and the next Save wrote the
    /// default over the user's real profile. Only a genuinely missing row falls back.
    pub async fn profile(&self) -> anyhow::Result<Profile> {
        let c = self.0.clone(); let g = c.lock().await;
        match g.query_row("SELECT name, notes FROM profile WHERE id = 1", [], |r|
            Ok(Profile { name: r.get(0)?, notes: r.get(1)? }))
        {
            Ok(p) => Ok(p),
            // `open()` seeds `id = 1` and the table CHECKs it, so "no row" means someone deleted
            // it out from under us: answer the defaults a fresh database would carry, and let
            // every other failure (busy, closed handle, corrupt file) reach the caller.
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Profile::factory()),
            Err(e) => Err(e.into()),
        }
    }
    /// Absent (`None`) leaves the column untouched, present (including `""`) overwrites — the
    /// wire contract. A failed `UPDATE` propagates instead of being swallowed: the handler
    /// then answers the mandated 200 `{"error": …}` rather than reporting the stale row as a
    /// saved profile, which the dashboard would print as "Profile saved".
    pub async fn set_profile(&self, name: Option<&str>, notes: Option<&str>)
        -> anyhow::Result<Profile>
    {
        {
            let c = self.0.clone(); let g = c.lock().await;
            // Connection::execute takes &self, so the guard must not be `mut` — `unused_mut`
            // is a rustc warning and the build runs with `-D warnings`.
            if let Some(n) = name { g.execute("UPDATE profile SET name = ?1 WHERE id = 1", params![n])?; }
            if let Some(v) = notes { g.execute("UPDATE profile SET notes = ?1 WHERE id = 1", params![v])?; }
        }   // the guard drops here: it is never held across the `await` below
        self.profile().await
    }

    /// The fetched-question cache, written by `POST /api/prompts/fetch`. A fetch REPLACES the
    /// whole set (one transaction: delete, then insert) rather than appending, so repeated
    /// fetches never accumulate duplicates; the built-in pool lives in code, not here.
    pub async fn replace_prompts(&self, items: &[crate::prompts::Question]) -> anyhow::Result<()> {
        let created = chrono::Local::now().to_rfc3339();
        let c = self.0.clone(); let g = c.lock().await;
        let tx = g.unchecked_transaction()?;
        tx.execute("DELETE FROM prompts", [])?;
        {
            let mut stmt = tx.prepare("INSERT INTO prompts (category, text, created_at) VALUES (?1, ?2, ?3)")?;
            for q in items {
                stmt.execute(params![q.category, q.text, created])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The cached fetched questions, oldest first (`ORDER BY id`): `GET /api/prompts` merges
    /// them behind the built-in pool in exactly this order.
    pub async fn list_prompts(&self) -> anyhow::Result<Vec<crate::prompts::Question>> {
        let c = self.0.clone(); let g = c.lock().await;
        let mut stmt = g.prepare("SELECT category, text FROM prompts ORDER BY id")?;
        let rows = stmt.query_map([], |row| Ok(crate::prompts::Question {
            category: row.get(0)?, text: row.get(1)?,
        }))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A real file in `temp_dir()`, one per test, deleted on drop (so a panicking assert still
    /// cleans up). Never the repo-root `lomora.db` — that is the production default `Config`
    /// resolves to, and two tests sharing a file would share rows.
    struct TempDb { path: PathBuf }

    impl TempDb {
        fn new(tag: &str) -> TempDb {
            static SEQ: AtomicUsize = AtomicUsize::new(1);
            let n = SEQ.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir();
            let path = dir.join(format!("lomora-store-{}-{n}-{tag}.db", std::process::id()));
            assert_eq!(path.parent(), Some(dir.as_path()), "test databases live in temp_dir only");
            // A crashed earlier run must not leak rows into this one.
            let _ = std::fs::remove_file(&path);
            TempDb { path }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) { let _ = std::fs::remove_file(&self.path); }
    }

    // The headline claim of the store: a saved profile outlives the process that saved it,
    // something an in-memory profile can never promise.
    #[tokio::test]
    async fn saved_profile_survives_closing_and_reopening_the_same_file() {
        let f = TempDb::new("reopen");
        let db = Db::open(&f.path).unwrap();
        db.set_profile(Some("Rig A"), Some("rtx 4090, 24GB")).await.unwrap();
        drop(db);                                   // the connection, and with it the file handle
        let reopened = Db::open(&f.path).unwrap();  // a new "process" over the same path
        let p = reopened.profile().await.unwrap();
        assert_eq!(p.name, "Rig A", "the name must survive the reopen");
        assert_eq!(p.notes, "rtx 4090, 24GB", "the notes must survive the reopen");
    }

    /// `SCHEMA` re-runs on every `open()`, so its `INSERT OR IGNORE` must neither reset a
    /// customized row nor add a second one.
    #[tokio::test]
    async fn profile_seed_is_idempotent_across_reopens() {
        let f = TempDb::new("seed");
        Db::open(&f.path).unwrap();
        let db = Db::open(&f.path).unwrap();
        db.set_profile(Some("Only once"), None).await.unwrap();
        drop(db);
        let again = Db::open(&f.path).unwrap();
        assert_eq!(again.profile().await.unwrap().name, "Only once", "a reopen must not reset it");
        let rows: i64 = {
            let g = again.0.lock().await;
            g.query_row("SELECT COUNT(*) FROM profile", [], |r| r.get(0)).unwrap()
        };
        assert_eq!(rows, 1, "the seed must not add a second row on reopen");
    }

    /// `insert_run` binds eleven parameters and `recent_runs` reads ten back, so every field is
    /// given a distinct value here: a shifted `params!` or `row.get(n)` position shows up in the
    /// `Debug` diff instead of passing unnoticed. The eleventh (`created_at`) is not in `RunRow`
    /// and is checked against the connection directly.
    #[tokio::test]
    async fn runs_round_trip_oldest_first_with_every_column_in_its_field() {
        let f = TempDb::new("runs");
        let db = Db::open(&f.path).unwrap();
        // `id` is caller-meaningless on insert (AUTOINCREMENT owns it) and is filled by reads,
        // so the expected rows below carry the ids a fresh table assigns: 1, 2, 3.
        let first = RunRow { id: 1, kind: "single".into(), timestamp: "09:41:03".into(),
            model: "model-one".into(), max_tokens: 128, ttft_ms: Some(41.5), total_s: 1.25,
            tokens: 64, tok_per_s: 51.2, concurrency: Some(1),
            base_url: Some("http://127.0.0.1:1234".into()) };
        // The middle row keeps the two NULL-able columns as `None` — the shape of a single run
        // with no concurrency field and no base URL.
        let second = RunRow { id: 2, kind: "stress".into(), timestamp: "09:41:09".into(),
            model: "model-two".into(), max_tokens: 256, ttft_ms: None, total_s: 2.5,
            tokens: 128, tok_per_s: 102.4, concurrency: None, base_url: None };
        let third = RunRow { id: 3, kind: "single".into(), timestamp: "09:41:15".into(),
            model: "model-three".into(), max_tokens: 512, ttft_ms: Some(404.75), total_s: 3.75,
            tokens: 192, tok_per_s: 204.8, concurrency: Some(4),
            base_url: Some("http://127.0.0.1:11434".into()) };
        for mut r in [first.clone(), second.clone(), third.clone()] { r.id = 0; db.insert_run(&r).await.unwrap(); }

        // `ORDER BY id DESC LIMIT 2` then `reverse()` == the last two rows, oldest-first.
        assert_eq!(db.recent_runs(2).await.unwrap(), vec![second.clone(), third.clone()],
                   "recent_runs must answer oldest-first");
        assert_eq!(db.recent_runs(10).await.unwrap(), vec![first.clone(), second, third],
                   "the whole deque, oldest-first");

        // `created_at` (?2) and `timestamp` (?3) are both TEXT, so a swap between them is
        // invisible to everything above; read the row straight off the connection.
        let (created_at, timestamp) = {
            let g = db.0.lock().await;
            g.query_row("SELECT created_at, timestamp FROM runs ORDER BY id LIMIT 1", [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).unwrap()
        };
        assert_eq!(timestamp, "09:41:03", "?3 is the caller's timestamp, not created_at");
        chrono::DateTime::parse_from_rfc3339(&created_at)
            .unwrap_or_else(|e| panic!("?2 must be an RFC 3339 stamp, got {created_at:?}: {e}"));
    }

    #[tokio::test]
    async fn recent_runs_of_an_empty_table_is_an_empty_vec() {
        let f = TempDb::new("empty");
        let db = Db::open(&f.path).unwrap();
        assert_eq!(db.recent_runs(5).await.unwrap(), Vec::<RunRow>::new());
    }

    /// A table written before the history stamp widened from `HH:MM:SS` to
    /// `YYYY-MM-DD HH:MM:SS` holds bare time-of-day rows — the date only ever lived in
    /// `created_at`. The next `open()` must re-stamp those rows from their own `created_at`
    /// so the dashboard's Time column is uniform; rows that already carry a date (19 chars)
    /// never match and are left untouched.
    #[tokio::test]
    async fn a_time_only_stamp_is_dated_from_created_at_on_the_next_open() {
        let f = TempDb::new("date-backfill");
        {
            let db = Db::open(&f.path).unwrap();
            let row = RunRow { id: 0, kind: "single".into(), timestamp: "09:41:03".into(),
                model: "legacy".into(), max_tokens: 64, ttft_ms: Some(41.5), total_s: 1.25,
                tokens: 32, tok_per_s: 25.6, concurrency: None, base_url: None };
            db.insert_run(&row).await.unwrap();
        }
        // Reopening the same file is a restart — the migration runs against the stored row.
        let db = Db::open(&f.path).unwrap();
        let rows = db.recent_runs(10).await.unwrap();
        assert_eq!(rows.len(), 1);
        let ts = rows[0].timestamp.clone();
        assert_eq!(ts.len(), 19, "the stamp now carries the date: {ts}");
        assert!(ts.ends_with(" 09:41:03"), "the original wall time survives: {ts}");
        chrono::NaiveDateTime::parse_from_str(&ts, "%Y-%m-%d %H:%M:%S")
            .unwrap_or_else(|e| panic!("timestamp must be `%Y-%m-%d %H:%M:%S`, got {ts:?}: {e}"));
        // The date must come from the row's own `created_at`, not from migration time: both
        // strings start with the same local calendar date by construction.
        let created_at = { let g = db.0.lock().await;
            g.query_row("SELECT created_at FROM runs LIMIT 1", [],
                |r| r.get::<_, String>(0)).unwrap() };
        assert_eq!(&ts[..10], &created_at[..10],
                   "date comes from the row's own created_at: {ts:?} vs {created_at:?}");
    }

    #[tokio::test]
    async fn kind_check_rejects_an_unknown_run_kind() {
        let db = Db::in_memory();
        let bad = RunRow { id: 0, kind: "banana".into(), timestamp: "09:41:03".into(),
            model: "m".into(), max_tokens: 1, ttft_ms: None, total_s: 1.0, tokens: 1,
            tok_per_s: 1.0, concurrency: None, base_url: None };
        let err = db.insert_run(&bad).await.unwrap_err();
        let sqlite = err.downcast_ref::<rusqlite::Error>()
            .expect("the CHECK failure must still be a rusqlite error");
        match sqlite {
            rusqlite::Error::SqliteFailure(f, Some(msg)) => {
                assert_eq!(f.code, rusqlite::ErrorCode::ConstraintViolation, "{msg}");
                assert!(msg.contains("CHECK"), "the constraint should name itself: {msg}");
            }
            other => panic!("expected a CHECK constraint failure, got {other:?}"),
        }
        // The rejection is per-statement, not per-connection: the row below still inserts.
        db.insert_run(&RunRow { kind: "stress".into(), ..bad }).await.unwrap();
        assert_eq!(db.recent_runs(5).await.unwrap().len(), 1);
    }

    /// `Db::open`'s `PRAGMA busy_timeout = 5000`, read back off the connection it configured.
    #[tokio::test]
    async fn open_sets_a_busy_timeout() {
        let f = TempDb::new("busy");
        let db = Db::open(&f.path).unwrap();
        let ms: i64 = {
            let g = db.0.lock().await;
            g.query_row("PRAGMA busy_timeout", [], |r| r.get(0)).unwrap()
        };
        assert_eq!(ms, 5000, "two handles over one file must wait, not fail instantly");
    }

    /// The default database now lives in a `data/` folder a fresh checkout does not have;
    /// opening a path whose parent directory is missing must create it, not fail.
    #[tokio::test]
    async fn open_creates_missing_parent_directories() {
        let root = std::env::temp_dir().join(format!("lomora-mkdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("nested").join("runs.db");
        let db = Db::open(&path).unwrap();
        let row = RunRow { id: 0, kind: "single".into(), timestamp: "09:41:03".into(),
            model: "m".into(), max_tokens: 1, ttft_ms: None, total_s: 1.0, tokens: 1,
            tok_per_s: 1.0, concurrency: None, base_url: None };
        db.insert_run(&row).await.unwrap();
        assert!(path.exists(), "the database file must exist under the new directory");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The two swallowing bugs at the store level: a failed write is `Err` rather than the stale
    /// row, and a failed read is `Err` rather than the factory default laundered into a success.
    #[tokio::test]
    async fn failed_write_and_failed_read_propagate() {
        let db = Db::profile_writes_fail_for_tests();
        // Reads still answer from the seeded row — which is what made the swallowed write look
        // like a successful save to the caller.
        assert_eq!(db.profile().await.unwrap().name, "My workstation");
        let err = db.set_profile(Some("Rig A"), None).await.unwrap_err();
        assert!(err.to_string().contains("blocked by the test"), "{err}");

        // A handle with no schema: `profile()` must propagate, never default.
        let bare = Db::unreadable_for_tests();
        let err = bare.profile().await.unwrap_err();
        assert!(err.to_string().contains("no such table"), "{err}");
        let err = bare.set_profile(Some("x"), Some("y")).await.unwrap_err();
        assert!(err.to_string().contains("no such table"), "{err}");
    }

    /// The one fallback that stays: a *missing row* arrives as `QueryReturnedNoRows`, not as a
    /// failure, and answers the factory defaults.
    #[tokio::test]
    async fn missing_profile_row_falls_back_to_factory_defaults() {
        let db = Db::in_memory();
        {
            let g = db.0.lock().await;
            g.execute("DELETE FROM profile WHERE id = 1", []).unwrap();
        }
        let p = db.profile().await.unwrap();
        assert_eq!(p.name, "My workstation");
        assert_eq!(p.notes, "");
    }
}
