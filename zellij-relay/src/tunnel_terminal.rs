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

    // Writer task: drain encoded TerminalMessage bytes onto the sink.
    let writer_slug = entry.slug.clone();
    let writer_handle = tokio::spawn(async move {
        while let Some(bytes) = terminal_rx.recv().await {
            if let Err(e) = sink.send(Message::Binary(bytes.into())).await {
                tracing::debug!(slug = %writer_slug, error = %e, "terminal writer error");
                break;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    // Reader task body runs inline.
    while let Some(frame) = stream.next().await {
        let bytes = match frame {
            Ok(Message::Binary(b)) => b,
            Ok(Message::Text(t)) => t.as_bytes().to_vec().into(),
            Ok(Message::Close(_)) => break,
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
                let sender = entry
                    .viewers
                    .lock()
                    .unwrap()
                    .get(&client_id)
                    .and_then(|v| v.terminal_sink_tx.clone());
                match sender {
                    Some(tx) => {
                        // Phase 3+: `data` is always opaque bytes. On the
                        // relay path it is ciphertext; the browser sets
                        // `binaryType = "arraybuffer"` and decrypts before
                        // feeding the terminal. Forward as-is.
                        let _ = tx.send(Message::Binary(data.into()));
                    },
                    None => {
                        tracing::debug!(
                            slug = %entry.slug,
                            client_id,
                            "TerminalFrameData for unknown client_id, dropping"
                        );
                    },
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
