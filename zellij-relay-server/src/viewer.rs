//! Viewer-facing HTTP + WebSocket surface for `/r/:slug/...`.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::Request;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path as AxumPath, Query, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use uuid::Uuid;
use zellij_relay_protocol::{ControlMessage, TerminalMessage};

use crate::registry::{PakeResponseResult, PendingHandshake, TunnelEntry, ViewerHandle, ViewerSession};
use crate::router::AppState;

const PAKE_ROUND_TRIP_TIMEOUT_SECS: u64 = 5;
pub const HANDSHAKE_HEADER: &str = "x-zellij-handshake";
/// Maximum simultaneous viewers per tunnel/slug. Each viewer is its own
/// per-viewer encrypted 1:1 stream, so the cap applies uniformly regardless
/// of role. Over-cap registrations fall through to `uniform_unauthorised()`
/// so the browser cannot distinguish a cap hit from an auth rejection.
const MAX_VIEWERS: usize = 10;

#[derive(Deserialize, Default)]
pub struct LoginRequest {
    /// The viewer's opaque SPAKE2 start message (serialised as a JSON byte
    /// array). Carries nothing derived from the secret the relay could grind.
    #[serde(default)]
    pub viewer_msg: Vec<u8>,
    #[serde(default)]
    pub link_id: Vec<u8>,
    #[serde(default)]
    pub remember_me: bool,
}

#[derive(Serialize)]
pub struct LoginResponse {
    pub success: bool,
    /// The sharer's SPAKE2 message. The browser feeds this to `pake_finish`.
    pub sharer_msg: Vec<u8>,
    /// The sharer's key-confirmation tag. The browser verifies it before
    /// sending its own confirmation on `/session`; a mismatch means a wrong
    /// secret or a tampering relay.
    pub sharer_confirm: Vec<u8>,
    pub handshake: String,
}

#[derive(Deserialize, Default)]
pub struct SessionRequest {
    /// The viewer's key-confirmation tag (second PAKE round-trip).
    #[serde(default)]
    pub viewer_confirm: Vec<u8>,
}

#[derive(Serialize)]
pub struct SessionResponse {
    pub web_client_id: String,
    pub client_id: u32,
    pub is_read_only: bool,
    /// Always `true` on the relay path (every connection is E2E-encrypted).
    /// Kept so the browser JS can cross-check against the challenge-page claim.
    pub e2e_encrypted: bool,
    /// HKDF `info` component for the per-viewer session key. The browser
    /// folds the same `tunnel_id` (plus its `client_id`) into key derivation.
    pub tunnel_id: String,
}

fn virtual_web_client_id(tunnel_id: &str, client_id: u32) -> String {
    // Namespaced by tunnel_id so two tunnels for the same session (e.g. a
    // read-only and a read-write slug) never collide in the sharer's
    // ConnectionTable. Must match the sharer's `relay_virtual_web_client_id`.
    format!("relay-{}-{}", tunnel_id, client_id)
}

fn uniform_unauthorised() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"error":"unauthorised"}"#,
    )
        .into_response()
}

fn store_session(entry: &Arc<TunnelEntry>, viewer_id: Uuid, session: ViewerSession) {
    entry.sessions.lock().unwrap().insert(viewer_id, session);
}

/// First PAKE round-trip: forward the viewer's SPAKE2 message to the sharer
/// and await its `PakeResponse`.
async fn challenge_pake(
    entry: &Arc<TunnelEntry>,
    request_id: &[u8],
    viewer_msg: Vec<u8>,
    link_id: Vec<u8>,
) -> Option<PakeResponseResult> {
    let (tx, rx) = oneshot::channel();
    entry
        .pending_pake_responses
        .lock()
        .unwrap()
        .insert(request_id.to_vec(), tx);

    let frame = ControlMessage::PakeChallenge {
        request_id: request_id.to_vec(),
        viewer_msg,
        link_id,
    };
    if entry.control_tx.send(frame.encode()).is_err() {
        entry
            .pending_pake_responses
            .lock()
            .unwrap()
            .remove(request_id);
        return None;
    }

    match timeout(Duration::from_secs(PAKE_ROUND_TRIP_TIMEOUT_SECS), rx).await {
        Ok(Ok(resp)) => Some(resp),
        _ => {
            entry
                .pending_pake_responses
                .lock()
                .unwrap()
                .remove(request_id);
            None
        },
    }
}

