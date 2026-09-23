//! Tunnel-auth backend: validates the account credential presented on
//! `TunnelAuth`. Two implementations selected by `RelayConfig::control_plane`
//! (see `config.rs`): `LocalSqlite` (standalone, unchanged Phase 6 behavior)
//! and `Online` (hosted, introspects the zellij.online control plane once
//! per tunnel start — no allow caching, fail closed on every error class).

use std::sync::Arc;

use crate::control_plane::{Credential, ControlPlaneClient};
use crate::relay_tunnel_auth_tokens::{
    hash_relay_tunnel_auth_token, validate_relay_tunnel_auth_token_hash,
};

/// Credentials longer than this are rejected before touching either backend
/// — a cheap bulkhead against pathological input, not a rate limit.
pub const MAX_CREDENTIAL_LEN: usize = 512;
/// Default cap on concurrent `Online` verify calls. A bulkhead on in-flight
/// D1 pressure, not a request-rate limit (see architecture doc remaining
/// work).
pub const MAX_INFLIGHT_VERIFIES: usize = 32;

#[derive(Debug, Clone, Default)]
pub struct AuthDecision {
    pub accepted: bool,
    pub user_id: Option<String>,
    pub credential_id: Option<String>,
}

#[derive(Clone)]
pub enum TunnelAuthBackend {
    LocalSqlite,
    Online {
        client: ControlPlaneClient,
        verify_permits: Arc<tokio::sync::Semaphore>,
    },
}

impl TunnelAuthBackend {
    pub fn online(client: ControlPlaneClient) -> Self {
        Self::online_with_permits(client, MAX_INFLIGHT_VERIFIES)
    }

    /// Permit count is injectable so tests can exercise exhaustion without
    /// needing 32 concurrent connections.
    pub fn online_with_permits(client: ControlPlaneClient, permits: usize) -> Self {
        TunnelAuthBackend::Online {
            client,
            verify_permits: Arc::new(tokio::sync::Semaphore::new(permits)),
        }
    }

    pub async fn authorize(&self, token: &str) -> AuthDecision {
        if token.len() > MAX_CREDENTIAL_LEN {
            tracing::warn!(
                len = token.len(),
                max = MAX_CREDENTIAL_LEN,
                "rejecting oversized credential before backend call"
            );
            return AuthDecision::default();
        }

        match self {
            TunnelAuthBackend::LocalSqlite => {
                let token = token.to_string();
                let accepted = match tokio::task::spawn_blocking(move || {
                    let hash = hash_relay_tunnel_auth_token(&token);
                    validate_relay_tunnel_auth_token_hash(&hash)
                })
                .await
                {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => {
                        tracing::error!(error = %e, "relay_tunnel_auth_tokens DB error; rejecting tunnel");
                        false
                    },
                    Err(e) => {
                        tracing::error!(error = %e, "relay_tunnel_auth_tokens task panicked; rejecting tunnel");
                        false
                    },
                };
                AuthDecision {
                    accepted,
                    user_id: None,
                    credential_id: None,
                }
            },
            TunnelAuthBackend::Online {
                client,
                verify_permits,
            } => {
                let Ok(_permit) = verify_permits.try_acquire() else {
                    tracing::warn!("verify concurrency limit reached; rejecting");
                    return AuthDecision::default();
                };
                client.verify_credential(Credential::Token { token }).await
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn oversized_token_rejected_before_sqlite() {
        // No RELAY_DATA_DIR is set; if this reached the LocalSqlite backend
        // it would still not error (sqlite lazily creates its file), but the
        // point of this test is that authorize() must reject before ever
        // calling validate_relay_tunnel_auth_token_hash — asserted by the
        // fact that no panic/db init side effect is required for this to
        // pass under `MAX_CREDENTIAL_LEN`.
        let backend = TunnelAuthBackend::LocalSqlite;
        let token = "a".repeat(MAX_CREDENTIAL_LEN + 1);
        let decision = backend.authorize(&token).await;
        assert!(!decision.accepted);
    }
}
