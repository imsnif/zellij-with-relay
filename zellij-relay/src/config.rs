//! Runtime configuration for the relay. Sourced from environment variables so
//! that a development `cargo run -p zellij-relay` remains zero-setup.

use std::env;

pub const ENV_BIND_ADDR: &str = "RELAY_BIND_ADDR";
pub const ENV_PUBLIC_URL_TEMPLATE: &str = "RELAY_PUBLIC_URL_TEMPLATE";

pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8765";
pub const DEFAULT_PUBLIC_URL_TEMPLATE: &str = "http://localhost:8765/r/{slug}";

#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub bind_addr: String,
    /// Must contain the `{slug}` placeholder.
    pub public_url_template: String,
}

impl RelayConfig {
    pub fn from_env() -> Self {
        Self {
            bind_addr: env::var(ENV_BIND_ADDR).unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string()),
            public_url_template: env::var(ENV_PUBLIC_URL_TEMPLATE)
                .unwrap_or_else(|_| DEFAULT_PUBLIC_URL_TEMPLATE.to_string()),
        }
    }
}

pub fn format_public_url(template: &str, slug: &str) -> String {
    template.replace("{slug}", slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn with_env_unset<F: FnOnce()>(f: F) {
        std::env::remove_var(ENV_BIND_ADDR);
        std::env::remove_var(ENV_PUBLIC_URL_TEMPLATE);
        f();
    }

    #[test]
    #[serial]
    fn defaults_when_env_unset() {
        with_env_unset(|| {
            let cfg = RelayConfig::from_env();
            assert_eq!(cfg.bind_addr, "127.0.0.1:8765");
            assert_eq!(cfg.public_url_template, "http://localhost:8765/r/{slug}");
        });
    }

    #[test]
    #[serial]
    fn env_overrides_defaults() {
        with_env_unset(|| {
            std::env::set_var(ENV_BIND_ADDR, "0.0.0.0:9001");
            std::env::set_var(
                ENV_PUBLIC_URL_TEMPLATE,
                "https://relay.example.com/r/{slug}",
            );
            let cfg = RelayConfig::from_env();
            assert_eq!(cfg.bind_addr, "0.0.0.0:9001");
            assert_eq!(cfg.public_url_template, "https://relay.example.com/r/{slug}");
            std::env::remove_var(ENV_BIND_ADDR);
            std::env::remove_var(ENV_PUBLIC_URL_TEMPLATE);
        });
    }

    #[test]
    fn format_public_url_substitutes_slug() {
        assert_eq!(format_public_url("http://x/r/{slug}", "abc"), "http://x/r/abc");
        assert_eq!(format_public_url("http://x/static", "abc"), "http://x/static");
    }
}
