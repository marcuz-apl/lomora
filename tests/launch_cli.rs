// Launch-time behaviour: the argv/env switches that decide what happens to the terminal before
// the server starts. These run the real binary (`CARGO_BIN_EXE_lomora`) rather than the library,
// because the whole point is what a user sees when they type `lomora --help` or double-click it:
// the process exit code, the usage text, and whether a server is actually listening afterwards.
//
// Every test here sets `LOMORA_DB` to a fresh temporary file and `PORT` to a port the test grabbed
// and released, so nothing writes into the repository's own `data/lomora.db` and no test can
// collide with a Lomora a developer is running.
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(8);

/// A free port, grabbed by binding 0 and released by dropping the listener. Racy in principle,
/// in practice good enough for a local test, and a collision shows up as a failed connect rather
/// than a false pass.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    l.local_addr().expect("local addr").port()
}

fn temp_db(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("lomora-launch-{}-{}.db", label, std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

/// Spawn the binary with a private database and port, inheriting nothing else from the caller's
/// environment that would change the outcome.
fn launch(args: &[&str], port: u16, db: &PathBuf) -> Child {
    Command::new(env!("CARGO_BIN_EXE_lomora"))
        .args(args)
        .env("PORT", port.to_string())
        .env("LOMORA_DB", db)
        .env_remove("LOMORA_HIDDEN")
        .env_remove("LOMORA_DAEMON")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary starts")
}

/// Read both pipes to the end. A child that never exits (the bug this file is about: an unknown
/// flag was ignored and the server started anyway) is killed at the deadline so the assertion
/// fails with a message instead of hanging the suite.
struct Drained { status: Option<ExitStatus>, out: String, err: String }

fn drain(mut child: Child, deadline: Duration) -> Drained {
    let start = Instant::now();
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    // Poll for exit while the child runs; the pipes are blocking, so they are only read once the
    // child is gone (a dead child cannot produce more bytes, and a live one must not be waited on).
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                let (mut o, mut e) = (String::new(), String::new());
                if let Some(h) = out.as_mut() { let _ = h.read_to_string(&mut o); }
                if let Some(h) = err.as_mut() { let _ = h.read_to_string(&mut e); }
                return Drained { status: Some(status), out: o, err: e };
            }
            None => {
                if start.elapsed() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Drained { status: None, out: String::new(), err: String::new() };
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

#[test]
fn help_prints_the_usage_and_exits_zero() {
    let db = temp_db("help");
    let r = drain(launch(&["--help"], free_port(), &db), DEADLINE);
    let status = r.status.expect("`--help` must exit rather than start a server");
    assert!(status.success(), "`--help` exits 0, got {status}");
    let text = format!("{}{}", r.out, r.err);
    assert!(text.contains("--hidden"), "the usage names the switches:\n{text}");
    assert!(text.contains("--console"), "the usage names every switch:\n{text}");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn an_unknown_flag_is_refused_before_the_server_starts() {
    let db = temp_db("typo");
    let port = free_port();
    let r = drain(launch(&["--hidne"], port, &db), DEADLINE);
    let status = r.status.expect("a mistyped flag must exit, not start the server");
    assert_eq!(status.code(), Some(2), "unknown options exit 2:\n{}{}", r.out, r.err);
    let text = format!("{}{}", r.out, r.err);
    assert!(text.contains("--hidne"), "the message quotes what it rejected:\n{text}");
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
           "a refused launch leaves the port free");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn a_bare_argument_is_refused_too() {
    let db = temp_db("positional");
    let r = drain(launch(&["data/lomora.db"], free_port(), &db), DEADLINE);
    let status = r.status.expect("a stray argument must exit, not start the server");
    assert_eq!(status.code(), Some(2), "unexpected arguments exit 2:\n{}{}", r.out, r.err);
    let _ = std::fs::remove_file(&db);
}

// The daemon half is Unix-only: on Windows `--hidden` hides a console window, which no test can
// observe from inside the process (verified by hand against the release build instead).
#[cfg(unix)]
#[test]
fn hidden_detaches_a_server_that_keeps_serving_after_the_parent_exits() {
    let db = temp_db("daemon");
    let port = free_port();
    let pid_file = db.parent().expect("db has a parent").join("lomora.pid");
    let _ = std::fs::remove_file(&pid_file);

    let child = launch(&["--hidden"], port, &db);
    // The parent is the process the test spawned: it must report and exit, leaving the daemon
    // running in its own session.
    let r = drain(child, DEADLINE);
    let status = r.status.expect("the launching process exits once the daemon is up");
    assert!(status.success(), "the launcher exits 0:\n{}{}", r.out, r.err);
    assert!(r.out.contains("listening"), "the launcher says what happened:\n{}", r.out);

    let served = std::net::TcpStream::connect(("127.0.0.1", port));
    assert!(served.is_ok(), "the detached server answers on :{port}");
    drop(served);

    let written = std::fs::read_to_string(&pid_file).expect("a pid file records the daemon");
    let pid: u32 = written.trim().parse().expect("the pid file holds a pid");
    // `kill -0` is the portable liveness probe: it signals nothing and fails on an unknown pid
    // (`/proc` exists on Linux but not on macOS, so the filesystem is not the check).
    let alive = Command::new("kill").args(["-0", &pid.to_string()])
        .status().map(|s| s.success()).unwrap_or(false);
    assert!(alive, "the recorded pid {pid} is a live process");

    // Stop it the documented way, then confirm the port went quiet again.
    let killed = Command::new("kill").arg(pid.to_string()).status();
    assert!(killed.map(|s| s.success()).unwrap_or(true), "kill {pid}");
    let mut quiet = false;
    for _ in 0..40 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_err() { quiet = true; break; }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(quiet, "killing the recorded pid stops the server");
    let _ = std::fs::remove_file(&pid_file);
    let _ = std::fs::remove_file(&db);
}
