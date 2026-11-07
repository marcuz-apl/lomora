//! The launch switches, parsed before anything else happens.
//!
//! Lomora used to read only the environment and ignore `argv` completely, which meant a mistyped
//! `lomora --hidne` quietly started a server instead of saying anything. Options are therefore
//! refused rather than skipped, and the whole grammar lives here as pure functions so the exit
//! codes and precedence are testable without spawning a process.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action { Run, Help }

/// What the user asked for regarding the terminal this process is attached to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Console {
    /// Close the console when it belongs to Lomora alone; leave a shared terminal untouched.
    Auto,
    /// The user explicitly asked for no window (`--hidden`, `LOMORA_HIDDEN=1`).
    Hidden,
    /// The user explicitly asked to keep the window (`--console`).
    Visible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Launch {
    pub action: Action,
    pub console: Console,
    /// True in the re-exec'd Unix child, which must never daemonize again.
    pub daemonized: bool,
}

pub const USAGE: &str = "\
Lomora - local LLM benchmark dashboard

Usage: lomora [OPTIONS]

Options:
  --hidden    Run without a console window. Windows: a shell launch already hands over to a
              tray-only background process; this asks for it explicitly. Linux and macOS:
              start detached in the background and exit once the dashboard answers; the pid
              goes to lomora.pid beside the database.
  --console   Keep the console window and stay in the foreground, even on double-click
              (Windows only).
  --help      Show this message and exit.

Environment:
  PORT, LOMORA_DB, LMSTUDIO_URL, OLLAMA_URL, LOMORA_PROMPTS_URLS,
  LOMORA_HIDDEN (1/true/on/yes, same as --hidden), LOMORA_SQL (1/true/on/yes).
";

/// `LOMORA_HIDDEN` and `LOMORA_DAEMON` share the one-word grammar of `LOMORA_SQL`: only an
/// explicit yes counts, so a mistyped value cannot change how the process starts.
fn yes(env: Option<&str>) -> bool {
    matches!(env.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
             Some("1" | "true" | "on" | "yes"))
}

/// Parse the argument list plus the two launch variables. `argv` excludes the program name.
///
/// Precedence: the environment sets the starting point, the last option on the command line wins,
/// and a process already running as a daemon (`daemon_env` yes) ignores both because its terminal
/// is already gone.
pub fn parse(argv: &[String], hidden_env: Option<&str>, daemon_env: Option<&str>)
    -> Result<Launch, String>
{
    if yes(daemon_env) {
        return Ok(Launch { action: Action::Run, console: Console::Visible, daemonized: true });
    }
    let mut launch = Launch {
        action: Action::Run,
        console: if yes(hidden_env) { Console::Hidden } else { Console::Auto },
        daemonized: false,
    };
    for arg in argv {
        match arg.as_str() {
            "--hidden" => launch.console = Console::Hidden,
            "--console" => launch.console = Console::Visible,
            "--help" | "-h" => launch.action = Action::Help,
            // Anything else - a typo, a stray path, an option from an older guide - is refused
            // with the usage rather than ignored, because ignoring it means the user gets a
            // server they did not ask for and no explanation of why the flag "did nothing".
            other => return Err(format!("unknown option: {other}\n\n{USAGE}")),
        }
    }
    Ok(launch)
}

/// The real-process entry: `argv` minus the program name, plus the two launch variables.
pub fn from_env() -> Result<Launch, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    parse(&argv, std::env::var("LOMORA_HIDDEN").ok().as_deref(),
          std::env::var("LOMORA_DAEMON").ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::{Action, Console, Launch, parse};

    fn s(args: &[&str]) -> Vec<String> { args.iter().map(|a| a.to_string()).collect() }
    const L: Launch = Launch { action: Action::Run, console: Console::Auto, daemonized: false };

    /// Every test below asks about a launch that parses, so the `Result` is unwrapped once here
    /// and the refusals keep their own explicit `expect_err`.
    fn p(args: &[&str], hidden: Option<&str>, daemon: Option<&str>) -> Launch {
        parse(&s(args), hidden, daemon).expect("these options parse")
    }

    #[test]
    fn no_arguments_and_no_environment_still_means_auto() {
        assert_eq!(p(&[], None, None), Launch { console: Console::Auto, ..L });
    }

    #[test]
    fn the_hidden_switch_and_its_environment_variable_both_ask_for_no_window() {
        assert_eq!(p(&["--hidden"], None, None), Launch { console: Console::Hidden, ..L });
        assert_eq!(p(&[], Some("1"), None), Launch { console: Console::Hidden, ..L });
        assert_eq!(p(&[], Some(" YES "), None), Launch { console: Console::Hidden, ..L },
                   "the same one-word grammar as LOMORA_SQL");
        assert_eq!(p(&[], Some("ture"), None), Launch { console: Console::Auto, ..L },
                   "a mistyped value cannot change how the process starts");
    }

    #[test]
    fn the_last_console_option_on_the_line_wins() {
        assert_eq!(p(&["--hidden", "--console"], None, None),
                   Launch { console: Console::Visible, ..L });
        assert_eq!(p(&["--console", "--hidden"], None, None),
                   Launch { console: Console::Hidden, ..L });
    }

    #[test]
    fn help_is_an_action_not_a_console_choice() {
        assert_eq!(p(&["--help"], None, None), Launch { action: Action::Help, ..L });
        assert_eq!(p(&["-h"], None, None), Launch { action: Action::Help, ..L });
    }

    #[test]
    fn a_process_marked_as_already_daemonized_never_daemonizes_again() {
        // The re-exec'd child carries the same `--hidden` argv it was started with; without this
        // it would spawn another child forever.
        assert_eq!(p(&["--hidden"], None, Some("1")),
                   Launch { action: Action::Run, console: Console::Visible, daemonized: true });
    }

    #[test]
    fn anything_unrecognised_is_refused_with_the_offending_token_quoted() {
        for bad in ["--hidne", "--hidden=true", "-hidden", "data/lomora.db", ""] {
            let err = parse(&s(&[bad]), None, None).expect_err("{bad} must be refused");
            assert!(err.contains(bad) || bad.is_empty(), "the message names {bad:?}: {err}");
            assert!(err.contains("--hidden"), "and shows the usage: {err}");
        }
    }
}
