//! Viewer-facing HTTP + WebSocket surface for `/r/:slug/...`.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path as AxumPath, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    Json,
};
use axum::extract::Request;
use axum_extra::extract::cookie::{Cookie, SameSite};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use uuid::Uuid;
use zellij_relay_protocol::{ControlMessage, TerminalMessage};

use crate::registry::{TunnelEntry, ViewerHandle, ViewerSession};
use crate::router::AppState;

const AUTH_CHALLENGE_TIMEOUT_SECS: u64 = 5;
const SESSION_COOKIE: &str = "relay_session";

#[derive(Deserialize, Default)]
pub struct LoginRequest {
    pub auth_token: String,
    #[serde(default)]
    pub remember_me: bool,
}

#[derive(Serialize)]
pub struct LoginResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Deserialize, Default)]
pub struct SessionRequest {
    /// Phase 2: `auth_token` may be supplied directly on the /session call
    /// (matches the plan) or omitted when the relay_session cookie already
    /// exists (matches the browser JS flow of /command/login → /session).
    #[serde(default)]
    pub auth_token: Option<String>,
}

#[derive(Serialize)]
pub struct SessionResponse {
    pub web_client_id: String,
    pub client_id: u32,
    pub is_read_only: bool,
}

fn hash_auth_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn virtual_web_client_id(client_id: u32) -> String {
    format!("relay-client-{}", client_id)
}

fn uniform_unauthorised() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"error":"unauthorised"}"#,
    )
        .into_response()
}

pub async fn serve_html(
    AxumPath(slug): AxumPath<String>,
    State(_state): State<AppState>,
) -> Html<String> {
    let base_url = format!("/r/{}/", slug);
    let html = zellij_web_client_assets::INDEX_HTML
        .replace("IS_AUTHENTICATED", "false")
        .replace("BASE_URL", &base_url);
    Html(html)
}

pub async fn version(
    AxumPath(slug): AxumPath<String>,
    State(state): State<AppState>,
) -> Response {
    let version = state
        .registry
        .get(&slug)
        .map(|e| e.zellij_version.clone())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    ([(header::CONTENT_TYPE, "text/plain")], version).into_response()
}

pub async fn serve_asset(
    AxumPath((_slug, path)): AxumPath<(String, String)>,
) -> Response {
    match zellij_web_client_assets::lookup(&path) {
        None => (StatusCode::NOT_FOUND, "Not Found").into_response(),
        Some(asset) => (
            [(header::CONTENT_TYPE, asset.content_type)],
            asset.contents,
        )
            .into_response(),
    }
}

async fn challenge_and_register(
    entry: &Arc<TunnelEntry>,
    auth_token: &str,
    slug: &str,
) -> Option<(Uuid, Cookie<'static>, ViewerSession)> {
    let token_hash = hash_auth_token(auth_token);
    let request_id = Uuid::new_v4().as_bytes().to_vec();

    let (tx, rx) = oneshot::channel();
    entry
        .pending_auths
        .lock()
        .unwrap()
        .insert(request_id.clone(), tx);

    let frame = ControlMessage::AuthChallenge {
        request_id: request_id.clone(),
        token_hash: token_hash.clone(),
    };
    if entry.control_tx.send(frame.encode()).is_err() {
        entry.pending_auths.lock().unwrap().remove(&request_id);
        return None;
    }

    let resp = match timeout(Duration::from_secs(AUTH_CHALLENGE_TIMEOUT_SECS), rx).await {
        Ok(Ok(resp)) => resp,
        _ => {
            entry.pending_auths.lock().unwrap().remove(&request_id);
            return None;
        },
    };

    if !resp.accepted {
        return None;
    }

    if entry
        .control_tx
        .send(
            ControlMessage::ClientConnected {
                client_id: resp.client_id,
            }
            .encode(),
        )
        .is_err()
    {
        return None;
    }

    let session_id = Uuid::new_v4();
    let session = ViewerSession {
        client_id: resp.client_id,
        token_hash,
        is_read_only: resp.is_read_only,
    };
    entry.sessions.lock().unwrap().insert(session_id, session.clone());

    let cookie = Cookie::build((SESSION_COOKIE, session_id.to_string()))
        .http_only(true)
        .same_site(SameSite::Strict)
        .path(format!("/r/{}/", slug))
        .build();

    Some((session_id, cookie, session))
}

