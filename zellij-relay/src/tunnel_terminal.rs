//! `/tunnel/terminal` WebSocket handler.
//!
//! The Zellij instance connects here after receiving `TunnelEstablished`,
//! passing the slug as a `?slug=...` query parameter, and sends a
//! `TerminalMessage::Ready { tunnel_id }` as its first frame. The relay
//! verifies the slug matches, then holds the socket open. Phase 1 has no
//! actual payload forwarding yet.

use std::collections::HashMap;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::Response,
};
use futures_util::StreamExt;
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

    // Hold the socket open until EOF.
    while let Some(frame) = socket.next().await {
        match frame {
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {
                // Phase 1 has no defined terminal-tunnel traffic beyond Ready.
            },
        }
    }
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
