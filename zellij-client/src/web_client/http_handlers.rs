use crate::web_client::authentication::{AuthTokenHash, IsReadOnly, SessionTokenHash};
use crate::web_client::types::{AppState, CreateClientIdResponse, LoginRequest, LoginResponse};
use crate::web_client::utils::parse_cookies;
use axum::{
    extract::{Path as AxumPath, Request, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    Json,
};
use axum_extra::extract::cookie::{Cookie, SameSite};
use uuid::Uuid;
use zellij_relay_protocol::crypto;
use zellij_utils::{consts::VERSION, web_authentication_tokens::create_session_token};

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

pub async fn serve_html(State(state): State<AppState>, request: Request) -> Html<String> {
    let cookies = parse_cookies(&request);
    let is_authenticated = cookies.get("session_token").is_some();
    let auth_value = if is_authenticated { "true" } else { "false" };
    let base_url = html_escape(
        &state
            .config
            .lock()
            .unwrap()
            .web_client
            .base_url
            .clone()
            .unwrap_or("/".to_string()),
    );
    let expected_e2e = if state.encrypt_web_sharing {
        "true"
    } else {
        "false"
    };

    // Local web clients are not relay-fan-out viewers, so the clipper is
    // never instantiated. Stamp sentinels; the /session JSON is
    // authoritative.
    let html = Html(
        zellij_web_client_assets::INDEX_HTML
            .replace("IS_AUTHENTICATED", &format!("{}", auth_value))
            .replace("EXPECTED_E2E", expected_e2e)
            .replace("IS_READ_ONLY", "false")
            .replace("SESSION_ROWS", "0")
            .replace("SESSION_COLS", "0")
            .replace("AUTH_MODE", "local")
            .replace("BASE_URL", &base_url),
    );
    html
}

pub async fn login_handler(
    State(state): State<AppState>,
    Json(login_request): Json<LoginRequest>,
) -> impl IntoResponse {
    match create_session_token(
        &login_request.auth_token,
        login_request.remember_me.unwrap_or(false),
    ) {
        Ok(session_token) => {
            let is_https = state.is_https;
            let cookie = if login_request.remember_me.unwrap_or(false) {
                // Persistent cookie for remember_me
                Cookie::build(("session_token", session_token))
                    .http_only(true)
                    .secure(is_https)
                    .same_site(SameSite::Strict)
                    .path("/")
                    .max_age(time::Duration::weeks(4))
                    .build()
            } else {
                // Session cookie - NO max_age means it expires when browser closes/refreshes
                Cookie::build(("session_token", session_token))
                    .http_only(true)
                    .secure(is_https)
                    .same_site(SameSite::Strict)
                    .path("/")
                    .build()
            };

            let mut response = Json(LoginResponse {
                success: true,
                message: "Login successful".to_string(),
            })
            .into_response();

            if let Ok(cookie_header) = axum::http::HeaderValue::from_str(&cookie.to_string()) {
                response.headers_mut().insert("set-cookie", cookie_header);
            }

            response
        },
        Err(_) => (
            StatusCode::UNAUTHORIZED,
            Json(LoginResponse {
                success: false,
                message: "Invalid authentication token".to_string(),
            }),
        )
            .into_response(),
    }
}

pub async fn create_new_client(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Result<Json<CreateClientIdResponse>, (StatusCode, impl IntoResponse)> {
    // Extract is_read_only from request extensions (set by auth middleware)
    let is_read_only = request
        .extensions()
        .get::<IsReadOnly>()
        .copied()
        .unwrap_or(IsReadOnly(true))
        .0;
    let session_token_hash = request
        .extensions()
        .get::<SessionTokenHash>()
        .cloned()
        .ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json("Missing session info".to_string()),
        ))?;
    let auth_token_hash = request.extensions().get::<AuthTokenHash>().cloned();

    let web_client_id = String::from(Uuid::new_v4());
    let os_input = state
        .client_os_api_factory
        .create_client_os_api()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(e.to_string())))?;

    state.connection_table.lock().unwrap().add_new_client(
        web_client_id.to_owned(),
        os_input,
        is_read_only,
        session_token_hash.0,
    );

    // Derive + stash the per-client E2E key when the opt-in is on. Missing
    // `auth_token_hash` here means the auth middleware could not look it
    // up (DB miss); fall back to plaintext rather than reject the session.
    let e2e_encrypted = if state.encrypt_web_sharing {
        match auth_token_hash {
            Some(h) => {
                let key = crypto::derive_key(&h.0, &state.local_tunnel_id);
                state
                    .connection_table
                    .lock()
                    .unwrap()
                    .set_client_e2e_key(&web_client_id, key);
                true
            },
            None => {
                log::warn!(
                    "encrypt_web_sharing enabled but no auth_token_hash for client {} — falling back to plaintext",
                    web_client_id
                );
                false
            },
        }
    } else {
        false
    };

    Ok(Json(CreateClientIdResponse {
        web_client_id,
        is_read_only,
        e2e_encrypted,
        tunnel_id: state.local_tunnel_id.clone(),
        session_rows: 0,
        session_cols: 0,
    }))
}

pub async fn get_static_asset(AxumPath(path): AxumPath<String>) -> impl IntoResponse {
    match zellij_web_client_assets::lookup(&path) {
        None => (
            [(header::CONTENT_TYPE, "text/html")],
            "Not Found".as_bytes(),
        ),
        Some(asset) => (
            [(header::CONTENT_TYPE, asset.content_type)],
            asset.contents,
        ),
    }
}

pub async fn version_handler() -> &'static str {
    VERSION
}
