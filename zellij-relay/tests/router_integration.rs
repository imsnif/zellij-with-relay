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
