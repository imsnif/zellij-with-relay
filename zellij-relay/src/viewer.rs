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

use crate::registry::{RoGroup, TunnelEntry, ViewerHandle, ViewerRouting, ViewerSession};
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
    /// Whether the Zellij side will encrypt TerminalFrameData payloads on
    /// this connection. Mirrored from the Zellij `AuthResponse`; on the
    /// relay path this is always `true` in Phase 3+.
    pub e2e_encrypted: bool,
    /// HKDF `info` parameter for per-client E2E key derivation. The
    /// browser recomputes the same key locally from this value + its
    /// typed raw auth token's SHA-256 hash.
    pub tunnel_id: String,
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
    // The relay path is unconditionally E2E-encrypted in Phase 3+. The
    // same string is served for valid and unknown slugs — enumeration
    // safety is preserved because the encryption-state claim is a global
    // policy, not per-slug information.
    let html = zellij_web_client_assets::INDEX_HTML
        .replace("IS_AUTHENTICATED", "false")
        .replace("EXPECTED_E2E", "true")
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

/// Outcome of resolving a new viewer's auth token against the tunnel.
/// Either returns the joined `ViewerSession` (ready to be returned to
/// the browser) or `None` for a uniform-401 fallthrough.
struct RegisterOutcome {
    cookie: Cookie<'static>,
    session: ViewerSession,
    client_id: u32,
}

fn finalise_ro_first(
    entry: &Arc<TunnelEntry>,
    slug: &str,
    token_hash: &str,
    client_id: u32,
    e2e_encrypted: bool,
) -> Option<RegisterOutcome> {
    let viewer_id = Uuid::new_v4();
    let mut viewer_ids = std::collections::HashSet::new();
    viewer_ids.insert(viewer_id);
    entry.ro_groups.lock().unwrap().insert(
        token_hash.to_string(),
        RoGroup {
            validated: true,
            active_client_id: Some(client_id),
            e2e_encrypted,
            viewer_ids,
        },
    );
    entry
        .client_id_to_token_hash
        .lock()
        .unwrap()
        .insert(client_id, token_hash.to_string());
    let session = ViewerSession {
        viewer_id,
        routing: ViewerRouting::Ro(token_hash.to_string()),
        token_hash: token_hash.to_string(),
        is_read_only: true,
        e2e_encrypted,
    };
    let cookie = build_session_cookie(slug, viewer_id);
    store_session(entry, viewer_id, session.clone());
    emit_ro_viewer_update(entry, token_hash);
    Some(RegisterOutcome {
        cookie,
        session,
        client_id,
    })
}

fn finalise_ro_join(
    entry: &Arc<TunnelEntry>,
    slug: &str,
    token_hash: &str,
    group: RoGroup,
) -> Option<RegisterOutcome> {
    let viewer_id = Uuid::new_v4();
    let client_id = group.active_client_id?;
    {
        let mut groups = entry.ro_groups.lock().unwrap();
        let entry_mut = groups.get_mut(token_hash)?;
        entry_mut.viewer_ids.insert(viewer_id);
    }
    let session = ViewerSession {
        viewer_id,
        routing: ViewerRouting::Ro(token_hash.to_string()),
        token_hash: token_hash.to_string(),
        is_read_only: true,
        e2e_encrypted: group.e2e_encrypted,
    };
    let cookie = build_session_cookie(slug, viewer_id);
    store_session(entry, viewer_id, session.clone());
    emit_ro_viewer_update(entry, token_hash);
    Some(RegisterOutcome {
        cookie,
        session,
        client_id,
    })
}

fn finalise_ro_revive(
    entry: &Arc<TunnelEntry>,
    slug: &str,
    token_hash: &str,
    client_id: u32,
    e2e_encrypted: bool,
) -> Option<RegisterOutcome> {
    let viewer_id = Uuid::new_v4();
    {
        let mut groups = entry.ro_groups.lock().unwrap();
        let group = groups.get_mut(token_hash)?;
        group.active_client_id = Some(client_id);
        group.e2e_encrypted = e2e_encrypted;
        group.viewer_ids.insert(viewer_id);
    }
    entry
        .client_id_to_token_hash
        .lock()
        .unwrap()
        .insert(client_id, token_hash.to_string());
    let session = ViewerSession {
        viewer_id,
        routing: ViewerRouting::Ro(token_hash.to_string()),
        token_hash: token_hash.to_string(),
        is_read_only: true,
        e2e_encrypted,
    };
    let cookie = build_session_cookie(slug, viewer_id);
    store_session(entry, viewer_id, session.clone());
    emit_ro_viewer_update(entry, token_hash);
    Some(RegisterOutcome {
        cookie,
        session,
        client_id,
    })
}

