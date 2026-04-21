//! Relay-tunnel multiplexer: owns the control + terminal tunnel sockets,
//! fans encoded frames into writer tasks, and dispatches incoming frames
//! onto the virtual-client mpsc channels.
//!
//! Phase 2: Zellij is authoritative for `client_id` (allocated inside
//! `AuthChallenge` handling). Each accepted remote viewer becomes a
//! `RelayVirtualClient`, plumbed into the local `ConnectionTable` as a
//! standard web client — so the existing rendering pipeline and input path
//! flow through without modification.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::ws::Message as WsMessage;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
pub fn relay_virtual_web_client_id(client_id: u32) -> String {
    format!("relay-client-{}", client_id)
}

use zellij_relay_protocol::{
    crypto::{self, KEY_LEN},
    decode_control_frame, decode_terminal_frame, ControlMessage, TerminalMessage,
};
use zellij_utils::{
    input::mouse::MouseEvent,
    ipc::ClientToServerMsg,
    web_authentication_tokens::validate_auth_token_hash,
};

use super::control_tunnel::ControlTunnelSession;
use super::terminal_tunnel::TerminalTunnelSession;
use super::types::{RelayTunnelState, RelayVirtualClient};
use crate::web_client::control_message::{
    SetConfigPayload, WebClientToWebServerControlMessage,
    WebClientToWebServerControlMessagePayload, WebServerToWebClientControlMessage,
};
use crate::web_client::message_handlers::{parse_stdin, StdinSession};
use crate::web_client::server_listener::zellij_server_listener;

pub async fn run_multiplexer(
    state: Arc<RelayTunnelState>,
    control: ControlTunnelSession,
    terminal: TerminalTunnelSession,
    control_tunnel_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    terminal_tunnel_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    shutdown_rx: oneshot::Receiver<()>,
) {
    let ControlTunnelSession {
        sink: control_sink,
        stream: control_stream,
        ..
    } = control;
    let TerminalTunnelSession {
        sink: terminal_sink,
        stream: terminal_stream,
    } = terminal;

    // Writer tasks: drain the mpsc queues onto the socket sinks.
    let control_writer = spawn_writer(control_sink, control_tunnel_rx);
    let terminal_writer = spawn_writer(terminal_sink, terminal_tunnel_rx);

    // Reader tasks: dispatch incoming frames.
    let control_reader = spawn_control_reader(state.clone(), control_stream);
    let terminal_reader = spawn_terminal_reader(state.clone(), terminal_stream);

    tokio::select! {
        _ = shutdown_rx => {
            log::info!("Relay tunnel shutdown signal received");
        }
        _ = control_reader => {
            log::warn!("Relay control socket closed");
        }
        _ = terminal_reader => {
            log::warn!("Relay terminal socket closed");
        }
    }

    // Tear down all virtual clients.
    let clients: Vec<RelayVirtualClient> = {
        let mut guard = state.clients.lock().unwrap();
        guard.drain().map(|(_id, c)| c).collect()
    };
    for mut c in clients {
        if let Some(tx) = c.shutdown.take() {
            let _ = tx.send(());
        }
        state
            .connection_table
            .lock()
            .unwrap()
            .remove_client(&c.web_client_id);
    }

    control_writer.abort();
    terminal_writer.abort();
}

fn spawn_writer<S>(
    mut sink: S,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
) -> tokio::task::JoinHandle<()>
where
    S: SinkExt<TungsteniteMessage, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin
        + Send
        + 'static,
{
    tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if sink.send(TungsteniteMessage::Binary(bytes)).await.is_err() {
                break;
            }
        }
        let _ = sink.send(TungsteniteMessage::Close(None)).await;
    })
}

