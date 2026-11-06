//! The only module in the crate that knows SQL. `routes.rs` and the benchmark tasks go
//! through `Db`, which owns the schema, the `runs` rows the history API reads back
//! oldest-first, and the `profile` singleton row. `Db::run_sql` additionally runs a script the
//! operator typed, behind the `LOMORA_SQL` switch and its interrupt budget.
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
    /// The label written beside `base_url` when the row was inserted. Filled by reads, like `id`;
    /// `insert_run` ignores a caller-set value and writes its own derivation from `base_url`, so
    /// the two columns cannot disagree.
    pub provider: Option<String>,
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
    /// `concurrency` and `base_url` are internal columns and never reach the wire; `provider`
    /// is the label stored with the row, so an edit through the SQL console changes what the
    /// dashboard shows. A `None` `ttft_ms` stays a present null key — stress rows answer
    /// `"ttft_ms": null`, not a missing field.
    pub fn to_history_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "timestamp": self.timestamp,
            "provider": self.provider,
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
  created_at TEXT NOT NULL, timestamp TEXT NOT NULL, provider TEXT, model TEXT NOT NULL,
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

/// Page size of the raw-SQL console: one statement's `SELECT` never returns more rows than this,
/// and `truncated` says when the cut happened. The console is a tool for editing rows, not a way
/// to stream a whole table through the dashboard.
pub const SQL_MAX_ROWS: usize = 200;

/// The budget one console run gets, across all its statements. SQLite's progress handler is the
/// enforcement (rusqlite's `hooks` feature): every `SQL_PROGRESS_OPS` virtual-machine instructions
/// it asks the clock, and a `true` answer aborts the running statement with `SQLITE_INTERRUPT`
/// rather than letting a runaway query hold the connection — and with it the whole dashboard —
/// open-ended.
pub const SQL_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// VM instructions between progress-handler calls. Coarse enough that the handler is a rounding
/// error next to the query it is watching, fine enough that a 10 s budget is overshot by
/// milliseconds, not seconds.
const SQL_PROGRESS_OPS: i32 = 2_000;

/// Monotonic milliseconds since this process started — the clock the interrupt deadline is kept
/// against. `SystemTime` would let an NTP correction extend or cancel a running budget.
fn elapsed_ms() -> i64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as i64
}

/// One statement of a console script, in the order it ran. A statement that failed carries the
/// SQLite message in `error` and ends the script; `rows` is one JSON array per result row in
/// column order, `changes` is `sqlite3_changes` for a write.
#[derive(Debug, Clone, PartialEq)]
pub struct SqlResult {
    pub sql: String, pub columns: Vec<String>, pub rows: Vec<Vec<serde_json::Value>>,
    pub truncated: bool, pub changes: usize, pub error: Option<String>,
}

impl SqlResult {
    /// The wire shape, keys in this order (see the `preserve_order` note in Cargo.toml). The
    /// console is opt-in tooling, so the raw statement text goes back to the caller with its
    /// result — it is the user's own SQL, echoed for the per-statement heading.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "sql": self.sql, "columns": self.columns, "rows": self.rows,
            "truncated": self.truncated, "changes": self.changes, "error": self.error,
        })
    }

    fn ok_write(sql: String, changes: usize) -> SqlResult {
        SqlResult { sql, columns: Vec::new(), rows: Vec::new(), truncated: false,
                    changes, error: None }
    }
}