fn store_session(entry: &Arc<TunnelEntry>, viewer_id: Uuid, session: ViewerSession) {
    entry.sessions.lock().unwrap().insert(viewer_id, session);
}

fn build_session_cookie(slug: &str, viewer_id: Uuid) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, viewer_id.to_string()))
        .http_only(true)
        .same_site(SameSite::Strict)
        .path(format!("/r/{}/", slug))
        .build()
}

fn emit_ro_viewer_update(entry: &Arc<TunnelEntry>, token_hash: &str) {
    let count = entry
        .ro_groups
        .lock()
        .unwrap()
        .get(token_hash)
        .map(|g| g.viewer_ids.len())
        .unwrap_or(0) as u32;
    let _ = entry.control_tx.send(
        ControlMessage::ReadOnlyViewerUpdate {
            token_hash: token_hash.to_string(),
            count,
        }
        .encode(),
    );
}

async fn challenge_once(
    entry: &Arc<TunnelEntry>,
    token_hash: &str,
) -> Option<crate::registry::AuthResponseResult> {
    let request_id = Uuid::new_v4().as_bytes().to_vec();
    let (tx, rx) = oneshot::channel();
    entry
        .pending_auths
        .lock()
        .unwrap()
        .insert(request_id.clone(), tx);

    let frame = ControlMessage::AuthChallenge {
        request_id: request_id.clone(),
        token_hash: token_hash.to_string(),
    };
    if entry.control_tx.send(frame.encode()).is_err() {
        entry.pending_auths.lock().unwrap().remove(&request_id);
        return None;
    }

    match timeout(Duration::from_secs(AUTH_CHALLENGE_TIMEOUT_SECS), rx).await {
        Ok(Ok(resp)) => Some(resp),
        _ => {
            entry.pending_auths.lock().unwrap().remove(&request_id);
            None
        },
    }
}