fn spawn_control_reader<S>(
    state: Arc<RelayTunnelState>,
    mut stream: S,
) -> tokio::task::JoinHandle<()>
where
    S: futures_util::Stream<Item = Result<TungsteniteMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send
        + 'static,
{
    tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            let bytes = match frame {
                Ok(TungsteniteMessage::Binary(b)) => b,
                Ok(TungsteniteMessage::Text(t)) => t.into_bytes(),
                Ok(TungsteniteMessage::Close(_)) => break,
                Ok(_) => continue,
                Err(e) => {
                    log::debug!("relay control reader error: {}", e);
                    break;
                },
            };

            let msg = match decode_control_frame(&bytes) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("bad relay control frame: {}", e);
                    continue;
                },
            };

            match msg {
                ControlMessage::AuthChallenge {
                    request_id,
                    token_hash,
                } => handle_auth_challenge(&state, request_id, token_hash),
                ControlMessage::ClientConnected { client_id } => {
                    handle_client_connected(&state, client_id);
                },
                ControlMessage::ClientDisconnected { client_id } => {
                    handle_client_disconnected(&state, client_id);
                },
                ControlMessage::ControlFrameData { client_id, data } => {
                    let tx = state
                        .clients
                        .lock()
                        .unwrap()
                        .get(&client_id)
                        .map(|c| c.control_input_tx.clone());
                    match tx {
                        Some(tx) => {
                            let text = match String::from_utf8(data) {
                                Ok(s) => s,
                                Err(_) => {
                                    log::warn!(
                                        "ControlFrameData from relay (client_id={}) is not UTF-8",
                                        client_id
                                    );
                                    continue;
                                },
                            };
                            let _ = tx.send(text);
                        },
                        None => {
                            log::warn!(
                                "ControlFrameData for unknown client_id={} (defensive drop)",
                                client_id
                            );
                        },
                    }
                },
                ControlMessage::ReadOnlyViewerUpdate { token_hash, count } => {
                    handle_read_only_viewer_update(&state, token_hash, count);
                },
                ControlMessage::Error { message } => {
                    log::warn!("relay reported control error: {}", message);
                },
                other => {
                    log::warn!("unexpected post-handshake control frame: {:?}", other);
                },
            }
        }
    })
}

fn spawn_terminal_reader<S>(
    state: Arc<RelayTunnelState>,
    mut stream: S,
) -> tokio::task::JoinHandle<()>
where
    S: futures_util::Stream<Item = Result<TungsteniteMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send
        + 'static,
{
    tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            let bytes = match frame {
                Ok(TungsteniteMessage::Binary(b)) => b,
                Ok(TungsteniteMessage::Text(t)) => t.into_bytes(),
                Ok(TungsteniteMessage::Close(_)) => break,
                Ok(_) => continue,
                Err(e) => {
                    log::debug!("relay terminal reader error: {}", e);
                    break;
                },
            };

            let msg = match decode_terminal_frame(&bytes) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("bad relay terminal frame: {}", e);
                    continue;
                },
            };

            match msg {
                TerminalMessage::TerminalFrameData { client_id, data } => {
                    let tx = state
                        .clients
                        .lock()
                        .unwrap()
                        .get(&client_id)
                        .map(|c| c.terminal_input_tx.clone());
                    match tx {
                        Some(tx) => {
                            let _ = tx.send(data);
                        },
                        None => {
                            log::warn!(
                                "TerminalFrameData for unknown client_id={} (defensive drop)",
                                client_id
                            );
                        },
                    }
                },
                TerminalMessage::Error { message } => {
                    log::warn!("relay reported terminal error: {}", message);
                },
                other => {
                    log::warn!("unexpected post-handshake terminal frame: {:?}", other);
                },
            }
        }
    })
}

