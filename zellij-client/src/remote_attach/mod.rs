mod auth;
mod config;
pub mod http_client;
pub mod websockets;

#[cfg(test)]
mod unit;

pub use websockets::WebSocketConnections;

use crate::os_input_output::ClientOsApi;
use crate::RemoteClientError;
use tokio::runtime::Handle;
use zellij_relay_protocol::crypto;
use zellij_utils::remote_session_tokens;

/// Subset of attach state the caller needs to run the terminal loop.
/// Extends `WebSocketConnections` with an optional E2E key so the
/// terminal loop can encrypt/decrypt when the server advertised
/// `e2e_encrypted: true` on /session.
pub struct AttachedSession {
    pub connections: WebSocketConnections,
    /// When `Some`, the terminal loop must encrypt outbound frames and
    /// decrypt inbound frames with this key. `None` means plaintext.
    pub e2e_key: Option<[u8; crypto::KEY_LEN]>,
    pub e2e_encrypted: bool,
}

impl AttachedSession {
    /// Wrap plain connections without an E2E key. Used by tests that
    /// construct the loop manually.
    #[cfg(test)]
    pub fn plain(connections: WebSocketConnections) -> Self {
        Self {
            connections,
            e2e_key: None,
            e2e_encrypted: false,
        }
    }
}

// In tests, only attempt once (no retries) to avoid interactive prompts
// In production, allow up to 3 attempts (initial + 2 retries)
#[cfg(test)]
const MAX_AUTH_ATTEMPTS: u32 = 1;

#[cfg(not(test))]
const MAX_AUTH_ATTEMPTS: u32 = 3;

/// Attach to a remote Zellij session via HTTP(S)
///
/// This function handles the complete authentication flow including:
/// - URL validation
/// - Session token management (--forget, --token flags)
/// - Trying saved session tokens
/// - Interactive authentication with retry logic
/// - Saving session tokens when --remember is used
///
/// Returns WebSocketConnections on success
/// Attach to a remote session.
///
/// `extra_relay_urls` carries additional relay URLs (typically the
/// local `relay_server_url` config) whose hosts should be treated as
/// known relays for the downgrade-refusal check. Added on top of the
/// hard-coded `zellij.dev` entry. Invalid URLs are logged and ignored.
pub fn attach_to_remote_session(
    runtime: Handle,
    _os_input: Box<dyn ClientOsApi>,
    remote_session_url: &str,
    token: Option<String>,
    remember: bool,
    forget: bool,
    ca_cert: Option<&std::path::Path>,
    insecure: bool,
    extra_relay_urls: &[String],
) -> Result<AttachedSession, RemoteClientError> {
    // Extract server URL for token management
    let server_url = extract_server_url(remote_session_url)?;

    // Handle --forget flag
    if forget {
        let _ = remote_session_tokens::delete_session_token(&server_url);
    }

    // If --token provided, delete saved session token
    if token.is_some() {
        let _ = remote_session_tokens::delete_session_token(&server_url);
    }

    if token.is_none() {
        if let Some(attached) = try_to_connect_with_saved_session_token(
            runtime.clone(),
            remote_session_url,
            &server_url,
            ca_cert,
            insecure,
        )? {
            return Ok(attached);
        }
    }

    // Preserve any existing remembered E2E state so a cross-run
    // downgrade (saved=true, fresh auth=false) is refused inside
    // `authenticate_with_retry`.
    let saved_e2e = remote_session_tokens::get_remote_session(&server_url)
        .ok()
        .flatten()
        .map(|s| s.e2e_encrypted)
        .unwrap_or(false);

    // Extract hostnames from the configured relay URLs so the
    // known-relay check can force E2E expectation for them too.
    let extra_known_hosts = hosts_from_urls(extra_relay_urls);

    // Normal auth flow with retry logic
    authenticate_with_retry(
        runtime,
        remote_session_url,
        token,
        remember,
        ca_cert,
        insecure,
        saved_e2e,
        &extra_known_hosts,
    )
}

fn hosts_from_urls(urls: &[String]) -> Vec<String> {
    urls.iter()
        .filter_map(|u| match url::Url::parse(u) {
            Ok(parsed) => parsed.host_str().map(|h| h.to_lowercase()),
            Err(e) => {
                log::warn!("ignoring invalid relay_server_url '{}': {}", u, e);
                None
            },
        })
        .collect()
}

