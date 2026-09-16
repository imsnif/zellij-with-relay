//! `/tunnel/terminal` WebSocket handler.
//!
//! The Zellij instance connects here after receiving `TunnelEstablished`,
//! passing the slug as a `?slug=...` query parameter, and sends a
//! `TerminalMessage::Ready { tunnel_id, binding_secret }` as its first frame,
//! presenting the relay-generated secret from `TunnelEstablished` (not the
//! account credential — see decision #1 in the hosted-control-plane plan).
//! After the handshake the socket is split:
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
use zellij_relay_protocol::{decode_terminal_frame, TerminalMessage, TunnelErrorCode};

use crate::heartbeat::{now_millis, spawn_server_heartbeat, HEARTBEAT_TIMEOUT_SECS};
use crate::registry::hash_terminal_binding_secret;
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
        let _ = send_error(
            &mut socket,
            TunnelErrorCode::MissingSlug,
            "missing ?slug query parameter",
        )
        .await;
        return;
    }

    let entry = match state.registry.get(&slug) {
        Some(e) => e,
        None => {
            tracing::warn!(%slug, "terminal tunnel request for unknown slug");
            let _ = send_error(&mut socket, TunnelErrorCode::UnknownSlug, "unknown slug").await;
            return;
        },
    };

    let first = match socket.next().await {
        Some(Ok(Message::Binary(bytes))) => bytes,
        Some(Ok(other)) => {
            tracing::warn!(?other, "unexpected first frame on terminal tunnel");
            let _ = send_error(
                &mut socket,
                TunnelErrorCode::UnexpectedFrame,
                "expected binary TunnelReady frame",
            )
            .await;
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
            let _ = send_error(
                &mut socket,
                TunnelErrorCode::MalformedFrame,
                "malformed TerminalFrame",
            )
            .await;
            return;
        },
    };
    // `first` is the raw serialized ready frame — it holds a plaintext copy of
    // the terminal binding secret. Drop it now so it does not linger for the
    // whole socket lifetime.
    drop(first);

    match msg {
        TerminalMessage::Ready {
            tunnel_id,
            binding_secret,
        } if tunnel_id == entry.tunnel_id.to_string() => {
            // Binding-secret check only — no backend, no sqlite. The relay
            // always issues 64 lowercase hex chars; a non-matching shape is a
            // cheap reject before the hash compare. Timing on the compare is
            // not a useful oracle: it is over SHA-256 digests of a 32-byte
            // random secret, so a leaked comparison prefix reveals nothing
            // grindable. The account credential never touches this socket
            // (decision #1).
            let shape_ok = binding_secret.len() == 64
                && binding_secret
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
            let accepted = shape_ok
                && hash_terminal_binding_secret(&binding_secret)
                    == entry.terminal_binding_secret_hash;
            if !accepted {
                tracing::info!(
                    slug = %entry.slug,
                    "rejecting terminal tunnel: relay terminal binding rejected"
                );
                let _ = send_error(
                    &mut socket,
                    TunnelErrorCode::TerminalBindingRejected,
                    "relay terminal binding rejected",
                )
                .await;
                return;
            }
            *entry.terminal_linked_flag.lock().unwrap() = true;
            entry.terminal_linked.notify_waiters();
            tracing::info!(slug = %entry.slug, "terminal tunnel linked");
        },
        other => {
            tracing::warn!(?other, "terminal tunnel first message was not matching Ready");
            let _ = send_error(
                &mut socket,
                TunnelErrorCode::TunnelIdMismatch,
                "first frame must be TunnelReady with matching tunnel_id",
            )
            .await;
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
                let Some(vid) = entry.viewer_for_client_id(client_id) else {
                    tracing::debug!(
                        slug = %entry.slug,
                        client_id,
                        "TerminalFrameData for unknown client_id, dropping"
                    );
                    continue;
                };
                // `data` is always ciphertext (`nonce || ciphertext`) under
                // the per-viewer session key. The browser sets
                // `binaryType = "arraybuffer"` and decrypts before feeding
                // the terminal. The relay forwards as-is to the single
                // viewer — there is no fan-out (per-viewer keys make one
                // ciphertext undecryptable by others).
                let mut viewers = entry.viewers.lock().unwrap();
                if let Some(handle) = viewers.get_mut(&vid) {
                    let msg = Message::Binary(data.into());
                    if let Some(tx) = &handle.terminal_sink_tx {
                        let _ = tx.send(msg);
                    } else if handle.terminal_backlog.len()
                        < crate::registry::MAX_TERMINAL_BACKLOG_FRAMES
                    {
                        handle.terminal_backlog.push(msg);
                    }
                }
            },
            TerminalMessage::Error { message, .. } => {
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

async fn send_error(
    socket: &mut WebSocket,
    code: TunnelErrorCode,
    message: &str,
) -> anyhow::Result<()> {
    let frame = TerminalMessage::Error {
        message: message.to_string(),
        code,
    };
    socket
        .send(Message::Binary(frame.encode().into()))
        .await
        .map_err(Into::into)
}
