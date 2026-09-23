//! Integration tests for the relay router, exercising the SPAKE2 viewer
//! handshake end to end: a fake Zellij sharer peer (control tunnel) runs the
//! SPAKE2 responder via `zellij_relay_protocol::crypto`, and a reqwest viewer
//! runs the initiator over `POST /command/login` + `POST /session`. The relay
//! only forwards opaque SPAKE2 blobs — these tests confirm the routing and the
//! wrong-secret rejection.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use zellij_relay_server::{
    registry::Registry,
    relay_tunnel_auth_tokens::{store_new_relay_tunnel_auth_token, ENV_DATA_DIR},
    router::{build_router, AppState},
};
use zellij_relay_protocol::{
    crypto, decode_control_frame, decode_terminal_frame, ControlMessage, TerminalMessage,
    TunnelErrorCode, PROTOCOL_VERSION,
};

const URL_TEMPLATE: &str = "http://localhost:8765/r/{slug}";
const TEST_PIN: &[u8] = b"483921";

fn shared_test_token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let scratch =
            std::env::temp_dir().join(format!("zellij-relay-it-{}", uuid::Uuid::new_v4()));
        std::env::set_var(ENV_DATA_DIR, &scratch);
        store_new_relay_tunnel_auth_token(Some("integration-test".into()))
            .expect("mint relay tunnel auth token for integration tests")
    })
}

async fn spawn_router() -> (String, String, Registry) {
    let _ = shared_test_token();
    let state = AppState::new(URL_TEMPLATE.to_string(), vec![]);
    let registry = state.registry.clone();
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service()).await;
    });
    let http = format!("http://{}", addr);
    let ws = format!("ws://{}", addr);
    (http, ws, registry)
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn read_next_binary(stream: &mut WsStream) -> Option<Vec<u8>> {
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Binary(b)) => return Some(b.to_vec()),
            Ok(Message::Text(t)) => return Some(t.as_bytes().to_vec()),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => continue,
        }
    }
    None
}

async fn perform_control_handshake(
    ws_base: &str,
    read_only: bool,
) -> (WsStream, String, String, String, String) {
    let url = format!("{}/tunnel/control", ws_base);
    let (mut ws_stream, _) = connect_async(&url).await.expect("connect control");
    let auth = ControlMessage::Auth {
        token: shared_test_token().to_string(),
        session_name: "test-session".into(),
        protocol_version: PROTOCOL_VERSION,
        zellij_version: "0.45.0".into(),
        requested_slug: String::new(),
        read_only,
    };
    ws_stream.send(Message::Binary(auth.encode().into())).await.unwrap();
    let reply_bytes = read_next_binary(&mut ws_stream)
        .await
        .expect("established reply");
    match decode_control_frame(&reply_bytes).expect("decode reply") {
        ControlMessage::Established {
            public_url,
            slug,
            tunnel_id,
            terminal_binding_secret,
        } => (ws_stream, public_url, slug, tunnel_id, terminal_binding_secret),
        other => panic!("expected Established, got {:?}", other),
    }
}

/// A fake Zellij sharer: drive the SPAKE2 responder for one slug. `secret`
/// is the PIN the sharer published; viewers presenting the same PIN succeed.
async fn run_fake_sharer(mut control: WsStream, secret: Vec<u8>, slug: String) {
    // request_id -> (client_id, pake_key, viewer_msg, sharer_msg)
    let mut pending: HashMap<Vec<u8>, (u32, Vec<u8>, Vec<u8>, Vec<u8>)> = HashMap::new();
    let mut next_client_id = 1u32;
    while let Some(bytes) = read_next_binary(&mut control).await {
        let Ok(msg) = decode_control_frame(&bytes) else {
            continue;
        };
        match msg {
            ControlMessage::PakeChallenge {
                request_id,
                viewer_msg,
                ..
            } => {
                let (state, sharer_msg) = crypto::pake_start(&secret, &slug);
                let key = match crypto::pake_finish(state, &viewer_msg) {
                    Ok(k) => k,
                    Err(_) => continue,
                };
                let client_id = next_client_id;
                next_client_id += 1;
                let confirm = crypto::confirmation_tag(
                    &key,
                    crypto::CONFIRM_LABEL_SHARER,
                    &viewer_msg,
                    &sharer_msg,
                );
                pending.insert(
                    request_id.clone(),
                    (client_id, key, viewer_msg, sharer_msg.clone()),
                );
                let resp = ControlMessage::PakeResponse {
                    request_id,
                    client_id,
                    accepted: true,
                    sharer_msg,
                    sharer_confirm: confirm.to_vec(),
                };
                let _ = control.send(Message::Binary(resp.encode().into())).await;
            },
            ControlMessage::PakeConfirm {
                request_id,
                viewer_confirm,
            } => {
                let Some((client_id, key, viewer_msg, sharer_msg)) = pending.remove(&request_id)
                else {
                    continue;
                };
                let ok = crypto::verify_confirmation(
                    &viewer_confirm,
                    &key,
                    crypto::CONFIRM_LABEL_VIEWER,
                    &viewer_msg,
                    &sharer_msg,
                );
                let result = ControlMessage::PakeResult {
                    request_id,
                    client_id,
                    accepted: ok,
                };
                let _ = control.send(Message::Binary(result.encode().into())).await;
            },
            _ => {},
        }
    }
}

