//! `/tunnel/control` WebSocket handler.
//!
//! Decodes a `ControlMessage::Auth` from the first frame, allocates a slug
//! and tunnel id, and replies with `ControlMessage::Established`. After the
//! handshake the socket is split in two:
//!
//! - A writer task drains `entry.control_tx` and pushes encoded
//!   `ControlMessage` bytes into the socket sink.
//! - A reader task dispatches incoming frames:
//!     * `PakeResponse` / `PakeResult` → resolve the matching oneshot in
//!       `entry.pending_pake_responses` / `entry.pending_pake_results`.
//!     * `ClientDisconnected` → tears down the viewer for that `client_id`.
//!     * Any post-handshake `Auth` / `Established` is logged and dropped.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
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
use zellij_relay_protocol::{
    decode_control_frame, ControlMessage, TunnelErrorCode, SUPPORTED_PROTOCOL_VERSIONS,
};

use crate::events::{
    STOP_REASON_CONTROL_SOCKET_CLOSED, STOP_REASON_ESTABLISHED_SEND_FAILED,
    STOP_REASON_HEARTBEAT_TIMEOUT,
};
use crate::heartbeat::{now_millis, spawn_server_heartbeat, HEARTBEAT_TIMEOUT_SECS};

use crate::registry::{
    generate_terminal_binding_secret, hash_terminal_binding_secret, PakeResponseResult, TunnelEntry,
};
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
            let _ = send_error(
                &mut socket,
                TunnelErrorCode::UnexpectedFrame,
                "expected binary TunnelAuth frame",
            )
            .await;
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
            let _ = send_error(
                &mut socket,
                TunnelErrorCode::MalformedFrame,
                "malformed ControlFrame",
            )
            .await;
            return;
        },
    };
    // `first` is the raw serialized auth frame — it holds a plaintext copy of
    // the account credential. Drop it now so it does not linger for the whole
    // socket lifetime (paired with the `drop(token)` after authorization
    // below). Not zeroization, just bounding the lifetime of the copy.
    drop(first);

    let (token, session_name, zellij_version, requested_slug, protocol_version, read_only) = match msg {
        ControlMessage::Auth {
            token,
            session_name,
            zellij_version,
            requested_slug,
            protocol_version,
            read_only,
        } => (
            token,
            session_name,
            zellij_version,
            requested_slug,
            protocol_version,
            read_only,
        ),
        other => {
            tracing::warn!(?other, "control tunnel first message was not Auth");
            let _ = send_error(
                &mut socket,
                TunnelErrorCode::UnexpectedFrame,
                "first frame must be TunnelAuth",
            )
            .await;
            return;
        },
    };

    // Reject tunnels whose declared `protocol_version` falls outside the
    // supported range. The rejection carries a typed
    // `TunnelErrorCode::ProtocolVersionUnsupported` with the supported range so
    // the sharer's plugin renders a dedicated protocol-mismatch state without
    // parsing prose.
    if !SUPPORTED_PROTOCOL_VERSIONS.contains(&protocol_version) {
        let message = format!(
            "Relay requires protocol version {}\u{2013}{}, but this Zellij speaks version {}. Please update Zellij.",
            SUPPORTED_PROTOCOL_VERSIONS.start(),
            SUPPORTED_PROTOCOL_VERSIONS.end(),
            protocol_version,
        );
        tracing::warn!(
            protocol_version,
            supported_start = *SUPPORTED_PROTOCOL_VERSIONS.start(),
            supported_end = *SUPPORTED_PROTOCOL_VERSIONS.end(),
            %zellij_version,
            "rejecting tunnel: protocol version mismatch"
        );
        let _ = send_error(
            &mut socket,
            TunnelErrorCode::ProtocolVersionUnsupported {
                supported_min: *SUPPORTED_PROTOCOL_VERSIONS.start(),
                supported_max: *SUPPORTED_PROTOCOL_VERSIONS.end(),
                offered_version: protocol_version,
            },
            &message,
        )
        .await;
        return;
    }

    // Account-credential check, delegated to the configured backend
    // (standalone `LocalSqlite` or hosted `Online` control-plane
    // introspection — see `tunnel_auth.rs`). Rejections of every kind
    // (unknown/empty credential, DB error, backend fail-closed) surface
    // uniformly with `TunnelErrorCode::AuthRejected` — the sharer-side share
    // plugin branches on that code to surface the `<relay rejected auth token>`
    // state.
    let decision = state.tunnel_auth.authorize(&token).await;
    // The account credential is not stored in tunnel state or referenced past
    // authorization (decision #1) — drop it now, before it could ever leak into
    // the `TunnelEntry` or a later log line.
    drop(token);
    if !decision.accepted {
        tracing::info!(
            %zellij_version,
            %session_name,
            "rejecting tunnel: relay tunnel auth rejected"
        );
        let _ = send_error(
            &mut socket,
            TunnelErrorCode::AuthRejected,
            "relay tunnel auth rejected",
        )
        .await;
        return;
    }

    // Phase 6 reconnect: honour the client's `requested_slug` if it was
    // supplied and is still free. Occupied slugs fall back to a fresh
    // random slug so a stale reconnect attempt never hijacks a live
    // tunnel. Empty string means fresh tunnel → always generate.
    let slug = if !requested_slug.is_empty() && state.registry.get(&requested_slug).is_none() {
        tracing::info!(%requested_slug, "reusing client-requested slug on reconnect");
        requested_slug
    } else {
        if !requested_slug.is_empty() {
            tracing::info!(
                %requested_slug,
                "requested slug unavailable on reconnect — allocating fresh"
            );
        }
        slug::generate()
    };
    let tunnel_id = Uuid::new_v4();
    let public_url = state.render_public_url(&slug);
    let binding_secret = generate_terminal_binding_secret();
    let binding_secret_hash = hash_terminal_binding_secret(&binding_secret);

    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let entry = Arc::new(TunnelEntry {
        tunnel_id,
        slug: slug.clone(),
        public_url: public_url.clone(),
        session_name: session_name.clone(),
        zellij_version,
        read_only,
        created_at: Instant::now(),
        terminal_linked: Arc::new(Notify::new()),
        terminal_linked_flag: Arc::new(Mutex::new(false)),
        control_tx,
        terminal_tx: Mutex::new(None),
        pending_pake_responses: Mutex::new(HashMap::new()),
        pending_pake_results: Mutex::new(HashMap::new()),
        pending_handshakes: Mutex::new(HashMap::new()),
        viewers: Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
        client_id_to_viewer: Mutex::new(HashMap::new()),
        user_id: decision.user_id,
        credential_id: decision.credential_id,
        terminal_binding_secret_hash: binding_secret_hash,
    });
    state.registry.insert(entry.clone());
    state.event_sink.tunnel_started(&entry);
    tracing::info!(%slug, %tunnel_id, %session_name, "tunnel established");

    let established = ControlMessage::Established {
        public_url: public_url.clone(),
        slug: slug.clone(),
        tunnel_id: tunnel_id.to_string(),
        terminal_binding_secret: binding_secret,
    };
    if let Err(e) = socket.send(Message::Binary(established.encode().into())).await {
        tracing::warn!(error = %e, "failed to send TunnelEstablished");
        if let Some(removed) = state.registry.remove(&slug) {
            state
                .event_sink
                .tunnel_stopped(&removed, STOP_REASON_ESTABLISHED_SEND_FAILED);
        }
        return;
    }

    // Split the control socket: writer task drains control_rx plus a
    // dedicated heartbeat-ping channel, reader task dispatches incoming
    // frames.
    let (mut sink, mut stream) = socket.split();

    let (hb_ping_tx, mut hb_ping_rx) = mpsc::unbounded_channel::<Message>();
    let pong_tx = hb_ping_tx.clone();
    let last_activity = Arc::new(AtomicU64::new(now_millis()));

    let writer_entry_slug = slug.clone();
    let writer_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                bytes = control_rx.recv() => match bytes {
                    Some(b) => {
                        if let Err(e) = sink.send(Message::Binary(b.into())).await {
                            tracing::debug!(slug = %writer_entry_slug, error = %e, "control writer error");
                            break;
                        }
                    }
                    None => break,
                },
                ping = hb_ping_rx.recv() => match ping {
                    Some(msg) => {
                        if let Err(e) = sink.send(msg).await {
                            tracing::debug!(slug = %writer_entry_slug, error = %e, "control writer ping error");
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
        spawn_server_heartbeat(hb_ping_tx, last_activity.clone(), "control");

    // Reader task: dispatch incoming ControlMessages.
    let reader_entry = entry.clone();
    let mut stop_reason = STOP_REASON_CONTROL_SOCKET_CLOSED;
    'reader: loop {
        let frame = tokio::select! {
            frame = stream.next() => frame,
            _ = &mut hb_tripped => {
                tracing::warn!(
                    slug = %reader_entry.slug,
                    "control tunnel silent >{}s — closing",
                    HEARTBEAT_TIMEOUT_SECS
                );
                stop_reason = STOP_REASON_HEARTBEAT_TIMEOUT;
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
                // Axum does not auto-reply; mirror the ping back as a
                // pong so the peer's watchdog stays happy.
                let _ = pong_tx.send(Message::Pong(p));
                continue;
            },
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
            ControlMessage::PakeResponse {
                request_id,
                client_id,
                accepted,
                sharer_msg,
                sharer_confirm,
            } => {
                let sender = reader_entry
                    .pending_pake_responses
                    .lock()
                    .unwrap()
                    .remove(&request_id);
                if let Some(sender) = sender {
                    let _ = sender.send(PakeResponseResult {
                        accepted,
                        client_id,
                        sharer_msg,
                        sharer_confirm,
                    });
                } else {
                    tracing::warn!(slug = %reader_entry.slug, "PakeResponse for unknown request_id");
                }
            },
            ControlMessage::PakeResult {
                request_id,
                client_id: _,
                accepted,
            } => {
                let sender = reader_entry
                    .pending_pake_results
                    .lock()
                    .unwrap()
                    .remove(&request_id);
                if let Some(sender) = sender {
                    let _ = sender.send(accepted);
                } else {
                    tracing::warn!(slug = %reader_entry.slug, "PakeResult for unknown request_id");
                }
            },
            ControlMessage::ControlFrameData { client_id, data } => {
                let Some(vid) = reader_entry.viewer_for_client_id(client_id) else {
                    tracing::debug!(
                        slug = %reader_entry.slug,
                        client_id,
                        "ControlFrameData for unknown client_id, dropping"
                    );
                    continue;
                };
                let viewers = reader_entry.viewers.lock().unwrap();
                if let Some(handle) = viewers.get(&vid) {
                    if let Some(tx) = &handle.control_sink_tx {
                        let _ = tx.send(Message::Binary(data.into()));
                    }
                }
            },
            ControlMessage::ClientDisconnected { client_id } => {
                let vid = reader_entry.viewer_for_client_id(client_id);
                reader_entry
                    .client_id_to_viewer
                    .lock()
                    .unwrap()
                    .remove(&client_id);
                let mut closed = false;
                if let Some(vid) = vid {
                    let mut viewers = reader_entry.viewers.lock().unwrap();
                    if let Some(mut handle) = viewers.remove(&vid) {
                        if let Some(tx) = handle.disconnect_terminal.take() {
                            let _ = tx.send(());
                        }
                        if let Some(tx) = handle.disconnect_control.take() {
                            let _ = tx.send(());
                        }
                        closed = true;
                    }
                }
                tracing::info!(
                    slug = %reader_entry.slug,
                    client_id,
                    closed,
                    "sharer requested ClientDisconnected"
                );
            },
            ControlMessage::Error { message, .. } => {
                tracing::warn!(slug = %reader_entry.slug, %message, "relay-peer control error");
            },
            other => {
                tracing::warn!(slug = %reader_entry.slug, ?other, "unexpected control frame after handshake");
            },
        }
    }

    // Tunnel closing: drop registry entry and force-close all viewers.
    if let Some(removed) = state.registry.remove(&reader_entry.slug) {
        state.event_sink.tunnel_stopped(&removed, stop_reason);
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
    hb_handle.abort();
    tracing::info!(slug = %reader_entry.slug, "tunnel closed");
}

async fn send_error(
    socket: &mut WebSocket,
    code: TunnelErrorCode,
    message: &str,
) -> anyhow::Result<()> {
    let frame = ControlMessage::Error {
        message: message.to_string(),
        code,
    };
    socket
        .send(Message::Binary(frame.encode().into()))
        .await
        .map_err(Into::into)
}
