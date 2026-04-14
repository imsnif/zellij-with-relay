use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::Response,
    routing::any,
    Router,
};
use futures_util::StreamExt;
use tokio::net::TcpListener;
use tokio::time::timeout;
use zellij_relay_protocol::{decode_control_frame, ControlMessage};
use zellij_utils::input::{config::Config, options::Options};

use crate::os_input_output::ClientOsApi;
use crate::web_client::relay::{start_relay_tunnel, stop_relay_tunnel};
use crate::web_client::types::{ClientOsApiFactory, ConnectionTable, SessionManager};

#[derive(Debug)]
struct UnusedOsApiFactory;
impl ClientOsApiFactory for UnusedOsApiFactory {
    fn create_client_os_api(&self) -> Result<Box<dyn ClientOsApi>, Box<dyn std::error::Error>> {
        Err("unused in these handshake-only tests".into())
    }
}

#[derive(Debug)]
struct UnusedSessionManager;
impl SessionManager for UnusedSessionManager {
    fn session_exists(&self, _n: &str) -> Result<bool, Box<dyn std::error::Error>> {
        Ok(false)
    }
    fn get_resurrection_layout(
        &self,
        _n: &str,
    ) -> Option<zellij_utils::input::layout::Layout> {
        None
    }
    fn spawn_session_if_needed(
        &self,
        _n: &str,
        _os_input: Box<dyn ClientOsApi>,
        _exists: bool,
        _pipe: &PathBuf,
        _first: zellij_utils::ipc::ClientToServerMsg,
    ) {
    }
}

/// Helper: bundle the extra args that `start_relay_tunnel` needs. These
/// tests only exercise the handshake, so dummy impls are sufficient — the
/// multiplexer never dispatches a virtual client in the happy-path test.
fn dummy_relay_context() -> (
    Arc<Mutex<ConnectionTable>>,
    Arc<dyn ClientOsApiFactory>,
    Arc<dyn SessionManager>,
    Arc<Mutex<Config>>,
    Options,
    PathBuf,
) {
    (
        Arc::new(Mutex::new(ConnectionTable::default())),
        Arc::new(UnusedOsApiFactory),
        Arc::new(UnusedSessionManager),
        Arc::new(Mutex::new(Config::default())),
        Options::default(),
        PathBuf::from("/tmp/zellij-relay-tests"),
    )
}

async fn spawn_router(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router.into_make_service()).await;
    });
    format!("ws://{}", addr)
}

fn happy_relay_router() -> Router {
    use axum::extract::Query;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct State {
        tunnel_id: Arc<Mutex<Option<String>>>,
    }
    let state = State::default();

    async fn control_handler(
        ws: WebSocketUpgrade,
        axum::extract::State(state): axum::extract::State<State>,
    ) -> Response {
        ws.on_upgrade(move |mut socket: WebSocket| async move {
            let msg = socket.next().await;
            let bytes = match msg {
                Some(Ok(Message::Binary(b))) => b,
                _ => return,
            };
            match decode_control_frame(&bytes) {
                Ok(ControlMessage::Auth { .. }) => {},
                _ => return,
            }
            let tunnel_id = "tid-happy".to_string();
            *state.tunnel_id.lock().unwrap() = Some(tunnel_id.clone());
            let est = ControlMessage::Established {
                public_url: "http://localhost:8765/r/abc12345".to_string(),
                slug: "abc12345".to_string(),
                tunnel_id: tunnel_id.clone(),
            };
            let _ = socket.send(Message::Binary(est.encode().into())).await;
            while let Some(frame) = socket.next().await {
                match frame {
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => {},
                }
            }
        })
    }

    async fn terminal_handler(
        ws: WebSocketUpgrade,
        Query(_params): Query<HashMap<String, String>>,
    ) -> Response {
        ws.on_upgrade(|mut socket: WebSocket| async move {
            let _ = socket.next().await;
            while let Some(frame) = socket.next().await {
                match frame {
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => {},
                }
            }
        })
    }

    Router::new()
        .route("/tunnel/control", any(control_handler))
        .route("/tunnel/terminal", any(terminal_handler))
        .with_state(state)
}

#[tokio::test]
async fn start_relay_tunnel_happy_path() {
    let relay_url = spawn_router(happy_relay_router()).await;
    let (ct, factory, sm, cfg, opts, path) = dummy_relay_context();
    let public_url = start_relay_tunnel(
        1u16,
        relay_url,
        "sess".to_string(),
        "0.45.0".to_string(),
        ct,
        factory,
        sm,
        cfg,
        opts,
        path,
    )
    .await
    .expect("start_relay_tunnel ok");
    assert_eq!(public_url, "http://localhost:8765/r/abc12345");

    assert!(stop_relay_tunnel(1u16).await);
    assert!(!stop_relay_tunnel(1u16).await);
}

#[tokio::test]
async fn stop_relay_tunnel_unknown_id_returns_false() {
    assert!(!stop_relay_tunnel(999u16).await);
}

fn rejection_router() -> Router {
    async fn control_handler(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(|mut socket: WebSocket| async move {
            let _ = socket.next().await;
            let err = ControlMessage::Error {
                message: "rejected".to_string(),
            };
            let _ = socket.send(Message::Binary(err.encode().into())).await;
        })
    }
    Router::new().route("/tunnel/control", any(control_handler))
}

#[tokio::test]
async fn start_relay_tunnel_surfaces_rejection() {
    let relay_url = spawn_router(rejection_router()).await;
    let (ct, factory, sm, cfg, opts, path) = dummy_relay_context();
    let result = start_relay_tunnel(
        2u16,
        relay_url,
        "sess".to_string(),
        "0.45.0".to_string(),
        ct,
        factory,
        sm,
        cfg,
        opts,
        path,
    )
    .await;
    let err = result.expect_err("should be rejected");
    let rendered = format!("{:#}", err);
    assert!(rendered.contains("rejected"), "got: {rendered}");
    assert!(!stop_relay_tunnel(2u16).await);
}

fn closing_router() -> Router {
    async fn control_handler(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(|mut socket: WebSocket| async move {
            let _ = socket.next().await;
            drop(socket);
        })
    }
    Router::new().route("/tunnel/control", any(control_handler))
}

#[tokio::test]
async fn start_relay_tunnel_handles_closed_socket() {
    let relay_url = spawn_router(closing_router()).await;
    let (ct, factory, sm, cfg, opts, path) = dummy_relay_context();
    let result = timeout(
        Duration::from_secs(5),
        start_relay_tunnel(
            3u16,
            relay_url,
            "sess".to_string(),
            "0.45.0".to_string(),
            ct,
            factory,
            sm,
            cfg,
            opts,
            path,
        ),
    )
    .await
    .expect("should not hang");
    assert!(result.is_err());
    assert!(!stop_relay_tunnel(3u16).await);
}