/// Second PAKE round-trip: forward the viewer's confirmation tag and await
/// the sharer's `PakeResult`. Returns whether the sharer accepted.
async fn confirm_pake(
    entry: &Arc<TunnelEntry>,
    request_id: Vec<u8>,
    viewer_confirm: Vec<u8>,
) -> Option<bool> {
    let (tx, rx) = oneshot::channel();
    entry
        .pending_pake_results
        .lock()
        .unwrap()
        .insert(request_id.clone(), tx);

    let frame = ControlMessage::PakeConfirm {
        request_id: request_id.clone(),
        viewer_confirm,
    };
    if entry.control_tx.send(frame.encode()).is_err() {
        entry.pending_pake_results.lock().unwrap().remove(&request_id);
        return None;
    }

    match timeout(Duration::from_secs(PAKE_ROUND_TRIP_TIMEOUT_SECS), rx).await {
        Ok(Ok(accepted)) => Some(accepted),
        _ => {
            entry.pending_pake_results.lock().unwrap().remove(&request_id);
            None
        },
    }
}

pub async fn post_login(
    AxumPath(slug): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Response {
    // Honest-relay defense-in-depth: rate-limit logins per client IP.
    let ip = client_ip(&headers);
    if !state.login_attempt_allowed(&ip) {
        tracing::info!(slug = %slug, ip = %ip, "login rate-limited");
        return uniform_unauthorised();
    }
    let Some(entry) = state.registry.get(&slug) else {
        return uniform_unauthorised();
    };

    // Uniform viewer cap: live viewers + in-flight handshakes.
    {
        let live = entry.client_id_to_viewer.lock().unwrap().len();
        let pending = entry.pending_handshakes.lock().unwrap().len();
        if live + pending >= MAX_VIEWERS {
            tracing::info!(slug = %slug, cap = MAX_VIEWERS, "viewer cap hit — rejecting registration");
            return uniform_unauthorised();
        }
    }

    let request_id = Uuid::new_v4().as_bytes().to_vec();
    let Some(resp) = challenge_pake(&entry, &request_id, req.viewer_msg, req.link_id).await else {
        return uniform_unauthorised();
    };
    if !resp.accepted {
        return uniform_unauthorised();
    }

    let viewer_id = Uuid::new_v4();
    entry.pending_handshakes.lock().unwrap().insert(
        viewer_id,
        PendingHandshake {
            request_id,
            client_id: resp.client_id,
        },
    );

    Json(LoginResponse {
        success: true,
        sharer_msg: resp.sharer_msg,
        sharer_confirm: resp.sharer_confirm,
        handshake: viewer_id.to_string(),
    })
    .into_response()
}

fn session_response(entry: &Arc<TunnelEntry>, session: &ViewerSession) -> Response {
    Json(SessionResponse {
        web_client_id: virtual_web_client_id(&entry.tunnel_id.to_string(), session.client_id),
        client_id: session.client_id,
        is_read_only: session.is_read_only,
        e2e_encrypted: true,
        tunnel_id: entry.tunnel_id.to_string(),
    })
    .into_response()
}

pub async fn post_session(
    AxumPath(slug): AxumPath<String>,
    State(state): State<AppState>,
    request: Request,
) -> Response {
    let Some(entry) = state.registry.get(&slug) else {
        return uniform_unauthorised();
    };

    let Some(viewer_id) = handshake_viewer_id(request.headers()) else {
        return uniform_unauthorised();
    };

    // Idempotent re-POST after a finalised handshake.
    if let Some(session) = entry.sessions.lock().unwrap().get(&viewer_id).cloned() {
        return session_response(&entry, &session);
    }

    let Some(pending) = entry
        .pending_handshakes
        .lock()
        .unwrap()
        .get(&viewer_id)
        .cloned()
    else {
        return uniform_unauthorised();
    };

    let (_parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, 16 * 1024).await {
        Ok(b) => b,
        Err(_) => return uniform_unauthorised(),
    };
    let Ok(req) = serde_json::from_slice::<SessionRequest>(&bytes) else {
        return uniform_unauthorised();
    };

    let accepted = confirm_pake(&entry, pending.request_id.clone(), req.viewer_confirm).await;
    match accepted {
        Some(true) => {},
        _ => {
            entry.pending_handshakes.lock().unwrap().remove(&viewer_id);
            return uniform_unauthorised();
        },
    }

    entry.pending_handshakes.lock().unwrap().remove(&viewer_id);
    entry
        .client_id_to_viewer
        .lock()
        .unwrap()
        .insert(pending.client_id, viewer_id);
    let session = ViewerSession {
        viewer_id,
        client_id: pending.client_id,
        is_read_only: entry.read_only,
    };
    store_session(&entry, viewer_id, session.clone());
    session_response(&entry, &session)
}

/// Best-effort client IP from the forwarding headers a fronting proxy sets.
/// Falls back to a shared `"unknown"` bucket on direct connections.
fn client_ip(headers: &HeaderMap) -> String {
    for h in ["x-forwarded-for", "x-real-ip"] {
        if let Some(v) = headers.get(h).and_then(|v| v.to_str().ok()) {
            if let Some(first) = v.split(',').next() {
                let ip = first.trim();
                if !ip.is_empty() {
                    return ip.to_string();
                }
            }
        }
    }
    "unknown".to_string()
}

fn handshake_viewer_id(headers: &HeaderMap) -> Option<Uuid> {
    let id = headers.get(HANDSHAKE_HEADER)?.to_str().ok()?;
    Uuid::parse_str(id).ok()
}

#[derive(Deserialize, Default)]
pub struct WsQuery {
    #[serde(default)]
    handshake: String,
}

fn resolve_session(entry: &Arc<TunnelEntry>, handshake: &str) -> Option<ViewerSession> {
    let viewer_id = Uuid::parse_str(handshake).ok()?;
    entry.sessions.lock().unwrap().get(&viewer_id).cloned()
}

fn origin_rejected(state: &AppState, headers: &HeaderMap) -> bool {
    match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        Some(origin) => !state.origin_allowed(origin),
        None => false,
    }
}