/// Drive the whole register-a-viewer flow.
///
/// * Already-cached r/o token: short-circuit — either join the live
///   fan-out group, or re-challenge if the group went dormant.
/// * Otherwise: one AuthChallenge round-trip. On `is_read_only: true`,
///   open a fresh r/o group. On `is_read_only: false`, announce the
///   client with `ClientConnected` as the r/w path.
async fn register_viewer(
    entry: &Arc<TunnelEntry>,
    auth_token: &str,
    slug: &str,
) -> Option<RegisterOutcome> {
    let token_hash = hash_auth_token(auth_token);
    // Bind the cached lookup to a local so the MutexGuard temporary does
    // not live across the `.await` inside the dormant-group branch.
    let cached_group: Option<RoGroup> = entry
        .ro_groups
        .lock()
        .unwrap()
        .get(&token_hash)
        .cloned();
    if let Some(group) = cached_group {
        if group.active_client_id.is_some() {
            return finalise_ro_join(entry, slug, &token_hash, group);
        }
        // Dormant group: re-challenge to allocate a fresh client_id.
        let resp = challenge_once(entry, &token_hash).await?;
        if !resp.accepted || !resp.is_read_only {
            return None;
        }
        return finalise_ro_revive(entry, slug, &token_hash, resp.client_id, resp.e2e_encrypted);
    }

    let resp = challenge_once(entry, &token_hash).await?;
    if !resp.accepted {
        return None;
    }
    if resp.is_read_only {
        return finalise_ro_first(entry, slug, &token_hash, resp.client_id, resp.e2e_encrypted);
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
    let viewer_id = Uuid::new_v4();
    entry
        .client_id_to_viewer
        .lock()
        .unwrap()
        .insert(resp.client_id, viewer_id);
    let session = ViewerSession {
        viewer_id,
        routing: ViewerRouting::Rw(resp.client_id),
        token_hash,
        is_read_only: false,
        e2e_encrypted: resp.e2e_encrypted,
    };
    let cookie = build_session_cookie(slug, viewer_id);
    store_session(entry, viewer_id, session.clone());
    Some(RegisterOutcome {
        cookie,
        session,
        client_id: resp.client_id,
    })
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
    match register_viewer(&entry, &req.auth_token, &slug).await {
        Some(RegisterOutcome { cookie, .. }) => {
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
        let client_id = match &session.routing {
            ViewerRouting::Rw(id) => *id,
            ViewerRouting::Ro(token_hash) => entry
                .ro_groups
                .lock()
                .unwrap()
                .get(token_hash)
                .and_then(|g| g.active_client_id)
                .unwrap_or(0),
        };
        let body = SessionResponse {
            web_client_id: virtual_web_client_id(client_id),
            client_id,
            is_read_only: session.is_read_only,
            e2e_encrypted: session.e2e_encrypted,
            tunnel_id: entry.tunnel_id.to_string(),
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

    match register_viewer(&entry, &auth_token, &slug).await {
        Some(RegisterOutcome {
            cookie,
            session,
            client_id,
        }) => {
            let body = SessionResponse {
                web_client_id: virtual_web_client_id(client_id),
                client_id,
                is_read_only: session.is_read_only,
                e2e_encrypted: session.e2e_encrypted,
                tunnel_id: entry.tunnel_id.to_string(),
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
    let viewer_id = Uuid::parse_str(id).ok()?;
    entry.sessions.lock().unwrap().get(&viewer_id).cloned()
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
            .entry(session.viewer_id)
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
            match &reader_session.routing {
                ViewerRouting::Ro(_) => {
                    // Defense-in-depth: r/o viewers must never inject
                    // input. Drain and drop.
                    tracing::warn!(
                        slug = %reader_entry.slug,
                        viewer_id = %reader_session.viewer_id,
                        bytes = bytes.len(),
                        "r/o viewer attempted input — dropped"
                    );
                    continue;
                },
                ViewerRouting::Rw(client_id) => {
                    let tm = TerminalMessage::TerminalFrameData {
                        client_id: *client_id,
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
            match &reader_session.routing {
                ViewerRouting::Ro(_) => {
                    // r/o control messages are also ignored: no resize
                    // propagates to Zellij (all clipping is client-side).
                    tracing::debug!(
                        slug = %reader_entry.slug,
                        viewer_id = %reader_session.viewer_id,
                        "r/o viewer control frame dropped"
                    );
                    continue;
                },
                ViewerRouting::Rw(client_id) => {
                    let cm = ControlMessage::ControlFrameData {
                        client_id: *client_id,
                        data: bytes,
                    };
                    if reader_entry.control_tx.send(cm.encode()).is_err() {
                        break;
                    }
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
    }
    entry.sessions.lock().unwrap().remove(&session.viewer_id);
    match &session.routing {
        ViewerRouting::Rw(client_id) => {
            entry
                .client_id_to_viewer
                .lock()
                .unwrap()
                .remove(client_id);
            let _ = entry.control_tx.send(
                ControlMessage::ClientDisconnected {
                    client_id: *client_id,
                }
                .encode(),
            );
        },
        ViewerRouting::Ro(token_hash) => {
            let (active_client_id, new_count) = {
                let mut groups = entry.ro_groups.lock().unwrap();
                if let Some(group) = groups.get_mut(token_hash) {
                    group.viewer_ids.remove(&session.viewer_id);
                    let count = group.viewer_ids.len();
                    let acid = if count == 0 {
                        group.active_client_id.take()
                    } else {
                        None
                    };
                    (acid, count as u32)
                } else {
                    (None, 0)
                }
            };
            if let Some(cid) = active_client_id {
                entry
                    .client_id_to_token_hash
                    .lock()
                    .unwrap()
                    .remove(&cid);
            }
            let _ = entry.control_tx.send(
                ControlMessage::ReadOnlyViewerUpdate {
                    token_hash: token_hash.clone(),
                    count: new_count,
                }
                .encode(),
            );
        },
    }
}
