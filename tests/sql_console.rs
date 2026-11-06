mod support;
use support::{spawn_cfg_on_db, spawn_cfg_on_db_with_state};
use serde_json::{json, Value};

/// A `Config` with the console switch set explicitly. `Config::from_env()` would honour
/// `LOMORA_SQL`, but a test may not write the process environment (`set_var` is unsafe in
/// edition 2024 precisely because it leaks into every other test in this binary), so the switch
/// is set on the built value instead — the same field `from_env` writes.
fn cfg(sql_console: bool) -> lomora::config::Config {
    let mut c = lomora::config::Config::from_env();
    c.sql_console = sql_console;
    c
}

/// The gate, proven where it matters: with `LOMORA_SQL` unset a `POST /api/sql` is refused
/// before its text is looked at, and the statement it carried never reaches the database. The
/// row count below is checked on the same handle the app is serving, so a refusal that ran the
/// SQL anyway cannot pass.
#[tokio::test]
async fn a_server_without_the_switch_refuses_and_executes_nothing() {
    let db = lomora::store::Db::in_memory();
    let (base, _h) = spawn_cfg_on_db(cfg(false), db.clone()).await;
    let c = reqwest::Client::new();

    let r = c.post(format!("{base}/api/sql")).json(&json!({"sql": "CREATE TABLE evil (a)"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200, "a refusal is the runtime-failure envelope");
    let body: Value = r.json().await.unwrap();
    assert!(body["error"].is_string(), "the refusal must name itself, got {body}");
    let evil = db.run_sql("SELECT count(*) FROM sqlite_master WHERE name = 'evil'",
                          lomora::store::SQL_BUDGET).await.unwrap();
    assert_eq!(evil[0].rows[0][0], json!(0), "a refused statement must not have run");

    let r = c.get(format!("{base}/api/sql/backup")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["error"].is_string(), "the backup is behind the same switch, got {body}");

    // The refusal is not a 404 either: the routes exist, so the dashboard can tell "the console
    // is off" from "this build has no console".
    let r = c.post(format!("{base}/api/sql")).json(&json!({"sql": "SELECT 1"}))
        .send().await.unwrap();
    assert_ne!(r.status(), 404, "a disabled console refuses, it does not hide the route");
}

/// The gate is not frozen at startup: the tray menu writes the same cell the handlers read, so
/// the console opens and closes without a restart. Both directions are proven against the live
/// server and the store handle it is serving from — an open gate that still refuses, or a closed
/// gate that runs the statement anyway, fails here.
#[tokio::test]
async fn the_sql_gate_can_be_flipped_at_runtime_without_a_restart() {
    let db = lomora::store::Db::in_memory();
    let (base, _h, state) = spawn_cfg_on_db_with_state(cfg(false), db.clone()).await;
    let c = reqwest::Client::new();

    let body: Value = c.post(format!("{base}/api/sql"))
        .json(&json!({"sql": "CREATE TABLE tray_opened (a)"}))
        .send().await.unwrap().json().await.unwrap();
    assert!(body["error"].is_string(), "closed at startup, got {body}");

    // Exactly what the tray's Settings item does.
    state.sql.set(true);
    let body: Value = c.post(format!("{base}/api/sql"))
        .json(&json!({"sql": "CREATE TABLE tray_opened (a)"}))
        .send().await.unwrap().json().await.unwrap();
    assert!(body["statements"][0]["error"].is_null(), "opened by the tray, got {body}");
    assert_eq!(table_count(&db, "tray_opened").await, 1, "an opened console runs the script");

    state.sql.set(false);
    let body: Value = c.post(format!("{base}/api/sql"))
        .json(&json!({"sql": "CREATE TABLE tray_closed (a)"}))
        .send().await.unwrap().json().await.unwrap();
    assert!(body["error"].is_string(), "closed again, got {body}");
    assert_eq!(table_count(&db, "tray_closed").await, 0, "a closed console runs nothing");
}

async fn table_count(db: &lomora::store::Db, name: &str) -> i64 {
    let result = db.run_sql(
        &format!("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='{name}'"),
        lomora::store::SQL_BUDGET,
    ).await.unwrap();
    result[0].rows[0][0].as_i64().unwrap()
}

/// The console runs a whole script and answers one result per statement, in order: DDL, then a
/// write with its `changes`, then a read with its `columns` and `rows`.
#[tokio::test]
async fn an_enabled_console_answers_one_result_per_statement() {
    let db = lomora::store::Db::in_memory();
    let (base, _h) = spawn_cfg_on_db(cfg(true), db).await;
    let r = reqwest::Client::new().post(format!("{base}/api/sql"))
        .json(&json!({"sql": "CREATE TABLE t (a INTEGER);
                             INSERT INTO t VALUES (7), (8);
                             SELECT a FROM t ORDER BY a"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let statements = body["statements"].as_array().expect("statements must be an array");
    assert_eq!(statements.len(), 3, "{body}");
    assert_eq!(statements[0].as_object().unwrap().keys().collect::<Vec<_>>(),
               vec!["sql", "columns", "rows", "truncated", "changes", "error"],
               "every statement result carries these keys in this order");
    assert_eq!(statements[1]["changes"], json!(2));
    assert_eq!(statements[2]["columns"], json!(["a"]));
    assert_eq!(statements[2]["rows"], json!([[7], [8]]));
    assert!(statements.iter().all(|s| s["error"].is_null()), "{body}");
}

/// The console may edit data but not destroy schema. SQLite's authorizer refuses these at
/// prepare time, so quoting or comments cannot dodge the check, and nothing runs: the table,
/// index, view, trigger, column and row below must all survive. Every `DROP`, every `ALTER TABLE`
/// (add, drop column, rename) is refused. ATTACH is refused too — the console edits Lomora's
/// database, not an arbitrary file — and `writable_schema` would bypass every table-level check
/// if it were allowed.
#[tokio::test]
async fn the_console_refuses_schema_destructive_statements() {
    let db = lomora::store::Db::in_memory();
    let (base, _h) = spawn_cfg_on_db(cfg(true), db).await;
    async fn sql(base: &str, script: &str) -> Value {
        reqwest::Client::new().post(format!("{base}/api/sql"))
            .json(&json!({"sql": script})).send().await.unwrap().json().await.unwrap()
    }
    // `t2` has a spare column so `DROP COLUMN` is a *valid* ALTER: on a one-column table SQLite
    // refuses it by itself before the authorizer is ever asked, which would prove nothing.
    let seeded = sql(&base, "CREATE TABLE t (a INTEGER);
                             CREATE INDEX t_a ON t (a);
                             CREATE VIEW v AS SELECT a FROM t;
                             CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a = a; END;
                             INSERT INTO t VALUES (1);
                             CREATE TABLE t2 (a INTEGER, b INTEGER);
                             INSERT INTO t2 VALUES (1, 2)").await;
    assert!(seeded["statements"].as_array().unwrap().iter()
                .all(|s| s["error"].is_null()), "{seeded}");

    let attach_target = std::env::temp_dir()
        .join(format!("lomora-guard-attach-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&attach_target);
    let attach = format!("ATTACH DATABASE '{}' AS extra", attach_target.display());
    for script in [
        "DROP TABLE t",
        "ALTER TABLE t ADD COLUMN b INTEGER",
        "ALTER TABLE t2 DROP COLUMN b",
        "ALTER TABLE t2 RENAME TO t3",
        "DROP INDEX t_a",
        "DROP VIEW v",
        "DROP TRIGGER tr",
        attach.as_str(),
        "PRAGMA writable_schema = ON",
    ] {
        let body = sql(&base, script).await;
        let err = body["statements"][0]["error"].as_str().unwrap_or_default();
        assert!(err.to_ascii_lowercase().contains("not authori"),
                "{script} must be refused, got {body}");
    }
    assert!(!attach_target.exists(), "a refused ATTACH must not create the file");

    let survived = sql(&base, "SELECT count(*) FROM sqlite_master
                               WHERE name IN ('t','t_a','v','tr')").await;
    assert_eq!(survived["statements"][0]["rows"], json!([[4]]),
               "refused statements must change nothing: {survived}");
    let rows = sql(&base, "SELECT count(*) FROM t").await;
    assert_eq!(rows["statements"][0]["rows"], json!([[1]]), "the row must survive: {rows}");
    let columns = sql(&base, "SELECT name FROM pragma_table_info('t2') ORDER BY cid").await;
    assert_eq!(columns["statements"][0]["rows"], json!([["a"], ["b"]]),
               "DROP COLUMN must leave the column in place: {columns}");
    let readable = sql(&base, "PRAGMA writable_schema").await;
    assert_eq!(readable["statements"][0]["rows"], json!([[0]]),
               "reading the pragma is not a schema change: {readable}");
}

/// The guard stops schema destruction, not the toolbox's job: reading, inserting, updating and
/// deleting rows — and creating an index — all run as typed.
#[tokio::test]
async fn ordinary_crud_still_runs_with_the_guard_on() {
    let db = lomora::store::Db::in_memory();
    let (base, _h) = spawn_cfg_on_db(cfg(true), db).await;
    let body: Value = reqwest::Client::new().post(format!("{base}/api/sql"))
        .json(&json!({"sql": "CREATE TABLE w (a INTEGER, b TEXT);
                             INSERT INTO w VALUES (1,'x');
                             UPDATE w SET b = 'y' WHERE a = 1;
                             DELETE FROM w WHERE a = 1;
                             INSERT INTO w VALUES (2,'z');
                             CREATE INDEX w_a ON w (a);
                             SELECT a, b FROM w"}))
        .send().await.unwrap().json().await.unwrap();
    let statements = body["statements"].as_array().unwrap();
    assert!(statements.iter().all(|s| s["error"].is_null()), "{body}");
    assert_eq!(statements[1]["changes"], json!(1));
    assert_eq!(statements[2]["changes"], json!(1));
    assert_eq!(statements[3]["changes"], json!(1));
    assert_eq!(statements[6]["rows"], json!([[2, "z"]]), "{body}");
}

/// A statement error is the console's output, not a transport failure: HTTP 200 with the SQLite
/// message inside the statement result.
#[tokio::test]
async fn a_statement_error_rides_inside_the_result_not_the_status_line() {
    let db = lomora::store::Db::in_memory();
    let (base, _h) = spawn_cfg_on_db(cfg(true), db).await;
    let r = reqwest::Client::new().post(format!("{base}/api/sql"))
        .json(&json!({"sql": "SELECT * FROM nope"})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let err = body["statements"][0]["error"].as_str().unwrap_or_default();
    assert!(err.contains("no such table"), "{body}");
}

/// The body is validated like every other JSON POST in this API: a wrong type is the 422
/// `{"detail": [… {"type": "string_type"}]}` envelope, and it is answered before the console
/// runs anything.
#[tokio::test]
async fn a_malformed_sql_body_is_the_shared_422_envelope() {
    let db = lomora::store::Db::in_memory();
    let (base, _h) = spawn_cfg_on_db(cfg(true), db).await;
    let c = reqwest::Client::new();
    let r = c.post(format!("{base}/api/sql")).json(&json!({"sql": 5})).send().await.unwrap();
    assert_eq!(r.status(), 422);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["detail"][0]["type"], "string_type", "{body}");
    let r = c.post(format!("{base}/api/sql")).json(&json!({})).send().await.unwrap();
    assert_eq!(r.status(), 422, "the script field is required");
}

/// The backup is the database as a file: `GET /api/sql/backup` hands back a compacted copy of
/// exactly what is stored, as an attachment. Opening the bytes in a second process must show the
/// rows the running app wrote — which is the whole point of offering it beside the console.
#[tokio::test]
async fn the_backup_endpoint_streams_the_database_as_a_file() {
    let db = lomora::store::Db::in_memory();
    let (base, _h) = spawn_cfg_on_db(cfg(true), db).await;
    let row = json!({"sql": "INSERT INTO runs (kind, created_at, timestamp, model, max_tokens, \
                             total_s, tokens, tok_per_s) VALUES \
                             ('single','2026-11-05T09:41:03','2026-11-05 09:41:03','m',8,1.0,8,8.0)"});
    let r = reqwest::Client::new().post(format!("{base}/api/sql")).json(&row)
        .send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["statements"][0]["changes"], json!(1));

    let r = reqwest::Client::new().get(format!("{base}/api/sql/backup")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "application/octet-stream");
    let disposition = r.headers()["content-disposition"].to_str().unwrap().to_string();
    assert!(disposition.starts_with("attachment;"), "{disposition}");
    assert!(disposition.contains(".db"), "the download needs a name SQLite tools accept: {disposition}");
    let bytes = r.bytes().await.unwrap();
    assert_eq!(&bytes[..16], b"SQLite format 3\0", "the body must be a real database file");

    // A second handle over the same bytes sees the row the app wrote, and the schema.
    let file = std::env::temp_dir()
        .join(format!("lomora-backup-{}-{}.db", std::process::id(), bytes.len()));
    std::fs::write(&file, &bytes).unwrap();
    let conn = rusqlite::Connection::open(&file).unwrap();
    let runs: i64 = conn.query_row("SELECT count(*) FROM runs", [], |r| r.get(0)).unwrap();
    assert_eq!(runs, 1, "the backup must carry the live rows");
    let tables: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'runs%' ORDER BY name")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    };
    assert_eq!(tables, vec!["runs".to_string()]);
    drop(conn);
    std::fs::remove_file(&file).ok();
}

/// Nothing the console does is limited to the two tables Lomora owns: the operator asked for raw
/// SQL, so a script that creates a table and rows is answered the same way. This is also the
/// proof that the read is capped at `SQL_MAX_ROWS` through the endpoint.
#[tokio::test]
async fn the_console_caps_a_big_read_at_its_page_size() {
    let db = lomora::store::Db::in_memory();
    let (base, _h) = spawn_cfg_on_db(cfg(true), db).await;
    let r = reqwest::Client::new().post(format!("{base}/api/sql"))
        .json(&json!({"sql": "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c \
                              WHERE x < 300) SELECT x FROM c"}))
        .send().await.unwrap();
    let body: Value = r.json().await.unwrap();
    let rows = body["statements"][0]["rows"].as_array().expect("rows must be an array");
    assert_eq!(rows.len(), lomora::store::SQL_MAX_ROWS);
    assert_eq!(body["statements"][0]["truncated"], json!(true), "{body}");
}
