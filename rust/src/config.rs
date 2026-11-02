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
    use super::normalize_base;
    #[test]
    fn strips_one_or_many_trailing_slashes() {
        assert_eq!(normalize_base("http://x:1/"), "http://x:1");
        assert_eq!(normalize_base("http://x:1///"), "http://x:1");
        assert_eq!(normalize_base("http://x:1"), "http://x:1");
    }
}