/// Try to connect using a saved session token
/// Returns Ok(Some(attached)) on success, Ok(None) if should retry with auth
fn try_to_connect_with_saved_session_token(
    runtime: Handle,
    remote_session_url: &str,
    server_url: &str,
    ca_cert: Option<&std::path::Path>,
    insecure: bool,
) -> Result<Option<AttachedSession>, RemoteClientError> {
    let Ok(Some(saved)) = remote_session_tokens::get_remote_session(server_url) else {
        return Ok(None);
    };
    // we have a saved session token, let's try to authenticate with it
    let ca_cert_owned = ca_cert.map(|p| p.to_path_buf());
    let remote_url = remote_session_url.to_string();
    let saved_for_attach = saved.clone();
    // Cookie-based reuse cannot recover the raw auth token, so an E2E
    // session cannot be decrypted without re-prompting. Fall through to
    // the normal auth path — the saved E2E flag is preserved for the
    // cross-run downgrade check in `authenticate_with_retry`.
    if saved.e2e_encrypted {
        return Ok(None);
    }
    match runtime.block_on(async move {
        remote_attach_with_session_token(
            &remote_url,
            &saved_for_attach,
            ca_cert_owned.as_deref(),
            insecure,
        )
        .await
    }) {
        Ok(attached) => {
            // Downgrade guard: if we remembered an E2E-confirmed session
            // and the server now claims unencrypted, refuse. This catches
            // a post-compromise scenario where the server starts serving
            // plaintext after a token has been saved.
            if saved.e2e_encrypted && !attached.e2e_encrypted {
                eprintln!(
                    "Refused: this session was previously end-to-end encrypted but the \
                     server now reports it is not. Run with --forget to re-authenticate \
                     if you are sure."
                );
                return Err(RemoteClientError::ConnectionFailed(
                    "E2E downgrade refused".to_string(),
                ));
            }
            Ok(Some(attached))
        },
        Err(RemoteClientError::SessionTokenExpired) => {
            // Session expired - delete and return to retry
            let _ = remote_session_tokens::delete_session_token(server_url);
            eprintln!("Session expired, please re-authenticate");
            Ok(None)
        },
        Err(e) => Err(e),
    }
}

fn authenticate_with_retry(
    runtime: Handle,
    remote_session_url: &str,
    initial_token: Option<String>,
    remember: bool,
    ca_cert: Option<&std::path::Path>,
    insecure: bool,
    saved_e2e: bool,
    extra_known_hosts: &[String],
) -> Result<AttachedSession, RemoteClientError> {
    use dialoguer::{Confirm, Password};

    // Display the encryption state before the token prompt. The
    // indicator is fetched via a pre-flight GET of the challenge page —
    // the relay's `serve_html` always returns `EXPECTED_E2E=true`, the
    // local web server reflects the `encrypt_web_sharing` option.
    //
    // Known-relay URLs (`zellij.dev` + subdomains, plus any host taken
    // from `extra_known_hosts`) force the locked state regardless of
    // what the challenge page claims.
    let expected_e2e = runtime.block_on(async {
        probe_expected_e2e(remote_session_url, ca_cert, insecure, extra_known_hosts).await
    });
    print_expected_e2e_banner(expected_e2e, remote_session_url);

    let mut attempt = 0;
    let mut current_token = initial_token;

    loop {
        attempt += 1;

        let auth_token = match &current_token {
            Some(t) => t.clone(),
            None => Password::new()
                .with_prompt("Enter authentication token")
                .interact()
                .map_err(|e| RemoteClientError::IoError(e))?,
        };

        let ca_cert_owned = ca_cert.map(|p| p.to_path_buf());
        let auth_token_for_attach = auth_token.clone();
        let remote_url = remote_session_url.to_string();
        match runtime.block_on(async move {
            remote_attach(
                &remote_url,
                &auth_token_for_attach,
                remember,
                ca_cert_owned.as_deref(),
                insecure,
            )
            .await
        }) {
            Ok((attached, remembered_cookie)) => {
                // Cross-check: if the challenge advertised E2E but the
                // server's auth response does not, refuse before any
                // WS frames are sent.
                if expected_e2e && !attached.e2e_encrypted {
                    eprintln!(
                        "Refused: this URL advertised end-to-end encryption but the \
                         server did not confirm it. Not connecting."
                    );
                    return Err(RemoteClientError::ConnectionFailed(
                        "E2E downgrade refused".to_string(),
                    ));
                }
                // Cross-run downgrade check: a past successful auth to
                // this URL recorded E2E on, and the fresh auth now says
                // off — refuse.
                if saved_e2e && !attached.e2e_encrypted {
                    eprintln!(
                        "Refused: a previous successful connection to this URL was \
                         end-to-end encrypted but the server now reports it is not. \
                         Run with --forget to clear the stored state if intentional."
                    );
                    return Err(RemoteClientError::ConnectionFailed(
                        "E2E downgrade refused".to_string(),
                    ));
                }
                // Save session token if we got one
                if let Some(cookie) = remembered_cookie {
                    let server_url = extract_server_url(remote_session_url)?;
                    let _ = remote_session_tokens::save_remote_session(
                        &server_url,
                        &cookie.name,
                        &cookie.value,
                        attached.e2e_encrypted,
                    );
                }
                return Ok(attached);
            },
            Err(RemoteClientError::InvalidAuthToken) => {
                eprintln!("Invalid authentication token");

                if attempt >= MAX_AUTH_ATTEMPTS {
                    eprintln!(
                        "Maximum authentication attempts ({}) exceeded.",
                        MAX_AUTH_ATTEMPTS
                    );
                    return Err(RemoteClientError::InvalidAuthToken);
                }

                match Confirm::new()
                    .with_prompt("Try again?")
                    .default(true)
                    .interact()
                {
                    Ok(true) => {
                        current_token = None;
                        continue;
                    },
                    Ok(false) => {
                        return Err(RemoteClientError::InvalidAuthToken);
                    },
                    Err(e) => {
                        return Err(RemoteClientError::IoError(e));
                    },
                }
            },
            Err(e) => {
                return Err(e);
            },
        }
    }
}

