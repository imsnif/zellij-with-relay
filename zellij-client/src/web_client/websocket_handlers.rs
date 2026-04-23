use crate::web_client::authentication::SessionTokenHash;
use crate::web_client::control_message::{
    SetConfigPayload, WebClientToWebServerControlMessage,
    WebClientToWebServerControlMessagePayload, WebServerToWebClientControlMessage,
};
use crate::web_client::local_heartbeat::{
    now_millis, spawn_local_ws_heartbeat, HEARTBEAT_TIMEOUT_SECS,
};
use crate::web_client::message_handlers::{
    parse_stdin, render_to_client, send_control_messages_to_client, StdinSession,
};
use crate::web_client::server_listener::zellij_server_listener;
use crate::web_client::types::{AppState, TerminalParams};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path as AxumPath, Query, State,
    },
    response::IntoResponse,
};
use futures::StreamExt;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use tokio_util::sync::CancellationToken;
use zellij_relay_protocol::crypto;
use zellij_utils::{input::mouse::MouseEvent, ipc::ClientToServerMsg};

pub async fn ws_handler_control(
    ws: WebSocketUpgrade,
    _path: Option<AxumPath<String>>,
    State(state): State<AppState>,
    axum::Extension(session_token_hash): axum::Extension<SessionTokenHash>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_control(socket, state, session_token_hash))
}

pub async fn ws_handler_terminal(
    ws: WebSocketUpgrade,
    session_name: Option<AxumPath<String>>,
    Query(params): Query<TerminalParams>,
    State(state): State<AppState>,
    axum::Extension(session_token_hash): axum::Extension<SessionTokenHash>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| {
        handle_ws_terminal(socket, session_name, params, state, session_token_hash)
    })
}

async fn handle_ws_control(
    socket: WebSocket,
    state: AppState,
    session_token_hash: SessionTokenHash,
) {
    let payload = SetConfigPayload::from(&*state.config.lock().unwrap());
    let set_config_msg = WebServerToWebClientControlMessage::SetConfig(payload);

    let (control_socket_tx, mut control_socket_rx) = socket.split();

    let (control_channel_tx, control_channel_rx) = tokio::sync::mpsc::unbounded_channel();
    send_control_messages_to_client(control_channel_rx, control_socket_tx);

    let _ = control_channel_tx.send(Message::Text(
        serde_json::to_string(&set_config_msg).unwrap().into(),
    ));

    // Phase 6 (Session A): start the heartbeat watchdog. Pings are
    // injected into `control_channel_tx` so they share the sink with
    // regular control frames.
    log::info!("[hb-local-control] wiring heartbeat for incoming /ws/control connection");
    let last_activity = Arc::new(AtomicU64::new(now_millis()));
    let (hb_handle, mut hb_tripped) = spawn_local_ws_heartbeat(
        control_channel_tx.clone(),
        last_activity.clone(),
        "control",
    );

    let send_message_to_server = |deserialized_msg: WebClientToWebServerControlMessage| {
        let Some(client_connection) = state
            .connection_table
            .lock()
            .unwrap()
            .get_client_os_api(&deserialized_msg.web_client_id)
            .cloned()
        else {
            log::error!("Unknown web_client_id: {}", deserialized_msg.web_client_id);
            return;
        };
        let client_msg = match deserialized_msg.payload {
            WebClientToWebServerControlMessagePayload::TerminalResize(size) => {
                ClientToServerMsg::TerminalResize { new_size: size }
            },
        };

        let _ = client_connection.send_to_server(client_msg);
    };

    let mut set_client_control_channel = false;

    loop {
        let msg = tokio::select! {
            next = control_socket_rx.next() => match next {
                Some(Ok(msg)) => msg,
                _ => break,
            },
            _ = &mut hb_tripped => {
                log::warn!(
                    "local control ws silent >{}s — closing",
                    HEARTBEAT_TIMEOUT_SECS
                );
                break;
            }
        };
        last_activity.store(now_millis(), Ordering::Relaxed);
        match msg {
            Message::Ping(payload) => {
                log::info!(
                    "[hb-local-control] inbound PING ({} bytes) — queueing PONG",
                    payload.len()
                );
                let _ = control_channel_tx.send(Message::Pong(payload));
                continue;
            },
            Message::Pong(payload) => {
                log::info!(
                    "[hb-local-control] inbound PONG ({} bytes) — last_activity refreshed",
                    payload.len()
                );
                continue;
            },
            Message::Text(msg) => {
                let deserialized_msg: Result<WebClientToWebServerControlMessage, _> =
                    serde_json::from_str(&msg);
                match deserialized_msg {
                    Ok(deserialized_msg) => {
                        if !state
                            .connection_table
                            .lock()
                            .unwrap()
                            .verify_client_ownership(
                                &deserialized_msg.web_client_id,
                                &session_token_hash.0,
                            )
                        {
                            log::error!(
                                "Client attempted to use web_client_id {} that does not belong to their session",
                                deserialized_msg.web_client_id
                            );
                            return;
                        }
                        if !set_client_control_channel {
                            set_client_control_channel = true;
                            state
                                .connection_table
                                .lock()
                                .unwrap()
                                .add_client_control_tx(
                                    &deserialized_msg.web_client_id,
                                    control_channel_tx.clone(),
                                );
                        }
                        send_message_to_server(deserialized_msg);
                    },
                    Err(e) => {
                        log::error!("Failed to deserialize client msg: {:?}", e);
                    },
                }
            },
            Message::Close(_) => {
                break;
            },
            _ => {
                log::error!("Unsupported messagetype : {:?}", msg);
            },
        }
    }
    hb_handle.abort();
}