/// `POST /r/:slug/command/login` — first half of the browser auth flow.
/// Performs the AuthChallenge round-trip, sets the `relay_session` cookie,
/// and returns a 200 LoginResponse. Uniform 401 on any failure.
pub async fn post_login(
    AxumPath(slug): AxumPath<String>,
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Response {
    let Some(entry) = state.registry.get(&slug) else {
        return uniform_unauthorised();
    };
    match challenge_and_register(&entry, &req.auth_token, &slug).await {
        Some((_session_id, cookie, _session)) => {
            let mut response = Json(LoginResponse {
                success: true,
                message: "Login successful".to_string(),
            })
            .into_response();
            if let Ok(cookie_header) =
                axum::http::HeaderValue::from_str(&cookie.to_string())
            {
                response
                    .headers_mut()
                    .insert(header::SET_COOKIE, cookie_header);
            }
            response
        },
        None => uniform_unauthorised(),
    }
}

/// `POST /r/:slug/session` — either a plan-style one-shot (auth_token in
/// body) or a two-step flow where the browser first called /command/login
/// and now presents the cookie. Returns the Zellij-allocated client_id.
pub async fn post_session(
    AxumPath(slug): AxumPath<String>,
    State(state): State<AppState>,
    request: Request,
) -> Response {
    let Some(entry) = state.registry.get(&slug) else {
        return uniform_unauthorised();
    };

    // Try the cookie first: this is the JS flow's second step.
    if let Some(session) = resolve_session(&entry, request.headers()) {
        let body = SessionResponse {
            web_client_id: virtual_web_client_id(session.client_id),
            client_id: session.client_id,
            is_read_only: session.is_read_only,
        };
        return Json(body).into_response();
    }

    // Otherwise fall back to body-form `{auth_token}` (plan form).
    let (_parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, 16 * 1024).await {
        Ok(b) => b,
        Err(_) => return uniform_unauthorised(),
    };
    let Ok(req) = serde_json::from_slice::<SessionRequest>(&bytes) else {
        return uniform_unauthorised();
    };
    let Some(auth_token) = req.auth_token else {
        return uniform_unauthorised();
    };

    match challenge_and_register(&entry, &auth_token, &slug).await {
        Some((_session_id, cookie, session)) => {
            let body = SessionResponse {
                web_client_id: virtual_web_client_id(session.client_id),
                client_id: session.client_id,
                is_read_only: session.is_read_only,
            };
            let mut response = Json(body).into_response();
            if let Ok(cookie_header) =
                axum::http::HeaderValue::from_str(&cookie.to_string())
            {
                response
                    .headers_mut()
                    .insert(header::SET_COOKIE, cookie_header);
            }
            response
        },
        None => uniform_unauthorised(),
    }
}

fn lookup_session_id(entry: &Arc<TunnelEntry>, client_id: u32) -> Option<Uuid> {
    entry
        .sessions
        .lock()
        .unwrap()
        .iter()
        .find(|(_, s)| s.client_id == client_id)
        .map(|(id, _)| *id)
}

fn parse_cookies(headers: &HeaderMap) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for value in headers.get_all(header::COOKIE) {
        if let Ok(s) = value.to_str() {
            for part in s.split(';') {
                if let Ok(c) = Cookie::parse(part.trim().to_owned()) {
                    out.insert(c.name().to_owned(), c.value().to_owned());
                }
            }
        }
    }
    out
}

fn resolve_session(entry: &Arc<TunnelEntry>, headers: &HeaderMap) -> Option<ViewerSession> {
    let cookies = parse_cookies(headers);
    let id = cookies.get(SESSION_COOKIE)?;
    let session_id = Uuid::parse_str(id).ok()?;
    entry.sessions.lock().unwrap().get(&session_id).cloned()
}

pub async fn ws_terminal(
    AxumPath(slug): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    ws_terminal_inner(slug, state, headers, ws)
}

pub async fn ws_terminal_with_session(
    AxumPath((slug, _session)): AxumPath<(String, String)>,
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    ws_terminal_inner(slug, state, headers, ws)
}

fn ws_terminal_inner(
    slug: String,
    state: AppState,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(entry) = state.registry.get(&slug) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Some(session) = resolve_session(&entry, &headers) else {
        return (StatusCode::UNAUTHORIZED, "unauthorised").into_response();
    };
    ws.on_upgrade(move |socket| handle_viewer_terminal(socket, entry, session))
}

pub async fn ws_control(
    AxumPath(slug): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(entry) = state.registry.get(&slug) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Some(session) = resolve_session(&entry, &headers) else {
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
            .entry(session.client_id)
            .or_insert_with(ViewerHandle::default);
        handle.is_read_only = session.is_read_only;
        handle.terminal_sink_tx = Some(out_tx);
        handle.disconnect_terminal = Some(disconnect_tx);
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
    let client_id = session.client_id;
    let reader = tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            let bytes = match frame {
                Ok(Message::Binary(b)) => b.to_vec(),
                Ok(Message::Text(t)) => t.as_bytes().to_vec(),
                Ok(Message::Close(_)) => break,
                Ok(_) => continue,
                Err(_) => break,
            };
            let tm = TerminalMessage::TerminalFrameData {
                client_id,
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

    cleanup_viewer(&entry, client_id);
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
            .entry(session.client_id)
            .or_insert_with(ViewerHandle::default);
        handle.is_read_only = session.is_read_only;
        handle.control_sink_tx = Some(out_tx);
        handle.disconnect_control = Some(disconnect_tx);
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
    let client_id = session.client_id;
    let reader = tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            let bytes = match frame {
                Ok(Message::Text(t)) => t.as_bytes().to_vec(),
                Ok(Message::Binary(b)) => b.to_vec(),
                Ok(Message::Close(_)) => break,
                Ok(_) => continue,
                Err(_) => break,
            };
            let cm = ControlMessage::ControlFrameData {
                client_id,
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

    cleanup_viewer(&entry, client_id);
}

fn cleanup_viewer(entry: &Arc<TunnelEntry>, client_id: u32) {
    let mut viewers = entry.viewers.lock().unwrap();
    if let Some(handle) = viewers.get_mut(&client_id) {
        handle.control_sink_tx = None;
        handle.terminal_sink_tx = None;
    }
    // If neither side is active, remove and notify Zellij.
    let should_remove = viewers
        .get(&client_id)
        .map(|h| h.control_sink_tx.is_none() && h.terminal_sink_tx.is_none())
        .unwrap_or(false);
    if should_remove {
        viewers.remove(&client_id);
        drop(viewers);
        let _ = entry
            .control_tx
            .send(ControlMessage::ClientDisconnected { client_id }.encode());
    }
}