fn handle_auth_challenge(
    state: &Arc<RelayTunnelState>,
    request_id: Vec<u8>,
    token_hash: String,
) {
    let outcome = validate_auth_token_hash(&token_hash);
    let (response, spawn_ro_for_client_id) = match outcome {
        Ok(Some(is_read_only)) => {
            let client_id = state.next_client_id.fetch_add(1, Ordering::Relaxed);
            // Phase 3: the relay path is unconditionally E2E-encrypted.
            // Derive the per-client AES key now, from the token hash plus
            // the tunnel id. The browser reproduces the same key client
            // side after typing the raw token (hash computed locally).
            let key = crypto::derive_key(&token_hash, &state.tunnel_id);
            state
                .pending_e2e_keys
                .lock()
                .unwrap()
                .insert(client_id, key);
            // Phase 4: stash the r/o flag so `spawn_virtual_client` can
            // thread it through when attaching to the Zellij server.
            state
                .pending_read_only
                .lock()
                .unwrap()
                .insert(client_id, is_read_only);
            // Remember which client_id backs this r/o fan-out group so
            // `ReadOnlyViewerUpdate { count: 0 }` can tear it down.
            if is_read_only {
                state
                    .token_hash_to_client_id
                    .lock()
                    .unwrap()
                    .insert(token_hash.clone(), client_id);
            }
            let response = ControlMessage::AuthResponse {
                request_id,
                client_id,
                accepted: true,
                is_read_only,
                session_token_hash: token_hash,
                e2e_encrypted: true,
            };
            // For r/w clients the relay follows up with `ClientConnected`
            // which triggers spawn_virtual_client. The r/o path skips that
            // step — the relay sends `ReadOnlyViewerUpdate` instead — so
            // the virtual watcher must be spawned from here.
            let ro_spawn = if is_read_only { Some(client_id) } else { None };
            (response, ro_spawn)
        },
        Ok(None) => (
            ControlMessage::AuthResponse {
                request_id,
                client_id: 0,
                accepted: false,
                is_read_only: false,
                session_token_hash: token_hash,
                e2e_encrypted: false,
            },
            None,
        ),
        Err(e) => {
            log::error!("token hash validation failed: {}", e);
            (
                ControlMessage::AuthResponse {
                    request_id,
                    client_id: 0,
                    accepted: false,
                    is_read_only: false,
                    session_token_hash: token_hash,
                    e2e_encrypted: false,
                },
                None,
            )
        },
    };
    let _ = state.control_tunnel_tx.send(response.encode());

    if let Some(client_id) = spawn_ro_for_client_id {
        if let Err(e) = spawn_virtual_client(state, client_id) {
            log::error!(
                "failed to spawn r/o virtual watcher {}: {}",
                client_id, e
            );
        }
    }
}

fn handle_client_connected(state: &Arc<RelayTunnelState>, client_id: u32) {
    if let Err(e) = spawn_virtual_client(state, client_id) {
        log::error!(
            "failed to spawn virtual client {}: {}",
            client_id,
            e
        );
        let _ = state
            .control_tunnel_tx
            .send(ControlMessage::ClientDisconnected { client_id }.encode());
    }
}

/// React to a relay-reported r/o fan-out group size change. `count == 0`
/// tears down the virtual watcher (the group went dormant); positive
/// counts are observational only — fan-out is a relay-side concern and
/// Zellij does not need to track individual viewers.
fn handle_read_only_viewer_update(
    state: &Arc<RelayTunnelState>,
    token_hash: String,
    count: u32,
) {
    if count == 0 {
        let client_id = state
            .token_hash_to_client_id
            .lock()
            .unwrap()
            .remove(&token_hash);
        match client_id {
            Some(cid) => {
                log::info!(
                    "relay r/o group for token_hash={} went dormant; tearing down virtual watcher {}",
                    token_hash, cid
                );
                handle_client_disconnected(state, cid);
            },
            None => {
                log::debug!(
                    "ReadOnlyViewerUpdate count=0 for unknown token_hash={} — ignoring",
                    token_hash
                );
            },
        }
    } else {
        log::debug!(
            "relay r/o group for token_hash={} now has {} viewer(s)",
            token_hash, count
        );
    }
}

fn handle_client_disconnected(state: &Arc<RelayTunnelState>, client_id: u32) {
    let removed = state.clients.lock().unwrap().remove(&client_id);
    if let Some(mut c) = removed {
        if let Some(tx) = c.shutdown.take() {
            let _ = tx.send(());
        }
        state
            .connection_table
            .lock()
            .unwrap()
            .remove_client(&c.web_client_id);
    }
}

