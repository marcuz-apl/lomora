//! Structural guard for the port's headline hygiene fix: one `reqwest::Client` per process,
//! built in `main.rs` (and in the test harness), never inside a handler. Python constructs a
//! fresh `httpx.AsyncClient` on every request, throwing away the connection pool; a future
//! handler copied from Python would silently reintroduce that if nothing failed.
//!
//! A behavioural test cannot see this — a per-request client still answers every endpoint
//! correctly, only slower — so this reads the crate sources at compile time. It is a
//! deliberate white-box check, narrow enough to be unambiguous: the only allowed construction
//! sites are `main.rs` (the process client) and `bin/mock_upstream.rs` (no client at all).

/// Every module a handler could live in. `include_str!` makes the check part of the build: a
/// renamed file fails to compile here instead of silently escaping the scan.
const MODULES: &[(&str, &str)] = &[
    ("app.rs", include_str!("../src/app.rs")),
    ("assets.rs", include_str!("../src/assets.rs")),
    ("bench.rs", include_str!("../src/bench.rs")),
    ("config.rs", include_str!("../src/config.rs")),
    ("error.rs", include_str!("../src/error.rs")),
    ("hw.rs", include_str!("../src/hw.rs")),
    ("mock.rs", include_str!("../src/mock.rs")),
    ("models.rs", include_str!("../src/models.rs")),
    ("routes.rs", include_str!("../src/routes.rs")),
    ("store.rs", include_str!("../src/store.rs")),
];

#[test]
fn no_module_outside_main_builds_an_http_client() {
    for (name, src) in MODULES {
        for needle in ["Client::new()", "Client::builder()"] {
            assert!(!src.contains(needle),
                "{name} constructs {needle}: use AppState.http, the shared pooled client");
        }
    }
}

#[test]
fn main_builds_exactly_one_shared_pooled_client() {
    let main = include_str!("../src/main.rs");
    assert_eq!(main.matches("reqwest::Client::builder()").count(), 1,
        "main.rs must build exactly one client for the whole process");
    assert!(main.contains("pool_max_idle_per_host"), "the shared client must keep idle connections");
    assert!(!main.contains("reqwest::Client::new()"), "use the builder so the pool and timeout are set");
}
