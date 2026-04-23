//! Phase 6 (Session A): shared heartbeat primitives for the relay's
//! server-side WebSocket handlers. Mirrors the cadence used by the
//! Zellij-side multiplexer in
//! `zellij-client/src/web_client/relay/multiplexer.rs`.
//!
//! A per-socket `last_activity_at: AtomicU64` is refreshed on every
//! inbound frame (binary, text, ping, pong). A spawned watchdog task
//! fires a `Ping` every `HEARTBEAT_INTERVAL_SECS` seconds and trips the
//! returned oneshot if the socket has been silent for longer than
//! `HEARTBEAT_TIMEOUT_SECS`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::Message as WsMessage;
use tokio::sync::{mpsc, oneshot};

pub const HEARTBEAT_INTERVAL_SECS: u64 = 30;
pub const HEARTBEAT_TIMEOUT_SECS: u64 = 60;

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Spawn the heartbeat task for a server-side WebSocket.
///
/// - `ping_sink` receives `Message::Ping` frames the writer should send.
/// - `last_activity` is the shared timestamp the reader refreshes on
///   every decoded frame.
///
/// Returns a `JoinHandle` for the task (callers abort on teardown) plus
/// a oneshot that fires if the silence budget is exceeded. Consumers
/// typically `tokio::select!` the oneshot alongside their reader loop.
pub fn spawn_server_heartbeat(
    ping_sink: mpsc::UnboundedSender<WsMessage>,
    last_activity: Arc<AtomicU64>,
    which: &'static str,
) -> (tokio::task::JoinHandle<()>, oneshot::Receiver<()>) {
    let (tripped_tx, tripped_rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(
            HEARTBEAT_INTERVAL_SECS / 2,
        ));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick is immediate — skip so we don't ping mid-handshake.
        ticker.tick().await;
        let mut ticks_since_ping: u32 = 0;
        loop {
            ticker.tick().await;
            ticks_since_ping += 1;
            if u64::from(ticks_since_ping) * (HEARTBEAT_INTERVAL_SECS / 2)
                >= HEARTBEAT_INTERVAL_SECS
            {
                if ping_sink
                    .send(WsMessage::Ping(b"hb".to_vec().into()))
                    .is_err()
                {
                    break;
                }
                ticks_since_ping = 0;
            }
            let last = last_activity.load(Ordering::Relaxed);
            let now = now_millis();
            if now.saturating_sub(last) > HEARTBEAT_TIMEOUT_SECS * 1000 {
                tracing::warn!(
                    tunnel = which,
                    silent_for_ms = now.saturating_sub(last),
                    "heartbeat watchdog tripping"
                );
                let _ = tripped_tx.send(());
                break;
            }
        }
    });
    (handle, tripped_rx)
}
