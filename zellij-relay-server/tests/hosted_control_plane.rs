//! Integration tests for hosted-mode tunnel auth (`TunnelAuthBackend::Online`)
//! and event reporting (`EventSink::Online`). Modeled on
//! `router_integration.rs`: a real axum router bound to `127.0.0.1:0`, driven
//! over real WebSocket connections — but here the relay's control-plane
//! calls are pointed at an in-test mock `zellij.online` server instead of
//! real sqlite, so these exercise the fail-closed / no-allow-caching /
//! binding-secret contracts end to end.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    extract::State,
    http::{
        header::{AUTHORIZATION, CONTENT_TYPE},
        StatusCode,
    },
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use zellij_relay_protocol::{
    decode_control_frame, decode_terminal_frame, ControlMessage, TerminalMessage,
    PROTOCOL_VERSION,
};
use zellij_relay_server::{
    control_plane::ControlPlaneClient,
    events::EventSenderConfig,
    events::EventSink,
    router::{build_router, AppState},
    tunnel_auth::{TunnelAuthBackend, MAX_CREDENTIAL_LEN},
};

const URL_TEMPLATE: &str = "http://localhost:8765/r/{slug}";
const RELAY_SECRET: &str = "test-relay-secret";

// ---- mock control plane ----

#[derive(Clone)]
enum VerifyBehavior {
    Valid { user_id: String, credential_id: String },
    Invalid,
    Status(u16),
    Hang(Duration),
}

impl Default for VerifyBehavior {
    fn default() -> Self {
        VerifyBehavior::Invalid
    }
}

struct RecordedVerify {
    authorization: Option<String>,
    content_type: Option<String>,
    body: Value,
}

#[derive(Default)]
struct MockState {
    verify_behavior: VerifyBehavior,
    verify_requests: Vec<RecordedVerify>,
    events: Vec<Value>,
    events_fail_first_n: usize,
    events_calls: usize,
}

type MockHandle = Arc<Mutex<MockState>>;

async fn mock_verify(
    State(state): State<MockHandle>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let authorization = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let behavior = {
        let mut s = state.lock().unwrap();
        s.verify_requests.push(RecordedVerify {
            authorization,
            content_type,
            body,
        });
        s.verify_behavior.clone()
    };
    match behavior {
        VerifyBehavior::Valid {
            user_id,
            credential_id,
        } => Json(json!({"valid": true, "userId": user_id, "credentialId": credential_id}))
            .into_response(),
        VerifyBehavior::Invalid => Json(json!({"valid": false})).into_response(),
        VerifyBehavior::Status(code) => {
            StatusCode::from_u16(code).unwrap().into_response()
        },
        VerifyBehavior::Hang(dur) => {
            tokio::time::sleep(dur).await;
            Json(json!({"valid": false})).into_response()
        },
    }
}

async fn mock_events(State(state): State<MockHandle>, Json(body): Json<Value>) -> Response {
    let mut s = state.lock().unwrap();
    s.events_calls += 1;
    if s.events_calls <= s.events_fail_first_n {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    s.events.push(body);
    Json(json!({"ok": true})).into_response()
}

async fn spawn_mock_control_plane() -> (String, MockHandle) {
    let state: MockHandle = Arc::new(Mutex::new(MockState::default()));
    let app = Router::new()
        .route("/api/relay/verify", post(mock_verify))
        .route("/api/relay/events", post(mock_events))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service()).await;
    });
    (format!("http://{}", addr), state)
}

