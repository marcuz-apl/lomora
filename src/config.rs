use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub lmstudio_url: String,
    pub ollama_url: String,
    pub port: u16,
    pub db_path: PathBuf,
    pub static_dir: Option<PathBuf>,
}

/// Trailing-slash strip, mirroring `os.getenv(...).rstrip("/")` in main.py.
pub fn normalize_base(raw: &str) -> String {
    raw.trim_end_matches('/').to_string()
}

/// `base = (base_url or LMSTUDIO).rstrip("/")` from main.py: an explicit `base_url` query
/// param wins, otherwise the configured default. Python's `or` falls back for `None` and `""`;
/// the `trim()` here additionally falls back for whitespace-only input, which Python would try
/// to use as a base URL and fail on. No client sends `base_url=%20`, so the difference is
/// unreachable in practice and the stricter (never-error) side was chosen.
///
/// The default itself honours `OLLAMA_URL` as a second choice (see `default_base`), which is
/// what makes the port's reading of that variable real instead of Python's dead declaration.
pub fn resolve_base(cfg: &Config, explicit: Option<&str>) -> String {
    normalize_base(explicit.filter(|s| !s.trim().is_empty()).unwrap_or(&cfg.lmstudio_url))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// The server-side default when a request omits `base_url`: an explicit `LMSTUDIO_URL` wins,
/// then an explicit `OLLAMA_URL`, then LM Studio's conventional address. The dashboard always
/// sends an explicit `base_url` (the provider dropdown hardcodes both URLs in the page), so
/// nothing visible changes — but this is the one place Python's unused `OLLAMA_URL`
/// (`main.py:16`, declared and never read) belongs. Pure so the precedence is testable without
/// mutating the process environment (`set_var` is unsafe in edition 2024).
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
            db_path: PathBuf::from(env_or("LOMORA_DB", "lomora.db")),
            static_dir: std::env::var("LOMORA_STATIC").ok()
                .filter(|v| !v.trim().is_empty()).map(PathBuf::from),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, default_base, normalize_base, resolve_base};
    use std::path::PathBuf;

    // `OLLAMA_URL` was dead in Python; this pins the one place it is honoured. `from_env` feeds
    // these same env values into `default_base`, which is the only untestable link (setting the
    // process env from a test is unsafe and would leak across parallel tests).
    #[test]
    fn the_base_url_fallback_prefers_lmstudio_then_ollama_then_the_default() {
        assert_eq!(default_base(Some("http://lm:1234/".into()), Some("http://ol:11434".into())),
                   "http://lm:1234", "an explicit LM Studio URL wins");
        assert_eq!(default_base(None, Some("http://ol:11434/".into())), "http://ol:11434",
                   "Ollama is honoured when LM Studio is not configured");
        assert_eq!(default_base(None, None), "http://localhost:1234",
                   "neither set keeps Python's own default");
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
        }
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
}