/// Run the viewer half of the SPAKE2 handshake against the relay's HTTP
/// endpoints using `secret`. Returns the parsed `/session` JSON on success.
async fn viewer_handshake(
    http: &str,
    slug: &str,
    secret: &[u8],
) -> Result<serde_json::Value, reqwest::StatusCode> {
    let client = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .expect("reqwest client");
    let (vstate, viewer_msg) = crypto::pake_start(secret, slug);

    let login_url = format!("{}/r/{}/command/login", http, slug);
    let login_res = client
        .post(&login_url)
        .json(&serde_json::json!({ "viewer_msg": viewer_msg }))
        .send()
        .await
        .expect("login POST");
    if !login_res.status().is_success() {
        return Err(login_res.status());
    }
    let login_body: serde_json::Value = login_res.json().await.expect("login json");
    let sharer_msg: Vec<u8> = serde_json::from_value(login_body["sharer_msg"].clone()).unwrap();
    let sharer_confirm: Vec<u8> =
        serde_json::from_value(login_body["sharer_confirm"].clone()).unwrap();
    let handshake = login_body["handshake"].as_str().expect("handshake id").to_string();

    let key = crypto::pake_finish(vstate, &sharer_msg).expect("viewer finish");
    let sharer_ok = crypto::verify_confirmation(
        &sharer_confirm,
        &key,
        crypto::CONFIRM_LABEL_SHARER,
        &viewer_msg,
        &sharer_msg,
    );
    // A wrong PIN produces a divergent key, so the sharer's tag fails here.
    // Still send our (doomed) confirmation so the relay/sharer reject path
    // is exercised exactly as a real viewer would drive it.
    let viewer_confirm =
        crypto::confirmation_tag(&key, crypto::CONFIRM_LABEL_VIEWER, &viewer_msg, &sharer_msg);
    let _ = sharer_ok;

    let session_url = format!("{}/r/{}/session", http, slug);
    let session_res = client
        .post(&session_url)
        .header("X-Zellij-Handshake", &handshake)
        .json(&serde_json::json!({ "viewer_confirm": viewer_confirm.to_vec() }))
        .send()
        .await
        .expect("session POST");
    if !session_res.status().is_success() {
        return Err(session_res.status());
    }
    Ok(session_res.json().await.expect("session json"))
}

#[tokio::test]
async fn control_handshake_happy_path() {
    let (_http, ws, registry) = spawn_router().await;
    let (_control_ws, public_url, slug, tunnel_id, terminal_binding_secret) =
        perform_control_handshake(&ws, false).await;

    assert_eq!(public_url, URL_TEMPLATE.replace("{slug}", &slug));
    assert_eq!(slug.len(), 8);
    assert_eq!(terminal_binding_secret.len(), 64);
    assert!(terminal_binding_secret
        .bytes()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));

    let entry = registry.get(&slug).expect("registry entry");
    assert_eq!(entry.tunnel_id.to_string(), tunnel_id);
    assert_eq!(entry.slug, slug);
    assert!(!entry.read_only);
}

