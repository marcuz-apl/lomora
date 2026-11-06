use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug)]
pub struct Config {
    pub lmstudio_url: String,
    pub ollama_url: String,
    pub port: u16,
    pub db_path: PathBuf,
    pub static_dir: Option<PathBuf>,
    /// `(category, url)` pairs the question fetcher samples from; `LOMORA_PROMPTS_URLS`
    /// overrides the built-in benchmark set (`category=url` pairs, comma-separated).
    pub prompts_sources: Vec<(String, String)>,
    /// `LOMORA_SQL`: the starting state of the SQL console (`POST /api/sql`,
    /// `GET /api/sql/backup`), off unless the environment says yes — see `parse_sql_switch`.
    /// Request handlers read the live `SqlGate` built from this value, so the tray can change
    /// it for the session without touching the environment.
    pub sql_console: bool,
}

/// Trailing-slash strip applied to every configured base URL.
pub fn normalize_base(raw: &str) -> String {
    raw.trim_end_matches('/').to_string()
}

/// An explicit `base_url` query param wins over the configured default. The fallback covers
/// `None` and `""`, and the `trim()` here additionally falls back for whitespace-only input,
/// which would otherwise be used as a base URL and fail upstream. No client sends
/// `base_url=%20`, so the difference is unreachable in practice and the stricter
/// (never-error) side was chosen.
///
/// The default itself honours `OLLAMA_URL` as a second choice (see `default_base`).
pub fn resolve_base(cfg: &Config, explicit: Option<&str>) -> String {
    normalize_base(explicit.filter(|s| !s.trim().is_empty()).unwrap_or(&cfg.lmstudio_url))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// The default database location: `data/lomora.db` relative to the working directory, so run
/// history lives beside the app instead of loose in the repo root. `Db::open` creates the
/// folder when it is missing; `LOMORA_DB` overrides the whole path.
pub fn default_db_path() -> PathBuf {
    PathBuf::from("data").join("lomora.db")
}

/// The three built-in benchmark sources, one per question family. Each URL was fetched and its
/// format confirmed before being baked in (2026-11-03): GSM8K is newline-delimited
/// `{"question", "answer"}`, HumanEval is a gzipped JSONL of `{"prompt", …}`, TruthfulQA's
/// `mc_task.json` is a JSON array of objects with a `question` field.
pub fn default_prompts_sources() -> Vec<(String, String)> {
    vec![
        ("math".into(),
         "https://raw.githubusercontent.com/openai/grade-school-math/master/grade_school_math/data/test.jsonl".into()),
        ("coding".into(),
         "https://raw.githubusercontent.com/openai/human-eval/master/data/HumanEval.jsonl.gz".into()),
        ("knowledge".into(),
         "https://raw.githubusercontent.com/sylinrl/TruthfulQA/main/data/mc_task.json".into()),
    ]
}

/// `LOMORA_PROMPTS_URLS` grammar: `category=url` pairs, comma-separated, whitespace trimmed.
/// Segments without an `=` are skipped, and an env value that parses to nothing falls back to
/// the built-in sources — a mistyped override must not silently disable the fetcher. Pure so it
/// is testable without touching the process environment.
pub fn parse_prompts_sources(env: Option<&str>) -> Vec<(String, String)> {
    let Some(raw) = env.map(str::trim).filter(|v| !v.is_empty()) else {
        return default_prompts_sources();
    };
    let pairs: Vec<(String, String)> = raw.split(',')
        .filter_map(|seg| {
            let seg = seg.trim();
            let (cat, url) = seg.split_once('=')?;
            let (cat, url) = (cat.trim(), url.trim());
            if cat.is_empty() || url.is_empty() { return None; }
            Some((cat.to_string(), url.to_string()))
        })
        .collect();
    if pairs.is_empty() { default_prompts_sources() } else { pairs }
}

/// `LOMORA_SQL` grammar: one word, trimmed and lower-cased, that opens the SQL console only
/// when it says yes. `1`, `true`, `on` and `yes` open it; anything else — including `0`, `""` and
/// a mistyped `ture` — leaves it closed, because the console runs arbitrary row-level SQL and the
/// safe side of an ambiguous value is off. Pure so both sides are testable without touching the process
/// environment.
pub fn parse_sql_switch(env: Option<&str>) -> bool {
    matches!(env.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
             Some("1" | "true" | "on" | "yes"))
}

/// The live state of the `LOMORA_SQL` gate. `Config::sql_console` is only the starting point:
/// the routes read this cell on every request, so the tray menu can open or close the console
/// for the life of the process. Cloning shares the cell, which is how one `SqlGate` reaches both
/// the server state and the tray thread. Its scope is deliberately session-only — the
/// environment remains the source of truth for the next launch.
#[derive(Clone, Debug)]
pub struct SqlGate(Arc<AtomicBool>);

impl SqlGate {
    pub fn new(open: bool) -> SqlGate {
        SqlGate(Arc::new(AtomicBool::new(open)))
    }

    pub fn enabled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn set(&self, open: bool) {
        self.0.store(open, Ordering::Relaxed);
    }
}

/// The server-side default when a request omits `base_url`: an explicit `LMSTUDIO_URL` wins,
/// then an explicit `OLLAMA_URL`, then LM Studio's conventional address. The dashboard always
/// sends an explicit `base_url` (the provider dropdown hardcodes both URLs in the page), so
/// nothing visible changes — but this is where `OLLAMA_URL` is honoured as a fallback. Pure so
/// the precedence is testable without mutating the process environment (`set_var` is unsafe in
/// edition 2024).
fn default_base(lmstudio: Option<String>, ollama: Option<String>) -> String {
    normalize_base(&lmstudio.or(ollama).unwrap_or_else(|| "http://localhost:1234".into()))
}

impl Config {
    pub fn from_env() -> Config {
        let env_set = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
        Config {
            lmstudio_url: default_base(env_set("LMSTUDIO_URL"), env_set("OLLAMA_URL")),
            ollama_url:   normalize_base(&env_or("OLLAMA_URL",   "http://localhost:11434")),
            port: env_or("PORT", "8501").parse().unwrap_or(8501),
            db_path: env_set("LOMORA_DB").map(PathBuf::from).unwrap_or_else(default_db_path),
            static_dir: std::env::var("LOMORA_STATIC").ok()
                .filter(|v| !v.trim().is_empty()).map(PathBuf::from),
            prompts_sources: parse_prompts_sources(
                std::env::var("LOMORA_PROMPTS_URLS").ok().as_deref()),
            sql_console: parse_sql_switch(std::env::var("LOMORA_SQL").ok().as_deref()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, SqlGate, default_base, normalize_base, resolve_base};
    use std::path::PathBuf;

    // This pins the one place `OLLAMA_URL` is honoured. `from_env` feeds
    // these same env values into `default_base`, which is the only untestable link (setting the
    // process env from a test is unsafe and would leak across parallel tests).
    #[test]
    fn the_base_url_fallback_prefers_lmstudio_then_ollama_then_the_default() {
        assert_eq!(default_base(Some("http://lm:1234/".into()), Some("http://ol:11434".into())),
                   "http://lm:1234", "an explicit LM Studio URL wins");
        assert_eq!(default_base(None, Some("http://ol:11434/".into())), "http://ol:11434",
                   "Ollama is honoured when LM Studio is not configured");
        assert_eq!(default_base(None, None), "http://localhost:1234",
                   "neither set keeps the conventional default");
    }

    #[test]
    fn strips_one_or_many_trailing_slashes() {
        assert_eq!(normalize_base("http://x:1/"), "http://x:1");
        assert_eq!(normalize_base("http://x:1///"), "http://x:1");
        assert_eq!(normalize_base("http://x:1"), "http://x:1");
    }

    fn cfg_with(lmstudio_url: &str) -> Config {
        Config {
            lmstudio_url: lmstudio_url.to_string(),
            ollama_url: String::new(),
            port: 0,
            db_path: PathBuf::from("x.db"),
            static_dir: None,
            prompts_sources: Vec::new(),
            sql_console: false,
        }
    }

    // Run history lives under `data/` so the database does not sit loose in the repo root
    // next to Cargo.toml; `LOMORA_DB` still overrides the whole path.
    #[test]
    fn the_default_database_path_is_under_data() {
        assert_eq!(super::default_db_path(), PathBuf::from("data").join("lomora.db"));
    }

    // The env grammar for LOMORA_PROMPTS_URLS: `category=url` pairs, comma-separated; junk
    // segments are skipped; nothing (or nothing valid) falls back to the built-in benchmark set.
    #[test]
    fn prompts_sources_env_grammar() {
        use super::parse_prompts_sources;
        let two = parse_prompts_sources(Some(" math=http://a/1 , coding=http://b/2 "));
        assert_eq!(two, vec![("math".into(), "http://a/1".into()),
                             ("coding".into(), "http://b/2".into())]);
        // An item-level "category" field can override the source category on the wire; the
        // parser only needs to carry the source's fallback.
        let custom = parse_prompts_sources(Some("trivia=http://c/3,broken,=http://d/4"));
        assert_eq!(custom, vec![("trivia".into(), "http://c/3".into())]);
        assert_eq!(parse_prompts_sources(None), super::default_prompts_sources());
        assert_eq!(parse_prompts_sources(Some("  ")), super::default_prompts_sources());
        assert_eq!(parse_prompts_sources(Some("noequals,=x")), super::default_prompts_sources());
    }

    #[test]
    fn explicit_base_url_wins_and_is_stripped() {
        let cfg = cfg_with("http://lm:1234");
        assert_eq!(resolve_base(&cfg, Some("http://ollama:11434/")), "http://ollama:11434");
    }

    #[test]
    fn missing_or_empty_base_url_falls_back_to_lmstudio() {
        let cfg = cfg_with("http://lm:1234/");
        assert_eq!(resolve_base(&cfg, None), "http://lm:1234");
        assert_eq!(resolve_base(&cfg, Some("")), "http://lm:1234");
    }

    /// `LOMORA_SQL` is the gate on the SQL console, so the default side matters more than
    /// the yes side: unset, blank, `0` and unrecognised text all keep it closed, and only an
    /// explicit yes (case- and whitespace-insensitive) opens it.
    /// The environment is only the starting state. The tray opens and closes the same cell the
    /// request handlers read, so a clone is the server's view, not a copy.
    #[test]
    fn the_sql_gate_can_be_opened_and_closed_at_runtime() {
        let gate = SqlGate::new(false);
        assert!(!gate.enabled(), "the environment default is the starting state");
        gate.set(true);
        assert!(gate.enabled(), "the tray can open the console without a restart");
        let server_view = gate.clone();
        server_view.set(false);
        assert!(!gate.enabled(), "a clone shares the cell, so the change reaches every reader");
    }

    #[test]
    fn the_sql_console_stays_off_until_the_env_says_yes() {
        use super::parse_sql_switch;
        assert!(parse_sql_switch(Some("1")), "the documented value opens the console");
        assert!(parse_sql_switch(Some("true")));
        assert!(parse_sql_switch(Some(" TRUE ")), "surrounding space and case are noise");
        assert!(!parse_sql_switch(None), "no env means no console");
        assert!(!parse_sql_switch(Some("")));
        assert!(!parse_sql_switch(Some("0")), "an explicit no stays closed");
        assert!(!parse_sql_switch(Some("yes-ish")), "unrecognised text does not open it");
    }
}
