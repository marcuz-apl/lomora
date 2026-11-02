//! The only module in the crate that knows SQL. `routes.rs` and the benchmark tasks go
//! through `Db`, which owns the schema, the `runs` rows Task 7 reads back oldest-first, and
//! the `profile` singleton row that replaces Python's in-memory `system_profile` dict.
//! One `rusqlite::Connection` behind a `tokio::sync::Mutex` — no connection pool: every
//! statement here is a single row write or a <60-row read, and the mutex is what keeps
//! rusqlite's `!Sync`-by-design `Connection` off the async executor's hands.

use std::path::Path;
use std::sync::Arc;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct RunRow {
    pub kind: String, pub timestamp: String, pub model: String, pub max_tokens: i64,
    pub ttft_ms: Option<f64>, pub total_s: f64, pub tokens: i64, pub tok_per_s: f64,
    pub concurrency: Option<i64>, pub base_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Profile { pub name: String, pub notes: String }

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runs (
  id INTEGER PRIMARY KEY, kind TEXT NOT NULL CHECK (kind IN ('single','stress')),
  created_at TEXT NOT NULL, timestamp TEXT NOT NULL, model TEXT NOT NULL,
  max_tokens INTEGER NOT NULL, ttft_ms REAL, total_s REAL NOT NULL,
  tokens INTEGER NOT NULL, tok_per_s REAL NOT NULL, concurrency INTEGER, base_url TEXT);
CREATE TABLE IF NOT EXISTS profile (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  name TEXT NOT NULL DEFAULT 'My workstation', notes TEXT NOT NULL DEFAULT '');
INSERT OR IGNORE INTO profile (id, name, notes) VALUES (1, 'My workstation', '');";

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Db> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db(Arc::new(Mutex::new(conn))))
    }
    pub fn default_profile() -> Db {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch(SCHEMA).expect("schema");
        Db(Arc::new(Mutex::new(conn)))
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
    /// ORDER BY id DESC LIMIT n, then reversed: Python returns list(deque) oldest-first and
    /// index.html reverses it itself. Spec 5.1.
    pub async fn recent_runs(&self, limit: i64) -> anyhow::Result<Vec<RunRow>> {
        let c = self.0.clone(); let g = c.lock().await;
        let mut stmt = g.prepare("SELECT kind, timestamp, model, max_tokens, ttft_ms, total_s, \
                   tokens, tok_per_s, concurrency, base_url FROM runs ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit], |row| Ok(RunRow {
            kind: row.get(0)?, timestamp: row.get(1)?, model: row.get(2)?, max_tokens: row.get(3)?,
            ttft_ms: row.get(4)?, total_s: row.get(5)?, tokens: row.get(6)?, tok_per_s: row.get(7)?,
            concurrency: row.get(8)?, base_url: row.get(9)?,
        }))?;
        let mut out = rows.collect::<Result<Vec<_>, _>>()?;
        out.reverse();
        Ok(out)
    }
    pub async fn profile(&self) -> Profile {
        let c = self.0.clone(); let g = c.lock().await;
        g.query_row("SELECT name, notes FROM profile WHERE id = 1", [], |r|
            Ok(Profile { name: r.get(0)?, notes: r.get(1)? }))
            .unwrap_or(Profile { name: "My workstation".into(), notes: String::new() })
    }
    pub async fn set_profile(&self, name: Option<&str>, notes: Option<&str>) -> Profile {
        let c = self.0.clone(); let g = c.lock().await;
        // Connection::execute takes &self, so the guard must not be `mut` — `unused_mut`
        // is a rustc warning and the build runs with `-D warnings`.
        if let Some(n) = name { let _ = g.execute("UPDATE profile SET name = ?1 WHERE id = 1", params![n]); }
        if let Some(v) = notes { let _ = g.execute("UPDATE profile SET notes = ?1 WHERE id = 1", params![v]); }
        drop(g);
        self.profile().await
    }
}