/// Known-relay hostname list, mirroring `KNOWN_RELAY_HOSTS` in the
/// browser auth.js. Forces `expected_e2e = true` regardless of what the
/// challenge page claims.
const KNOWN_RELAY_HOSTS: &[&str] = &["zellij.dev"];

fn host_is_known_relay(host: &str, extra_known_hosts: &[String]) -> bool {
    let host = host.to_lowercase();
    let matches = |candidate: &str| -> bool {
        let candidate = candidate.trim().to_lowercase();
        if candidate.is_empty() {
            return false;
        }
        host == candidate || host.ends_with(&format!(".{}", candidate))
    };
    KNOWN_RELAY_HOSTS.iter().any(|r| matches(r))
        || extra_known_hosts.iter().any(|r| matches(r))
}

async fn probe_expected_e2e(
    remote_session_url: &str,
    ca_cert: Option<&std::path::Path>,
    insecure: bool,
    extra_known_hosts: &[String],
) -> bool {
    // Known-relay short-circuit: do not trust the challenge page to
    // advertise encryption on `zellij.dev` or any host from the local
    // config's `relay_server_url` — always require it.
    if let Ok(parsed) = url::Url::parse(remote_session_url) {
        if let Some(host) = parsed.host_str() {
            if host_is_known_relay(host, extra_known_hosts) {
                return true;
            }
        }
    }

    // Best-effort GET of the challenge page. A failure (network error,
    // bad URL) falls back to `false` — the auth flow's cross-check on
    // the AuthResponse still catches a downgrade.
    let server_base_url = match extract_server_url(remote_session_url) {
        Ok(u) => u,
        Err(_) => return false,
    };
    let http_client = match http_client::HttpClientWithCookies::new(ca_cert, insecure) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let req = match isahc::Request::get(&server_base_url)
        .header("User-Agent", "http-terminal-client/1.0")
        .body(Vec::new())
    {
        Ok(r) => r,
        Err(_) => return false,
    };
    let mut resp = match http_client.send_with_cookies(req).await {
        Ok(r) => r,
        Err(_) => return false,
    };
    use isahc::AsyncReadResponseExt;
    let body = match resp.text().await {
        Ok(t) => t,
        Err(_) => return false,
    };
    // Look for the `EXPECTED_E2E` placeholder the server stamped in.
    body.contains("id=\"zellij-expected-e2e\" value=\"true\"")
}

fn print_expected_e2e_banner(expected_e2e: bool, url: &str) {
    // ANSI SGR colour codes picked to survive both dark and light
    // terminal backgrounds: bold green for the affirmative, bold yellow
    // for the negative. No brackets or leading labels — the phrase is
    // emphasised inline in the status sentence.
    const BOLD_GREEN: &str = "\x1b[1;32m";
    const BOLD_YELLOW: &str = "\x1b[1;33m";
    const RESET: &str = "\x1b[0m";
    if expected_e2e {
        eprintln!(
            "Connecting to {} — session is {}end-to-end encrypted{}",
            url, BOLD_GREEN, RESET
        );
    } else {
        eprintln!(
            "Connecting to {} — session is {}not end-to-end encrypted{}",
            url, BOLD_YELLOW, RESET
        );
    }
}

