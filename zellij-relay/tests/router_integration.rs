use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use zellij_relay::{
    registry::Registry,
    router::{build_router, AppState},
};
use zellij_relay_protocol::{
    decode_control_frame, decode_terminal_frame, ControlMessage, TerminalMessage,
    PROTOCOL_VERSION,
};

const URL_TEMPLATE: &str = "http://localhost:8765/r/{slug}";

async fn spawn_router() -> (String, String, Registry) {
    let state = AppState::new(URL_TEMPLATE.to_string());
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

type WsStream = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

async fn read_next_binary(stream: &mut WsStream) -> Option<Vec<u8>> {
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Binary(b)) => return Some(b),
            Ok(Message::Text(t)) => return Some(t.into_bytes()),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => continue,
        }
    }
    None
}

async fn perform_control_handshake(
    ws_base: &str,
) -> (WsStream, String, String, String) {
    let url = format!("{}/tunnel/control", ws_base);
    let (mut ws_stream, _) = connect_async(&url).await.expect("connect control");
    let auth = ControlMessage::Auth {
        token: String::new(),
        session_name: "test-session".into(),
        protocol_version: PROTOCOL_VERSION,
        zellij_version: "0.45.0".into(),
    };
    ws_stream
        .send(Message::Binary(auth.encode()))
        .await
        .unwrap();
    let reply_bytes = read_next_binary(&mut ws_stream)
        .await
        .expect("established reply");
    let reply = decode_control_frame(&reply_bytes).expect("decode reply");
    let (public_url, slug, tunnel_id) = match reply {
        ControlMessage::Established {
            public_url,
            slug,
            tunnel_id,
        } => (public_url, slug, tunnel_id),
        other => panic!("expected Established, got {:?}", other),
    };
    (ws_stream, public_url, slug, tunnel_id)
}

#[tokio::test]
async fn control_handshake_happy_path() {
    let (_http, ws, registry) = spawn_router().await;
    let (_control_ws, public_url, slug, tunnel_id) = perform_control_handshake(&ws).await;

    assert_eq!(public_url, URL_TEMPLATE.replace("{slug}", &slug));
    assert_eq!(slug.len(), 8);

    let entry = registry.get(&slug).expect("registry entry");
    assert_eq!(entry.tunnel_id.to_string(), tunnel_id);
    assert_eq!(entry.slug, slug);
}

#[tokio::test]
async fn terminal_channel_links_tunnel() {
    let (_http, ws, registry) = spawn_router().await;
    let (_control_ws, _public_url, slug, tunnel_id) = perform_control_handshake(&ws).await;

    let url = format!("{}/tunnel/terminal?slug={}", ws, slug);
    let (mut ws_stream, _) = connect_async(&url).await.expect("connect terminal");
    let ready = TerminalMessage::Ready {
        tunnel_id: tunnel_id.clone(),
    };
    ws_stream
        .send(Message::Binary(ready.encode()))
        .await
        .unwrap();

    let result = timeout(Duration::from_millis(500), ws_stream.next()).await;
    match result {
        Err(_elapsed) => {},
        Ok(Some(Ok(Message::Close(_)))) => panic!("socket was closed"),
        Ok(Some(Ok(msg))) => panic!("unexpected message: {msg:?}"),
        Ok(Some(Err(e))) => panic!("unexpected error: {e}"),
        Ok(None) => panic!("stream ended"),
    }

    let entry = registry.get(&slug).expect("entry present");
    assert!(*entry.terminal_linked_flag.lock().unwrap());
}

#[tokio::test]
async fn terminal_channel_unknown_slug_rejected() {
    let (_http, ws, _registry) = spawn_router().await;

    let url = format!("{}/tunnel/terminal?slug=doesnotexist", ws);
    let (mut ws_stream, _) = connect_async(&url).await.expect("connect terminal");

    let next_bytes = timeout(Duration::from_secs(1), read_next_binary(&mut ws_stream))
        .await
        .expect("relay should respond quickly");
    match next_bytes {
        Some(bytes) => {
            let msg = decode_terminal_frame(&bytes).expect("decode terminal frame");
            match msg {
                TerminalMessage::Error { message } => {
                    assert!(message.to_lowercase().contains("slug"), "got {:?}", message);
                },
                other => panic!("expected Error, got {:?}", other),
            }
        },
        None => {},
    }
}