#[tokio::test]
async fn control_handshake_read_only_flag() {
    let (_http, ws, registry) = spawn_router().await;
    let (_control_ws, _public_url, slug, _tunnel_id, _secret) =
        perform_control_handshake(&ws, true).await;
    let entry = registry.get(&slug).expect("registry entry");
    assert!(entry.read_only);
}

#[tokio::test]
async fn terminal_channel_links_tunnel() {
    let (_http, ws, _registry) = spawn_router().await;
    let (_control_ws, _public_url, slug, tunnel_id, terminal_binding_secret) =
        perform_control_handshake(&ws, false).await;

    let url = format!("{}/tunnel/terminal?slug={}", ws, slug);
    let (mut ws_stream, _) = connect_async(&url).await.expect("connect terminal");
    let ready = TerminalMessage::Ready {
        tunnel_id: tunnel_id.clone(),
        binding_secret: terminal_binding_secret,
    };
    ws_stream.send(Message::Binary(ready.encode().into())).await.unwrap();
    // The relay does not reply to a matching Ready; absence of an Error frame
    // within a short window indicates the link succeeded.
    let early = tokio::time::timeout(Duration::from_millis(200), read_next_binary(&mut ws_stream))
        .await;
    if let Ok(Some(bytes)) = early {
        if let Ok(TerminalMessage::Error { message, .. }) = decode_terminal_frame(&bytes) {
            panic!("unexpected terminal error after Ready: {}", message);
        }
    }
}

#[tokio::test]
async fn terminal_channel_rejects_bad_binding_secret() {
    let (_http, ws, _registry) = spawn_router().await;
    let (_control_ws, _public_url, slug, tunnel_id, _secret) =
        perform_control_handshake(&ws, false).await;

    let url = format!("{}/tunnel/terminal?slug={}", ws, slug);
    let (mut ws_stream, _) = connect_async(&url).await.expect("connect terminal");
    let ready = TerminalMessage::Ready {
        tunnel_id: tunnel_id.clone(),
        // Well-formed shape but wrong value — must be rejected with the
        // terminal-binding code, NOT the account auth-rejected code.
        binding_secret: "0".repeat(64),
    };
    ws_stream.send(Message::Binary(ready.encode().into())).await.unwrap();
    let bytes = read_next_binary(&mut ws_stream)
        .await
        .expect("expected rejection frame");
    match decode_terminal_frame(&bytes).expect("decode terminal frame") {
        TerminalMessage::Error { code, .. } => {
            assert_eq!(code, TunnelErrorCode::TerminalBindingRejected);
        },
        other => panic!("expected Error frame, got {:?}", other),
    }
}

#[tokio::test]
async fn pake_viewer_round_trip_succeeds() {
    let (http, ws, registry) = spawn_router().await;
    let (control_ws, _public_url, slug, _tunnel_id, _secret) =
        perform_control_handshake(&ws, false).await;

    let sharer = tokio::spawn(run_fake_sharer(control_ws, TEST_PIN.to_vec(), slug.clone()));

    let body = viewer_handshake(&http, &slug, TEST_PIN)
        .await
        .expect("correct PIN should authenticate");
    assert_eq!(body["is_read_only"], false);
    assert_eq!(body["e2e_encrypted"], true);
    assert!(body["client_id"].as_u64().unwrap() >= 1);
    assert!(body["tunnel_id"].as_str().unwrap_or("").len() > 0);

    // The relay registered a 1:1 client_id → viewer routing.
    let entry = registry.get(&slug).expect("entry");
    let client_id = body["client_id"].as_u64().unwrap() as u32;
    assert!(entry.viewer_for_client_id(client_id).is_some());

    sharer.abort();
}

#[tokio::test]
async fn pake_viewer_wrong_pin_rejected() {
    let (http, ws, _registry) = spawn_router().await;
    let (control_ws, _public_url, slug, _tunnel_id, _secret) =
        perform_control_handshake(&ws, false).await;

    // Sharer published TEST_PIN; the viewer presents a different one.
    let sharer = tokio::spawn(run_fake_sharer(control_ws, TEST_PIN.to_vec(), slug.clone()));

    let result = viewer_handshake(&http, &slug, b"000000").await;
    assert_eq!(result.unwrap_err(), reqwest::StatusCode::UNAUTHORIZED);

    sharer.abort();
}
