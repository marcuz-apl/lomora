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
/// param wins, otherwise the configured LM Studio URL is used, and the result is always
/// trailing-slash stripped. Python's `or` falls back for `None` and `""`; the `trim()` here
/// additionally falls back for whitespace-only input, which Python would try to use as a base
/// URL and fail on. No client sends `base_url=%20`, so the difference is unreachable in
/// practice and the stricter (never-error) side was chosen.
pub fn resolve_base(cfg: &Config, explicit: Option<&str>) -> String {
    normalize_base(explicit.filter(|s| !s.trim().is_empty()).unwrap_or(&cfg.lmstudio_url))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

impl Config {
    pub fn from_env() -> Config {
        Config {
            lmstudio_url: normalize_base(&env_or("LMSTUDIO_URL", "http://localhost:1234")),
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
    use super::{Config, normalize_base, resolve_base};
    use std::path::PathBuf;

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