async fn remote_attach(
    server_url: &str,
    auth_token: &str,
    remember_me: bool,
    ca_cert: Option<&std::path::Path>,
    insecure: bool,
) -> Result<(AttachedSession, Option<auth::RememberedCookie>), RemoteClientError> {
    let server_base_url = extract_server_url(server_url)?;
    let session_name = extract_session_name(server_url)?;
    let auth_result =
        auth::authenticate(&server_base_url, auth_token, remember_me, ca_cert, insecure).await?;
    let e2e_key = derive_e2e_key_if_needed(&auth_result, auth_token);
    let connections = websockets::establish_websocket_connections(
        &auth_result.web_client_id,
        &auth_result.http_client,
        &server_base_url,
        &session_name,
        ca_cert,
        insecure,
    )
    .await
    .map_err(|e| RemoteClientError::ConnectionFailed(e.to_string()))?;
    let attached = AttachedSession {
        connections,
        e2e_key,
        e2e_encrypted: auth_result.e2e_encrypted,
    };
    Ok((attached, auth_result.remembered))
}

async fn remote_attach_with_session_token(
    server_url: &str,
    saved: &zellij_utils::remote_session_tokens::SavedRemoteSession,
    ca_cert: Option<&std::path::Path>,
    insecure: bool,
) -> Result<AttachedSession, RemoteClientError> {
    let server_base_url = extract_server_url(server_url)?;
    let session_name = extract_session_name(server_url)?;
    let (session_data, http_client) = auth::validate_session_token(
        &server_base_url,
        &saved.cookie_name,
        &saved.cookie_value,
        ca_cert,
        insecure,
    )
    .await?;
    // Session-token reuse cannot recover the raw auth token, so E2E is
    // only preserved across reuse when the saved record says so AND the
    // server still advertises it. Mismatch is caught by the caller.
    let e2e_key = None;
    let connections = websockets::establish_websocket_connections(
        &session_data.web_client_id,
        &http_client,
        &server_base_url,
        &session_name,
        ca_cert,
        insecure,
    )
    .await
    .map_err(|e| RemoteClientError::ConnectionFailed(e.to_string()))?;
    Ok(AttachedSession {
        connections,
        e2e_key,
        e2e_encrypted: session_data.e2e_encrypted,
    })
}

fn derive_e2e_key_if_needed(
    auth_result: &auth::AuthResult,
    raw_token: &str,
) -> Option<[u8; crypto::KEY_LEN]> {
    if !auth_result.e2e_encrypted {
        return None;
    }
    let tunnel_id = match &auth_result.tunnel_id {
        Some(t) => t,
        None => {
            eprintln!(
                "Server reported e2e_encrypted=true but did not include a tunnel_id; \
                 cannot derive key. Treating as plaintext."
            );
            return None;
        },
    };
    // Server derives the key from the SHA-256 hex digest of the raw auth
    // token. Reproduce that same input here so the keys match.
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(raw_token.as_bytes());
    let token_hash_hex = format!("{:x}", hasher.finalize());
    Some(crypto::derive_key(&token_hash_hex, tunnel_id))
}

/// Produce the base URL that the attach client should hit with
/// `/command/login`, `/session`, `/ws/terminal`, etc.
///
/// For a local web URL like `https://host/session-name` we want
/// `https://host` — the path segment is a session name, not an API prefix.
///
/// For a relay URL like `https://host/r/<slug>` or
/// `https://host/r/<slug>/<session>` we want `https://host/r/<slug>` —
/// the relay mounts every viewer endpoint under that prefix.
pub fn extract_server_url(full_url: &str) -> Result<String, RemoteClientError> {
    let parsed = url::Url::parse(full_url)?;
    let path = parsed.path();
    let mut base_url = parsed.clone();
    base_url.set_query(None);
    base_url.set_fragment(None);

    // If the URL is a relay URL (path starts with `/r/<slug>`), preserve
    // the `/r/<slug>` prefix. Otherwise clear the path.
    let segments: Vec<&str> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let preserved_path = if segments.len() >= 2 && segments[0] == "r" {
        format!("/r/{}", segments[1])
    } else {
        String::new()
    };
    base_url.set_path(&preserved_path);
    Ok(base_url.to_string().trim_end_matches('/').to_string())
}

fn extract_session_name(server_url: &str) -> Result<String, RemoteClientError> {
    let parsed_url = url::Url::parse(server_url)?;
    let path = parsed_url.path();
    let segments: Vec<&str> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    // Relay URL: `/r/<slug>` → no session, `/r/<slug>/<session>` → session.
    if segments.len() >= 2 && segments[0] == "r" {
        if segments.len() >= 3 {
            Ok(segments[2..].join("/"))
        } else {
            Ok(String::new())
        }
    } else if path.len() > 1 && path.starts_with('/') {
        // Local URL: everything after the leading `/` (back-compat with
        // pre-relay behaviour — tests assert "path/to/session" etc.).
        Ok(path[1..].trim_end_matches('/').to_string())
    } else {
        Ok(String::new())
    }
}
