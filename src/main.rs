use std::process::ExitCode;

use lomora::cli::Action;
#[cfg(unix)]
use lomora::cli::Console;
use lomora::config::Config;
use tokio::net::TcpListener;

/// Everything that must happen before a thread exists runs in this sync part: options, the console
/// decision and the detach. Only `run` hands over to the tokio runtime.
fn main() -> ExitCode {
    // Parsed first, and refused hard: an unknown option must never quietly start a server.
    let launch = match lomora::cli::from_env() {
        Ok(l) => l,
        Err(msg) => { eprintln!("{msg}"); return ExitCode::from(2); }
    };
    if launch.action == Action::Help {
        print!("{}", lomora::cli::USAGE);
        return ExitCode::SUCCESS;
    }

    let cfg = Config::from_env();

    // `--hidden` on Unix means "go away and keep working": the launcher re-execs a detached copy
    // of this binary, waits for the port to answer, reports, and exits. The child arrives back here
    // with `LOMORA_DAEMON=1` and skips the whole branch.
    #[cfg(unix)]
    if launch.console == Console::Hidden && !launch.daemonized {
        return match lomora::console::daemonize(cfg.port) {
            Ok(code) => code,
            Err(e) => { eprintln!("Error: {e:?}"); ExitCode::FAILURE }
        };
    }

    // Windows launches from a shell take the same route, `--console` being the one exception. The
    // console belongs to the shell, so the choice is either to pin the server to a window whose
    // only exit kills it, or to hand over to a console-less child that the tray icon can reach.
    // This is the second one.
    #[cfg(windows)]
    if !launch.daemonized && lomora::console::detach_needed(launch.console) {
        return match lomora::console::detach(cfg.port) {
            Ok(code) => code,
            Err(e) => { eprintln!("Error: {e:?}"); ExitCode::FAILURE }
        };
    }

    // Windows: close the console when it belongs to this process alone, so the app has no taskbar
    // window and lives only in the tray. Doing it before the hardware sampler starts is what keeps
    // the flash to the window-creation time. `hidden` also decides whether the server gets a tray
    // icon - the same launch has no other way back. A detached child has no console at all, so the
    // tray is its only surface too; without this the server would run with no way to reach it.
    #[cfg(windows)]
    let hidden = lomora::console::apply_console(launch.console) || launch.daemonized;
    #[cfg(not(windows))]
    let hidden = lomora::console::apply_console(launch.console);
    if launch.daemonized { lomora::console::write_pid_file(&cfg.db_path); }

    match run(cfg, hidden) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // A failure nobody can see is worse than the window this stream exists to hide. With
            // the console closed there is nothing left to print to, so `report_failure` puts the
            // error in a message box; every other launch still gets the stderr line.
            lomora::console::report_failure(&format!("{e:?}"));
            ExitCode::FAILURE
        }
    }
}

/// The runtime is built here rather than by `#[tokio::main]` because the console and daemon
/// decisions above must run while the process is still single-threaded.
fn run(cfg: Config, hidden: bool) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(serve(cfg, hidden))
}

async fn serve(cfg: Config, hidden: bool) -> anyhow::Result<()> {
    let port = cfg.port;
    // GPU index 0 only; NVML absent -> NoGpu, which answers with the GPU keys missing rather
    // than with nulls. The handle re-checks for NVML on a slow cadence, so a driver installed
    // while the dashboard is open is picked up without restarting the process.
    let gpu: std::sync::Arc<dyn lomora::hw::Gpu> = match lomora::hw::NvmlGpu::open(0) {
        Some(g) => std::sync::Arc::new(g),
        None => std::sync::Arc::new(lomora::hw::NoGpu),
    };
    let gpu = lomora::hw::GpuHandle::rechecking(gpu);
    // Same handle for the sampler and `GET /api/profile`: one NVML context per process.
    let hw = lomora::hw::Sampler::spawn(gpu.clone(), std::time::Duration::from_millis(500)).await;
    // One client for the whole process, so the connection pool is reused. The 10 s model-list
    // and streaming benchmark overrides live at the call sites; this ceiling is the long-run
    // upper bound.
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(300))
        .pool_max_idle_per_host(64).build().unwrap();
    // SQLite keeps the profile row across restarts, where an in-memory profile would reset to
    // defaults. `Db::open` must read `cfg.db_path` before `cfg` moves into `AppState::new`.
    let db = lomora::store::Db::open(&cfg.db_path)?;
    // Profile facts (CPU brand, core counts, OS string) are read once here, never per
    // request.
    let sys = lomora::hw::read_profile_static();
    let state = lomora::app::AppState::new(cfg, hw, http, db, sys, gpu);
    // The tray's Settings item writes the same gate the request handlers read.
    let sql = state.sql.clone();
    let router = lomora::app::build_router(state);
    let addr = format!("0.0.0.0:{port}");
    // Bound first: a tray icon pointing at a port nobody serves would be worse than no icon.
    let listener = TcpListener::bind(&addr).await?;
    // After the console is closed there is no stdout to write to, and a `println!` against the
    // stale handle would panic. The tray is the status surface for that launch.
    if !hidden { println!("Lomora listening on http://{addr}"); }
    // The tray exists only where the console was closed. Its Quit stops the server through the
    // channel, so `with_graceful_shutdown` lets in-flight requests (and SQLite writes) finish
    // instead of cutting the process off mid-write.
    let quit = lomora::tray::start(hidden, port, sql);
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            match quit {
                Some(receiver) => { let _ = receiver.await; }
                None => std::future::pending::<()>().await,
            }
        })
        .await?;
    Ok(())
}
