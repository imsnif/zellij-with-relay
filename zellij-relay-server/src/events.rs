//! Tunnel lifecycle event reporting to the zellij.online control plane.
//! Best-effort: orderly lifecycle paths enqueue paired `tunnel_started` /
//! `tunnel_stopped` events, but a relay crash, queue overflow, or exhausted
//! retries can lose one side. The events endpoint's upsert tolerates
//! reordering and duplicates (idempotent on `tunnel_id`).

use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc;

use crate::control_plane::ControlPlaneClient;
use crate::registry::TunnelEntry;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayEvent {
    TunnelStarted {
        tunnel_id: String,
        user_id: String,
        slug: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        credential_id: Option<String>,
        zellij_version: String,
    },
    TunnelStopped {
        tunnel_id: String,
        user_id: String,
        slug: String,
        reason: String,
    },
}

pub const EVENT_QUEUE_CAPACITY: usize = 256;
pub const STOP_REASON_CONTROL_SOCKET_CLOSED: &str = "control_socket_closed";
pub const STOP_REASON_HEARTBEAT_TIMEOUT: &str = "heartbeat_timeout";
pub const STOP_REASON_ESTABLISHED_SEND_FAILED: &str = "established_send_failed";

#[derive(Clone)]
pub struct EventSenderConfig {
    pub queue_capacity: usize,
    pub request_timeout: Duration,
    pub retry_delays: Vec<Duration>,
}

impl Default for EventSenderConfig {
    fn default() -> Self {
        Self {
            queue_capacity: EVENT_QUEUE_CAPACITY,
            request_timeout: crate::control_plane::EVENTS_TIMEOUT,
            retry_delays: vec![
                Duration::from_secs(1),
                Duration::from_secs(5),
                Duration::from_secs(15),
            ],
        }
    }
}

#[derive(Clone)]
pub enum EventSink {
    Noop,
    Online(mpsc::Sender<RelayEvent>),
}

impl EventSink {
    pub fn spawn_online(client: ControlPlaneClient, cfg: EventSenderConfig) -> EventSink {
        let (tx, mut rx) = mpsc::channel::<RelayEvent>(cfg.queue_capacity);
        let request_timeout = cfg.request_timeout;
        let retry_delays = cfg.retry_delays;
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let mut attempt_result = client.post_event(&event, request_timeout).await;
                for delay in &retry_delays {
                    if attempt_result.is_ok() {
                        break;
                    }
                    tokio::time::sleep(*delay).await;
                    attempt_result = client.post_event(&event, request_timeout).await;
                }
                if let Err(e) = attempt_result {
                    tracing::warn!(?event, error = %e, "giving up on relay event after retries");
                }
            }
        });
        EventSink::Online(tx)
    }

    /// `try_send`; on a full queue, drop the event and log — the documented
    /// "history may gap" contract during outages.
    pub fn emit(&self, event: RelayEvent) {
        match self {
            EventSink::Noop => {},
            EventSink::Online(tx) => {
                if let Err(e) = tx.try_send(event) {
                    tracing::warn!(error = %e, "relay event queue full or closed; dropping event");
                }
            },
        }
    }

    /// No-op if `entry.user_id` is `None` — a standalone-auth tunnel has no
    /// account to attribute the event to.
    pub fn tunnel_started(&self, entry: &TunnelEntry) {
        let Some(user_id) = entry.user_id.clone() else {
            return;
        };
        self.emit(RelayEvent::TunnelStarted {
            tunnel_id: entry.tunnel_id.to_string(),
            user_id,
            slug: entry.slug.clone(),
            credential_id: entry.credential_id.clone(),
            zellij_version: entry.zellij_version.clone(),
        });
    }

    pub fn tunnel_stopped(&self, entry: &TunnelEntry, reason: &str) {
        let Some(user_id) = entry.user_id.clone() else {
            return;
        };
        self.emit(RelayEvent::TunnelStopped {
            tunnel_id: entry.tunnel_id.to_string(),
            user_id,
            slug: entry.slug.clone(),
            reason: reason.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_started_serde_exact() {
        let event = RelayEvent::TunnelStarted {
            tunnel_id: "t1".into(),
            user_id: "u1".into(),
            slug: "abc".into(),
            credential_id: Some("c1".into()),
            zellij_version: "0.45.0".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            r#"{"type":"tunnel_started","tunnel_id":"t1","user_id":"u1","slug":"abc","credential_id":"c1","zellij_version":"0.45.0"}"#
        );
    }

    #[test]
    fn tunnel_started_omits_credential_id_when_none() {
        let event = RelayEvent::TunnelStarted {
            tunnel_id: "t1".into(),
            user_id: "u1".into(),
            slug: "abc".into(),
            credential_id: None,
            zellij_version: "0.45.0".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("credential_id"));
    }

    #[test]
    fn tunnel_stopped_serde_exact() {
        let event = RelayEvent::TunnelStopped {
            tunnel_id: "t1".into(),
            user_id: "u1".into(),
            slug: "abc".into(),
            reason: "control_socket_closed".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            r#"{"type":"tunnel_stopped","tunnel_id":"t1","user_id":"u1","slug":"abc","reason":"control_socket_closed"}"#
        );
    }

    #[tokio::test]
    async fn queue_overflow_drops_without_blocking_or_panicking() {
        let (tx, _rx) = mpsc::channel::<RelayEvent>(1);
        let sink = EventSink::Online(tx);
        // Fill the single-slot queue, then emit again — must not block or panic.
        sink.emit(RelayEvent::TunnelStopped {
            tunnel_id: "t1".into(),
            user_id: "u1".into(),
            slug: "abc".into(),
            reason: "x".into(),
        });
        sink.emit(RelayEvent::TunnelStopped {
            tunnel_id: "t2".into(),
            user_id: "u1".into(),
            slug: "abc".into(),
            reason: "x".into(),
        });
    }

    #[test]
    fn tunnel_started_with_no_user_id_emits_nothing() {
        let (tx, mut rx) = mpsc::channel::<RelayEvent>(4);
        let sink = EventSink::Online(tx);
        let (control_tx, _rx2) = mpsc::unbounded_channel();
        let entry = TunnelEntry {
            tunnel_id: uuid::Uuid::new_v4(),
            slug: "abc".into(),
            public_url: "http://localhost/r/abc".into(),
            session_name: "s".into(),
            zellij_version: "0.45.0".into(),
            read_only: false,
            created_at: std::time::Instant::now(),
            terminal_linked: std::sync::Arc::new(tokio::sync::Notify::new()),
            terminal_linked_flag: std::sync::Arc::new(std::sync::Mutex::new(false)),
            control_tx,
            terminal_tx: std::sync::Mutex::new(None),
            pending_pake_responses: std::sync::Mutex::new(Default::default()),
            pending_pake_results: std::sync::Mutex::new(Default::default()),
            pending_handshakes: std::sync::Mutex::new(Default::default()),
            viewers: std::sync::Mutex::new(Default::default()),
            sessions: std::sync::Mutex::new(Default::default()),
            client_id_to_viewer: std::sync::Mutex::new(Default::default()),
            user_id: None,
            credential_id: None,
            terminal_binding_secret_hash: String::new(),
        };
        sink.tunnel_started(&entry);
        assert!(rx.try_recv().is_err(), "no event should be queued");
    }
}