pub async fn ws_terminal(
    AxumPath(slug): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    ws_terminal_inner(slug, state, query, headers, ws)
}

pub async fn ws_terminal_with_session(
    AxumPath((slug, _session)): AxumPath<(String, String)>,
    State(state): State<AppState>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    ws_terminal_inner(slug, state, query, headers, ws)
}

fn ws_terminal_inner(
    slug: String,
    state: AppState,
    query: WsQuery,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if origin_rejected(&state, &headers) {
        return (StatusCode::FORBIDDEN, "forbidden origin").into_response();
    }
    let Some(entry) = state.registry.get(&slug) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Some(session) = resolve_session(&entry, &query.handshake) else {
        return (StatusCode::UNAUTHORIZED, "unauthorised").into_response();
    };
    ws.on_upgrade(move |socket| handle_viewer_terminal(socket, entry, session))
}

pub async fn ws_control(
    AxumPath(slug): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if origin_rejected(&state, &headers) {
        return (StatusCode::FORBIDDEN, "forbidden origin").into_response();
    }
    let Some(entry) = state.registry.get(&slug) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Some(session) = resolve_session(&entry, &query.handshake) else {
        return (StatusCode::UNAUTHORIZED, "unauthorised").into_response();
    };
    ws.on_upgrade(move |socket| handle_viewer_control(socket, entry, session))
}