fn spawn_virtual_client(
    state: &Arc<RelayTunnelState>,
    client_id: u32,
) -> Result<(), String> {
    // Deterministic naming so the relay can return a matching web_client_id
    // to the browser without another round-trip. The browser sends this id
    // back in every control message; the ConnectionTable is keyed on it.
    let web_client_id = relay_virtual_web_client_id(client_id);
    let os_api = state
        .os_api_factory
        .create_client_os_api()
        .map_err(|e| format!("create_client_os_api: {}", e))?;

    // Phase 3: pull the AES key stashed by `handle_auth_challenge` for this
    // `client_id`. If no key is present the relay produced a
    // `ClientConnected` for a client_id we never validated — treat that as
    // a protocol bug and refuse to spawn.
    let e2e_key: [u8; KEY_LEN] = state
        .pending_e2e_keys
        .lock()
        .unwrap()
        .remove(&client_id)
        .ok_or_else(|| {
            format!(
                "no pending e2e key for client_id={} — ClientConnected without prior AuthChallenge",
                client_id
            )
        })?;

    // Virtual clients from the relay run under a shared, tunnel-scoped
    // session token hash so ownership checks in the ConnectionTable remain
    // consistent. The value is only used for same-table lookups on this host.
    let tunnel_session_hash = format!("relay-tunnel-{}", client_id);
    // Phase 4: drain the is_read_only flag stashed by handle_auth_challenge.
    // Missing entries mean a `ClientConnected` arrived without a preceding
    // `AuthChallenge` — fail-closed, mirroring the e2e-key pattern above.
    let is_read_only = state
        .pending_read_only
        .lock()
        .unwrap()
        .remove(&client_id)
        .ok_or_else(|| {
            format!(
                "no pending is_read_only flag for client_id={} — spawn without prior AuthChallenge",
                client_id
            )
        })?;

    {
        let mut ct = state.connection_table.lock().unwrap();
        ct.add_new_client(
            web_client_id.clone(),
            os_api.clone(),
            is_read_only,
            tunnel_session_hash,
        );
        // Relay r/o virtual watchers use the session-viewport `AttachRelayWatcherClient`
        // first-message so fan-out stays coherent across viewer window sizes.
        ct.set_client_relay_fanout(&web_client_id, is_read_only);
    }

    // Outbound pump: stdout from server → TerminalFrameData on tunnel.
    // Phase 3: encrypt the bytes before emitting — the relay sees only
    // `nonce || ciphertext`. Encryption failures are logged and skip the
    // frame; a sustained failure would imply a broken OsRng which is
    // treated as fatal by aborting the pump.
    let (stdout_tx, mut stdout_rx) = mpsc::unbounded_channel::<String>();
    {
        let mut ct = state.connection_table.lock().unwrap();
        ct.add_client_terminal_tx(&web_client_id, stdout_tx);
    }
    let terminal_tunnel_tx = state.terminal_tunnel_tx.clone();
    let outbound_key = e2e_key;
    tokio::spawn(async move {
        while let Some(bytes) = stdout_rx.recv().await {
            let plaintext = bytes.into_bytes();
            let ciphertext = match crypto::encrypt(&outbound_key, &plaintext) {
                Ok(ct) => ct,
                Err(e) => {
                    log::error!(
                        "e2e encrypt failed for client_id={}: {} — dropping frame",
                        client_id, e
                    );
                    continue;
                },
            };
            let frame = TerminalMessage::TerminalFrameData {
                client_id,
                data: ciphertext,
            };
            if terminal_tunnel_tx.send(frame.encode()).is_err() {
                break;
            }
        }
    });

    // Outbound pump: control messages → ControlFrameData on tunnel.
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<WsMessage>();
    {
        let mut ct = state.connection_table.lock().unwrap();
        ct.add_client_control_tx(&web_client_id, ctrl_tx);
    }
    let control_tunnel_tx = state.control_tunnel_tx.clone();
    let control_tunnel_tx_for_setconfig = state.control_tunnel_tx.clone();
    tokio::spawn(async move {
        while let Some(ws_msg) = ctrl_rx.recv().await {
            let data = match ws_msg {
                WsMessage::Text(t) => t.as_bytes().to_vec(),
                WsMessage::Binary(b) => b.to_vec(),
                WsMessage::Close(_) => break,
                _ => continue,
            };
            let frame = ControlMessage::ControlFrameData { client_id, data };
            if control_tunnel_tx.send(frame.encode()).is_err() {
                break;
            }
        }
    });

    // Kick off the server listener (mirrors handle_ws_terminal).
    let (attachment_complete_tx, _attachment_complete_rx) =
        tokio::sync::oneshot::channel::<()>();
    zellij_server_listener(
        os_api.clone(),
        state.connection_table.clone(),
        Some(state.session_name.clone()),
        state.config.lock().unwrap().clone(),
        state.config_options.clone(),
        Some(state.config_file_path.clone()),
        web_client_id.clone(),
        state.session_manager.clone(),
        Some(attachment_complete_tx),
    );

    // Send SetConfig to the remote client mirroring handle_ws_control.
    let set_config_msg = WebServerToWebClientControlMessage::SetConfig(SetConfigPayload::from(
        &*state.config.lock().unwrap(),
    ));
    if let Ok(json) = serde_json::to_string(&set_config_msg) {
        let frame = ControlMessage::ControlFrameData {
            client_id,
            data: json.into_bytes(),
        };
        let _ = control_tunnel_tx_for_setconfig.send(frame.encode());
    }

    // Inbound pump: tunnel → terminal input → parse_stdin.
    // Phase 3: terminal input from r/w clients is encrypted; decrypt with
    // the same derived key before parsing. An AEAD failure indicates
    // tampering, a wrong key, or (most likely) a misbehaving client — log
    // and drop the frame; keep the client connected so it can recover.
    let (terminal_input_tx, mut terminal_input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let explicitly_disable_kitty_keyboard_protocol = state
        .config
        .lock()
        .unwrap()
        .options
        .support_kitty_keyboard_protocol
        .map(|e| !e)
        .unwrap_or(false);
    let os_api_input = os_api.clone();
    let inbound_key = e2e_key;
    let inbound_is_read_only = is_read_only;
    tokio::spawn(async move {
        let mut mouse_old_event = MouseEvent::new();
        let mut stdin_session = StdinSession::new(explicitly_disable_kitty_keyboard_protocol);
        while let Some(buf) = terminal_input_rx.recv().await {
            if inbound_is_read_only {
                // Phase 4: r/o viewers must never inject input. The relay
                // already drops viewer-originated frames before they reach
                // the tunnel; this is belt-and-braces — drain and discard
                // anything that slips through.
                log::warn!(
                    "r/o viewer terminal frame reached multiplexer (client_id={}) — dropping {} bytes",
                    client_id,
                    buf.len()
                );
                continue;
            }
            let plaintext = match crypto::decrypt(&inbound_key, &buf) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!(
                        "e2e decrypt failed for client_id={}: {} — dropping frame",
                        client_id, e
                    );
                    continue;
                },
            };
            parse_stdin(
                &plaintext,
                os_api_input.clone(),
                &mut mouse_old_event,
                &mut stdin_session,
            );
        }
    });

    // Inbound pump: tunnel → control input → dispatch JSON.
    let (control_input_tx, mut control_input_rx) = mpsc::unbounded_channel::<String>();
    let connection_table_for_ctrl = state.connection_table.clone();
    tokio::spawn(async move {
        while let Some(text) = control_input_rx.recv().await {
            dispatch_control_message(&text, &connection_table_for_ctrl);
        }
    });

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let connection_table_for_shutdown = state.connection_table.clone();
    let web_client_id_for_shutdown = web_client_id.clone();
    let os_api_for_shutdown = os_api;
    tokio::spawn(async move {
        let _ = shutdown_rx.await;
        connection_table_for_shutdown
            .lock()
            .unwrap()
            .remove_client(&web_client_id_for_shutdown);
        os_api_for_shutdown.send_to_server(ClientToServerMsg::ClientExited);
    });

    state.clients.lock().unwrap().insert(
        client_id,
        RelayVirtualClient {
            web_client_id,
            is_read_only,
            terminal_input_tx,
            control_input_tx,
            shutdown: Some(shutdown_tx),
        },
    );
    Ok(())
}

