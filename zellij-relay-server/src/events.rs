//! Tunnel lifecycle event reporting to the zellij.online control plane.
//! Best-effort: orderly lifecycle paths enqueue paired `tunnel_started` /
//! `tunnel_stopped` events, but a relay crash, queue overflow, or exhausted
//! retries can lose one side. The events endpoint's upsert tolerates
//! reordering and duplicates (idempotent on `tunnel_id`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::control_plane::{ControlPlaneClient, PostEventError};
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
    ViewerCount {
        tunnel_id: String,
        count: u64,
        seq: u64,
    },
}

pub enum QueuedEvent {
    Ready(RelayEvent),
    ViewerCount(Arc<ViewerCountReporter>),
    Flush(oneshot::Sender<()>),
}

pub const EVENT_QUEUE_CAPACITY: usize = 256;
pub const STOP_REASON_CONTROL_SOCKET_CLOSED: &str = "control_socket_closed";
pub const STOP_REASON_RELAY_SHUTDOWN: &str = "relay_shutdown";
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
    Online(mpsc::Sender<QueuedEvent>),
}

async fn deliver(
    client: &ControlPlaneClient,
    event: &RelayEvent,
    request_timeout: Duration,
    retry_delays: &[Duration],
) {
    let mut outcome = client.post_event(event, request_timeout).await;
    for delay in retry_delays {
        if !matches!(outcome, Err(PostEventError::Transient(_))) {
            break;
        }
        tokio::time::sleep(*delay).await;
        outcome = client.post_event(event, request_timeout).await;
    }
    match outcome {
        Ok(()) => {
            log::info!("relay-debug: relay event delivered: {:?}", event);
        },
        Err(PostEventError::Permanent(e)) => {
            tracing::warn!(?event, error = %e, "relay event rejected by control plane; not retrying");
        },
        Err(PostEventError::Transient(e)) => {
            tracing::warn!(?event, error = %e, "giving up on relay event after retries");
        },
    }
}

impl EventSink {
    pub fn spawn_online(client: ControlPlaneClient, cfg: EventSenderConfig) -> EventSink {
        let (tx, mut rx) = mpsc::channel::<QueuedEvent>(cfg.queue_capacity);
        let request_timeout = cfg.request_timeout;
        let retry_delays = cfg.retry_delays;
        tokio::spawn(async move {
            while let Some(queued) = rx.recv().await {
                let event = match queued {
                    QueuedEvent::Ready(event) => event,
                    QueuedEvent::ViewerCount(reporter) => match reporter.take_report() {
                        Some(event) => event,
                        None => continue,
                    },
                    QueuedEvent::Flush(ack) => {
                        let _ = ack.send(());
                        continue;
                    },
                };
                deliver(&client, &event, request_timeout, &retry_delays).await;
            }
        });
        EventSink::Online(tx)
    }

    pub async fn flush(&self, budget: Duration) -> bool {
        let EventSink::Online(tx) = self else {
            return true;
        };
        let (ack_tx, ack_rx) = oneshot::channel();
        let wait = async {
            tx.send(QueuedEvent::Flush(ack_tx)).await.ok()?;
            ack_rx.await.ok()
        };
        matches!(tokio::time::timeout(budget, wait).await, Ok(Some(())))
    }

    fn try_enqueue(&self, queued: QueuedEvent) -> bool {
        match self {
            EventSink::Noop => false,
            EventSink::Online(tx) => match tx.try_send(queued) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(error = %e, "relay event queue full or closed; dropping event");
                    false
                },
            },
        }
    }

    /// `try_send`; on a full queue, drop the event and log — the documented
    /// "history may gap" contract during outages.
    pub fn emit(&self, event: RelayEvent) {
        self.try_enqueue(QueuedEvent::Ready(event));
    }

    /// No-op if `entry.user_id` is `None` — a standalone-auth tunnel has no
    /// account to attribute the event to.
    pub fn tunnel_started(&self, entry: &TunnelEntry) {
        let Some(user_id) = entry.user_id.clone() else {
            return;
        };
        entry.viewer_count.start(RelayEvent::TunnelStarted {
            tunnel_id: entry.tunnel_id.to_string(),
            user_id,
            slug: entry.slug.clone(),
            credential_id: entry.credential_id.clone(),
            zellij_version: entry.zellij_version.clone(),
        });
    }

    pub fn tunnel_stopped(&self, entry: &TunnelEntry, reason: &str) {
        if !entry.viewer_count.stop() {
            log::info!(
                "relay-debug: tunnel_stopped skipped, already reported: tunnel_id={} reason={}",
                entry.tunnel_id,
                reason
            );
            return;
        }
        let Some(user_id) = entry.user_id.clone() else {
            log::info!(
                "relay-debug: tunnel_stopped skipped, standalone tunnel: tunnel_id={} reason={}",
                entry.tunnel_id,
                reason
            );
            return;
        };
        log::info!(
            "relay-debug: tunnel_stopped queued: tunnel_id={} reason={}",
            entry.tunnel_id,
            reason
        );
        self.emit(RelayEvent::TunnelStopped {
            tunnel_id: entry.tunnel_id.to_string(),
            user_id,
            slug: entry.slug.clone(),
            reason: reason.to_string(),
        });
    }
}