async fn handle_viewer_terminal(
    socket: WebSocket,
    entry: Arc<TunnelEntry>,
    session: ViewerSession,
) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
    let (disconnect_tx, mut disconnect_rx) = oneshot::channel::<()>();

    {
        let mut viewers = entry.viewers.lock().unwrap();
        let handle = viewers
            .entry(session.viewer_id)
            .or_insert_with(ViewerHandle::default);
        handle.is_read_only = session.is_read_only;
        let backlog = std::mem::take(&mut handle.terminal_backlog);
        for msg in backlog {
            let _ = out_tx.send(msg);
        }
        handle.terminal_sink_tx = Some(out_tx);
        handle.disconnect_terminal = Some(disconnect_tx);
        entry.viewer_count.set_count(viewers.len());
    }

    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    let reader_entry = entry.clone();
    let reader_session = session.clone();
    let reader = tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            let bytes = match frame {
                Ok(Message::Binary(b)) => b.to_vec(),
                Ok(Message::Text(t)) => t.as_bytes().to_vec(),
                Ok(Message::Close(_)) => break,
                Ok(_) => continue,
                Err(_) => break,
            };
            if reader_session.is_read_only {
                // Input gating: read-only viewers must never inject stdin.
                tracing::warn!(
                    slug = %reader_entry.slug,
                    viewer_id = %reader_session.viewer_id,
                    bytes = bytes.len(),
                    "r/o viewer attempted input — dropped"
                );
                continue;
            }
            let tm = TerminalMessage::TerminalFrameData {
                client_id: reader_session.client_id,
                data: bytes,
            };
            let tx_clone = reader_entry.terminal_tx.lock().unwrap().clone();
            match tx_clone {
                Some(tx) => {
                    if tx.send(tm.encode()).is_err() {
                        break;
                    }
                },
                None => {
                    tracing::warn!(
                        slug = %reader_entry.slug,
                        "terminal tunnel not yet linked; dropping viewer frame"
                    );
                },
            }
        }
    });

    tokio::select! {
        _ = &mut disconnect_rx => {},
        _ = writer => {},
        _ = reader => {},
    }

    cleanup_viewer(&entry, &session);
}

async fn handle_viewer_control(
    socket: WebSocket,
    entry: Arc<TunnelEntry>,
    session: ViewerSession,
) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
    let (disconnect_tx, mut disconnect_rx) = oneshot::channel::<()>();

    {
        let mut viewers = entry.viewers.lock().unwrap();
        let handle = viewers
            .entry(session.viewer_id)
            .or_insert_with(ViewerHandle::default);
        handle.is_read_only = session.is_read_only;
        handle.control_sink_tx = Some(out_tx);
        handle.disconnect_control = Some(disconnect_tx);
        entry.viewer_count.set_count(viewers.len());
    }

    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    let reader_entry = entry.clone();
    let reader_session = session.clone();
    let reader = tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            let bytes = match frame {
                Ok(Message::Text(t)) => t.as_bytes().to_vec(),
                Ok(Message::Binary(b)) => b.to_vec(),
                Ok(Message::Close(_)) => break,
                Ok(_) => continue,
                Err(_) => break,
            };
            // Control frames (resize, etc.) are forwarded for every viewer,
            // including read-only ones, so each viewer's stream is sized to
            // its own viewport. Actual input is blocked at the terminal
            // channel above and again server-side by the read-only client
            // flag, so forwarding control here cannot inject keystrokes.
            let cm = ControlMessage::ControlFrameData {
                client_id: reader_session.client_id,
                data: bytes,
            };
            if reader_entry.control_tx.send(cm.encode()).is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = &mut disconnect_rx => {},
        _ = writer => {},
        _ = reader => {},
    }

    cleanup_viewer(&entry, &session);
}

fn cleanup_viewer(entry: &Arc<TunnelEntry>, session: &ViewerSession) {
    let should_remove = {
        let mut viewers = entry.viewers.lock().unwrap();
        if let Some(handle) = viewers.get_mut(&session.viewer_id) {
            handle.control_sink_tx = None;
            handle.terminal_sink_tx = None;
        }
        viewers
            .get(&session.viewer_id)
            .map(|h| h.control_sink_tx.is_none() && h.terminal_sink_tx.is_none())
            .unwrap_or(false)
    };
    if !should_remove {
        return;
    }
    {
        let mut viewers = entry.viewers.lock().unwrap();
        viewers.remove(&session.viewer_id);
        entry.viewer_count.set_count(viewers.len());
    }
    entry.sessions.lock().unwrap().remove(&session.viewer_id);
    entry
        .client_id_to_viewer
        .lock()
        .unwrap()
        .remove(&session.client_id);
    let _ = entry.control_tx.send(
        ControlMessage::ClientDisconnected {
            client_id: session.client_id,
        }
        .encode(),
    );
}