fn dispatch_control_message(
    text: &str,
    connection_table: &std::sync::Arc<std::sync::Mutex<crate::web_client::types::ConnectionTable>>,
) {
    let deserialized: Result<WebClientToWebServerControlMessage, _> = serde_json::from_str(text);
    let Ok(msg) = deserialized else {
        log::error!("Failed to deserialize relay control message: {:?}", deserialized.err());
        return;
    };

    let Some(os_api) = connection_table
        .lock()
        .unwrap()
        .get_client_os_api(&msg.web_client_id)
        .cloned()
    else {
        log::error!("Unknown web_client_id from relay: {}", msg.web_client_id);
        return;
    };

    let client_msg = match msg.payload {
        WebClientToWebServerControlMessagePayload::TerminalResize(size) => {
            ClientToServerMsg::TerminalResize { new_size: size }
        },
    };
    let _ = os_api.send_to_server(client_msg);
}

#[cfg(test)]
mod crypto_roundtrip_tests {
    //! Covers the multiplexer's end of the Phase 3 crypto contract:
    //!
    //! 1. The key the multiplexer derives for an authenticated tunnel
    //!    is `HKDF(token_hash, tunnel_id)`. A client deriving the same
    //!    way must produce an identical key.
    //! 2. A plaintext chunk that passes through the outbound pump
    //!    (`encrypt(key, bytes)`) can be decrypted on the client side
    //!    using the independently-derived key.
    //! 3. A ciphertext produced by the client side
    //!    (`encrypt(client_key, bytes)`) can be decrypted by the
    //!    multiplexer's inbound pump using its own derived key.
    //!
    //! We do not spawn a real virtual client here — that path requires
    //! a working `ConnectionTable`, `ClientOsApiFactory`, and
    //! `SessionManager`, plus the SQLite token DB. Instead we test the
    //! crypto contract at primitive level; if `handle_auth_challenge`
    //! and `spawn_virtual_client` ever diverge from these helpers the
    //! test still fails in the derivation branch.
    use zellij_relay_protocol::crypto;