#[tokio::test]
async fn terminal_ready_with_wrong_tunnel_id_rejected() {
    let (_http, ws, _registry) = spawn_router().await;
    let (_control_ws, _public_url, slug, _tunnel_id) = perform_control_handshake(&ws).await;

    let url = format!("{}/tunnel/terminal?slug={}", ws, slug);
    let (mut ws_stream, _) = connect_async(&url).await.expect("connect terminal");

    let bogus = TerminalMessage::Ready {
        tunnel_id: "bogus-id".into(),
    };
    ws_stream
        .send(Message::Binary(bogus.encode()))
        .await
        .unwrap();

    let next_bytes = timeout(Duration::from_secs(1), read_next_binary(&mut ws_stream))
        .await
        .expect("relay should respond quickly");
    match next_bytes {
        Some(bytes) => {
            let msg = decode_terminal_frame(&bytes).expect("decode terminal frame");
            match msg {
                TerminalMessage::Error { .. } => {},
                other => panic!("expected Error, got {:?}", other),
            }
        },
        None => {},
    }
}

/// End-to-end tunnel flow: simulate a Zellij side (control tunnel +
/// terminal tunnel) and a viewer side (HTTP `POST /session` +
/// `GET /ws/terminal`), verifying that:
///
/// * the relay forwards `AuthChallenge` to the Zellij side,
/// * `AuthResponse` flows back and is surfaced in the viewer's
///   `POST /session` response (including `e2e_encrypted` + `tunnel_id`),
/// * subsequent `TerminalFrameData` emitted on the terminal tunnel
///   reaches the viewer's terminal WS as **opaque Binary bytes** —
///   matching the Phase 3 requirement that the relay forwards
///   ciphertext without inspection.
#[tokio::test]
async fn full_tunnel_flow_forwards_ciphertext_opaquely() {
    use serde_json::json;

    let (http, ws, _registry) = spawn_router().await;
    let (mut control_ws, _public_url, slug, _tunnel_id) =
        perform_control_handshake(&ws).await;

    // Open + link the terminal tunnel as the Zellij side would.
    let term_url = format!("{}/tunnel/terminal?slug={}", ws, slug);
    let (mut terminal_ws, _) = connect_async(&term_url).await.expect("connect terminal");
    let ready = TerminalMessage::Ready {
        tunnel_id: _tunnel_id.clone(),
    };
    terminal_ws
        .send(Message::Binary(ready.encode()))
        .await
        .unwrap();

    // Give the relay a moment to link the two sockets.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Concurrently: viewer POSTs /session with a raw auth_token; we
    // reply from the simulated-Zellij side as soon as the
    // AuthChallenge arrives.
    let session_url = format!("{}/r/{}/session", http, slug);
    let http_client = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .expect("reqwest client");
    let viewer_post = http_client
        .post(&session_url)
        .json(&json!({"auth_token": "viewer-raw-token"}));
    let viewer_fut = async { viewer_post.send().await.expect("viewer /session POST") };

    let zellij_fut = async {
        // Relay forwards AuthChallenge on the control tunnel.
        let bytes = read_next_binary(&mut control_ws)
            .await
            .expect("AuthChallenge");
        let challenge =
            decode_control_frame(&bytes).expect("decode AuthChallenge");
        let (request_id, _token_hash) = match challenge {
            ControlMessage::AuthChallenge {
                request_id,
                token_hash,
            } => (request_id, token_hash),
            other => panic!("expected AuthChallenge, got {:?}", other),
        };

        // Simulated Zellij accepts with E2E on.
        let response = ControlMessage::AuthResponse {
            request_id,
            client_id: 42,
            accepted: true,
            is_read_only: false,
            session_token_hash: "ignored".into(),
            e2e_encrypted: true,
        };
        control_ws
            .send(Message::Binary(response.encode()))
            .await
            .expect("send AuthResponse");

        // Relay should now emit ClientConnected for the allocated id.
        let bytes = read_next_binary(&mut control_ws)
            .await
            .expect("ClientConnected");
        let connected = decode_control_frame(&bytes).expect("decode");
        match connected {
            ControlMessage::ClientConnected { client_id } => {
                assert_eq!(client_id, 42);
            },
            other => panic!("expected ClientConnected, got {:?}", other),
        }
    };

    let (viewer_resp, ()) = tokio::join!(viewer_fut, zellij_fut);
    assert_eq!(viewer_resp.status(), reqwest::StatusCode::OK);

    // Extract the relay_session cookie from the Set-Cookie header
    // before consuming the body.
    let set_cookie_header = viewer_resp
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .find(|h| h.starts_with("relay_session="))
        .map(str::to_owned)
        .expect("expected relay_session cookie in /session response");
    let cookie_header = set_cookie_header
        .split(';')
        .next()
        .expect("non-empty Set-Cookie")
        .trim()
        .to_string();

    let body: serde_json::Value =
        viewer_resp.json().await.expect("session JSON body");
    assert_eq!(body["client_id"], 42);
    assert_eq!(body["e2e_encrypted"], true);
    assert_eq!(body["web_client_id"], "relay-client-42");
    assert_eq!(body["tunnel_id"], _tunnel_id);

    // Now drive an opaque `TerminalFrameData` through the tunnel.
    // The relay must forward the exact bytes to the viewer's terminal
    // WS as a Binary frame — no UTF-8 inspection, no payload rewrite.

    let viewer_ws_url = format!("ws://{}/r/{}/ws/terminal", strip_scheme(&http), slug);
    let request = tokio_tungstenite::tungstenite::http::Request::builder()
        .uri(&viewer_ws_url)
        .header("Host", strip_scheme(&http))
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header(
            "Sec-WebSocket-Key",
            tokio_tungstenite::tungstenite::handshake::client::generate_key(),
        )
        .header("Sec-WebSocket-Version", "13")
        .header("Cookie", cookie_header)
        .body(())
        .expect("build WS request");
    let (mut viewer_ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("viewer /ws/terminal");

    // Emit a ciphertext-shaped payload — random bytes with non-UTF-8
    // bytes mixed in so any accidental UTF-8 path would mangle it.
    let ciphertext: Vec<u8> = vec![0x00, 0xff, 0xab, 0x42, 0xfe, 0x13, 0x00, 0x80];
    let frame = TerminalMessage::TerminalFrameData {
        client_id: 42,
        data: ciphertext.clone(),
    };
    terminal_ws
        .send(Message::Binary(frame.encode()))
        .await
        .expect("send TerminalFrameData");

    let received = timeout(Duration::from_secs(2), viewer_ws.next())
        .await
        .expect("viewer frame arrived in time")
        .expect("stream yielded")
        .expect("no ws error");
    match received {
        Message::Binary(b) => assert_eq!(b, ciphertext),
        other => panic!(
            "expected Binary frame on viewer WS (Phase 3 opaque forwarding), got {:?}",
            other
        ),
    }
}

fn strip_scheme(url: &str) -> &str {
    url.trim_start_matches("http://").trim_start_matches("https://")
}

/// Three browser tabs pasting the same r/o token must:
///
/// * trigger **exactly one** `AuthChallenge` round-trip,
/// * result in `ReadOnlyViewerUpdate { count: 1, 2, 3 }` on the control
///   tunnel (join path),
/// * receive the same ciphertext bytes when the simulated Zellij emits a
///   single `TerminalFrameData` addressed to the fan-out group,
/// * on sequential close, produce `ReadOnlyViewerUpdate { count: 2, 1, 0 }`
///   and **no** `ClientDisconnected` (destruction is count-driven).
///
/// A fourth viewer opened afterwards triggers a fresh `AuthChallenge`
/// because the group went dormant.
#[tokio::test]
async fn ro_fanout_three_viewers() {
    use serde_json::json;

    let (http, ws, _registry) = spawn_router().await;
    let (mut control_ws, _public_url, slug, tunnel_id) =
        perform_control_handshake(&ws).await;

    let term_url = format!("{}/tunnel/terminal?slug={}", ws, slug);
    let (mut terminal_ws, _) = connect_async(&term_url).await.expect("connect terminal");
    let ready = TerminalMessage::Ready {
        tunnel_id: tunnel_id.clone(),
    };
    terminal_ws
        .send(Message::Binary(ready.encode()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let session_url = format!("{}/r/{}/session", http, slug);
    let raw_token = "ro-shared-token";
    let zellij_client_id = 77u32;

    // ---------- viewer #1: triggers the only AuthChallenge ----------

    let http_client_a = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .expect("reqwest client");
    let viewer_a_fut = async {
        http_client_a
            .post(&session_url)
            .json(&json!({"auth_token": raw_token}))
            .send()
            .await
            .expect("viewer A /session POST")
    };

    let zellij_fut = async {
        let bytes = read_next_binary(&mut control_ws)
            .await
            .expect("AuthChallenge");
        let challenge = decode_control_frame(&bytes).expect("decode");
        let request_id = match challenge {
            ControlMessage::AuthChallenge { request_id, .. } => request_id,
            other => panic!("expected AuthChallenge, got {:?}", other),
        };
        let response = ControlMessage::AuthResponse {
            request_id,
            client_id: zellij_client_id,
            accepted: true,
            is_read_only: true,
            session_token_hash: "ignored".into(),
            e2e_encrypted: true,
        };
        control_ws
            .send(Message::Binary(response.encode()))
            .await
            .expect("AuthResponse send");

        // Relay must NOT send ClientConnected for r/o. It sends
        // ReadOnlyViewerUpdate { count: 1 } instead.
        let bytes = read_next_binary(&mut control_ws)
            .await
            .expect("ReadOnlyViewerUpdate #1");
        let msg = decode_control_frame(&bytes).expect("decode");
        match msg {
            ControlMessage::ReadOnlyViewerUpdate { count, .. } => {
                assert_eq!(count, 1, "first viewer: count must be 1");
            },
            ControlMessage::ClientConnected { .. } => {
                panic!("r/o path must not emit ClientConnected");
            },
            other => panic!("expected ReadOnlyViewerUpdate, got {:?}", other),
        }
    };

    let (resp_a, ()) = tokio::join!(viewer_a_fut, zellij_fut);
    assert_eq!(resp_a.status(), reqwest::StatusCode::OK);
    let set_cookie_a = extract_cookie(&resp_a);
    let body_a: serde_json::Value = resp_a.json().await.expect("JSON body");
    assert_eq!(body_a["is_read_only"], true);
    assert_eq!(body_a["client_id"], zellij_client_id);

    // ---------- viewer #2: cached — no AuthChallenge ----------

    let http_client_b = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .expect("reqwest client B");
    let resp_b = http_client_b
        .post(&session_url)
        .json(&json!({"auth_token": raw_token}))
        .send()
        .await
        .expect("viewer B /session");
    assert_eq!(resp_b.status(), reqwest::StatusCode::OK);

    let bytes = read_next_binary(&mut control_ws)
        .await
        .expect("ReadOnlyViewerUpdate #2");
    match decode_control_frame(&bytes).expect("decode") {
        ControlMessage::ReadOnlyViewerUpdate { count, .. } => {
            assert_eq!(count, 2, "second viewer: count must be 2");
        },
        other => panic!("expected ReadOnlyViewerUpdate, got {:?}", other),
    }
    let set_cookie_b = extract_cookie(&resp_b);
    let body_b: serde_json::Value = resp_b.json().await.expect("JSON body");
    assert_eq!(body_b["client_id"], zellij_client_id);

    // ---------- viewer #3: still cached ----------

    let http_client_c = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .expect("reqwest client C");
    let resp_c = http_client_c
        .post(&session_url)
        .json(&json!({"auth_token": raw_token}))
        .send()
        .await
        .expect("viewer C /session");
    assert_eq!(resp_c.status(), reqwest::StatusCode::OK);
    let bytes = read_next_binary(&mut control_ws)
        .await
        .expect("ReadOnlyViewerUpdate #3");
    match decode_control_frame(&bytes).expect("decode") {
        ControlMessage::ReadOnlyViewerUpdate { count, .. } => {
            assert_eq!(count, 3, "third viewer: count must be 3");
        },
        other => panic!("expected ReadOnlyViewerUpdate, got {:?}", other),
    }
    let set_cookie_c = extract_cookie(&resp_c);

    // ---------- all three receive the same ciphertext ----------

    let mut ws_a = connect_viewer_ws(&http, &slug, &set_cookie_a).await;
    let mut ws_b = connect_viewer_ws(&http, &slug, &set_cookie_b).await;
    let mut ws_c = connect_viewer_ws(&http, &slug, &set_cookie_c).await;

    // Give axum a moment to register all three terminal sinks.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let ciphertext: Vec<u8> = vec![0x00, 0xff, 0xab, 0x42, 0xfe, 0x13, 0x00, 0x80];
    let frame = TerminalMessage::TerminalFrameData {
        client_id: zellij_client_id,
        data: ciphertext.clone(),
    };
    terminal_ws
        .send(Message::Binary(frame.encode()))
        .await
        .expect("send TerminalFrameData");

    for (label, ws_stream) in [
        ("A", &mut ws_a),
        ("B", &mut ws_b),
        ("C", &mut ws_c),
    ] {
        let received = timeout(Duration::from_secs(2), ws_stream.next())
            .await
            .unwrap_or_else(|_| panic!("viewer {label}: frame timed out"))
            .unwrap_or_else(|| panic!("viewer {label}: stream closed"))
            .unwrap_or_else(|e| panic!("viewer {label}: ws error {e}"));
        match received {
            Message::Binary(b) => assert_eq!(b, ciphertext, "viewer {label} mismatch"),
            other => panic!("viewer {label}: expected Binary, got {:?}", other),
        }
    }

    // ---------- sequential close: counts 2, 1, 0 ----------

    drop(ws_a);
    expect_update(&mut control_ws, 2).await;

    drop(ws_b);
    expect_update(&mut control_ws, 1).await;

    drop(ws_c);
    expect_update(&mut control_ws, 0).await;

    // No ClientDisconnected must interleave: if the relay sent one, it
    // would have arrived before the count=0 update and `expect_update`
    // would have panicked on it. Belt-and-braces assertion: peek with a
    // short timeout and require no further frame.
    let quick = timeout(Duration::from_millis(100), read_next_binary(&mut control_ws)).await;
    assert!(
        quick.is_err(),
        "no further tunnel frames expected after count=0, got one"
    );

    // ---------- viewer #4 on the dormant group: fresh AuthChallenge ----------

    let http_client_d = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .expect("reqwest client D");
    let viewer_d_fut = async {
        http_client_d
            .post(&session_url)
            .json(&json!({"auth_token": raw_token}))
            .send()
            .await
            .expect("viewer D /session POST")
    };
    let zellij_revive_fut = async {
        let bytes = read_next_binary(&mut control_ws)
            .await
            .expect("AuthChallenge on wake-up");
        let challenge = decode_control_frame(&bytes).expect("decode");
        let request_id = match challenge {
            ControlMessage::AuthChallenge { request_id, .. } => request_id,
            other => panic!("expected AuthChallenge (dormant wake-up), got {:?}", other),
        };
        let response = ControlMessage::AuthResponse {
            request_id,
            client_id: zellij_client_id + 1,
            accepted: true,
            is_read_only: true,
            session_token_hash: "ignored".into(),
            e2e_encrypted: true,
        };
        control_ws
            .send(Message::Binary(response.encode()))
            .await
            .expect("revive AuthResponse send");
        let bytes = read_next_binary(&mut control_ws)
            .await
            .expect("revive ReadOnlyViewerUpdate");
        match decode_control_frame(&bytes).expect("decode") {
            ControlMessage::ReadOnlyViewerUpdate { count, .. } => {
                assert_eq!(count, 1, "revived group must start at count=1");
            },
            other => panic!("expected ReadOnlyViewerUpdate, got {:?}", other),
        }
    };
    let (resp_d, ()) = tokio::join!(viewer_d_fut, zellij_revive_fut);
    assert_eq!(resp_d.status(), reqwest::StatusCode::OK);
    let body_d: serde_json::Value = resp_d.json().await.expect("JSON body");
    assert_eq!(body_d["client_id"], zellij_client_id + 1);
}

fn extract_cookie(resp: &reqwest::Response) -> String {
    resp.headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .find(|h| h.starts_with("relay_session="))
        .and_then(|h| h.split(';').next())
        .map(|s| s.trim().to_string())
        .expect("relay_session cookie")
}

async fn connect_viewer_ws(http: &str, slug: &str, cookie: &str) -> WsStream {
    let host = strip_scheme(http);
    let url = format!("ws://{}/r/{}/ws/terminal", host, slug);
    let request = tokio_tungstenite::tungstenite::http::Request::builder()
        .uri(&url)
        .header("Host", host)
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header(
            "Sec-WebSocket-Key",
            tokio_tungstenite::tungstenite::handshake::client::generate_key(),
        )
        .header("Sec-WebSocket-Version", "13")
        .header("Cookie", cookie)
        .body(())
        .expect("build WS request");
    let (ws_stream, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("viewer WS connect");
    ws_stream
}

async fn expect_update(control_ws: &mut WsStream, expected: u32) {
    let bytes = read_next_binary(control_ws)
        .await
        .unwrap_or_else(|| panic!("expected ReadOnlyViewerUpdate count={expected}"));
    match decode_control_frame(&bytes).expect("decode") {
        ControlMessage::ReadOnlyViewerUpdate { count, .. } => {
            assert_eq!(count, expected);
        },
        ControlMessage::ClientDisconnected { client_id } => {
            panic!(
                "r/o teardown must be count-driven; got ClientDisconnected({client_id})"
            );
        },
        other => panic!(
            "expected ReadOnlyViewerUpdate count={expected}, got {:?}",
            other
        ),
    }
}

#[tokio::test]
async fn dropped_control_socket_evicts_registry() {
    let (_http, ws, registry) = spawn_router().await;
    let (control_ws, _public_url, slug, _tunnel_id) = perform_control_handshake(&ws).await;
    assert!(registry.get(&slug).is_some());

    drop(control_ws);

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if registry.get(&slug).is_none() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("registry entry not evicted within 2s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