/// A SQLite value in its JSON form. `NULL` stays null, integers and reals stay numbers, text
/// stays text, and a blob is lowercase hex — up to 256 bytes, past which the console reports the
/// size instead of shipping a megabyte of hex to the browser.
fn json_of(value: rusqlite::types::Value) -> serde_json::Value {
    use rusqlite::types::Value as V;
    match value {
        V::Null => serde_json::Value::Null,
        V::Integer(i) => serde_json::Value::from(i),
        V::Real(f) => serde_json::Value::from(f),
        V::Text(t) => serde_json::Value::from(t),
        V::Blob(b) if b.len() > 256 =>
            serde_json::Value::from(format!("<blob {} bytes>", b.len())),
        V::Blob(b) => serde_json::Value::from(
            b.iter().map(|byte| format!("{byte:02x}")).collect::<String>()),
    }
}

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
        // The provider label used to be derived at read time; it is a column now. An existing
        // database gets it here: one ALTER on the first open after the upgrade, then the UPDATE
        // fills rows written before the column existed. Only NULLs are filled, so a label edited
        // through the SQL console survives every later open. Both steps are no-ops once done.
        let has_provider: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('runs') WHERE name = 'provider')",
            [], |r| r.get(0))?;
        if !has_provider {
            conn.execute("ALTER TABLE runs ADD COLUMN provider TEXT", [])?;
        }
        conn.execute(
            "UPDATE runs SET provider = CASE \
                 WHEN base_url LIKE '%:1234%' THEN 'LM Studio' \
                 WHEN base_url LIKE '%:11434%' THEN 'Ollama' \
                 WHEN base_url LIKE '%:4788%' THEN 'Moderado' \
                 WHEN base_url LIKE '%:9931%' THEN 'llama.cpp' \
                 ELSE 'Custom' END \
             WHERE provider IS NULL AND base_url IS NOT NULL", [])?;
        // `provider` belongs immediately before `model`, but SQLite cannot move a column and an
        // upgraded table has it appended at the end. Such a table is rebuilt once: renaming it
        // lets `SCHEMA` create the declared layout, the rows are copied across carrying their
        // stored labels, and the old table is dropped — in one transaction, so a failure leaves
        // the original intact.
        let order: Vec<String> = {
            let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('runs')")?;
            let names = stmt.query_map([], |r| r.get::<_, String>(0))?;
            names.collect::<Result<_, _>>()?
        };
        let provider_at = order.iter().position(|c| c == "provider");
        let model_at = order.iter().position(|c| c == "model")
            .expect("the runs table always declares model");
        if provider_at != model_at.checked_sub(1) {
            conn.execute_batch(&format!(
                "BEGIN IMMEDIATE;\
                 DROP TABLE IF EXISTS runs_legacy;\
                 ALTER TABLE runs RENAME TO runs_legacy;\
                 {SCHEMA}\
                 INSERT INTO runs (id, kind, created_at, timestamp, provider, model, max_tokens, \
                                   ttft_ms, total_s, tokens, tok_per_s, concurrency, base_url) \
                   SELECT id, kind, created_at, timestamp, provider, model, max_tokens, ttft_ms, \
                          total_s, tokens, tok_per_s, concurrency, base_url \
                     FROM runs_legacy;\
                 DROP TABLE runs_legacy;\
                 COMMIT;"))?;
        }
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
        // The label is written here, once, so the column and the upstream it describes cannot
        // drift; `r.provider` is a read-side value and is deliberately ignored.
        let provider = r.base_url.as_deref().map(provider_name);
        let created = chrono::Local::now().to_rfc3339();
        let c = self.0.clone(); let g = c.lock().await;
        g.execute("INSERT INTO runs (kind, created_at, timestamp, model, max_tokens, ttft_ms, \
                   total_s, tokens, tok_per_s, concurrency, base_url, provider) \
                   VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![kind, created, ts, model, mt, ttft, total, tok, tps, conc, base, provider])?;
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
                   tokens, tok_per_s, concurrency, base_url, provider FROM runs \
                   ORDER BY id DESC LIMIT ?1 OFFSET ?2")?;
        let rows = stmt.query_map(params![limit, offset], |row| Ok(RunRow {
            id: row.get(0)?, kind: row.get(1)?, timestamp: row.get(2)?, model: row.get(3)?,
            max_tokens: row.get(4)?, ttft_ms: row.get(5)?, total_s: row.get(6)?,
            tokens: row.get(7)?, tok_per_s: row.get(8)?, concurrency: row.get(9)?,
            base_url: row.get(10)?, provider: row.get(11)?,
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

    /// Runs a whole SQL script for the raw-SQL console: statements in order, one result each,
    /// the first failure ending the script. Splitting is SQLite's own — `rusqlite::Batch` walks
    /// the `sqlite3_prepare_v2` tail exactly like the `sqlite3` CLI does, so a semicolon inside
    /// a string literal or a trigger body is never mistaken for a statement end, and a
    /// multi-statement text that `Connection::prepare` rejects with `MultipleStatement` runs
    /// fine. Rows are the tool's business — `SELECT`, `INSERT`, `UPDATE`, `DELETE` and `CREATE`
    /// all run — but the schema is not: the whole script is prepared under the `sql_authorizer`
    /// guard, so a `DROP TABLE` is refused by SQLite itself rather than by scanning the text.
    ///
    /// Errors that are *about a statement* come back inside `SqlResult::error` — they are the
    /// console's output, not a transport failure, and a guard refusal arrives the same way. Only
    /// SQLite refusing to install the progress handler or the authorizer reaches the
    /// `anyhow::Result`.
    pub async fn run_sql(&self, script: &str, budget: std::time::Duration)
        -> anyhow::Result<Vec<SqlResult>>
    {
        use std::sync::atomic::{AtomicI64, Ordering::Relaxed};
        let c = self.0.clone();
        // `mut` because `progress_handler` takes `&mut self`: it swaps a boxed closure in SQLite's
        // client data, which is not something a shared handle may do.
        let mut g = c.lock().await;
        let deadline = Arc::new(AtomicI64::new(0));
        let armed = deadline.clone();
        // The handler runs on this thread, inside SQLite's VM, every `SQL_PROGRESS_OPS`
        // instructions: an atomic load and a clock read, and `true` aborts whatever statement is
        // executing. Zero means disarmed, so a script that finishes early cannot be interrupted
        // by a stale deadline.
        g.progress_handler(SQL_PROGRESS_OPS, Some(move || {
            let dl = armed.load(Relaxed);
            dl != 0 && elapsed_ms() > dl
        }))?;
        // Armed after the install, and from this moment: the task may have queued behind a
        // benchmark for the connection, and the budget covers the script, not the wait.
        deadline.store(elapsed_ms() + budget.as_millis() as i64, Relaxed);
        // The schema guard comes on for the script and off again with it. SQLite consults it at
        // prepare time, per parsed statement, so `DROP TABLE` is refused by the database before
        // it can run — no text matching, and comments, quoting and casing cannot sneak past it.
        g.authorizer(Some(sql_authorizer))?;
        let out = run_script(&g, script);
        deadline.store(0, Relaxed);
        // Both hooks come off with the run, so ordinary dashboard queries pay nothing for a tool
        // they never use — and the dashboard's own writes are never refused by the guard. Any
        // flag the last statement picked up on its way out is spent here, on a query of our own,
        // rather than on the dashboard's next read.
        g.authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>)?;
        g.progress_handler(0, None::<fn() -> bool>)?;
        for _ in 0..3 {
            if !g.is_interrupted() { break; }
            let _ = g.query_row("SELECT 1", [], |r| r.get::<_, i64>(0));
        }
        Ok(out)
    }
    /// A compacted copy of the whole database as bytes, which is what `GET /api/sql/backup`
    /// offers for download. `VACUUM INTO` is the mechanism rather than a file read: it writes a
    /// defragmented copy from one read transaction, so the bytes are a consistent snapshot even
    /// while the console is mid-edit, it works on an in-memory database (a test handle, and
    /// `Db::in_memory` is what every test server runs on), and it cannot catch the `-journal`
    /// sidecar mid-write. The target is a temporary file, read back and removed on every path —
    /// including a failed `VACUUM`, which leaves whatever SQLite created behind.
    pub async fn backup_bytes(&self) -> anyhow::Result<Vec<u8>> {
        let c = self.0.clone(); let g = c.lock().await;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos()).unwrap_or(0);
        let tmp = std::env::temp_dir()
            .join(format!("lomora-backup-{}-{stamp}.db", std::process::id()));
        let target = tmp.to_str()
            .ok_or_else(|| anyhow::anyhow!("the temporary backup path is not valid UTF-8: {}",
                                          tmp.display()))?;
        let out = (|| -> anyhow::Result<Vec<u8>> {
            g.execute("VACUUM INTO ?1", params![target])?;
            std::fs::read(&tmp)
                .map_err(|e| anyhow::anyhow!("reading the backup copy {}: {e}", tmp.display()))
        })();
        // Removed whether the vacuum worked or not: a leftover copy is the kind of file that
        // silently grows one per retry in the user's temp folder.
        std::fs::remove_file(&tmp).ok();
        out
    }
}

/// The console's schema guard, installed on the connection only for the length of one script.
/// SQLite's authorizer is asked about each action of each statement as it is prepared, so this
/// decision comes from the parsed statement — a `DROP TABLE` hidden in a comment, a string, a
/// trigger body or different casing is still a `DROP TABLE`.
///
/// Rows are the console's business: `SELECT`, `INSERT`, `UPDATE`, `DELETE` and `CREATE` are
/// allowed. The schema is not: every `DROP` (table, index, view or trigger, temporary included),
/// `ALTER TABLE`, and a write to `PRAGMA writable_schema` (which would edit `sqlite_master`
/// directly) is denied. `ATTACH`/`DETACH` are denied with them — another database file is
/// another schema to destroy, and the guard cannot see inside it. A reading
/// `PRAGMA writable_schema` stays allowed; only the write is refused.
fn sql_authorizer(ctx: rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization {
    use rusqlite::hooks::{AuthAction as Action, Authorization as Verdict};
    match ctx.action {
        Action::DropTable { .. } | Action::DropTempTable { .. }
        | Action::DropIndex { .. } | Action::DropTempIndex { .. }
        | Action::DropView { .. } | Action::DropTempView { .. }
        | Action::DropTrigger { .. } | Action::DropTempTrigger { .. }
        | Action::AlterTable { .. }
        | Action::Attach { .. } | Action::Detach { .. } => Verdict::Deny,
        Action::Pragma { pragma_name, pragma_value }
            if pragma_value.is_some() && pragma_name.eq_ignore_ascii_case("writable_schema") =>
            Verdict::Deny,
        // The enum is `#[non_exhaustive]`: a future SQLite action we have not classified is
        // allowed, which keeps this guard a schema guard rather than a general lock.
        _ => Verdict::Allow,
    }
}

/// The script loop: prepare the next statement, run it, stop on its first error.
fn run_script(conn: &Connection, script: &str) -> Vec<SqlResult> {
    // In scope only here: `Batch` is walked through rusqlite's re-exported `FallibleIterator`,
    // and a module-wide import would put `next()` in scope for every iterator in this file.
    use rusqlite::fallible_iterator::FallibleIterator;
    let mut batch = rusqlite::Batch::new(conn, script);
    let mut out = Vec::new();
    loop {
        let stmt = match batch.next() {
            Ok(Some(stmt)) => stmt,
            Ok(None) => break,
            // A prepare failure has no statement to hang it on yet (bad syntax in the text that
            // follows): report it with an empty `sql` and stop.
            Err(e) => {
                out.push(SqlResult { sql: String::new(), columns: Vec::new(), rows: Vec::new(),
                                     truncated: false, changes: 0, error: Some(e.to_string()) });
                break;
            }
        };
        let result = run_statement(stmt);
        let stop = result.error.is_some();
        out.push(result);
        if stop { break; }
    }
    out
}

/// One prepared statement: rows if it has columns, `sqlite3_changes` if it does not. A write with
/// `RETURNING` takes the row branch, so its `changes` reads 0 and the returned rows carry the
/// effect — which is what the caller asked SQLite for.
fn run_statement(mut stmt: rusqlite::Statement<'_>) -> SqlResult {
    let sql = stmt.expanded_sql().unwrap_or_default().trim().to_string();
    if stmt.column_count() == 0 {
        return match stmt.execute([]) {
            Ok(changes) => SqlResult::ok_write(sql, changes),
            Err(e) => SqlResult { sql, columns: Vec::new(), rows: Vec::new(),
                                  truncated: false, changes: 0, error: Some(e.to_string()) },
        };
    }
    let columns: Vec<String> = stmt.column_names().map(str::to_string).collect();
    let width = columns.len();
    let (mut rows, mut truncated, mut error) = (Vec::new(), false, None::<String>);
    let mut result = stmt.raw_query();
    loop {
        if rows.len() == SQL_MAX_ROWS { truncated = true; break; }
        match result.next() {
            Err(e) => { error = Some(e.to_string()); break; }
            Ok(None) => break,
            Ok(Some(row)) => {
                let mut values = Vec::with_capacity(width);
                for i in 0..width {
                    match row.get::<_, rusqlite::types::Value>(i) {
                        Ok(v) => values.push(json_of(v)),
                        Err(e) => { error = Some(e.to_string()); break; }
                    }
                }
                if error.is_some() { break; }
                rows.push(values);
            }
        }
    }
    SqlResult { sql, columns, rows, truncated, changes: 0, error }
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

    /// `insert_run` binds twelve parameters and `recent_runs` reads twelve back, so every field is
    /// given a distinct value here: a shifted `params!` or `row.get(n)` position shows up in the
    /// `Debug` diff instead of passing unnoticed. `created_at` is not in `RunRow` and is checked
    /// against the connection directly. `provider` is written by `insert_run`
    /// from `base_url`, so the expected rows carry the label that derivation produces.
    #[tokio::test]
    async fn runs_round_trip_oldest_first_with_every_column_in_its_field() {
        let f = TempDb::new("runs");
        let db = Db::open(&f.path).unwrap();
        // `id` is caller-meaningless on insert (AUTOINCREMENT owns it) and is filled by reads,
        // so the expected rows below carry the ids a fresh table assigns: 1, 2, 3.
        let first = RunRow { id: 1, kind: "single".into(), timestamp: "09:41:03".into(),
            model: "model-one".into(), max_tokens: 128, ttft_ms: Some(41.5), total_s: 1.25,
            tokens: 64, tok_per_s: 51.2, concurrency: Some(1),
            base_url: Some("http://127.0.0.1:1234".into()),
            provider: Some("LM Studio".into()) };
        // The middle row keeps the NULL-able columns as `None` — the shape of a single run
        // with no concurrency field and no base URL.
        let second = RunRow { id: 2, kind: "stress".into(), timestamp: "09:41:09".into(),
            model: "model-two".into(), max_tokens: 256, ttft_ms: None, total_s: 2.5,
            tokens: 128, tok_per_s: 102.4, concurrency: None, base_url: None,
            provider: None };
        let third = RunRow { id: 3, kind: "single".into(), timestamp: "09:41:15".into(),
            model: "model-three".into(), max_tokens: 512, ttft_ms: Some(404.75), total_s: 3.75,
            tokens: 192, tok_per_s: 204.8, concurrency: Some(4),
            base_url: Some("http://127.0.0.1:11434".into()),
            provider: Some("Ollama".into()) };
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

    /// The provider is a stored column now: `insert_run` writes the label the History table
    /// renders, instead of `to_history_json` deriving it from `base_url` on every read.
    #[tokio::test]
    async fn insert_run_stores_the_provider_label_beside_the_upstream() {
        let f = TempDb::new("provider-insert");
        let db = Db::open(&f.path).unwrap();
        db.insert_run(&RunRow { id: 0, kind: "single".into(),
            timestamp: "2026-11-06 09:00:00".into(), model: "m".into(), max_tokens: 8,
            ttft_ms: None, total_s: 1.0, tokens: 8, tok_per_s: 8.0, concurrency: None,
            base_url: Some("http://127.0.0.1:1234".into()), provider: None }).await.unwrap();
        let stored: Option<String> = {
            let g = db.0.lock().await;
            g.query_row("SELECT provider FROM runs", [], |r| r.get(0)).unwrap()
        };
        assert_eq!(stored.as_deref(), Some("LM Studio"),
                   "the label must be written literally, not derived at read time");
        // The wire value comes from the column too: a label changed in the SQL console is what
        // the History table shows on the next read.
        { let g = db.0.lock().await;
          g.execute("UPDATE runs SET provider = 'Edited'", []).unwrap(); }
        let rows = db.recent_runs(1).await.unwrap();
        assert_eq!(rows[0].to_history_json()["provider"], "Edited",
                   "history JSON must read the stored label, not re-derive it from base_url");
    }

    /// A `runs` table created before the provider column existed gains it on the next `open()`,
    /// with every stored `base_url` mapped to the label the History table renders. Rows with no
    /// upstream keep NULL, and a label edited through the SQL console survives a reopen.
    #[tokio::test]
    async fn an_old_runs_table_gains_a_backfilled_provider_column() {
        let f = TempDb::new("provider-migrate");
        {
            // The pre-provider schema, exactly what shipped before this change.
            let conn = Connection::open(&f.path).unwrap();
            conn.execute_batch("\
                CREATE TABLE runs (
                  id INTEGER PRIMARY KEY, kind TEXT NOT NULL CHECK (kind IN ('single','stress')),
                  created_at TEXT NOT NULL, timestamp TEXT NOT NULL, model TEXT NOT NULL,
                  max_tokens INTEGER NOT NULL, ttft_ms REAL, total_s REAL NOT NULL,
                  tokens INTEGER NOT NULL, tok_per_s REAL NOT NULL, concurrency INTEGER,
                  base_url TEXT);
                INSERT INTO runs (kind, created_at, timestamp, model, max_tokens, total_s,
                                  tokens, tok_per_s, base_url) VALUES
                  ('single','2026-11-05T09:41:03+08:00','2026-11-05 09:41:03','a',8,1.0,8,8.0,'http://127.0.0.1:1234'),
                  ('single','2026-11-05T09:41:04+08:00','2026-11-05 09:41:04','b',8,1.0,8,8.0,'http://127.0.0.1:11434'),
                  ('single','2026-11-05T09:41:05+08:00','2026-11-05 09:41:05','c',8,1.0,8,8.0,'http://elsewhere:9999'),
                  ('single','2026-11-05T09:41:06+08:00','2026-11-05 09:41:06','d',8,1.0,8,8.0,NULL);")
                .unwrap();
        }
        let db = Db::open(&f.path).unwrap();
        let stored: Vec<Option<String>> = {
            let g = db.0.lock().await;
            let mut stmt = g.prepare("SELECT provider FROM runs ORDER BY id").unwrap();
            let rows = stmt.query_map([], |r| r.get(0)).unwrap();
            rows.collect::<Result<Vec<_>, _>>().unwrap()
        };
        assert_eq!(stored, vec![Some("LM Studio".into()), Some("Ollama".into()),
                                Some("Custom".into()), None],
                   "every stored upstream must be backfilled; a missing one stays NULL");
        {
            let g = db.0.lock().await;
            g.execute("UPDATE runs SET provider = 'Pinned' WHERE model = 'c'", []).unwrap();
        }
        drop(db);
        let again = Db::open(&f.path).unwrap();
        let pinned: Option<String> = {
            let g = again.0.lock().await;
            g.query_row("SELECT provider FROM runs WHERE model = 'c'", [], |r| r.get(0)).unwrap()
        };
        assert_eq!(pinned.as_deref(), Some("Pinned"),
                   "the backfill must only fill NULLs, never overwrite a chosen label");
    }

    #[tokio::test]
    async fn recent_runs_of_an_empty_table_is_an_empty_vec() {
        let f = TempDb::new("empty");
        let db = Db::open(&f.path).unwrap();
        assert_eq!(db.recent_runs(5).await.unwrap(), Vec::<RunRow>::new());
    }

    /// `provider` belongs beside `timestamp`, immediately before `model` — the order the
    /// dashboard and a `SELECT *` in the SQL console read best. A fresh file must declare it
    /// there, and an upgraded one must arrive at the same layout: SQLite only appends an added
    /// column and has no statement to move one, so the table is rebuilt.
    #[tokio::test]
    async fn provider_column_sits_immediately_before_model_on_fresh_and_upgraded_tables() {
        async fn column_order(db: &Db) -> Vec<String> {
            let g = db.0.lock().await;
            let mut stmt = g.prepare("SELECT name FROM pragma_table_info('runs')").unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.collect::<Result<_, _>>().unwrap()
        }
        fn provider_then_model(names: &[String]) {
            let provider = names.iter().position(|c| c == "provider").unwrap();
            let model = names.iter().position(|c| c == "model").unwrap();
            assert_eq!(model, provider + 1, "provider must precede model: {names:?}");
        }

        let fresh_file = TempDb::new("provider-order");
        let fresh = Db::open(&fresh_file.path).unwrap();
        provider_then_model(&column_order(&fresh).await);

        // A table that already has the column appended at the end (last session's upgrade) is
        // the interesting case: it must be rebuilt without losing a label edited by hand.
        let f = TempDb::new("provider-order-old");
        {
            let conn = Connection::open(&f.path).unwrap();
            conn.execute_batch("\
                CREATE TABLE runs (
                  id INTEGER PRIMARY KEY, kind TEXT NOT NULL CHECK (kind IN ('single','stress')),
                  created_at TEXT NOT NULL, timestamp TEXT NOT NULL, model TEXT NOT NULL,
                  max_tokens INTEGER NOT NULL, ttft_ms REAL, total_s REAL NOT NULL,
                  tokens INTEGER NOT NULL, tok_per_s REAL NOT NULL, concurrency INTEGER,
                  base_url TEXT);
                INSERT INTO runs (kind, created_at, timestamp, model, max_tokens, total_s,
                                  tokens, tok_per_s, base_url)
                  VALUES ('single','2026-11-05T09:41:03+08:00','2026-11-05 09:41:03',
                          'a',8,1.0,8,8.0,'http://127.0.0.1:11434');
                ALTER TABLE runs ADD COLUMN provider TEXT;
                UPDATE runs SET provider = 'Edited';")
                .unwrap();
        }
        let upgraded = Db::open(&f.path).unwrap();
        provider_then_model(&column_order(&upgraded).await);
        let row = upgraded.recent_runs(1).await.unwrap().remove(0);
        assert_eq!(row.provider.as_deref(), Some("Edited"),
                   "the rebuild must carry the stored label across, not re-derive it");
        assert_eq!(row.model, "a");
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
                tokens: 32, tok_per_s: 25.6, concurrency: None, base_url: None,
                provider: None };
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
            tok_per_s: 1.0, concurrency: None, base_url: None, provider: None };
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
            tok_per_s: 1.0, concurrency: None, base_url: None, provider: None };
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

    // ---- the raw-SQL console (store side) ------------------------------------------------
    //
    // The console is the one place a caller supplies SQL rather than the crate, so these tests
    // pin the contract the toolbox UI is built on: statements run in order as SQLite sees them,
    // each with its own result, the run stops at the first error, a big read is capped, and a
    // query that outlives its budget aborts without leaving the connection unusable.

    /// `SqlResult::rows` is a `Vec<Vec<Value>>` (one array per row, in column order), which does
    /// not compare against a `json!` literal directly; this turns the expected shape into the
    /// same tree the store builds.
    fn table(rows: &[Vec<serde_json::Value>]) -> serde_json::Value {
        serde_json::to_value(rows).expect("a Vec<Vec<Value>> always serializes")
    }

    /// A `SELECT` answers with its column names and one JSON array per row, in SQL order; the
    /// write statements report `changes` and no columns.
    #[tokio::test]
    async fn run_sql_reports_columns_rows_and_changes_per_statement() {
        let db = Db::in_memory();
        let seed = |kind: &str| RunRow {
            id: 0, kind: kind.into(), timestamp: "2026-11-05 09:41:03".into(),
            model: "m".into(), max_tokens: 8, ttft_ms: None, total_s: 1.0, tokens: 8,
            tok_per_s: 8.0, concurrency: None, base_url: None, provider: None };
        db.insert_run(&seed("single")).await.unwrap();
        db.insert_run(&seed("stress")).await.unwrap();

        let out = db.run_sql("SELECT id, model, concurrency FROM runs ORDER BY id",
                             super::SQL_BUDGET).await.unwrap();
        assert_eq!(out.len(), 1, "one statement in, one result out");
        assert_eq!(out[0].columns, vec!["id", "model", "concurrency"]);
        assert_eq!(table(&out[0].rows),
                   serde_json::json!([[1, "m", null], [2, "m", null]]));
        assert!(!out[0].truncated);
        assert_eq!(out[0].changes, 0, "a read changes nothing");
        assert_eq!(out[0].error, None, "{:?}", out[0].error);

        let out = db.run_sql("UPDATE runs SET model = 'renamed'", super::SQL_BUDGET)
            .await.unwrap();
        assert_eq!(out[0].changes, 2, "sqlite3_changes must reach the caller");
        assert!(out[0].columns.is_empty() && out[0].rows.is_empty(), "a write has no rows");
    }

    /// The whole script goes to SQLite the way the `sqlite3` CLI does it: no hand-rolled
    /// semicolon splitting, and a multi-statement text that `Connection::prepare` would reject
    /// with `MultipleStatement` runs statement by statement.
    #[tokio::test]
    async fn run_sql_executes_a_multi_statement_script_in_order() {
        let db = Db::in_memory();
        let out = db.run_sql("CREATE TABLE t (a INTEGER);
                              INSERT INTO t VALUES (1), (2);
                              SELECT count(*) FROM t;", super::SQL_BUDGET).await.unwrap();
        assert_eq!(out.len(), 3, "every statement gets a result: {out:?}");
        assert!(out.iter().all(|r| r.error.is_none()), "{:?}", out.iter().map(|r| &r.error).collect::<Vec<_>>());
        assert_eq!(out[1].changes, 2);
        assert_eq!(table(&out[2].rows), serde_json::json!([[2]]));
    }

    /// A script stops at its first failure — and the statements after it never run, so a typo
    /// halfway through an UPDATE chain cannot leave the table half-edited in silence.
    #[tokio::test]
    async fn run_sql_stops_at_the_first_error_and_reports_it() {
        let db = Db::in_memory();
        let out = db.run_sql("CREATE TABLE t (a INTEGER);
                              INSERT INTO t VALUES (1);
                              SELECT * FROM nope;
                              INSERT INTO t VALUES (2);", super::SQL_BUDGET).await.unwrap();
        assert_eq!(out.len(), 3, "the fourth statement is never reached");
        let err = out[2].error.as_deref().unwrap_or_default();
        assert!(err.contains("no such table"), "{err}");
        let left = db.run_sql("SELECT count(*) FROM t", super::SQL_BUDGET).await.unwrap();
        assert_eq!(table(&left[0].rows), serde_json::json!([[1]]), "only the first insert ran");
    }

    /// Reads are capped: the toolbox is a console, not a way to stream a whole table through
    /// the dashboard, and `truncated` tells the caller the page was cut.
    #[tokio::test]
    async fn run_sql_caps_a_big_select_and_flags_the_truncation() {
        let db = Db::in_memory();
        let out = db.run_sql("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c \
                              WHERE x < 300) SELECT x FROM c", super::SQL_BUDGET)
            .await.unwrap();
        assert_eq!(out[0].rows.len(), super::SQL_MAX_ROWS, "the cap is the documented page size");
        assert!(out[0].truncated, "and the caller is told the read was cut");
        assert_eq!(out[0].error, None, "truncating is not failing");
    }

    /// The budget: a query that outlives it aborts with SQLite's own interrupt error and the
    /// connection keeps working afterwards — the dashboard must survive its own console.
    #[tokio::test]
    async fn run_sql_aborts_a_query_that_outlives_its_budget_and_keeps_the_connection_usable() {
        let db = Db::in_memory();
        let started = std::time::Instant::now();
        let out = db.run_sql("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) \
                              SELECT count(*) FROM c", std::time::Duration::from_millis(150))
            .await.unwrap();
        let took = started.elapsed();
        let err = out.last().and_then(|r| r.error.clone())
            .expect("the runaway statement must fail, not hang");
        assert!(err.to_lowercase().contains("interrupt"), "{err}");
        // Both bounds matter: it ran for a moment (so the handler was not firing at once) and it
        // stopped on the budget rather than on some limit of its own — left to itself this query
        // never ends, and the test would hang instead of failing.
        assert!(took >= std::time::Duration::from_millis(100), "aborted after {took:?}");
        assert!(took < std::time::Duration::from_secs(3), "aborted after {took:?}");
        // The same connection still answers, and the handler is gone: a later read is not
        // interrupted by a stale deadline.
        let after = db.run_sql("SELECT 1", super::SQL_BUDGET).await.unwrap();
        assert_eq!(table(&after[0].rows), serde_json::json!([[1]]));
        assert_eq!(db.recent_runs(5).await.unwrap().len(), 0, "normal reads still work");
    }

    /// Every SQLite storage class reaches JSON as itself: NULL stays null, integers and reals
    /// stay numbers, text stays text, and a blob is reported as lowercase hex — until it is too
    /// big to be worth shipping, when the console names its size instead.
    #[tokio::test]
    async fn run_sql_maps_every_sqlite_type_to_json() {
        let db = Db::in_memory();
        let out = db.run_sql("SELECT NULL AS n, 42 AS i, 1.5 AS r, 'txt' AS t, x'010a' AS b",
                             super::SQL_BUDGET).await.unwrap();
        assert_eq!(table(&out[0].rows),
                   serde_json::json!([[null, 42, 1.5, "txt", "010a"]]));
        let big = db.run_sql("SELECT zeroblob(300) AS b", super::SQL_BUDGET).await.unwrap();
        assert_eq!(table(&big[0].rows), serde_json::json!([["<blob 300 bytes>"]]));
    }

    /// Whitespace and comments are not statements: an empty script runs nothing and is not an
    /// error, so an accidental Run on an empty box is harmless.
    #[tokio::test]
    async fn run_sql_of_an_empty_or_comment_only_script_runs_nothing() {
        let db = Db::in_memory();
        assert!(db.run_sql("", super::SQL_BUDGET).await.unwrap().is_empty());
        assert!(db.run_sql("  \n -- just a comment\n ", super::SQL_BUDGET).await.unwrap().is_empty());
    }
}