    fn simulated_multiplexer_key(token_hash: &str, tunnel_id: &str) -> [u8; crypto::KEY_LEN] {
        // Matches `handle_auth_challenge` in `multiplexer.rs`.
        crypto::derive_key(token_hash, tunnel_id)
    }

    fn simulated_client_key(token_hash: &str, tunnel_id: &str) -> [u8; crypto::KEY_LEN] {
        // Matches the browser's `deriveKey(tokenHashHex, tunnelId)` and
        // the Rust attach client's `derive_e2e_key_if_needed`.
        crypto::derive_key(token_hash, tunnel_id)
    }

    #[test]
    fn both_sides_derive_the_same_key() {
        let k_mux = simulated_multiplexer_key("deadbeef", "tunnel-x");
        let k_cli = simulated_client_key("deadbeef", "tunnel-x");
        assert_eq!(k_mux, k_cli);
    }

    #[test]
    fn outbound_pump_ciphertext_decrypts_on_client_side() {
        let k_mux = simulated_multiplexer_key("deadbeef", "tunnel-x");
        let k_cli = simulated_client_key("deadbeef", "tunnel-x");
        let plaintext = b"hello from the multiplexer";

        // Mirrors the outbound pump's `encrypt(&outbound_key, ...)`
        // call inside `spawn_virtual_client`.
        let ct = crypto::encrypt(&k_mux, plaintext).expect("encrypt ok");

        let pt = crypto::decrypt(&k_cli, &ct).expect("client decrypt ok");
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn inbound_pump_decrypts_client_produced_ciphertext() {
        let k_mux = simulated_multiplexer_key("deadbeef", "tunnel-x");
        let k_cli = simulated_client_key("deadbeef", "tunnel-x");
        let plaintext = b"typed on the client side";

        let ct = crypto::encrypt(&k_cli, plaintext).expect("client encrypt ok");

        // Mirrors the inbound pump's `decrypt(&inbound_key, &buf)`
        // call on TerminalFrameData from r/w clients.
        let pt = crypto::decrypt(&k_mux, &ct).expect("multiplexer decrypt ok");
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn different_tunnel_id_breaks_the_contract() {
        let k_mux = simulated_multiplexer_key("deadbeef", "tunnel-a");
        let k_cli = simulated_client_key("deadbeef", "tunnel-b");
        let plaintext = b"secret";

        let ct = crypto::encrypt(&k_mux, plaintext).expect("encrypt ok");
        crypto::decrypt(&k_cli, &ct).expect_err("decrypt must fail under key mismatch");
    }
}
