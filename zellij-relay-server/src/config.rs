//! Runtime configuration for the relay. Sourced from environment variables so
//! that a development `cargo run -p zellij-relay` remains zero-setup.

use std::env;
use std::fs;

use anyhow::{bail, Context, Result};

pub const ENV_BIND_ADDR: &str = "RELAY_BIND_ADDR";
pub const ENV_PUBLIC_URL_TEMPLATE: &str = "RELAY_PUBLIC_URL_TEMPLATE";
pub const ENV_ALLOWED_ORIGINS: &str = "RELAY_ALLOWED_ORIGINS";
pub const ENV_CONTROL_PLANE_URL: &str = "RELAY_CONTROL_PLANE_URL";
pub const ENV_CONTROL_PLANE_SECRET: &str = "RELAY_CONTROL_PLANE_SECRET";
pub const ENV_CONTROL_PLANE_SECRET_FILE: &str = "RELAY_CONTROL_PLANE_SECRET_FILE";

pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8765";
pub const DEFAULT_PUBLIC_URL_TEMPLATE: &str = "https://zellij.online/r/{slug}";
pub const DEFAULT_ALLOWED_ORIGINS: &str = "https://zellij.online";

/// Hosted-mode control plane connection (the zellij.online website). Present
/// iff `RELAY_CONTROL_PLANE_URL` is set — its presence is what selects the
/// `Online` tunnel-auth backend over the standalone `LocalSqlite` one.
#[derive(Clone)]
pub struct ControlPlaneConfig {
    /// Parsed and re-serialized, trailing slash trimmed. No userinfo, query,
    /// or fragment (rejected at parse time — see `from_env`).
    pub base_url: String,
    pub secret: String,
}

// Manual Debug impl: print base_url, redact secret as "<redacted>" — the
// service secret must never reach logs (RelayConfig derives Debug and is
// logged at startup).
impl std::fmt::Debug for ControlPlaneConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlaneConfig")
            .field("base_url", &self.base_url)
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub bind_addr: String,
    /// Must contain the `{slug}` placeholder.
    pub public_url_template: String,
    pub allowed_origins: Vec<String>,
    pub control_plane: Option<ControlPlaneConfig>,
}

impl RelayConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            bind_addr: env::var(ENV_BIND_ADDR).unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string()),
            public_url_template: env::var(ENV_PUBLIC_URL_TEMPLATE)
                .unwrap_or_else(|_| DEFAULT_PUBLIC_URL_TEMPLATE.to_string()),
            allowed_origins: parse_allowed_origins(
                &env::var(ENV_ALLOWED_ORIGINS)
                    .unwrap_or_else(|_| DEFAULT_ALLOWED_ORIGINS.to_string()),
            ),
            control_plane: control_plane_from_env()?,
        })
    }
}

/// Empty-string env values count as unset.
fn non_empty_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|s| !s.is_empty())
}

fn control_plane_from_env() -> Result<Option<ControlPlaneConfig>> {
    let url = non_empty_env(ENV_CONTROL_PLANE_URL);
    let secret_var = non_empty_env(ENV_CONTROL_PLANE_SECRET);
    let secret_file_var = non_empty_env(ENV_CONTROL_PLANE_SECRET_FILE);

    if secret_var.is_some() && secret_file_var.is_some() {
        bail!(
            "set only one of {} and {}",
            ENV_CONTROL_PLANE_SECRET,
            ENV_CONTROL_PLANE_SECRET_FILE
        );
    }

    let secret = match (&secret_var, &secret_file_var) {
        (Some(s), None) => Some(s.clone()),
        (None, Some(path)) => {
            let contents = fs::read_to_string(path)
                .with_context(|| format!("reading {} from {}", ENV_CONTROL_PLANE_SECRET_FILE, path))?;
            let trimmed = contents.trim().to_string();
            if trimmed.is_empty() {
                bail!("{} points to an empty file", ENV_CONTROL_PLANE_SECRET_FILE);
            }
            Some(trimmed)
        },
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!("handled above"),
    };

    match (url, secret) {
        (None, None) => Ok(None),
        (None, Some(_)) => bail!(
            "{} or {} is set but {} is not",
            ENV_CONTROL_PLANE_SECRET,
            ENV_CONTROL_PLANE_SECRET_FILE,
            ENV_CONTROL_PLANE_URL
        ),
        (Some(_), None) => bail!(
            "{} is set but no service secret is configured; set {} or {}",
            ENV_CONTROL_PLANE_URL,
            ENV_CONTROL_PLANE_SECRET,
            ENV_CONTROL_PLANE_SECRET_FILE
        ),
        (Some(raw_url), Some(secret)) => {
            let base_url = parse_control_plane_url(&raw_url)?;
            Ok(Some(ControlPlaneConfig { base_url, secret }))
        },
    }
}

