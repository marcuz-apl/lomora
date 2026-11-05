//! What happens to the terminal before the server starts, and the Unix detach that follows from
//! `--hidden`. The policy is a pure function of the requested mode and the environment the process
//! finds itself in, so the decisions are testable on every platform; the Win32 and `libc` calls
//! are thin wrappers around those decisions.
//!
//! The rule behind all of it: a console Lomora owns is Lomora's to hide, and a console it shares
//! with a shell is not. Hiding a shared console would hide the user's own terminal, and detaching
//! from it (`FreeConsole`) leaves the process writing to a handle that no longer exists - a later
//! `println!` then panics. So a shared terminal is left exactly as it is.

use std::path::{Path, PathBuf};

use crate::cli::Console;

/// The outcome of the Windows console policy for one launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WinPlan {
    /// The console exists only for this process: hide it.
    Hide,
    /// Nothing to do - either `--console` was asked for, or there is no console of our own.
    Keep,
    /// A shell shares the window, so `--hidden` declines and says why.
    KeepShared,
}

/// `attached` is the number of processes sharing this console: one is Lomora alone (double-click,
/// a shortcut, Task Scheduler), two or more means a shell is still sitting in that window.
pub fn windows_plan(mode: Console, attached: u32) -> WinPlan {
    match mode {
        Console::Visible => WinPlan::Keep,
        // Zero means there is no console handle at all (a service, or a detached parent), so
        // there is nothing to hide.
        _ if attached == 0 => WinPlan::Keep,
        _ if attached == 1 => WinPlan::Hide,
        // A shell owns that window. `--hidden` says so out loud; the default stays quiet because
        // nothing was asked for.
        Console::Hidden => WinPlan::KeepShared,
        _ => WinPlan::Keep,
    }
}

/// The pid file Lomora writes when it detaches: beside the database, named after it, so two
/// launches over different databases do not overwrite each other's pid.
pub fn pid_file(db_path: &Path) -> PathBuf {
    match db_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join("lomora.pid"),
        _ => PathBuf::from("lomora.pid"),
    }
}

/// Records the daemon's own pid so `kill $(cat data/lomora.pid)` works. Best effort: a stale file
/// after `kill -9` is informational only, and the port check is what actually proves a server up.
pub fn write_pid_file(db_path: &Path) {
    let path = pid_file(db_path);
    // `Db::open` creates this folder, but it runs a moment later; doing it here too keeps the pid
    // file from depending on the server reaching that point.
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::write(&path, format!("{}\n", std::process::id())).is_err() {
        // A permission problem here is one the database will hit too. Nothing to hide, but
        // nothing to stop the daemon for either.
        eprintln!("Lomora: could not write {} (continuing anyway)", path.display());
    }
}

/// Windows: apply the console policy for this launch. Called before the server starts, so the
/// window is already gone by the time the first hardware scan runs.
pub fn apply_console(mode: Console) {
    #[cfg(windows)]
    {
        match windows_plan(mode, win::tenants()) {
            WinPlan::Hide => win::hide(),
            WinPlan::Keep => {}
            WinPlan::KeepShared => println!(
                "Lomora: this console belongs to your shell, so it stays open. Ctrl+C stops Lomora."),
        }
    }
    #[cfg(not(windows))]
    let _ = mode;
}

/// Put a hidden window back, so a startup failure is never invisible. Only does anything when this
/// process actually hid the console; the return value says whether a window was revealed, which is
/// exactly when the caller must keep it open long enough to be read.
pub fn reveal_console() -> bool {
    #[cfg(windows)]
    { win::reveal() }
    #[cfg(not(windows))]
    false
}

/// Hold a revealed console open until the user has seen the error. A window that `reveal_console`
/// brought back would otherwise close with the process, leaving a flash and no message; a terminal
/// that never hid anything keeps its output by itself, so nothing is waited for (and a service
/// with no stdin never blocks).
pub fn wait_for_enter(revealed: bool) {
    #[cfg(windows)]
    if revealed {
        eprintln!("\nPress Enter to close this window.");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
    #[cfg(not(windows))]
    let _ = revealed;
}

#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_RESTORE};

    /// Whether this process hid the console, so `reveal` does not show one the user never saw.
    static HID: AtomicBool = AtomicBool::new(false);

    /// How many processes share our console. The buffer is small on purpose: a console with more
    /// than eight attached processes is already "shared", and the count is the only thing callers
    /// need.
    pub fn tenants() -> u32 {
        let mut pids = [0u32; 8];
        unsafe { GetConsoleProcessList(&mut pids) }
    }

    /// Hide our console window. `ShowWindow` returns the window's *previous* visibility, which
    /// nothing here needs.
    fn hide_window() {
        let hwnd = unsafe { GetConsoleWindow() };
        // Null when the process has no console at all (started as a service, or detached).
        if hwnd.is_invalid() { return; }
        let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
    }

    /// Bring a hidden window back. `SW_RESTORE` rather than `SW_SHOW`: a shortcut or launcher can
    /// create the console already minimized, and `SW_SHOW` would leave it minimized.
    fn show_window() {
        let hwnd = unsafe { GetConsoleWindow() };
        if hwnd.is_invalid() { return; }
        let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
    }

    pub fn hide() {
        HID.store(true, Ordering::SeqCst);
        hide_window();
    }

    /// Returns whether a hidden window was actually brought back.
    pub fn reveal() -> bool {
        if HID.swap(false, Ordering::SeqCst) { show_window(); true } else { false }
    }
}