async fn poll_for_event(
    mock: &MockHandle,
    budget: Duration,
    pred: impl Fn(&Value) -> bool,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        {
            let s = mock.lock().unwrap();
            if let Some(e) = s.events.iter().find(|e| pred(e)) {
                return Some(e.clone());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---- hosted relay router ----

async fn spawn_hosted_router(
    client: ControlPlaneClient,
    permits: usize,
    event_cfg: EventSenderConfig,
) -> (String, String) {
    let tunnel_auth = TunnelAuthBackend::online_with_permits(client.clone(), permits);
    let event_sink = EventSink::spawn_online(client, event_cfg);
    let state = AppState::new(URL_TEMPLATE.to_string(), vec![]).with_backends(tunnel_auth, event_sink);
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service()).await;
    });
    (format!("http://{}", addr), format!("ws://{}", addr))
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

async fn control_auth(ws_base: &str, token: &str) -> (WsStream, ControlMessage) {
    let url = format!("{}/tunnel/control", ws_base);
    let (mut ws_stream, _) = connect_async(&url).await.expect("connect control");
    let auth = ControlMessage::Auth {
        token: token.to_string(),
        session_name: "test-session".into(),
        protocol_version: PROTOCOL_VERSION,
        zellij_version: "0.45.0".into(),
        requested_slug: String::new(),
        read_only: false,
    };
    ws_stream
        .send(Message::Binary(auth.encode().into()))
        .await
        .unwrap();
    let bytes = read_next_binary(&mut ws_stream).await.expect("reply");
    let msg = decode_control_frame(&bytes).expect("decode reply");
    (ws_stream, msg)
}

fn default_client(cp_url: String) -> ControlPlaneClient {
    ControlPlaneClient::new(cp_url, RELAY_SECRET.to_string(), Duration::from_millis(500))
}

// ---- tests ----

#[tokio::test]
async fn hosted_accepts_valid_token_and_asserts_envelope() {
    let (cp_url, mock) = spawn_mock_control_plane().await;
    mock.lock().unwrap().verify_behavior = VerifyBehavior::Valid {
        user_id: "u1".into(),
        credential_id: "c1".into(),
    };
    let (_http, ws) = spawn_hosted_router(default_client(cp_url), 32, EventSenderConfig::default()).await;

    let (_ws_stream, msg) = control_auth(&ws, "zo_live_x").await;
    match msg {
        ControlMessage::Established {
            terminal_binding_secret,
            ..
        } => {
            assert_eq!(terminal_binding_secret.len(), 64);
            assert!(terminal_binding_secret
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        },
        other => panic!("expected Established, got {:?}", other),
    }

    let mock_guard = mock.lock().unwrap();
    assert_eq!(mock_guard.verify_requests.len(), 1);
    let req = &mock_guard.verify_requests[0];
    assert_eq!(req.authorization.as_deref(), Some("Bearer test-relay-secret"));
    assert!(req
        .content_type
        .as_deref()
        .unwrap_or("")
        .starts_with("application/json"));
    assert_eq!(
        req.body,
        json!({"credential": {"type": "token", "token": "zo_live_x"}})
    );
}

#[tokio::test]
async fn hosted_rejects_invalid_token() {
    let (cp_url, _mock) = spawn_mock_control_plane().await;
    let (_http, ws) = spawn_hosted_router(default_client(cp_url), 32, EventSenderConfig::default()).await;

    let (_ws_stream, msg) = control_auth(&ws, "zo_live_bad").await;
    match msg {
        ControlMessage::Error { message, .. } => {
            assert!(message.contains("relay tunnel auth rejected"));
        },
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn hosted_fails_closed_on_5xx() {
    let (cp_url, _mock) = spawn_mock_control_plane().await;
    _mock.lock().unwrap().verify_behavior = VerifyBehavior::Status(503);
    let (_http, ws) = spawn_hosted_router(default_client(cp_url), 32, EventSenderConfig::default()).await;

    let (_ws_stream, msg) = control_auth(&ws, "zo_live_x").await;
    match msg {
        ControlMessage::Error { message, .. } => {
            assert!(message.contains("relay tunnel auth rejected"));
        },
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn hosted_fails_closed_on_timeout() {
    let (cp_url, _mock) = spawn_mock_control_plane().await;
    _mock.lock().unwrap().verify_behavior = VerifyBehavior::Hang(Duration::from_secs(2));
    let (_http, ws) = spawn_hosted_router(default_client(cp_url), 32, EventSenderConfig::default()).await;

    let (_ws_stream, msg) = tokio::time::timeout(Duration::from_secs(1), control_auth(&ws, "zo_live_x"))
        .await
        .expect("client-side timeout (500ms) should reject well before 1s");
    match msg {
        ControlMessage::Error { message, .. } => {
            assert!(message.contains("relay tunnel auth rejected"));
        },
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn hosted_fails_closed_on_connection_refused() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let client = default_client(format!("http://{}", addr));
    let (_http, ws) = spawn_hosted_router(client, 32, EventSenderConfig::default()).await;

    let (_ws_stream, msg) = control_auth(&ws, "zo_live_x").await;
    match msg {
        ControlMessage::Error { message, .. } => {
            assert!(message.contains("relay tunnel auth rejected"));
        },
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn hosted_terminal_links_with_binding_secret() {
    let (cp_url, mock) = spawn_mock_control_plane().await;
    mock.lock().unwrap().verify_behavior = VerifyBehavior::Valid {
        user_id: "u1".into(),
        credential_id: "c1".into(),
    };
    let (_http, ws) = spawn_hosted_router(default_client(cp_url), 32, EventSenderConfig::default()).await;

    let (control_ws, msg) = control_auth(&ws, "zo_live_x").await;
    let (slug, tunnel_id, secret) = match msg {
        ControlMessage::Established {
            slug,
            tunnel_id,
            terminal_binding_secret,
            ..
        } => (slug, tunnel_id, terminal_binding_secret),
        other => panic!("expected Established, got {:?}", other),
    };

    let url = format!("{}/tunnel/terminal?slug={}", ws, slug);
    let (mut term_ws, _) = connect_async(&url).await.expect("connect terminal");
    let ready = TerminalMessage::Ready {
        tunnel_id: tunnel_id.clone(),
        binding_secret: secret,
    };
    term_ws
        .send(Message::Binary(ready.encode().into()))
        .await
        .unwrap();
    let early = tokio::time::timeout(Duration::from_millis(200), read_next_binary(&mut term_ws)).await;
    if let Ok(Some(bytes)) = early {
        if let Ok(TerminalMessage::Error { message, .. }) = decode_terminal_frame(&bytes) {
            panic!("unexpected terminal error after Ready: {}", message);
        }
    }

    assert_eq!(mock.lock().unwrap().verify_requests.len(), 1);

    let (mut term_ws2, _) = connect_async(&url).await.expect("connect terminal again");
    let bad_ready = TerminalMessage::Ready {
        tunnel_id,
        binding_secret: "0".repeat(64),
    };
    term_ws2
        .send(Message::Binary(bad_ready.encode().into()))
        .await
        .unwrap();
    let bytes = read_next_binary(&mut term_ws2).await.expect("expected rejection frame");
    match decode_terminal_frame(&bytes).expect("decode terminal frame") {
        TerminalMessage::Error { message, .. } => {
            assert!(
                message.contains("relay terminal binding rejected"),
                "unexpected rejection message: {}",
                message
            );
        },
        other => panic!("expected Error frame, got {:?}", other),
    }

    drop(control_ws);
}

#[tokio::test]
async fn hosted_no_allow_caching_across_tunnel_starts() {
    let (cp_url, mock) = spawn_mock_control_plane().await;
    mock.lock().unwrap().verify_behavior = VerifyBehavior::Valid {
        user_id: "u1".into(),
        credential_id: "c1".into(),
    };
    let (_http, ws) = spawn_hosted_router(default_client(cp_url), 32, EventSenderConfig::default()).await;

    let (_ws1, msg1) = control_auth(&ws, "zo_live_x").await;
    assert!(matches!(msg1, ControlMessage::Established { .. }));

    mock.lock().unwrap().verify_behavior = VerifyBehavior::Invalid;

    let (_ws2, msg2) = control_auth(&ws, "zo_live_x").await;
    match msg2 {
        ControlMessage::Error { message, .. } => assert!(message.contains("relay tunnel auth rejected")),
        other => panic!("expected Error, got {:?}", other),
    }

    assert_eq!(mock.lock().unwrap().verify_requests.len(), 2);
}

#[tokio::test]
async fn hosted_rejects_oversized_token() {
    let (cp_url, mock) = spawn_mock_control_plane().await;
    mock.lock().unwrap().verify_behavior = VerifyBehavior::Valid {
        user_id: "u1".into(),
        credential_id: "c1".into(),
    };
    let (_http, ws) = spawn_hosted_router(default_client(cp_url), 32, EventSenderConfig::default()).await;

    let big_token = "a".repeat(MAX_CREDENTIAL_LEN + 1);
    let (_ws_stream, msg) = control_auth(&ws, &big_token).await;
    match msg {
        ControlMessage::Error { message, .. } => assert!(message.contains("relay tunnel auth rejected")),
        other => panic!("expected Error, got {:?}", other),
    }
    assert_eq!(mock.lock().unwrap().verify_requests.len(), 0);
}

#[tokio::test]
async fn hosted_fails_closed_on_verify_permit_exhaustion() {
    let (cp_url, mock) = spawn_mock_control_plane().await;
    mock.lock().unwrap().verify_behavior = VerifyBehavior::Hang(Duration::from_secs(10));
    let client = ControlPlaneClient::new(cp_url, RELAY_SECRET.to_string(), Duration::from_secs(5));
    let (_http, ws) = spawn_hosted_router(client, 1, EventSenderConfig::default()).await;

    // Tunnel A occupies the single verify permit; fire-and-forget since it
    // won't resolve until the (long) Hang elapses.
    let ws_a = ws.clone();
    tokio::spawn(async move {
        let _ = control_auth(&ws_a, "zo_live_a").await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (_ws_b, msg) = tokio::time::timeout(Duration::from_secs(1), control_auth(&ws, "zo_live_b"))
        .await
        .expect("tunnel B should be rejected immediately, not wait on the exhausted permit");
    match msg {
        ControlMessage::Error { message, .. } => assert!(message.contains("relay tunnel auth rejected")),
        other => panic!("expected Error, got {:?}", other),
    }

    assert_eq!(mock.lock().unwrap().verify_requests.len(), 1);
}

#[tokio::test]
async fn hosted_emits_started_and_stopped_on_socket_drop() {
    let (cp_url, mock) = spawn_mock_control_plane().await;
    mock.lock().unwrap().verify_behavior = VerifyBehavior::Valid {
        user_id: "u1".into(),
        credential_id: "c1".into(),
    };
    let event_cfg = EventSenderConfig {
        queue_capacity: 256,
        request_timeout: Duration::from_millis(500),
        retry_delays: vec![Duration::from_millis(20), Duration::from_millis(20)],
    };
    let (_http, ws) = spawn_hosted_router(default_client(cp_url), 32, event_cfg).await;

    let (control_ws, msg) = control_auth(&ws, "zo_live_x").await;
    let (slug, tunnel_id) = match msg {
        ControlMessage::Established { slug, tunnel_id, .. } => (slug, tunnel_id),
        other => panic!("expected Established, got {:?}", other),
    };

    let started = poll_for_event(&mock, Duration::from_secs(3), |e| {
        e.get("type").and_then(|v| v.as_str()) == Some("tunnel_started")
            && e.get("tunnel_id").and_then(|v| v.as_str()) == Some(tunnel_id.as_str())
    })
    .await
    .expect("tunnel_started event");
    assert_eq!(started["user_id"], "u1");
    assert_eq!(started["slug"], slug);
    assert_eq!(started["credential_id"], "c1");
    assert_eq!(started["zellij_version"], "0.45.0");

    drop(control_ws);

    let stopped = poll_for_event(&mock, Duration::from_secs(3), |e| {
        e.get("type").and_then(|v| v.as_str()) == Some("tunnel_stopped")
            && e.get("tunnel_id").and_then(|v| v.as_str()) == Some(tunnel_id.as_str())
    })
    .await
    .expect("tunnel_stopped event");
    assert_eq!(stopped["reason"], "control_socket_closed");
}

#[tokio::test]
async fn hosted_event_retry_then_success() {
    let (cp_url, mock) = spawn_mock_control_plane().await;
    mock.lock().unwrap().verify_behavior = VerifyBehavior::Valid {
        user_id: "u1".into(),
        credential_id: "c1".into(),
    };
    mock.lock().unwrap().events_fail_first_n = 1;
    let event_cfg = EventSenderConfig {
        queue_capacity: 256,
        request_timeout: Duration::from_millis(500),
        retry_delays: vec![Duration::from_millis(20), Duration::from_millis(20)],
    };
    let (_http, ws) = spawn_hosted_router(default_client(cp_url), 32, event_cfg).await;

    let (_control_ws, msg) = control_auth(&ws, "zo_live_x").await;
    let tunnel_id = match msg {
        ControlMessage::Established { tunnel_id, .. } => tunnel_id,
        other => panic!("expected Established, got {:?}", other),
    };

    poll_for_event(&mock, Duration::from_secs(3), |e| {
        e.get("type").and_then(|v| v.as_str()) == Some("tunnel_started")
            && e.get("tunnel_id").and_then(|v| v.as_str()) == Some(tunnel_id.as_str())
    })
    .await
    .expect("tunnel_started should land after retry");

    assert!(
        mock.lock().unwrap().events_calls >= 2,
        "expected at least one retry"
    );
}
