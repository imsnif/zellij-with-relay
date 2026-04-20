use super::config::{LOGIN_ENDPOINT, SESSION_ENDPOINT};
use super::http_client::HttpClientWithCookies;
use crate::RemoteClientError;
use isahc::{AsyncReadResponseExt, Request};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct LoginRequest {
    auth_token: String,
    remember_me: bool,
}

#[derive(Deserialize)]
pub struct SessionResponse {
    pub web_client_id: String,
    /// Whether the server will encrypt TerminalFrameData payloads on this
    /// connection. Absent for pre-Phase-3 servers; treated as `false`.
    #[serde(default)]
    pub e2e_encrypted: bool,
    /// HKDF `info` parameter for per-client key derivation. Absent for
    /// pre-Phase-3 servers.
    #[serde(default)]
    pub tunnel_id: Option<String>,
}

/// Bundle returned to the attach caller: enough to establish WS
/// connections plus the E2E state needed to encrypt/decrypt frames.
pub struct AuthResult {
    pub web_client_id: String,
    pub http_client: HttpClientWithCookies,
    /// Set when `--remember` was passed and the server set a cookie.
    /// Carries both the cookie name (may be `session_token` or
    /// `relay_session`) and value so the attach client can restore it.
    pub remembered: Option<RememberedCookie>,
    pub e2e_encrypted: bool,
    pub tunnel_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RememberedCookie {
    pub name: String,
    pub value: String,
}

pub async fn authenticate(
    server_base_url: &str,
    auth_token: &str,
    remember_me: bool,
    ca_cert: Option<&std::path::Path>,
    insecure: bool,
) -> Result<AuthResult, RemoteClientError> {
    let http_client = HttpClientWithCookies::new(ca_cert, insecure)
        .map_err(|e| RemoteClientError::Other(Box::new(e)))?;

    // Step 1: Login with auth token
    let login_url = format!("{}{}", server_base_url, LOGIN_ENDPOINT);

    let login_request = LoginRequest {
        auth_token: auth_token.to_string(),
        remember_me,
    };

    let response = http_client
        .send_with_cookies(
            Request::post(login_url)
                .header("Content-Type", "application/json")
                .header("User-Agent", "http-terminal-client/1.0")
                .header("Accept", "application/json")
                .body(
                    serde_json::to_vec(&login_request)
                        .map_err(|e| RemoteClientError::Other(Box::new(e)))?,
                )
                .map_err(|e| RemoteClientError::Other(Box::new(e)))?,
        )
        .await
        .map_err(|e| RemoteClientError::ConnectionFailed(e.to_string()))?;

    // Handle HTTP status codes
    match response.status().as_u16() {
        401 => return Err(RemoteClientError::InvalidAuthToken),
        status if !response.status().is_success() => {
            return Err(RemoteClientError::ConnectionFailed(format!(
                "Server returned status {}",
                status
            )));
        },
        _ => {},
    }

    // Step 2: Get session/client ID
    let session_url = format!("{}{}", server_base_url, SESSION_ENDPOINT);

    let mut session_response = http_client
        .send_with_cookies(
            Request::post(session_url)
                .header("Content-Type", "application/json")
                .header("User-Agent", "http-terminal-client/1.0")
                .header("Accept", "application/json")
                .body("{}".as_bytes().to_vec())
                .map_err(|e| RemoteClientError::Other(Box::new(e)))?,
        )
        .await
        .map_err(|e| RemoteClientError::ConnectionFailed(e.to_string()))?;

    // Handle session response
    match session_response.status().as_u16() {
        401 => return Err(RemoteClientError::Unauthorized),
        status if !session_response.status().is_success() => {
            return Err(RemoteClientError::ConnectionFailed(format!(
                "Server returned status {}",
                status
            )));
        },
        _ => {},
    }

    let response_body = session_response
        .text()
        .await
        .map_err(|e| RemoteClientError::Other(Box::new(e)))?;
    let session_data: SessionResponse =
        serde_json::from_str(&response_body).map_err(|e| RemoteClientError::Other(Box::new(e)))?;

    // Prefer the well-known cookie names the local web server and the
    // relay set. We only surface one; any extra cookies stay in the jar
    // for the duration of this run but aren't persisted.
    let remembered = if remember_me {
        first_session_cookie(&http_client)
    } else {
        None
    };

    Ok(AuthResult {
        web_client_id: session_data.web_client_id,
        http_client,
        remembered,
        e2e_encrypted: session_data.e2e_encrypted,
        tunnel_id: session_data.tunnel_id,
    })
}

/// Return the first session-scoped cookie the server set. `session_token`
/// is the local web server's cookie; `relay_session` is the relay's.
fn first_session_cookie(http_client: &HttpClientWithCookies) -> Option<RememberedCookie> {
    for name in &["session_token", "relay_session"] {
        if let Some(value) = http_client.get_cookie(name) {
            return Some(RememberedCookie {
                name: (*name).to_string(),
                value,
            });
        }
    }
    None
}

pub async fn validate_session_token(
    server_base_url: &str,
    cookie_name: &str,
    cookie_value: &str,
    ca_cert: Option<&std::path::Path>,
    insecure: bool,
) -> Result<(SessionResponse, HttpClientWithCookies), RemoteClientError> {
    let http_client = HttpClientWithCookies::new(ca_cert, insecure)
        .map_err(|e| RemoteClientError::Other(Box::new(e)))?;

    // Pre-populate the session cookie (name differs between local web
    // server — `session_token` — and the relay — `relay_session`).
    http_client.set_cookie(cookie_name.to_string(), cookie_value.to_string());

    // Skip /login, go directly to /session endpoint
    let session_url = format!("{}{}", server_base_url, SESSION_ENDPOINT);

    let mut session_response = http_client
        .send_with_cookies(
            Request::post(session_url)
                .header("Content-Type", "application/json")
                .header("User-Agent", "http-terminal-client/1.0")
                .header("Accept", "application/json")
                .body("{}".as_bytes().to_vec())
                .map_err(|e| RemoteClientError::Other(Box::new(e)))?,
        )
        .await
        .map_err(|e| RemoteClientError::ConnectionFailed(e.to_string()))?;

    match session_response.status().as_u16() {
        401 => Err(RemoteClientError::SessionTokenExpired),
        status if !session_response.status().is_success() => Err(
            RemoteClientError::ConnectionFailed(format!("Server returned status {}", status)),
        ),
        _ => {
            let response_body = session_response
                .text()
                .await
                .map_err(|e| RemoteClientError::Other(Box::new(e)))?;
            let session_data: SessionResponse = serde_json::from_str(&response_body)
                .map_err(|e| RemoteClientError::Other(Box::new(e)))?;
            Ok((session_data, http_client))
        },
    }
}