/// Unix `--hidden`: re-exec this same binary detached, then report from the parent.
///
/// Re-exec rather than `fork`: `main` has not built the tokio runtime yet, but the process already
/// owns a loaded SQLite and a runtime-less thread set, and a forked copy of a process that is about
/// to build threads is how a daemon ends up deadlocked. A fresh `execve` in a new session has no
/// such shared state. The child gets `LOMORA_DAEMON=1` so it cannot daemonize itself again, and
/// null stdio so nothing it prints can outlive the parent that spawned it.
#[cfg(unix)]
pub fn daemonize(port: u16) -> anyhow::Result<std::process::ExitCode> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, ExitCode, Stdio};

    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(std::env::args_os().skip(1))
        .env("LOMORA_DAEMON", "1")
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // `setsid` detaches the child from the terminal's session, so closing the shell or pressing
    // Ctrl+C there cannot reach it. It is async-signal-safe, which is what `pre_exec` requires.
    unsafe {
        cmd.pre_exec(|| if libc::setsid() < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
    }
    let mut child = cmd.spawn()?;

    let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        // Liveness is checked before the port: if something else already answers on it, a child
        // that died immediately must still be reported as a failure, not mistaken for its own
        // server.
        if let Some(status) = child.try_wait()? {
            let why = status.code().map(|c| c.to_string())
                .unwrap_or_else(|| "killed by a signal".to_string());
            eprintln!("Lomora failed to start (exit {why}). Run the same command without --hidden \
                       to see the error.");
            return Ok(ExitCode::FAILURE);
        }
        if std::net::TcpStream::connect(addr).is_ok() {
            println!("Lomora listening on http://0.0.0.0:{port} (pid {}, detached)", child.id());
            return Ok(ExitCode::SUCCESS);
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("Lomora did not start listening on :{port} within 5 s; it has been stopped.");
            return Ok(ExitCode::FAILURE);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::{pid_file, windows_plan, WinPlan};
    use crate::cli::Console;
    use std::path::Path;

    // One attached process means the console was created for us (double-click, a shortcut, Task
    // Scheduler); two or more means a shell is sharing it, and that window is not ours to hide.
    #[test]
    fn a_console_lomora_owns_is_hidden_and_a_shared_one_is_not() {
        assert_eq!(windows_plan(Console::Auto, 1), WinPlan::Hide);
        assert_eq!(windows_plan(Console::Auto, 2), WinPlan::Keep,
                   "hiding a shared console would hide the user's terminal");
        assert_eq!(windows_plan(Console::Hidden, 2), WinPlan::KeepShared,
                   "an explicit --hidden still refuses to steal the shell's window, and says why");
    }

    // `--console` is the escape hatch for anyone who wants the log line and the Ctrl+C.
    #[test]
    fn the_visible_switch_never_hides_and_no_console_has_nothing_to_hide() {
        assert_eq!(windows_plan(Console::Visible, 1), WinPlan::Keep);
        assert_eq!(windows_plan(Console::Hidden, 1), WinPlan::Hide);
        assert_eq!(windows_plan(Console::Auto, 0), WinPlan::Keep,
                   "no console at all (service, detached parent) has nothing to hide");
    }

    #[test]
    fn the_pid_file_lives_beside_the_database() {
        assert_eq!(pid_file(Path::new("data/lomora.db")), Path::new("data/lomora.pid"));
        assert_eq!(pid_file(Path::new("/home/me/.lomora/data/lomora.db")),
                   Path::new("/home/me/.lomora/data/lomora.pid"));
        // A bare filename has no parent directory; the pid file then sits in the working
        // directory, next to where the database will be created.
        assert_eq!(pid_file(Path::new("lomora.db")), Path::new("lomora.pid"));
    }

    // Nothing was hidden in a test process, so there is no window to bring back and nothing to
    // wait for. The second call would hang the suite if the guard were missing.
    #[test]
    fn revealing_without_having_hidden_anything_is_a_no_op() {
        assert!(!super::reveal_console());
        super::wait_for_enter(false);
    }

    // The daemon writes its pid before the server gets as far as `Db::open`, so on a first run
    // the database folder may not exist yet. The pid file must still land, or the documented
    // `kill "$(cat data/lomora.pid)"` has nothing to read.
    #[test]
    fn the_pid_file_is_created_even_before_the_database_folder_exists() {
        let root = std::env::temp_dir().join(format!("lomora-pid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let db = root.join("nested").join("lomora.db");
        super::write_pid_file(&db);
        let text = std::fs::read_to_string(super::pid_file(&db)).expect("the pid file exists");
        assert_eq!(text.trim(), std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&root);
    }
}
