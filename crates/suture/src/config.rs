use std::net::SocketAddr;

/// Proxy configuration, sourced from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub openai_base: String,
    pub anthropic_base: String,
}

impl Config {
    /// Read from the process environment.
    pub fn from_env() -> Self {
        Self::from_map(|k| std::env::var(k).ok())
    }

    /// Read from an arbitrary key->value lookup (testable).
    pub fn from_map(get: impl Fn(&str) -> Option<String>) -> Self {
        let listen = get("SUTURE_LISTEN")
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| "127.0.0.1:8787".parse().unwrap());
        let trim = |s: String| s.trim_end_matches('/').to_string();
        let openai_base =
            trim(get("SUTURE_OPENAI_BASE").unwrap_or_else(|| "https://api.openai.com".to_string()));
        let anthropic_base = trim(
            get("SUTURE_ANTHROPIC_BASE").unwrap_or_else(|| "https://api.anthropic.com".to_string()),
        );
        Self { listen, openai_base, anthropic_base }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_env_absent() {
        let c = Config::from_map(|_| None);
        assert_eq!(c.openai_base, "https://api.openai.com");
        assert_eq!(c.anthropic_base, "https://api.anthropic.com");
        assert_eq!(c.listen.to_string(), "127.0.0.1:8787");
    }

    #[test]
    fn overrides_from_env() {
        let c = Config::from_map(|k| match k {
            "SUTURE_LISTEN" => Some("0.0.0.0:9000".to_string()),
            "SUTURE_OPENAI_BASE" => Some("http://localhost:1234".to_string()),
            _ => None,
        });
        assert_eq!(c.listen.to_string(), "0.0.0.0:9000");
        assert_eq!(c.openai_base, "http://localhost:1234");
        assert_eq!(c.anthropic_base, "https://api.anthropic.com");
    }

    #[test]
    fn trailing_slash_trimmed() {
        let c = Config::from_map(|k| match k {
            "SUTURE_OPENAI_BASE" => Some("http://x:1/".to_string()),
            _ => None,
        });
        assert_eq!(c.openai_base, "http://x:1");
    }
}
