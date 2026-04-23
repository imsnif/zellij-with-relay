//! `/tunnel/terminal` WebSocket handler.
//!
//! The Zellij instance connects here after receiving `TunnelEstablished`,
//! passing the slug as a `?slug=...` query parameter, and sends a
//! `TerminalMessage::Ready { tunnel_id }` as its first frame. After the
//! handshake the socket is split:
//!
//! - Writer task drains `entry.terminal_tx` → encoded `TerminalMessage`
//!   bytes pushed into the sink.
//! - Reader task dispatches `TerminalFrameData { client_id, data }` to the
//!   matching viewer's terminal sink as `Message::Binary`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use zellij_relay_protocol::{decode_terminal_frame, TerminalMessage};

use crate::heartbeat::{now_millis, spawn_server_heartbeat, HEARTBEAT_TIMEOUT_SECS};
use crate::router::AppState;

pub async fn handler(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Response {
    let slug = params.get("slug").cloned().unwrap_or_default();
    ws.on_upgrade(move |socket| handle_socket(socket, state, slug))
}

async fn handle_socket(mut socket: WebSocket, state: AppState, slug: String) {
    if slug.is_empty() {
        tracing::warn!("terminal tunnel request with missing slug");
        let _ = send_error(&mut socket, "missing ?slug query parameter").await;
        return;
    }

    let entry = match state.registry.get(&slug) {
        Some(e) => e,
        None => {
            tracing::warn!(%slug, "terminal tunnel request for unknown slug");
            let _ = send_error(&mut socket, "unknown slug").await;
            return;
        },
    };

    let first = match socket.next().await {
        Some(Ok(Message::Binary(bytes))) => bytes,
        Some(Ok(other)) => {
            tracing::warn!(?other, "unexpected first frame on terminal tunnel");
            let _ = send_error(&mut socket, "expected binary TunnelReady frame").await;
            return;
        },
        Some(Err(e)) => {
            tracing::warn!(error = %e, "terminal WS error before ready");
            return;
        },
        None => return,
    };

    let msg = match decode_terminal_frame(&first) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "failed to decode TerminalFrame");
            let _ = send_error(&mut socket, "malformed TerminalFrame").await;
            return;
        },
    };

    match msg {
        TerminalMessage::Ready { tunnel_id } if tunnel_id == entry.tunnel_id.to_string() => {
            *entry.terminal_linked_flag.lock().unwrap() = true;
            entry.terminal_linked.notify_waiters();
            tracing::info!(slug = %entry.slug, "terminal tunnel linked");
        },
        other => {
            tracing::warn!(?other, "terminal tunnel first message was not matching Ready");
            let _ = send_error(&mut socket, "first frame must be TunnelReady with matching tunnel_id").await;
            return;
        },
    }

    let (terminal_tx, mut terminal_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    *entry.terminal_tx.lock().unwrap() = Some(terminal_tx);

    let (mut sink, mut stream) = socket.split();

    let (hb_ping_tx, mut hb_ping_rx) = mpsc::unbounded_channel::<Message>();
    let pong_tx = hb_ping_tx.clone();
    let last_activity = Arc::new(AtomicU64::new(now_millis()));

    // Writer task: drain encoded TerminalMessage bytes + heartbeat pings
    // onto the sink.
    let writer_slug = entry.slug.clone();
    let writer_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                bytes = terminal_rx.recv() => match bytes {
                    Some(b) => {
                        if let Err(e) = sink.send(Message::Binary(b.into())).await {
                            tracing::debug!(slug = %writer_slug, error = %e, "terminal writer error");
                            break;
                        }
                    }
                    None => break,
                },
                ping = hb_ping_rx.recv() => match ping {
                    Some(msg) => {
                        if let Err(e) = sink.send(msg).await {
                            tracing::debug!(slug = %writer_slug, error = %e, "terminal writer ping error");
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    let (hb_handle, mut hb_tripped) =
        spawn_server_heartbeat(hb_ping_tx, last_activity.clone(), "terminal");

    // Reader task body runs inline.
    'reader: loop {
        let frame = tokio::select! {
            frame = stream.next() => frame,
            _ = &mut hb_tripped => {
                tracing::warn!(
                    slug = %entry.slug,
                    "terminal tunnel silent >{}s — closing",
                    HEARTBEAT_TIMEOUT_SECS
                );
                break 'reader;
            }
        };
        let Some(frame) = frame else { break 'reader };
        last_activity.store(now_millis(), Ordering::Relaxed);
        let bytes = match frame {
            Ok(Message::Binary(b)) => b,
            Ok(Message::Text(t)) => t.as_bytes().to_vec().into(),
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(p)) => {
                let _ = pong_tx.send(Message::Pong(p));
                continue;
            },
            Ok(_) => continue,
            Err(e) => {
                tracing::debug!(slug = %entry.slug, error = %e, "terminal reader error");
                break;
            },
        };

        let msg = match decode_terminal_frame(&bytes) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(slug = %entry.slug, error = %e, "bad terminal frame");
                continue;
            },
        };

        match msg {
            TerminalMessage::TerminalFrameData { client_id, data } => {
                let viewer_ids = entry.viewers_for_client_id(client_id);
                if viewer_ids.is_empty() {
                    tracing::debug!(
                        slug = %entry.slug,
                        client_id,
                        "TerminalFrameData for unknown client_id, dropping"
                    );
                    continue;
                }
                // Phase 3+: `data` is always opaque bytes. On the relay
                // path it is ciphertext; the browser sets
                // `binaryType = "arraybuffer"` and decrypts before feeding
                // the terminal. Forward as-is. Phase 4: fan out the same
                // bytes to every viewer when `client_id` addresses an r/o
                // group — the relay performs a memcpy per viewer and
                // never decrypts.
                let viewers = entry.viewers.lock().unwrap();
                if viewer_ids.len() == 1 {
                    if let Some(handle) = viewers.get(&viewer_ids[0]) {
                        if let Some(tx) = &handle.terminal_sink_tx {
                            let _ = tx.send(Message::Binary(data.into()));
                        }
                    }
                } else {
                    for vid in viewer_ids {
                        if let Some(handle) = viewers.get(&vid) {
                            if let Some(tx) = &handle.terminal_sink_tx {
                                let _ = tx.send(Message::Binary(data.clone().into()));
                            }
                        }
                    }
                }
            },
            TerminalMessage::Error { message } => {
                tracing::warn!(slug = %entry.slug, %message, "relay-peer terminal error");
            },
            other => {
                tracing::warn!(slug = %entry.slug, ?other, "unexpected terminal frame after handshake");
            },
        }
    }

    // Clear the terminal_tx so writers can't enqueue to a closed channel.
    *entry.terminal_tx.lock().unwrap() = None;
    writer_handle.abort();
    hb_handle.abort();
    tracing::info!(slug = %entry.slug, "terminal tunnel closed");
}

async fn send_error(socket: &mut WebSocket, message: &str) -> anyhow::Result<()> {
    let frame = TerminalMessage::Error {
        message: message.to_string(),
    };
    socket
        .send(Message::Binary(frame.encode().into()))
        .await
        .map_err(Into::into)
}