fn parse_control_plane_url(raw: &str) -> Result<String> {
    let parsed =
        url::Url::parse(raw).with_context(|| format!("{} is not a valid URL", ENV_CONTROL_PLANE_URL))?;

    let is_loopback_host = matches!(parsed.host_str(), Some("127.0.0.1") | Some("::1") | Some("localhost"));
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && is_loopback_host) {
        bail!(
            "{} must be https (http is allowed for loopback only)",
            ENV_CONTROL_PLANE_URL
        );
    }

    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        bail!(
            "{} must be a bare base URL (no userinfo, query, or fragment)",
            ENV_CONTROL_PLANE_URL
        );
    }

    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

pub fn parse_allowed_origins(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .collect()
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
        std::env::remove_var(ENV_CONTROL_PLANE_URL);
        std::env::remove_var(ENV_CONTROL_PLANE_SECRET);
        std::env::remove_var(ENV_CONTROL_PLANE_SECRET_FILE);
        f();
    }

    #[test]
    #[serial]
    fn defaults_when_env_unset() {
        with_env_unset(|| {
            let cfg = RelayConfig::from_env().unwrap();
            assert_eq!(cfg.bind_addr, "127.0.0.1:8765");
            assert_eq!(cfg.public_url_template, "https://zellij.online/r/{slug}");
            assert!(cfg.control_plane.is_none());
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
            let cfg = RelayConfig::from_env().unwrap();
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

    #[test]
    #[serial]
    fn hosted_parse_trims_trailing_slash() {
        with_env_unset(|| {
            std::env::set_var(ENV_CONTROL_PLANE_URL, "https://example.com/api/");
            std::env::set_var(ENV_CONTROL_PLANE_SECRET, "s3cret");
            let cfg = RelayConfig::from_env().unwrap();
            let cp = cfg.control_plane.expect("control plane configured");
            assert_eq!(cp.base_url, "https://example.com/api");
            assert_eq!(cp.secret, "s3cret");
        });
    }

    #[test]
    #[serial]
    fn url_without_secret_errs() {
        with_env_unset(|| {
            std::env::set_var(ENV_CONTROL_PLANE_URL, "https://example.com");
            let err = RelayConfig::from_env().unwrap_err();
            assert!(err.to_string().contains("no service secret"));
        });
    }

    #[test]
    #[serial]
    fn secret_without_url_errs() {
        with_env_unset(|| {
            std::env::set_var(ENV_CONTROL_PLANE_SECRET, "s3cret");
            let err = RelayConfig::from_env().unwrap_err();
            assert!(err.to_string().contains(ENV_CONTROL_PLANE_URL));
        });
    }

    #[test]
    #[serial]
    fn both_secrets_errs() {
        with_env_unset(|| {
            std::env::set_var(ENV_CONTROL_PLANE_URL, "https://example.com");
            std::env::set_var(ENV_CONTROL_PLANE_SECRET, "s3cret");
            std::env::set_var(ENV_CONTROL_PLANE_SECRET_FILE, "/tmp/whatever");
            let err = RelayConfig::from_env().unwrap_err();
            assert!(err.to_string().contains("only one of"));
        });
    }

    #[test]
    #[serial]
    fn secret_file_happy_path_trims_newline() {
        with_env_unset(|| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("secret");
            std::fs::write(&path, "s3cret\n").unwrap();
            std::env::set_var(ENV_CONTROL_PLANE_URL, "https://example.com");
            std::env::set_var(ENV_CONTROL_PLANE_SECRET_FILE, path.to_str().unwrap());
            let cfg = RelayConfig::from_env().unwrap();
            assert_eq!(cfg.control_plane.unwrap().secret, "s3cret");
        });
    }

    #[test]
    #[serial]
    fn secret_file_empty_errs() {
        with_env_unset(|| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("secret");
            std::fs::write(&path, "   \n").unwrap();
            std::env::set_var(ENV_CONTROL_PLANE_URL, "https://example.com");
            std::env::set_var(ENV_CONTROL_PLANE_SECRET_FILE, path.to_str().unwrap());
            let err = RelayConfig::from_env().unwrap_err();
            assert!(err.to_string().contains("empty file"));
        });
    }

    #[test]
    #[serial]
    fn invalid_url_errs() {
        with_env_unset(|| {
            std::env::set_var(ENV_CONTROL_PLANE_URL, "not a url");
            std::env::set_var(ENV_CONTROL_PLANE_SECRET, "s3cret");
            let err = RelayConfig::from_env().unwrap_err();
            assert!(err.to_string().contains("not a valid URL"));
        });
    }

    #[test]
    #[serial]
    fn plain_http_non_loopback_errs() {
        with_env_unset(|| {
            std::env::set_var(ENV_CONTROL_PLANE_URL, "http://example.com");
            std::env::set_var(ENV_CONTROL_PLANE_SECRET, "s3cret");
            let err = RelayConfig::from_env().unwrap_err();
            assert!(err.to_string().contains("must be https"));
        });
    }

    #[test]
    #[serial]
    fn plain_http_loopback_accepted() {
        with_env_unset(|| {
            for host in ["http://127.0.0.1:8787", "http://localhost:8787"] {
                std::env::set_var(ENV_CONTROL_PLANE_URL, host);
                std::env::set_var(ENV_CONTROL_PLANE_SECRET, "s3cret");
                let cfg = RelayConfig::from_env().unwrap();
                assert!(cfg.control_plane.is_some(), "expected {host} to be accepted");
            }
        });
    }

    #[test]
    #[serial]
    fn path_prefix_accepted() {
        with_env_unset(|| {
            std::env::set_var(ENV_CONTROL_PLANE_URL, "https://example.com/relay-api");
            std::env::set_var(ENV_CONTROL_PLANE_SECRET, "s3cret");
            let cfg = RelayConfig::from_env().unwrap();
            assert_eq!(cfg.control_plane.unwrap().base_url, "https://example.com/relay-api");
        });
    }

    #[test]
    #[serial]
    fn userinfo_query_fragment_each_rejected() {
        with_env_unset(|| {
            for bad_url in [
                "https://u:p@example.com/",
                "https://example.com/?x=1",
                "https://example.com/#frag",
            ] {
                std::env::set_var(ENV_CONTROL_PLANE_URL, bad_url);
                std::env::set_var(ENV_CONTROL_PLANE_SECRET, "s3cret");
                let err = RelayConfig::from_env().unwrap_err();
                assert!(
                    err.to_string().contains("bare base URL"),
                    "expected {bad_url} to be rejected, got: {err}"
                );
            }
        });
    }

    #[test]
    #[serial]
    fn debug_output_does_not_contain_secret() {
        with_env_unset(|| {
            std::env::set_var(ENV_CONTROL_PLANE_URL, "https://example.com");
            std::env::set_var(ENV_CONTROL_PLANE_SECRET, "SUPERSECRET");
            let cfg = RelayConfig::from_env().unwrap();
            let debug = format!("{:?}", cfg);
            assert!(!debug.contains("SUPERSECRET"), "debug leaked secret: {debug}");
            assert!(debug.contains("<redacted>"), "debug missing redaction marker: {debug}");
        });
    }
}
