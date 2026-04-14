//! `/tunnel/control` WebSocket handler.
//!
//! Decodes a `ControlMessage::Auth` from the first frame, allocates a slug
//! and tunnel id, and replies with `ControlMessage::Established`. After that
//! the socket is held open: Phase 1 simply keeps the tunnel entry alive in
//! the registry until the WS closes.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::Response,
};
use futures_util::StreamExt;
use tokio::sync::Notify;
use uuid::Uuid;
use zellij_relay_protocol::{decode_control_frame, ControlMessage};

use crate::registry::TunnelEntry;
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

    let entry = Arc::new(TunnelEntry {
        tunnel_id,
        slug: slug.clone(),
        public_url: public_url.clone(),
        session_name: session_name.clone(),
        zellij_version,
        created_at: Instant::now(),
        terminal_linked: Arc::new(Notify::new()),
        terminal_linked_flag: Arc::new(Mutex::new(false)),
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

    // Hold the socket open. Drain incoming frames; any close ends the tunnel.
    while let Some(frame) = socket.next().await {
        match frame {
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {
                // Phase 1: no other control messages are expected.
            },
        }
    }

    state.registry.remove(&slug);
    tracing::info!(%slug, "tunnel closed");
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