async fn handle_ws_terminal(
    socket: WebSocket,
    session_name: Option<AxumPath<String>>,
    params: TerminalParams,
    state: AppState,
    session_token_hash: SessionTokenHash,
) {
    let web_client_id = params.web_client_id;

    // Verify the session token owns this web_client_id
    if !state
        .connection_table
        .lock()
        .unwrap()
        .verify_client_ownership(&web_client_id, &session_token_hash.0)
    {
        log::error!(
            "Terminal WebSocket: client does not own web_client_id {}",
            web_client_id
        );
        return;
    }

    let Some(os_input) = state
        .connection_table
        .lock()
        .unwrap()
        .get_client_os_api(&web_client_id)
        .cloned()
    else {
        log::error!("Unknown web_client_id: {}", web_client_id);
        return;
    };

    let (client_terminal_channel_tx, mut client_terminal_channel_rx) = socket.split();
    let (stdout_channel_tx, stdout_channel_rx) = tokio::sync::mpsc::unbounded_channel();
    let e2e_key = state
        .connection_table
        .lock()
        .unwrap()
        .get_client_e2e_key(&web_client_id);
    state
        .connection_table
        .lock()
        .unwrap()
        .add_client_terminal_tx(&web_client_id, stdout_channel_tx);

    let (attachment_complete_tx, attachment_complete_rx) = tokio::sync::oneshot::channel();

    zellij_server_listener(
        os_input.clone(),
        state.connection_table.clone(),
        session_name.map(|p| p.0),
        state.config.lock().unwrap().clone(),
        state.config_options.clone(),
        Some(state.config_file_path.clone()),
        web_client_id.clone(),
        state.session_manager.clone(),
        Some(attachment_complete_tx),
    );

    let terminal_channel_cancellation_token = CancellationToken::new();
    let should_not_reconnect = state
        .connection_table
        .lock()
        .unwrap()
        .get_should_not_reconnect_flag(&web_client_id)
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));

    // Phase 6 (Session A): heartbeat pings routed into the render-side
    // writer via a dedicated Ping channel; activity watchdog sits on
    // the reader loop below.
    log::info!("[hb-local-terminal] wiring heartbeat for incoming /ws/terminal connection");
    let (terminal_ping_tx, terminal_ping_rx) =
        tokio::sync::mpsc::unbounded_channel::<Message>();
    let terminal_last_activity = Arc::new(AtomicU64::new(now_millis()));
    let (terminal_hb_handle, mut terminal_hb_tripped) = spawn_local_ws_heartbeat(
        terminal_ping_tx.clone(),
        terminal_last_activity.clone(),
        "terminal",
    );

    render_to_client(
        stdout_channel_rx,
        client_terminal_channel_tx,
        terminal_channel_cancellation_token.clone(),
        should_not_reconnect,
        e2e_key,
        terminal_ping_rx,
    );
    state
        .connection_table
        .lock()
        .unwrap()
        .add_client_terminal_channel_cancellation_token(
            &web_client_id,
            terminal_channel_cancellation_token,
        );

    let explicitly_disable_kitty_keyboard_protocol = state
        .config
        .lock()
        .unwrap()
        .options
        .support_kitty_keyboard_protocol
        .map(|e| !e)
        .unwrap_or(false);

    let _ = attachment_complete_rx.await;

    let mut mouse_old_event = MouseEvent::new();
    // Per-connection parser state. Hoisted so a CSI / Kitty sequence
    // split across two WebSocket frames resolves on the second frame.
    let mut stdin_session = StdinSession::new(explicitly_disable_kitty_keyboard_protocol);
    let finalize_idle = std::time::Duration::from_millis(50);
    loop {
        // Race the next frame against the heartbeat watchdog. When
        // termwiz is holding ambiguous-but-complete events from the
        // previous frame, also race against an idle timeout so the
        // held events still drain if no further frame arrives.
        let result = if stdin_session.pending_finalize() {
            tokio::select! {
                msg = client_terminal_channel_rx.next() => Some(msg),
                _ = tokio::time::sleep(finalize_idle) => None,
                _ = &mut terminal_hb_tripped => {
                    log::warn!(
                        "local terminal ws silent >{}s — closing",
                        HEARTBEAT_TIMEOUT_SECS
                    );
                    break;
                }
            }
        } else {
            tokio::select! {
                msg = client_terminal_channel_rx.next() => Some(msg),
                _ = &mut terminal_hb_tripped => {
                    log::warn!(
                        "local terminal ws silent >{}s — closing",
                        HEARTBEAT_TIMEOUT_SECS
                    );
                    break;
                }
            }
        };
        let msg = match result {
            Some(Some(Ok(m))) => m,
            Some(_) => break,
            None => {
                // Idle timeout fired with `pending_finalize` set:
                // drain any ambiguous-but-complete events termwiz held
                // back on the previous frame.
                if let Some(client_connection) = state
                    .connection_table
                    .lock()
                    .unwrap()
                    .get_client_os_api(&web_client_id)
                    .cloned()
                {
                    stdin_session.finalize(&*client_connection, &mut mouse_old_event);
                } else {
                    // No client to send drained events to — clear the
                    // flag so we don't busy-loop the idle timer.
                    stdin_session.clear_pending_finalize();
                }
                continue;
            },
        };
        terminal_last_activity.store(now_millis(), Ordering::Relaxed);
        match msg {
            Message::Ping(p) => {
                log::info!(
                    "[hb-local-terminal] inbound PING ({} bytes) — queueing PONG",
                    p.len()
                );
                let _ = terminal_ping_tx.send(Message::Pong(p));
                continue;
            },
            Message::Pong(p) => {
                log::info!(
                    "[hb-local-terminal] inbound PONG ({} bytes) — last_activity refreshed",
                    p.len()
                );
                continue;
            },
            Message::Binary(buf) => {
                let Some(client_connection) = state
                    .connection_table
                    .lock()
                    .unwrap()
                    .get_client_os_api(&web_client_id)
                    .cloned()
                else {
                    log::error!("Unknown web_client_id: {}", web_client_id);
                    continue;
                };
                // With E2E on, decrypt before handing to the parser; a
                // failure here implies tampering or a client bug — drop
                // the frame and wait for the next one.
                let parsed: Vec<u8> = match &e2e_key {
                    Some(key) => match crypto::decrypt(key, &buf) {
                        Ok(plaintext) => plaintext,
                        Err(e) => {
                            log::warn!(
                                "local e2e decrypt failed for client {}: {} — dropping frame",
                                web_client_id, e
                            );
                            continue;
                        }
                    },
                    None => buf.to_vec(),
                };
                parse_stdin(
                    &parsed,
                    client_connection.clone(),
                    &mut mouse_old_event,
                    &mut stdin_session,
                );
            },
            Message::Text(msg) => {
                let Some(client_connection) = state
                    .connection_table
                    .lock()
                    .unwrap()
                    .get_client_os_api(&web_client_id)
                    .cloned()
                else {
                    log::error!("Unknown web_client_id: {}", web_client_id);
                    continue;
                };
                if e2e_key.is_some() {
                    // With E2E on, plaintext frames from the client are a
                    // protocol error — the client should always encrypt.
                    log::warn!(
                        "got plaintext Text frame from client {} while E2E is on — dropping",
                        web_client_id
                    );
                    continue;
                }
                parse_stdin(
                    msg.as_bytes(),
                    client_connection.clone(),
                    &mut mouse_old_event,
                    &mut stdin_session,
                );
            },
            Message::Close(_) => {
                state
                    .connection_table
                    .lock()
                    .unwrap()
                    .remove_client(&web_client_id);
                break;
            },
        }
    }
    terminal_hb_handle.abort();
    os_input.send_to_server(ClientToServerMsg::ClientExited);
}