#[derive(Default)]
struct ViewerCountState {
    count: u64,
    reported: Option<u64>,
    seq: u64,
    queued: bool,
    started: bool,
    pending_start: Option<RelayEvent>,
    stopped: bool,
}

pub struct ViewerCountReporter {
    tunnel_id: String,
    sink: EventSink,
    state: Mutex<ViewerCountState>,
}

impl ViewerCountReporter {
    pub fn new(tunnel_id: Uuid, sink: EventSink) -> Arc<Self> {
        Arc::new(Self {
            tunnel_id: tunnel_id.to_string(),
            sink,
            state: Mutex::new(ViewerCountState::default()),
        })
    }

    pub fn set_count(self: &Arc<Self>, count: usize) {
        let mut state = self.state.lock().unwrap();
        state.count = count as u64;
        self.schedule(&mut state);
    }

    fn start(self: &Arc<Self>, event: RelayEvent) {
        let mut state = self.state.lock().unwrap();
        if state.stopped || state.started {
            return;
        }
        state.pending_start = Some(event);
        self.schedule(&mut state);
    }

    fn stop(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        let first = !state.stopped;
        state.stopped = true;
        state.pending_start = None;
        first
    }

    fn schedule(self: &Arc<Self>, state: &mut ViewerCountState) {
        if state.stopped {
            return;
        }
        if !state.started {
            let Some(start) = state.pending_start.clone() else {
                return;
            };
            if !self.sink.try_enqueue(QueuedEvent::Ready(start)) {
                return;
            }
            state.pending_start = None;
            state.started = true;
        }
        if state.queued || state.reported.unwrap_or(0) == state.count {
            return;
        }
        if self.sink.try_enqueue(QueuedEvent::ViewerCount(self.clone())) {
            state.queued = true;
        }
    }

