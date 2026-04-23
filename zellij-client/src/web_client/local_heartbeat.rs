//! Phase 6 (Session A): heartbeat helpers for the local web-server
//! WebSocket channels (`/ws/control`, `/ws/terminal`).
//!
//! Mirrors the cadence used by the relay tunnel server
//! (`zellij-relay/src/heartbeat.rs`) and the Zellij-side relay
//! multiplexer. A spawned watchdog task emits `Message::Ping` every 30s
//! and trips a oneshot if no inbound activity is observed for 60s.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::Message;
use tokio::sync::{mpsc, oneshot};

pub const HEARTBEAT_INTERVAL_SECS: u64 = 30;
pub const HEARTBEAT_TIMEOUT_SECS: u64 = 60;

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Spawn a keepalive + watchdog task for a local web-server
/// WebSocket. `ping_sink` receives the `Ping` frames the writer should
/// emit; `last_activity` is refreshed on every inbound frame. Returns a
/// `JoinHandle` and a oneshot tripped when the silence budget is
/// exceeded.
pub fn spawn_local_ws_heartbeat(
    ping_sink: mpsc::UnboundedSender<Message>,
    last_activity: Arc<AtomicU64>,
    which: &'static str,
) -> (tokio::task::JoinHandle<()>, oneshot::Receiver<()>) {
    let (tripped_tx, tripped_rx) = oneshot::channel::<()>();
    log::info!("[hb-local-{}] spawn: task started", which);
    let handle = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(
            HEARTBEAT_INTERVAL_SECS / 2,
        ));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        log::info!(
            "[hb-local-{}] loop entry, interval={}s",
            which,
            HEARTBEAT_INTERVAL_SECS / 2
        );
        ticker.tick().await;
        log::info!("[hb-local-{}] first tick burned", which);
        let mut ticks_since_ping: u32 = 0;
        loop {
            ticker.tick().await;
            ticks_since_ping += 1;
            log::info!(
                "[hb-local-{}] tick #{}: last_activity={}ms ago",
                which,
                ticks_since_ping,
                now_millis().saturating_sub(last_activity.load(Ordering::Relaxed))
            );
            if u64::from(ticks_since_ping) * (HEARTBEAT_INTERVAL_SECS / 2)
                >= HEARTBEAT_INTERVAL_SECS
            {
                match ping_sink.send(Message::Ping(b"hb".to_vec().into())) {
                    Ok(_) => {
                        log::info!("[hb-local-{}] ping enqueued to sink", which);
                    },
                    Err(e) => {
                        log::warn!(
                            "[hb-local-{}] ping sink closed ({:?}) — exiting heartbeat task",
                            which, e
                        );
                        break;
                    },
                }
                ticks_since_ping = 0;
            }
            let last = last_activity.load(Ordering::Relaxed);
            let now = now_millis();
            if now.saturating_sub(last) > HEARTBEAT_TIMEOUT_SECS * 1000 {
                log::warn!(
                    "[hb-local-{}] WATCHDOG TRIPPED: silent {}ms",
                    which,
                    now.saturating_sub(last)
                );
                let _ = tripped_tx.send(());
                break;
            }
        }
        log::info!("[hb-local-{}] heartbeat task exited", which);
    });
    (handle, tripped_rx)
}
