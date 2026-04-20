//! `/tunnel/control` WebSocket handler.
//!
//! Decodes a `ControlMessage::Auth` from the first frame, allocates a slug
//! and tunnel id, and replies with `ControlMessage::Established`. After the
//! handshake the socket is split in two:
//!
//! - A writer task drains `entry.control_tx` and pushes encoded
//!   `ControlMessage` bytes into the socket sink.
//! - A reader task dispatches incoming frames:
//!     * `AuthResponse` → resolves the oneshot in `entry.pending_auths`.
//!     * `ControlFrameData` → forwards the inner bytes to the matching
//!       viewer's control sink as `Message::Text`.
//!     * Any post-handshake `Auth` / `Established` is logged and dropped.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, Notify};
use uuid::Uuid;
use zellij_relay_protocol::{decode_control_frame, ControlMessage};

use crate::registry::{AuthResponseResult, TunnelEntry};
use crate::router::AppState;
use crate::slug;

pub async fn handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let first = match socket.next().await {
        Some(Ok(Message::Binary(bytes))) => bytes,
        Some(Ok(other)) => {
            tracing::warn!(?other, "unexpected first frame on control tunnel");
            let _ = send_error(&mut socket, "expected binary TunnelAuth frame").await;
            return;
        },
        Some(Err(e)) => {
            tracing::warn!(error = %e, "control WS error before auth");
            return;
        },
        None => return,
    };

    let msg = match decode_control_frame(&first) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "failed to decode ControlFrame");
            let _ = send_error(&mut socket, "malformed ControlFrame").await;
            return;
        },
    };

    let (session_name, zellij_version) = match msg {
        ControlMessage::Auth {
            session_name,
            zellij_version,
            ..
        } => (session_name, zellij_version),
        other => {
            tracing::warn!(?other, "control tunnel first message was not Auth");
            let _ = send_error(&mut socket, "first frame must be TunnelAuth").await;
            return;
        },
    };

    let slug = slug::generate();
    let tunnel_id = Uuid::new_v4();
    let public_url = state.render_public_url(&slug);

    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let entry = Arc::new(TunnelEntry {
        tunnel_id,
        slug: slug.clone(),
        public_url: public_url.clone(),
        session_name: session_name.clone(),
        zellij_version,
        created_at: Instant::now(),
        terminal_linked: Arc::new(Notify::new()),
        terminal_linked_flag: Arc::new(Mutex::new(false)),
        control_tx,
        terminal_tx: Mutex::new(None),
        pending_auths: Mutex::new(HashMap::new()),
        viewers: Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
    });
    state.registry.insert(entry.clone());
    tracing::info!(%slug, %tunnel_id, %session_name, "tunnel established");

    let established = ControlMessage::Established {
        public_url: public_url.clone(),
        slug: slug.clone(),
        tunnel_id: tunnel_id.to_string(),
    };
    if let Err(e) = socket.send(Message::Binary(established.encode().into())).await {
        tracing::warn!(error = %e, "failed to send TunnelEstablished");
        state.registry.remove(&slug);
        return;
    }

    // Split the control socket: writer task drains control_rx, reader task
    // dispatches incoming frames.
    let (mut sink, mut stream) = socket.split();

    let writer_entry_slug = slug.clone();
    let writer_handle = tokio::spawn(async move {
        while let Some(bytes) = control_rx.recv().await {
            if let Err(e) = sink.send(Message::Binary(bytes.into())).await {
                tracing::debug!(slug = %writer_entry_slug, error = %e, "control writer error");
                break;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    // Reader task: dispatch incoming ControlMessages.
    let reader_entry = entry.clone();
    while let Some(frame) = stream.next().await {
        let bytes = match frame {
            Ok(Message::Binary(b)) => b,
            Ok(Message::Text(t)) => t.as_bytes().to_vec().into(),
            Ok(Message::Close(_)) => break,
            Ok(_) => continue,
            Err(e) => {
                tracing::debug!(slug = %reader_entry.slug, error = %e, "control reader error");
                break;
            },
        };

        let msg = match decode_control_frame(&bytes) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(slug = %reader_entry.slug, error = %e, "bad control frame");
                continue;
            },
        };

        match msg {
            ControlMessage::AuthResponse {
                request_id,
                client_id,
                accepted,
                is_read_only,
                session_token_hash,
                e2e_encrypted,
            } => {
                let sender = reader_entry
                    .pending_auths
                    .lock()
                    .unwrap()
                    .remove(&request_id);
                if let Some(sender) = sender {
                    let _ = sender.send(AuthResponseResult {
                        accepted,
                        client_id,
                        is_read_only,
                        session_token_hash,
                        e2e_encrypted,
                    });
                } else {
                    tracing::warn!(slug = %reader_entry.slug, "AuthResponse for unknown request_id");
                }
            },
            ControlMessage::ControlFrameData { client_id, data } => {
                let sender = reader_entry
                    .viewers
                    .lock()
                    .unwrap()
                    .get(&client_id)
                    .and_then(|v| v.control_sink_tx.clone());
                match sender {
                    Some(tx) => {
                        let text = match std::str::from_utf8(&data) {
                            Ok(s) => s.to_owned(),
                            Err(_) => {
                                tracing::warn!(
                                    client_id,
                                    "ControlFrameData from Zellij is not UTF-8, dropping"
                                );
                                continue;
                            },
                        };
                        let _ = tx.send(Message::Text(text.into()));
                    },
                    None => {
                        tracing::debug!(
                            slug = %reader_entry.slug,
                            client_id,
                            "ControlFrameData for unknown client_id, dropping"
                        );
                    },
                }
            },
            ControlMessage::ClientDisconnected { client_id } => {
                let handle = reader_entry.viewers.lock().unwrap().remove(&client_id);
                if let Some(mut handle) = handle {
                    if let Some(tx) = handle.disconnect_terminal.take() {
                        let _ = tx.send(());
                    }
                    if let Some(tx) = handle.disconnect_control.take() {
                        let _ = tx.send(());
                    }
                }
            },
            ControlMessage::Error { message } => {
                tracing::warn!(slug = %reader_entry.slug, %message, "relay-peer control error");
            },
            other => {
                tracing::warn!(slug = %reader_entry.slug, ?other, "unexpected control frame after handshake");
            },
        }
    }

    // Tunnel closing: drop registry entry and force-close all viewers.
    if let Some(removed) = state.registry.remove(&reader_entry.slug) {
        let mut viewers = removed.viewers.lock().unwrap();
        for (_client_id, mut handle) in viewers.drain() {
            if let Some(tx) = handle.disconnect_terminal.take() {
                let _ = tx.send(());
            }
            if let Some(tx) = handle.disconnect_control.take() {
                let _ = tx.send(());
            }
        }
    }
    writer_handle.abort();
    tracing::info!(slug = %reader_entry.slug, "tunnel closed");
}

async fn send_error(socket: &mut WebSocket, message: &str) -> anyhow::Result<()> {
    let frame = ControlMessage::Error {
        message: message.to_string(),
    };
    socket
        .send(Message::Binary(frame.encode().into()))
        .await
        .map_err(Into::into)
}