    fn take_report(&self) -> Option<RelayEvent> {
        let mut state = self.state.lock().unwrap();
        state.queued = false;
        if state.stopped || state.reported.unwrap_or(0) == state.count {
            return None;
        }
        state.seq += 1;
        state.reported = Some(state.count);
        Some(RelayEvent::ViewerCount {
            tunnel_id: self.tunnel_id.clone(),
            count: state.count,
            seq: state.seq,
        })
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

    #[test]
    fn viewer_count_serde_exact() {
        let event = RelayEvent::ViewerCount {
            tunnel_id: "t1".into(),
            count: 3,
            seq: 17,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            r#"{"type":"viewer_count","tunnel_id":"t1","count":3,"seq":17}"#
        );
    }

    #[tokio::test]
    async fn queue_overflow_drops_without_blocking_or_panicking() {
        let (tx, _rx) = mpsc::channel::<QueuedEvent>(1);
        let sink = EventSink::Online(tx);
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

    fn make_entry(sink: EventSink, user_id: Option<&str>) -> TunnelEntry {
        let (control_tx, _rx2) = mpsc::unbounded_channel();
        let tunnel_id = Uuid::new_v4();
        TunnelEntry {
            tunnel_id,
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
            user_id: user_id.map(str::to_string),
            credential_id: None,
            terminal_binding_secret_hash: String::new(),
            viewer_count: ViewerCountReporter::new(tunnel_id, sink),
        }
    }

    fn drain(rx: &mut mpsc::Receiver<QueuedEvent>) -> Vec<RelayEvent> {
        let mut out = Vec::new();
        while let Ok(queued) = rx.try_recv() {
            match queued {
                QueuedEvent::Ready(event) => out.push(event),
                QueuedEvent::ViewerCount(reporter) => {
                    if let Some(event) = reporter.take_report() {
                        out.push(event);
                    }
                },
                QueuedEvent::Flush(ack) => {
                    let _ = ack.send(());
                },
            }
        }
        out
    }

    fn counts(events: &[RelayEvent]) -> Vec<(u64, u64)> {
        events
            .iter()
            .filter_map(|e| match e {
                RelayEvent::ViewerCount { count, seq, .. } => Some((*count, *seq)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn tunnel_started_with_no_user_id_emits_nothing() {
        let (tx, mut rx) = mpsc::channel::<QueuedEvent>(4);
        let sink = EventSink::Online(tx);
        let entry = make_entry(sink.clone(), None);
        sink.tunnel_started(&entry);
        entry.viewer_count.set_count(2);
        assert!(rx.try_recv().is_err(), "no event should be queued");
    }

    #[test]
    fn viewer_count_waits_for_tunnel_started() {
        let (tx, mut rx) = mpsc::channel::<QueuedEvent>(8);
        let sink = EventSink::Online(tx);
        let entry = make_entry(sink.clone(), Some("u1"));
        entry.viewer_count.set_count(1);
        assert!(rx.try_recv().is_err());
        sink.tunnel_started(&entry);
        let events = drain(&mut rx);
        assert!(matches!(events[0], RelayEvent::TunnelStarted { .. }));
        assert_eq!(counts(&events), vec![(1, 1)]);
    }

    #[test]
    fn viewer_count_merges_rapid_changes_and_increments_seq() {
        let (tx, mut rx) = mpsc::channel::<QueuedEvent>(8);
        let sink = EventSink::Online(tx);
        let entry = make_entry(sink.clone(), Some("u1"));
        sink.tunnel_started(&entry);
        entry.viewer_count.set_count(1);
        entry.viewer_count.set_count(2);
        entry.viewer_count.set_count(3);
        let events = drain(&mut rx);
        assert_eq!(counts(&events), vec![(3, 1)]);
        entry.viewer_count.set_count(0);
        assert_eq!(counts(&drain(&mut rx)), vec![(0, 2)]);
    }

    #[test]
    fn viewer_count_resends_start_after_queue_overflow() {
        let (tx, mut rx) = mpsc::channel::<QueuedEvent>(1);
        let sink = EventSink::Online(tx);
        sink.emit(RelayEvent::TunnelStopped {
            tunnel_id: "other".into(),
            user_id: "u1".into(),
            slug: "x".into(),
            reason: "x".into(),
        });
        let entry = make_entry(sink.clone(), Some("u1"));
        sink.tunnel_started(&entry);
        drain(&mut rx);
        entry.viewer_count.set_count(1);
        let events = drain(&mut rx);
        assert!(matches!(events[0], RelayEvent::TunnelStarted { .. }));
        entry.viewer_count.set_count(2);
        assert_eq!(counts(&drain(&mut rx)), vec![(2, 1)]);
    }

    #[test]
    fn viewer_count_not_sent_after_tunnel_stopped() {
        let (tx, mut rx) = mpsc::channel::<QueuedEvent>(8);
        let sink = EventSink::Online(tx);
        let entry = make_entry(sink.clone(), Some("u1"));
        sink.tunnel_started(&entry);
        entry.viewer_count.set_count(1);
        sink.tunnel_stopped(&entry, STOP_REASON_CONTROL_SOCKET_CLOSED);
        entry.viewer_count.set_count(0);
        let events = drain(&mut rx);
        assert!(counts(&events).is_empty());
        assert!(matches!(events.last(), Some(RelayEvent::TunnelStopped { .. })));
    }

    #[test]
    fn tunnel_stopped_is_reported_once() {
        let (tx, mut rx) = mpsc::channel::<QueuedEvent>(8);
        let sink = EventSink::Online(tx);
        let entry = make_entry(sink.clone(), Some("u1"));
        sink.tunnel_started(&entry);
        sink.tunnel_stopped(&entry, STOP_REASON_RELAY_SHUTDOWN);
        sink.tunnel_stopped(&entry, STOP_REASON_CONTROL_SOCKET_CLOSED);
        let stops: Vec<_> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                RelayEvent::TunnelStopped { reason, .. } => Some(reason),
                _ => None,
            })
            .collect();
        assert_eq!(stops, vec![STOP_REASON_RELAY_SHUTDOWN.to_string()]);
    }
}
