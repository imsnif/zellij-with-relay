//! HTTP client for the zellij.online control plane (hosted mode).
//!
//! Talks to `POST {base}/api/relay/verify` (credential introspection) and
//! `POST {base}/api/relay/events` (tunnel lifecycle reporting). Every failure
//! mode on `verify_credential` — transport error, timeout, non-2xx, malformed
//! JSON, or a well-formed-but-incomplete `valid:true` response — resolves to
//! a rejected `AuthDecision`. This is the introspect-once-per-tunnel-start,
//! fail-closed, no-allow-caching design (see the architecture doc).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::ControlPlaneConfig;
use crate::events::RelayEvent;
use crate::tunnel_auth::AuthDecision;

pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(5);
pub const EVENTS_TIMEOUT: Duration = Duration::from_secs(10);

/// The credential presented to `/api/relay/verify`. No `Debug` derive — it
/// carries the account token (see the crate-level redaction policy).
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Credential<'a> {
    Token { token: &'a str },
}

#[derive(Serialize)]
struct VerifyRequest<'a> {
    credential: Credential<'a>,
}

#[derive(Debug, Deserialize)]
struct VerifyResponse {
    valid: bool,
    #[serde(rename = "userId")]
    user_id: Option<String>,
    #[serde(rename = "credentialId")]
    credential_id: Option<String>,
}

/// No `Debug` derive (or a manual impl redacting `secret`) — this type holds
/// the relay's control-plane service secret.
#[derive(Clone)]
pub struct ControlPlaneClient {
    http: reqwest::Client,
    base_url: String,
    secret: String,
    verify_timeout: Duration,
}

impl ControlPlaneClient {
    pub fn new(base_url: String, secret: String, verify_timeout: Duration) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
            secret,
            verify_timeout,
        }
    }

    pub fn from_config(cfg: &ControlPlaneConfig) -> Self {
        Self::new(cfg.base_url.clone(), cfg.secret.clone(), VERIFY_TIMEOUT)
    }

    /// `POST {base}/api/relay/verify`. Fails closed on every error class —
    /// transport error, timeout, non-2xx, JSON parse failure (including a
    /// 200 with a malformed body), `{valid:false}`, or `valid:true` missing
    /// `userId` or `credentialId`.
    pub async fn verify_credential(&self, credential: Credential<'_>) -> AuthDecision {
        let url = format!("{}/api/relay/verify", self.base_url);
        let resp = match self
            .http
            .post(&url)
            .bearer_auth(&self.secret)
            .json(&VerifyRequest { credential })
            .timeout(self.verify_timeout)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "control plane verify request failed");
                return AuthDecision::default();
            },
        };

        if !resp.status().is_success() {
            tracing::warn!(status = %resp.status(), "control plane verify returned non-2xx");
            return AuthDecision::default();
        }

        let body: VerifyResponse = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "control plane verify response was not valid JSON");
                return AuthDecision::default();
            },
        };

        if !body.valid {
            return AuthDecision::default();
        }

        match (body.user_id, body.credential_id) {
            (Some(user_id), Some(credential_id)) => AuthDecision {
                accepted: true,
                user_id: Some(user_id),
                credential_id: Some(credential_id),
            },
            _ => {
                tracing::warn!(
                    "control plane verify returned valid:true but missing userId or credentialId"
                );
                AuthDecision::default()
            },
        }
    }

    /// `POST {base}/api/relay/events`. `Err` on transport error or non-2xx —
    /// the caller (the event sender task) retries.
    pub async fn post_event(&self, event: &RelayEvent, timeout: Duration) -> anyhow::Result<()> {
        let url = format!("{}/api/relay/events", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.secret)
            .json(event)
            .timeout(timeout)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("control plane events endpoint returned {}", resp.status());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_envelope_serde_exact() {
        let cred = Credential::Token { token: "abc" };
        let req = VerifyRequest { credential: cred };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"credential":{"type":"token","token":"abc"}}"#);
    }

    #[test]
    fn verify_response_deserializes_valid_with_ids() {
        let body: VerifyResponse =
            serde_json::from_str(r#"{"valid":true,"userId":"u","credentialId":"t"}"#).unwrap();
        assert!(body.valid);
        assert_eq!(body.user_id.as_deref(), Some("u"));
        assert_eq!(body.credential_id.as_deref(), Some("t"));
    }

    #[test]
    fn verify_response_deserializes_invalid() {
        let body: VerifyResponse = serde_json::from_str(r#"{"valid":false}"#).unwrap();
        assert!(!body.valid);
        assert!(body.user_id.is_none());
        assert!(body.credential_id.is_none());
    }

    #[test]
    fn verify_response_ignores_unknown_fields() {
        let body: VerifyResponse = serde_json::from_str(
            r#"{"valid":true,"userId":"u","credentialId":"t","limits":{"x":1}}"#,
        )
        .unwrap();
        assert!(body.valid);
        assert_eq!(body.user_id.as_deref(), Some("u"));
        assert_eq!(body.credential_id.as_deref(), Some("t"));
    }
}
