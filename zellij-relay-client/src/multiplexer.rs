use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use zellij_browser_bridge::control_frame::ControlFrame;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

/// Interval between tunnel keepalive pings (Zellij side). Chosen to match
/// the relay server's matching ping cadence and the nginx
/// `proxy_read_timeout` in `deploy/nginx/nginx.conf.template`.
pub const RELAY_HEARTBEAT_INTERVAL_SECS: u64 = 30;
/// Absolute silence budget. Two missed 30s pings produces roughly this
/// much silence; a tunnel quiet for longer is considered dead and the
/// reader/writer stack is torn down so the supervisor can reconnect.
pub const RELAY_HEARTBEAT_TIMEOUT_SECS: u64 = 60;

/// Outcome returned by `run_multiplexer`, consumed by the supervisor in
/// `relay/mod.rs::run_relay_tunnel_supervisor` to decide whether to
/// reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiplexerExitReason {
    /// Explicit shutdown requested (user pressed `I`, process exit, …).
    Shutdown,
    /// Tunnel dropped unexpectedly — reader error, heartbeat timeout, etc.
    TunnelDropped,
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
pub fn relay_virtual_web_client_id(tunnel_id: &str, client_id: u32) -> String {
    format!("relay-{}-{}", tunnel_id, client_id)
}

use zellij_relay_protocol::{
    crypto::{self, ViewerKeys},
    decode_control_frame, decode_terminal_frame, ControlMessage, TerminalMessage,
};
use zellij_utils::ipc::ClientToServerMsg;

use super::control_tunnel::ControlTunnelSession;
use super::terminal_tunnel::TerminalTunnelSession;
use super::types::{
    CredentialAccess, CredentialKind, GuestCredential, LinkId, PendingAdmission, PendingDeviceAuth,
    PendingEnroll, PendingPake, RelayTunnelState, RelayVirtualClient, ADMISSION_TIMEOUT_SECS,
    DEVICE_AUTH_TIMEOUT_SECS, ENROLL_TIMEOUT_SECS,
};

use zellij_browser_bridge::protocol::{
    DisplayConfig, ToBrowser, WebClientToWebServerControlMessagePayload,
};
use zellij_browser_bridge::{AttachSpec, Uplink, ViewerId, ViewerSink};

pub async fn run_multiplexer(
    state: Arc<RelayTunnelState>,
    control: ControlTunnelSession,
    terminal: TerminalTunnelSession,
    control_tunnel_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    terminal_tunnel_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    shutdown_rx: oneshot::Receiver<()>,
) -> MultiplexerExitReason {
    let ControlTunnelSession {
        sink: control_sink,
        stream: control_stream,
        ..
    } = control;
    let TerminalTunnelSession {
        sink: terminal_sink,
        stream: terminal_stream,
    } = terminal;

    // Heartbeat bookkeeping: every received frame refreshes
    // `last_activity_at`; a watchdog task trips if that timestamp ages
    // past the configured silence budget.
    crate::admissions::register_tunnel(&state);
    crate::device_roster::register_state(&state);
    crate::device_roster::start_revocation_watcher();
    crate::guest_links::register_state(&state);

    let control_last_activity = Arc::new(AtomicU64::new(now_millis()));
    let terminal_last_activity = Arc::new(AtomicU64::new(now_millis()));

    let (control_ping_tx, control_ping_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (terminal_ping_tx, terminal_ping_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Writer tasks: drain the mpsc queues onto the socket sinks.
    let control_writer = spawn_writer(control_sink, control_tunnel_rx, control_ping_rx);
    let terminal_writer = spawn_writer(terminal_sink, terminal_tunnel_rx, terminal_ping_rx);

    // Reader tasks: dispatch incoming frames and refresh activity marks.
    let mut control_reader =
        spawn_control_reader(state.clone(), control_stream, control_last_activity.clone());
    let mut terminal_reader = spawn_terminal_reader(
        state.clone(),
        terminal_stream,
        terminal_last_activity.clone(),
    );

    // Heartbeat tasks: periodic Ping emission + silence watchdog.
    let (control_hb_handle, control_hb_tripped) = spawn_heartbeat(
        control_ping_tx,
        control_last_activity,
        "control",
    );
    let (terminal_hb_handle, terminal_hb_tripped) = spawn_heartbeat(
        terminal_ping_tx,
        terminal_last_activity,
        "terminal",
    );

    #[derive(Clone, Copy)]
    enum ExitTrigger {
        Shutdown,
        ControlReader,
        TerminalReader,
        ControlHeartbeat,
        TerminalHeartbeat,
    }

    let exit_trigger = tokio::select! {
        _ = shutdown_rx => {
            log::info!("Relay tunnel shutdown signal received");
            ExitTrigger::Shutdown
        }
        _ = &mut control_reader => {
            log::warn!("Relay control socket closed");
            ExitTrigger::ControlReader
        }
        _ = &mut terminal_reader => {
            log::warn!("Relay terminal socket closed");
            ExitTrigger::TerminalReader
        }
        _ = control_hb_tripped => {
            log::warn!(
                "Relay control tunnel silent >{}s — tripping watchdog",
                RELAY_HEARTBEAT_TIMEOUT_SECS
            );
            ExitTrigger::ControlHeartbeat
        }
        _ = terminal_hb_tripped => {
            log::warn!(
                "Relay terminal tunnel silent >{}s — tripping watchdog",
                RELAY_HEARTBEAT_TIMEOUT_SECS
            );
            ExitTrigger::TerminalHeartbeat
        }
    };
    let exit_reason = match exit_trigger {
        ExitTrigger::Shutdown => MultiplexerExitReason::Shutdown,
        ExitTrigger::ControlReader
        | ExitTrigger::TerminalReader
        | ExitTrigger::ControlHeartbeat
        | ExitTrigger::TerminalHeartbeat => MultiplexerExitReason::TunnelDropped,
    };

    // Tear down all virtual clients. On reconnect the relay will
    // re-challenge each viewer through a fresh handshake, so draining
    // virtual clients across the break is correct.
    let clients: Vec<RelayVirtualClient> = {
        let mut guard = state.clients.lock().unwrap();
        guard.drain().map(|(_id, c)| c).collect()
    };
    for mut c in clients {
        if let Some(tx) = c.shutdown.take() {
            let _ = tx.send(());
        }
        state
            .bridge
            .roster()
            .lock()
            .unwrap()
            .remove(&c.web_client_id);
    }

    // Dropping a `JoinHandle` detaches its task. In particular, when the
    // shutdown receiver wins the select above, detached reader tasks would
    // retain the WebSocket stream halves and keep the relay-side tunnel alive.
    // Abort and await every non-winning task so all socket halves are dropped
    // before the supervisor reports that shutdown has completed.
    control_writer.abort();
    terminal_writer.abort();
    control_hb_handle.abort();
    terminal_hb_handle.abort();
    let _ = tokio::join!(
        control_writer,
        terminal_writer,
        control_hb_handle,
        terminal_hb_handle
    );

    match exit_trigger {
        ExitTrigger::ControlReader => {
            terminal_reader.abort();
            let _ = terminal_reader.await;
        },
        ExitTrigger::TerminalReader => {
            control_reader.abort();
            let _ = control_reader.await;
        },
        ExitTrigger::Shutdown
        | ExitTrigger::ControlHeartbeat
        | ExitTrigger::TerminalHeartbeat => {
            control_reader.abort();
            terminal_reader.abort();
            let _ = tokio::join!(control_reader, terminal_reader);
        },
    }
    exit_reason
}

/// Spawn a keepalive + watchdog task: emit a ping every
/// `RELAY_HEARTBEAT_INTERVAL_SECS` and trip the returned oneshot if no
/// activity has been recorded on this tunnel for longer than
/// `RELAY_HEARTBEAT_TIMEOUT_SECS`.
fn spawn_heartbeat(
    ping_tx: mpsc::UnboundedSender<Vec<u8>>,
    last_activity: Arc<AtomicU64>,
    which: &'static str,
) -> (tokio::task::JoinHandle<()>, oneshot::Receiver<()>) {
    let (tripped_tx, tripped_rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        // Wake every half-interval so the watchdog reacts quickly while
        // pings still fire on the full cadence.
        let mut ticker = tokio::time::interval(Duration::from_secs(
            RELAY_HEARTBEAT_INTERVAL_SECS / 2,
        ));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick is immediate; skip it so we don't ping the moment we
        // connect.
        ticker.tick().await;
        let mut ticks_since_ping: u32 = 0;
        loop {
            ticker.tick().await;
            ticks_since_ping += 1;
            if u64::from(ticks_since_ping) * (RELAY_HEARTBEAT_INTERVAL_SECS / 2)
                >= RELAY_HEARTBEAT_INTERVAL_SECS
            {
                if ping_tx.send(b"hb".to_vec()).is_err() {
                    // Writer task gone — tunnel is already tearing down.
                    break;
                }
                ticks_since_ping = 0;
            }
            let last = last_activity.load(Ordering::Relaxed);
            let now = now_millis();
            if now.saturating_sub(last) > RELAY_HEARTBEAT_TIMEOUT_SECS * 1000 {
                log::warn!(
                    "relay {} tunnel: last activity {}ms ago — tripping watchdog",
                    which,
                    now.saturating_sub(last)
                );
                let _ = tripped_tx.send(());
                break;
            }
        }
    });
    (handle, tripped_rx)
}

fn spawn_writer<S>(
    mut sink: S,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    mut ping_rx: mpsc::UnboundedReceiver<Vec<u8>>,
) -> tokio::task::JoinHandle<()>
where
    S: SinkExt<TungsteniteMessage, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin
        + Send
        + 'static,
{
    tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = rx.recv() => match frame {
                    Some(bytes) => {
                        if sink
                            .send(TungsteniteMessage::Binary(bytes.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    None => break,
                },
                ping = ping_rx.recv() => match ping {
                    Some(payload) => {
                        if sink
                            .send(TungsteniteMessage::Ping(payload.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
        let _ = sink.send(TungsteniteMessage::Close(None)).await;
    })
}

fn spawn_control_reader<S>(
    state: Arc<RelayTunnelState>,
    mut stream: S,
    last_activity: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()>
where
    S: futures_util::Stream<Item = Result<TungsteniteMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send
        + 'static,
{
    tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            // Any successful frame — binary, text, ping, pong — counts as
            // activity; refresh the watchdog clock before dispatch.
            last_activity.store(now_millis(), Ordering::Relaxed);
            let bytes = match frame {
                Ok(TungsteniteMessage::Binary(b)) => b.to_vec(),
                Ok(TungsteniteMessage::Text(t)) => t.as_bytes().to_vec(),
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

            dispatch_control_message(&state, msg);
        }
    })
}

pub(crate) fn dispatch_control_message(state: &Arc<RelayTunnelState>, msg: ControlMessage) {
    match msg {
        ControlMessage::PakeChallenge {
            request_id,
            viewer_msg,
            link_id,
        } => handle_pake_challenge(state, request_id, viewer_msg, link_id),
        ControlMessage::PakeConfirm {
            request_id,
            viewer_confirm,
        } => handle_pake_confirm(state, request_id, viewer_confirm),
        ControlMessage::ClientDisconnected { client_id } => {
            handle_client_disconnected(state, client_id);
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
                    let _ = tx.send(data);
                },
                None => {
                    if state
                        .pending_device_auth
                        .lock()
                        .unwrap()
                        .contains_key(&client_id)
                    {
                        handle_device_auth_frame(state, client_id, data);
                    } else if state.pending_enroll.lock().unwrap().contains_key(&client_id) {
                        handle_enroll_frame(state, client_id, data);
                    } else {
                        evict_orphan_client_at_relay(state, client_id, "control");
                    }
                },
            }
        },
        ControlMessage::Error { message, .. } => {
            log::warn!("relay reported control error: {}", message);
        },
        other => {
            log::warn!("unexpected post-handshake control frame: {:?}", other);
        },
    }
}

fn spawn_terminal_reader<S>(
    state: Arc<RelayTunnelState>,
    mut stream: S,
    last_activity: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()>
where
    S: futures_util::Stream<Item = Result<TungsteniteMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send
        + 'static,
{
    tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            last_activity.store(now_millis(), Ordering::Relaxed);
            let bytes = match frame {
                Ok(TungsteniteMessage::Binary(b)) => b.to_vec(),
                Ok(TungsteniteMessage::Text(t)) => t.as_bytes().to_vec(),
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
                            evict_orphan_client_at_relay(&state, client_id, "terminal");
                        },
                    }
                },
                TerminalMessage::Error { message, .. } => {
                    log::warn!("relay reported terminal error: {}", message);
                },
                other => {
                    log::warn!("unexpected post-handshake terminal frame: {:?}", other);
                },
            }
        }
    })
}

fn reject_pake_response(request_id: Vec<u8>) -> Vec<u8> {
    ControlMessage::PakeResponse {
        request_id,
        client_id: 0,
        accepted: false,
        sharer_msg: Vec::new(),
        sharer_confirm: Vec::new(),
    }
    .encode()
}

fn handle_pake_challenge(
    state: &Arc<RelayTunnelState>,
    request_id: Vec<u8>,
    viewer_msg: Vec<u8>,
    link_id: Vec<u8>,
) {
    let Some(credential) = state.select_credential(&link_id) else {
        log::warn!("PAKE challenge rejected: no credential matches the supplied link_id");
        let _ = state.control_tunnel_tx.send(reject_pake_response(request_id));
        return;
    };

    if credential.is_spent() {
        log::warn!(
            "PAKE challenge rejected: credential '{}' already used",
            credential.label
        );
        let _ = state.control_tunnel_tx.send(reject_pake_response(request_id));
        return;
    }

    let slug = state.slug.lock().unwrap().clone();
    let (pake_state, sharer_msg) = crypto::pake_start(&credential.secret, &slug);
    let pake_key = match crypto::pake_finish(pake_state, &viewer_msg) {
        Ok(k) => k,
        Err(e) => {
            log::warn!("pake_finish failed (malformed viewer message): {} — rejecting", e);
            let _ = state.control_tunnel_tx.send(reject_pake_response(request_id));
            return;
        },
    };

    let client_id = state.next_client_id.fetch_add(1, Ordering::Relaxed);
    let sharer_confirm = crypto::confirmation_tag(
        &pake_key,
        crypto::CONFIRM_LABEL_SHARER,
        &viewer_msg,
        &sharer_msg,
    );
    state.pending_pake.lock().unwrap().insert(
        request_id.clone(),
        PendingPake {
            client_id,
            pake_key,
            viewer_msg,
            sharer_msg: sharer_msg.clone(),
            link_id: credential.link_id,
        },
    );

    let resp = ControlMessage::PakeResponse {
        request_id,
        client_id,
        accepted: true,
        sharer_msg,
        sharer_confirm: sharer_confirm.to_vec(),
    };
    let _ = state.control_tunnel_tx.send(resp.encode());
}

fn handle_pake_confirm(
    state: &Arc<RelayTunnelState>,
    request_id: Vec<u8>,
    viewer_confirm: Vec<u8>,
) {
    let pending = state.pending_pake.lock().unwrap().remove(&request_id);
    let Some(pending) = pending else {
        log::warn!("PakeConfirm for unknown request_id");
        let _ = state.control_tunnel_tx.send(
            ControlMessage::PakeResult {
                request_id,
                client_id: 0,
                accepted: false,
            }
            .encode(),
        );
        return;
    };

    let credential = state.select_credential(&pending.link_id);

    let ok = crypto::verify_confirmation(
        &viewer_confirm,
        &pending.pake_key,
        crypto::CONFIRM_LABEL_VIEWER,
        &pending.viewer_msg,
        &pending.sharer_msg,
    );
    if !ok {
        log::warn!("PAKE confirmation failed — rejecting viewer");
        let _ = state.control_tunnel_tx.send(
            ControlMessage::PakeResult {
                request_id,
                client_id: pending.client_id,
                accepted: false,
            }
            .encode(),
        );
        return;
    }

    let keys = crypto::derive_viewer_keys(&pending.pake_key, &state.tunnel_id, pending.client_id);
    let sas = crypto::derive_sas(&pending.pake_key, &pending.viewer_msg, &pending.sharer_msg);
    let control_s2v = keys.control_s2v;
    let control_v2s = keys.control_v2s;
    state
        .pending_e2e_keys
        .lock()
        .unwrap()
        .insert(pending.client_id, keys);

    let is_device = credential
        .as_ref()
        .map(|c| matches!(c.kind, CredentialKind::Device))
        .unwrap_or(false);
    if is_device {
        let client_id = pending.client_id;
        let access = credential
            .as_ref()
            .map(|c| c.access)
            .unwrap_or(CredentialAccess::ReadWrite);
        let challenge = crypto::random_bytes(crypto::DEVICE_CHALLENGE_LEN);
        state.pending_device_auth.lock().unwrap().insert(
            client_id,
            PendingDeviceAuth {
                client_id,
                link_id: pending.link_id,
                access,
                challenge,
                control_s2v,
                control_v2s,
                out_seq: 0,
                last_in_seq: None,
                challenge_sent: false,
                created_at: std::time::Instant::now(),
            },
        );
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let state = state.clone();
            handle.spawn(async move {
                tokio::time::sleep(Duration::from_secs(DEVICE_AUTH_TIMEOUT_SECS)).await;
                if reject_device_auth(&state, client_id, "device authentication timed out") {
                    log::info!(
                        "device auth for client_id={} timed out after {}s — rejected",
                        client_id, DEVICE_AUTH_TIMEOUT_SECS
                    );
                }
            });
        }
        let _ = state.control_tunnel_tx.send(
            ControlMessage::PakeResult {
                request_id,
                client_id,
                accepted: true,
            }
            .encode(),
        );
        return;
    }

    let (access, label) = credential
        .as_ref()
        .map(|c| (c.access, c.label.clone()))
        .unwrap_or((CredentialAccess::ReadWrite, "guest".to_string()));

    let client_id = pending.client_id;
    state.pending_admissions.lock().unwrap().insert(
        client_id,
        PendingAdmission {
            client_id,
            link_id: pending.link_id,
            sas,
            access,
            label,
            claimed_name: None,
            created_at: std::time::Instant::now(),
        },
    );
    let _ = state
        .relay_event_notify
        .send(crate::types::RelayPluginEvent::AdmissionPending);

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let state = state.clone();
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_secs(ADMISSION_TIMEOUT_SECS)).await;
            if reject_pending(&state, client_id, "admission timed out") {
                log::info!(
                    "admission for client_id={} timed out after {}s — rejected",
                    client_id, ADMISSION_TIMEOUT_SECS
                );
            }
        });
    }

    let _ = state.control_tunnel_tx.send(
        ControlMessage::PakeResult {
            request_id,
            client_id,
            accepted: true,
        }
        .encode(),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmitResult {
    Admitted,
    NeedsCodeConfirm,
    Exhausted,
    SpawnFailed,
    NotFound,
}

pub(crate) fn admit_pending(
    state: &Arc<RelayTunnelState>,
    client_id: u32,
    code_confirmed: bool,
) -> AdmitResult {
    {
        let pending = state.pending_admissions.lock().unwrap();
        match pending.get(&client_id) {
            None => return AdmitResult::NotFound,
            Some(p) => {
                if matches!(p.access, CredentialAccess::ReadWrite) && !code_confirmed {
                    return AdmitResult::NeedsCodeConfirm;
                }
            },
        }
    }

    let pending = state.pending_admissions.lock().unwrap().remove(&client_id);
    let Some(pending) = pending else {
        return AdmitResult::NotFound;
    };

    let credential = state.select_credential(&pending.link_id);
    if let Some(c) = &credential {
        if !c.spend() {
            send_rejected(state, client_id, "link already used");
            cleanup_pending_keys(state, client_id);
            return AdmitResult::Exhausted;
        }
    }

    {
        let contenders: Vec<u32> = {
            let map = state.pending_admissions.lock().unwrap();
            map.values()
                .filter(|o| o.link_id == pending.link_id)
                .map(|o| o.client_id)
                .collect()
        };
        for other in contenders {
            log::warn!(
                "guest link '{}' admitted while another join was pending — link leaked; rotate \
                 related shares",
                pending.label
            );
            reject_pending(state, other, "link already used");
        }
    }

    let is_read_only = pending.access.is_read_only();
    let enroll = credential.as_ref().map(|c| c.enroll).unwrap_or(false);
    if enroll {
        return begin_pending_enroll(state, client_id, &pending, credential.as_ref());
    }
    let initial_control =
        vec![serde_json::to_vec(&ToBrowser::Admitted { enroll: false }).unwrap_or_default()];
    if let Err(e) = spawn_virtual_client(
        state,
        client_id,
        is_read_only,
        pending.link_id,
        initial_control,
        0,
        None,
    ) {
        log::error!("admit: failed to spawn virtual client {}: {}", client_id, e);
        cleanup_pending_keys(state, client_id);
        remove_spent_link_credential(state, &pending.link_id);
        return AdmitResult::SpawnFailed;
    }
    AdmitResult::Admitted
}

fn begin_pending_enroll(
    state: &Arc<RelayTunnelState>,
    client_id: u32,
    pending: &PendingAdmission,
    credential: Option<&Arc<GuestCredential>>,
) -> AdmitResult {
    let (control_s2v, control_v2s) = {
        let map = state.pending_e2e_keys.lock().unwrap();
        match map.get(&client_id) {
            Some(k) => (k.control_s2v, k.control_v2s),
            None => {
                log::error!("enroll admit: no pending e2e keys for client_id={}", client_id);
                return AdmitResult::SpawnFailed;
            },
        }
    };
    let label = credential
        .map(|c| c.label.clone())
        .unwrap_or_else(|| "device".to_string());
    state.pending_enroll.lock().unwrap().insert(
        client_id,
        PendingEnroll {
            client_id,
            link_id: pending.link_id,
            access: pending.access,
            label,
            control_s2v,
            control_v2s,
            out_seq: 1,
            last_in_seq: None,
            created_at: std::time::Instant::now(),
            enrolled_device_id: None,
        },
    );
    let admitted = serde_json::to_vec(&ToBrowser::Admitted { enroll: true }).unwrap_or_default();
    send_control_frame_data(state, client_id, &control_s2v, 0, &admitted);
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let state = state.clone();
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_secs(ENROLL_TIMEOUT_SECS)).await;
            if reject_enroll(&state, client_id, "enrollment timed out") {
                log::info!(
                    "enrollment for client_id={} timed out after {}s — dropped",
                    client_id, ENROLL_TIMEOUT_SECS
                );
            }
        });
    }
    AdmitResult::Admitted
}

fn handle_enroll_frame(state: &Arc<RelayTunnelState>, client_id: u32, data: Vec<u8>) {
    let (control_v2s, last_in_seq) = {
        let map = state.pending_enroll.lock().unwrap();
        match map.get(&client_id) {
            Some(p) => (p.control_v2s, p.last_in_seq),
            None => return,
        }
    };
    let (seq, plaintext) = match crypto::decrypt_seq(
        &control_v2s,
        crypto::FRAME_TYPE_CONTROL,
        crypto::DIRECTION_VIEWER_TO_SHARER,
        &data,
    ) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("enroll control decrypt failed for client_id={}: {}", client_id, e);
            return;
        },
    };
    if matches!(last_in_seq, Some(last) if seq <= last) {
        log::warn!("replayed enroll frame seq={} for client_id={} — dropping", seq, client_id);
        return;
    }
    {
        let mut map = state.pending_enroll.lock().unwrap();
        if let Some(p) = map.get_mut(&client_id) {
            p.last_in_seq = Some(seq);
        }
    }
    let text = match String::from_utf8(plaintext) {
        Ok(s) => s,
        Err(_) => return,
    };
    match serde_json::from_str::<WebClientToWebServerControlMessagePayload>(&text) {
        Ok(WebClientToWebServerControlMessagePayload::DeviceEnrollRequest {
            pubkey_alg,
            pubkey,
            requested_label: _,
        }) => {
            enroll_device_pending(state, client_id, &pubkey_alg, &pubkey);
        },
        Ok(WebClientToWebServerControlMessagePayload::EnrollComplete) => {
            finish_pending_enroll(state, client_id);
        },
        _ => {
            log::warn!("unexpected control frame during enrollment for client_id={}", client_id);
        },
    }
}

fn enroll_device_pending(
    state: &Arc<RelayTunnelState>,
    client_id: u32,
    pubkey_alg: &str,
    pubkey: &[u8],
) {
    if pubkey_alg != "ed25519" || pubkey.len() != crypto::DEVICE_PUBKEY_LEN {
        log::warn!("device enroll rejected: unsupported pubkey alg '{}'", pubkey_alg);
        return;
    }
    let (link_id, access, host_label, control_s2v, seq) = {
        let mut map = state.pending_enroll.lock().unwrap();
        let Some(p) = map.get_mut(&client_id) else {
            return;
        };
        let seq = p.out_seq;
        p.out_seq += 1;
        (p.link_id, p.access, p.label.clone(), p.control_s2v, seq)
    };
    let label = if host_label.trim().is_empty() {
        "device".to_string()
    } else {
        host_label
    };
    match crate::device_roster::enroll(
        &state.credentials,
        access,
        label,
        pubkey,
        pubkey_alg,
        Some(link_id),
    ) {
        Ok(enrolled) => {
            // The single-use enrollment link is now consumed: record the new
            // device id on the pending entry (so the live client is spawned
            // under the device identity) and drop the enrollment link
            // credential so it no longer lingers as a pending guest/enrollment
            // link.
            if let Ok(device_id) = LinkId::try_from(enrolled.device_id.as_slice()) {
                if let Some(p) = state.pending_enroll.lock().unwrap().get_mut(&client_id) {
                    p.enrolled_device_id = Some(device_id);
                }
            }
            state.credentials.lock().unwrap().remove(&link_id);

            let ack = ToBrowser::DeviceEnrollAck {
                device_id: enrolled.device_id,
                device_secret: enrolled.device_secret,
                scope: enrolled.scope,
                host_id: enrolled.host_id,
                access_read_only: enrolled.read_only,
            };
            if let Ok(json) = serde_json::to_vec(&ack) {
                send_control_frame_data(state, client_id, &control_s2v, seq, &json);
            }
            let _ = state
                .relay_event_notify
                .send(crate::types::RelayPluginEvent::DevicesChanged);
        },
        Err(e) => log::warn!("device enroll failed: {}", e),
    }
}

fn finish_pending_enroll(state: &Arc<RelayTunnelState>, client_id: u32) {
    let pending = state.pending_enroll.lock().unwrap().remove(&client_id);
    let Some(pending) = pending else {
        return;
    };
    // Attribute the live client to the enrolled device id (the enrollment link
    // it arrived on has been consumed); fall back to the link id only if the
    // enrollment did not complete.
    let client_link_id = pending.enrolled_device_id.unwrap_or(pending.link_id);
    if let Err(e) = spawn_virtual_client(
        state,
        client_id,
        pending.access.is_read_only(),
        client_link_id,
        Vec::new(),
        pending.out_seq,
        pending.last_in_seq,
    ) {
        log::error!("enroll finish: failed to spawn virtual client {}: {}", client_id, e);
        cleanup_pending_keys(state, client_id);
    }
}

fn reject_enroll(state: &Arc<RelayTunnelState>, client_id: u32, reason: &str) -> bool {
    let pending = state.pending_enroll.lock().unwrap().remove(&client_id);
    let Some(pending) = pending else {
        return false;
    };
    send_control_frame_data(
        state,
        client_id,
        &pending.control_s2v,
        pending.out_seq,
        &rejected_json(reason),
    );
    cleanup_pending_keys(state, client_id);
    true
}

pub(crate) fn reject_pending(state: &Arc<RelayTunnelState>, client_id: u32, reason: &str) -> bool {
    let removed = state.pending_admissions.lock().unwrap().remove(&client_id);
    if removed.is_none() {
        return false;
    }
    send_rejected(state, client_id, reason);
    cleanup_pending_keys(state, client_id);
    true
}

fn send_rejected(state: &Arc<RelayTunnelState>, client_id: u32, reason: &str) {
    let key = match state.pending_e2e_keys.lock().unwrap().get(&client_id) {
        Some(k) => k.control_s2v,
        None => return,
    };
    let json = match serde_json::to_vec(&ToBrowser::Rejected {
        reason: reason.to_string(),
    }) {
        Ok(j) => j,
        Err(_) => return,
    };
    if let Ok(ct) = crypto::encrypt_seq(
        &key,
        0,
        crypto::FRAME_TYPE_CONTROL,
        crypto::DIRECTION_SHARER_TO_VIEWER,
        &json,
    ) {
        let _ = state.control_tunnel_tx.send(
            ControlMessage::ControlFrameData {
                client_id,
                data: ct,
            }
            .encode(),
        );
    }
}

fn cleanup_pending_keys(state: &Arc<RelayTunnelState>, client_id: u32) {
    state.pending_e2e_keys.lock().unwrap().remove(&client_id);
}

fn send_control_frame_data(
    state: &Arc<RelayTunnelState>,
    client_id: u32,
    key: &[u8; 32],
    seq: u64,
    json: &[u8],
) {
    if let Ok(ct) = crypto::encrypt_seq(
        key,
        seq,
        crypto::FRAME_TYPE_CONTROL,
        crypto::DIRECTION_SHARER_TO_VIEWER,
        json,
    ) {
        let _ = state.control_tunnel_tx.send(
            ControlMessage::ControlFrameData {
                client_id,
                data: ct,
            }
            .encode(),
        );
    }
}

fn handle_device_auth_frame(state: &Arc<RelayTunnelState>, client_id: u32, data: Vec<u8>) {
    let (control_v2s, last_in_seq) = {
        let map = state.pending_device_auth.lock().unwrap();
        match map.get(&client_id) {
            Some(p) => (p.control_v2s, p.last_in_seq),
            None => return,
        }
    };
    let (seq, plaintext) = match crypto::decrypt_seq(
        &control_v2s,
        crypto::FRAME_TYPE_CONTROL,
        crypto::DIRECTION_VIEWER_TO_SHARER,
        &data,
    ) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("device-auth control decrypt failed for client_id={}: {}", client_id, e);
            return;
        },
    };
    if matches!(last_in_seq, Some(last) if seq <= last) {
        log::warn!("replayed device-auth frame seq={} for client_id={} — dropping", seq, client_id);
        return;
    }
    {
        let mut map = state.pending_device_auth.lock().unwrap();
        if let Some(p) = map.get_mut(&client_id) {
            p.last_in_seq = Some(seq);
        }
    }
    let text = match String::from_utf8(plaintext) {
        Ok(s) => s,
        Err(_) => return,
    };
    match serde_json::from_str::<WebClientToWebServerControlMessagePayload>(&text) {
        Ok(WebClientToWebServerControlMessagePayload::DeviceAuthRequest) => {
            send_device_auth_challenge(state, client_id);
        },
        Ok(WebClientToWebServerControlMessagePayload::DeviceAuthResponse { signature }) => {
            verify_device_auth(state, client_id, signature);
        },
        _ => {
            log::warn!("unexpected control frame during device auth for client_id={}", client_id);
        },
    }
}

fn send_device_auth_challenge(state: &Arc<RelayTunnelState>, client_id: u32) {
    let (control_s2v, seq, challenge) = {
        let mut map = state.pending_device_auth.lock().unwrap();
        let Some(p) = map.get_mut(&client_id) else {
            return;
        };
        if p.challenge_sent {
            return;
        }
        p.challenge_sent = true;
        let seq = p.out_seq;
        p.out_seq += 1;
        (p.control_s2v, seq, p.challenge.clone())
    };
    let json = serde_json::to_vec(&ToBrowser::DeviceAuthChallenge { nonce: challenge })
        .unwrap_or_default();
    send_control_frame_data(state, client_id, &control_s2v, seq, &json);
}

fn verify_device_auth(state: &Arc<RelayTunnelState>, client_id: u32, signature: Vec<u8>) {
    let pending = state.pending_device_auth.lock().unwrap().remove(&client_id);
    let Some(pending) = pending else {
        return;
    };
    let pubkey = match crate::device_roster::pinned_pubkey(&pending.link_id) {
        Some(pk) => pk,
        None => {
            log::warn!("device auth: no pinned key for client_id={} — rejecting", client_id);
            send_control_frame_data(
                state,
                client_id,
                &pending.control_s2v,
                pending.out_seq,
                &rejected_json("device not enrolled"),
            );
            cleanup_pending_keys(state, client_id);
            return;
        },
    };
    if !crypto::verify_device_challenge(&pubkey, &pending.challenge, &signature) {
        log::warn!("device signature verification failed for client_id={} — rejecting", client_id);
        send_control_frame_data(
            state,
            client_id,
            &pending.control_s2v,
            pending.out_seq,
            &rejected_json("device signature rejected"),
        );
        cleanup_pending_keys(state, client_id);
        return;
    }
    crate::device_roster::mark_used(&pending.link_id);
    let is_read_only = pending.access.is_read_only();
    let admitted = serde_json::to_vec(&ToBrowser::Admitted { enroll: false }).unwrap_or_default();
    if let Err(e) = spawn_virtual_client(
        state,
        client_id,
        is_read_only,
        pending.link_id,
        vec![admitted],
        pending.out_seq,
        pending.last_in_seq,
    ) {
        log::error!("device admit: failed to spawn virtual client {}: {}", client_id, e);
        cleanup_pending_keys(state, client_id);
    }
}

fn reject_device_auth(state: &Arc<RelayTunnelState>, client_id: u32, reason: &str) -> bool {
    let pending = state.pending_device_auth.lock().unwrap().remove(&client_id);
    let Some(pending) = pending else {
        return false;
    };
    send_control_frame_data(
        state,
        client_id,
        &pending.control_s2v,
        pending.out_seq,
        &rejected_json(reason),
    );
    cleanup_pending_keys(state, client_id);
    true
}

pub(crate) fn reject_pending_with_link(state: &Arc<RelayTunnelState>, link_id: &LinkId) -> usize {
    let admission_ids: Vec<u32> = {
        let map = state.pending_admissions.lock().unwrap();
        map.values()
            .filter(|p| &p.link_id == link_id)
            .map(|p| p.client_id)
            .collect()
    };
    let device_auth_ids: Vec<u32> = {
        let map = state.pending_device_auth.lock().unwrap();
        map.values()
            .filter(|p| &p.link_id == link_id)
            .map(|p| p.client_id)
            .collect()
    };
    let enroll_ids: Vec<u32> = {
        let map = state.pending_enroll.lock().unwrap();
        map.values()
            .filter(|p| &p.link_id == link_id)
            .map(|p| p.client_id)
            .collect()
    };
    let mut rejected = 0;
    for client_id in admission_ids {
        if reject_pending(state, client_id, "link revoked") {
            rejected += 1;
        }
    }
    for client_id in device_auth_ids {
        if reject_device_auth(state, client_id, "link revoked") {
            rejected += 1;
        }
    }
    for client_id in enroll_ids {
        if reject_enroll(state, client_id, "link revoked") {
            rejected += 1;
        }
    }
    rejected
}

fn rejected_json(reason: &str) -> Vec<u8> {
    serde_json::to_vec(&ToBrowser::Rejected {
        reason: reason.to_string(),
    })
    .unwrap_or_default()
}

fn client_id_is_tracked(state: &Arc<RelayTunnelState>, client_id: u32) -> bool {
    state.clients.lock().unwrap().contains_key(&client_id)
        || state.pending_e2e_keys.lock().unwrap().contains_key(&client_id)
        || state.pending_admissions.lock().unwrap().contains_key(&client_id)
        || state.pending_device_auth.lock().unwrap().contains_key(&client_id)
        || state.pending_enroll.lock().unwrap().contains_key(&client_id)
}

fn evict_orphan_client_at_relay(state: &Arc<RelayTunnelState>, client_id: u32, source: &str) {
    if client_id_is_tracked(state, client_id) {
        log::info!(
            "evict_orphan ({}): client_id={} is still tracked (mid-handshake/admission) — not evicting, dropping frame",
            source, client_id
        );
        return;
    }
    let sent = state
        .control_tunnel_tx
        .send(ControlMessage::ClientDisconnected { client_id }.encode())
        .is_ok();
    log::warn!(
        "evict_orphan ({}): orphan client_id={} untracked by this tunnel state — sent ClientDisconnected to relay (queued={})",
        source, client_id, sent
    );
}

pub(crate) fn client_link_ids(state: &Arc<RelayTunnelState>) -> Vec<LinkId> {
    state
        .clients
        .lock()
        .unwrap()
        .values()
        .map(|c| c.link_id)
        .collect()
}

fn handle_client_disconnected(state: &Arc<RelayTunnelState>, client_id: u32) {
    let removed = state.clients.lock().unwrap().remove(&client_id);
    if let Some(mut c) = removed {
        if let Some(tx) = c.shutdown.take() {
            let _ = tx.send(());
        }
        state
            .bridge
            .roster()
            .lock()
            .unwrap()
            .remove(&c.web_client_id);
        gc_spent_single_use_link(state, &c.link_id);
        let _ = state
            .relay_event_notify
            .send(crate::types::RelayPluginEvent::GuestConnectionsChanged);
    }
}

fn remove_spent_link_credential(state: &Arc<RelayTunnelState>, link_id: &LinkId) {
    let mut credentials = state.credentials.lock().unwrap();
    let spent_link = credentials
        .get(link_id)
        .map(|c| c.kind == CredentialKind::Link && c.is_spent())
        .unwrap_or(false);
    if spent_link {
        credentials.remove(link_id);
    }
}

fn gc_spent_single_use_link(state: &Arc<RelayTunnelState>, link_id: &LinkId) {
    let still_connected = state
        .clients
        .lock()
        .unwrap()
        .values()
        .any(|c| &c.link_id == link_id);
    if still_connected {
        return;
    }
    let mut credentials = state.credentials.lock().unwrap();
    let spent_link = credentials
        .get(link_id)
        .map(|c| c.kind == CredentialKind::Link && c.is_spent())
        .unwrap_or(false);
    if spent_link {
        credentials.remove(link_id);
    }
}

pub(crate) fn disconnect_clients_with_link(state: &Arc<RelayTunnelState>, link_id: &LinkId) -> usize {
    let to_kick: Vec<(u32, mpsc::UnboundedSender<Vec<u8>>)> = {
        let clients = state.clients.lock().unwrap();
        clients
            .iter()
            .filter(|(_, c)| &c.link_id == link_id)
            .map(|(id, c)| (*id, c.control_out_tx.clone()))
            .collect()
    };
    let exit_frame = serde_json::to_vec(&ToBrowser::Rejected {
        reason: "access revoked".to_string(),
    })
    .unwrap_or_default();
    for (client_id, control_out_tx) in &to_kick {
        if !exit_frame.is_empty() {
            let _ = control_out_tx.send(exit_frame.clone());
        }
        handle_client_disconnected(state, *client_id);
    }
    to_kick.len()
}

fn spawn_virtual_client(
    state: &Arc<RelayTunnelState>,
    client_id: u32,
    is_read_only: bool,
    link_id: LinkId,
    initial_control: Vec<Vec<u8>>,
    control_out_start_seq: u64,
    control_in_seen_seq: Option<u64>,
) -> Result<(), String> {
    let viewer_id = ViewerId(relay_virtual_web_client_id(&state.tunnel_id, client_id));
    let link = state
        .os_api_factory
        .create()
        .map_err(|e| format!("create: {}", e))?;

    let keys: ViewerKeys = state
        .pending_e2e_keys
        .lock()
        .unwrap()
        .remove(&client_id)
        .ok_or_else(|| {
            format!(
                "no pending e2e keys for client_id={} — spawn without a confirmed PAKE handshake",
                client_id
            )
        })?;

    let tunnel_session_hash = format!("relay-tunnel-{}", client_id);

    let (stdout_tx, mut stdout_rx) = mpsc::unbounded_channel::<String>();
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ControlFrame>();

    let _attachment_complete_rx = state.bridge.attach(AttachSpec {
        id: viewer_id.clone(),
        link: link.clone(),
        sink: ViewerSink {
            control_out: ctrl_tx,
            terminal_out: stdout_tx,
        },
        read_only: is_read_only,
        relay_fanout: true,
        token_hash: tunnel_session_hash,
        config: state.config.lock().unwrap().clone(),
        config_options: state.config_options.clone(),
        config_file_path: Some(state.config_file_path.clone()),
        session_name: Some(state.session_name.clone()),
    });

    let terminal_tunnel_tx = state.terminal_tunnel_tx.clone();
    let outbound_terminal_key = keys.terminal_s2v;
    tokio::spawn(async move {
        let mut seq = 0u64;
        while let Some(bytes) = stdout_rx.recv().await {
            let plaintext = bytes.into_bytes();
            let ciphertext = match crypto::encrypt_seq(
                &outbound_terminal_key,
                seq,
                crypto::FRAME_TYPE_TERMINAL,
                crypto::DIRECTION_SHARER_TO_VIEWER,
                &plaintext,
            ) {
                Ok(ct) => ct,
                Err(e) => {
                    log::error!(
                        "e2e terminal encrypt failed for client_id={}: {} — dropping frame",
                        client_id, e
                    );
                    continue;
                },
            };
            seq += 1;
            let frame = TerminalMessage::TerminalFrameData {
                client_id,
                data: ciphertext,
            };
            if terminal_tunnel_tx.send(frame.encode()).is_err() {
                break;
            }
        }
    });

    let (control_out_tx, mut control_out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let control_tunnel_tx = state.control_tunnel_tx.clone();
    let outbound_control_key = keys.control_s2v;
    tokio::spawn(async move {
        let mut seq = control_out_start_seq;
        while let Some(plaintext) = control_out_rx.recv().await {
            let data = match crypto::encrypt_seq(
                &outbound_control_key,
                seq,
                crypto::FRAME_TYPE_CONTROL,
                crypto::DIRECTION_SHARER_TO_VIEWER,
                &plaintext,
            ) {
                Ok(ct) => ct,
                Err(e) => {
                    log::error!(
                        "e2e control encrypt failed for client_id={}: {} — dropping frame",
                        client_id, e
                    );
                    continue;
                },
            };
            seq += 1;
            let frame = ControlMessage::ControlFrameData { client_id, data };
            if control_tunnel_tx.send(frame.encode()).is_err() {
                break;
            }
        }
    });

    for init in initial_control {
        let _ = control_out_tx.send(init);
    }

    let control_out_tx_for_bridge = control_out_tx.clone();
    let state_for_bridge_close = Arc::clone(state);
    tokio::spawn(async move {
        while let Some(ctrl_frame) = ctrl_rx.recv().await {
            let data = match ctrl_frame {
                ControlFrame::Text(t) => t.into_bytes(),
                ControlFrame::Binary(b) => b,
                ControlFrame::Close { reason, .. } => {
                    let exit_frame = serde_json::to_vec(&ToBrowser::Exit { reason })
                        .unwrap_or_default();
                    if !exit_frame.is_empty() {
                        let _ = control_out_tx_for_bridge.send(exit_frame);
                    }
                    handle_client_disconnected(&state_for_bridge_close, client_id);
                    break;
                },
                _ => continue,
            };
            if control_out_tx_for_bridge.send(data).is_err() {
                break;
            }
        }
    });

    let (terminal_input_tx, mut terminal_input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let explicitly_disable_kitty_keyboard_protocol = state
        .config
        .lock()
        .unwrap()
        .options
        .support_kitty_keyboard_protocol
        .map(|e| !e)
        .unwrap_or(false);
    let uplink_link = link.clone();
    let inbound_terminal_key = keys.terminal_v2s;
    tokio::spawn(async move {
        let mut uplink = Uplink::new(
            uplink_link,
            is_read_only,
            explicitly_disable_kitty_keyboard_protocol,
        );
        let mut window = crypto::ReplayWindow::new();
        while let Some(buf) = terminal_input_rx.recv().await {
            let (seq, plaintext) = match crypto::decrypt_seq(
                &inbound_terminal_key,
                crypto::FRAME_TYPE_TERMINAL,
                crypto::DIRECTION_VIEWER_TO_SHARER,
                &buf,
            ) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!(
                        "e2e terminal decrypt failed for client_id={}: {} — dropping frame",
                        client_id, e
                    );
                    continue;
                },
            };
            if !window.accept(seq) {
                log::warn!(
                    "replayed/reordered terminal frame seq={} for client_id={} — dropping",
                    seq, client_id
                );
                continue;
            }
            uplink.feed_terminal(&plaintext);
        }
    });

    let (control_input_tx, mut control_input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let inbound_control_key = keys.control_v2s;
    let roster_for_ctrl = state.bridge.roster();
    let viewer_id_for_ctrl = viewer_id.clone();
    let config_for_ctrl = state.config.clone();
    let zellij_version_for_ctrl = state.zellij_version.clone();
    let control_out_tx_for_resend = control_out_tx.clone();
    tokio::spawn(async move {
        let mut version_answered = false;
        let mut window = crypto::ReplayWindow::new();
        if let Some(seen) = control_in_seen_seq {
            window.accept(seen);
        }
        while let Some(buf) = control_input_rx.recv().await {
            let (seq, plaintext) = match crypto::decrypt_seq(
                &inbound_control_key,
                crypto::FRAME_TYPE_CONTROL,
                crypto::DIRECTION_VIEWER_TO_SHARER,
                &buf,
            ) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!(
                        "e2e control decrypt failed for client_id={}: {} — dropping frame",
                        client_id, e
                    );
                    continue;
                },
            };
            if !window.accept(seq) {
                log::warn!(
                    "replayed/reordered control frame seq={} for client_id={} — dropping",
                    seq, client_id
                );
                continue;
            }
            let text = match String::from_utf8(plaintext) {
                Ok(s) => s,
                Err(_) => {
                    log::warn!(
                        "decrypted control frame for client_id={} is not UTF-8 — dropping",
                        client_id
                    );
                    continue;
                },
            };
            if !version_answered {
                if let Ok(WebClientToWebServerControlMessagePayload::VersionRequest) =
                    serde_json::from_str::<WebClientToWebServerControlMessagePayload>(&text)
                {
                    version_answered = true;
                    log::info!(
                        "[zj] VersionRequest from client_id={}, answering VersionAnnounce+SetConfig",
                        client_id
                    );
                    let announce = ToBrowser::VersionAnnounce {
                        zellij_version: zellij_version_for_ctrl.clone(),
                        app_bundle_sha384: zellij_web_client_assets::app_bundle_sha384()
                            .to_string(),
                    };
                    if let Ok(json) = serde_json::to_string(&announce) {
                        let _ = control_out_tx_for_resend.send(json.into_bytes());
                    }
                    let display_config = DisplayConfig::from(&*config_for_ctrl.lock().unwrap());
                    if let Ok(json) = serde_json::to_string(&ToBrowser::SetConfig(display_config)) {
                        let _ = control_out_tx_for_resend.send(json.into_bytes());
                    }
                    continue;
                }
            }
            if let Ok(WebClientToWebServerControlMessagePayload::Ping) =
                serde_json::from_str::<WebClientToWebServerControlMessagePayload>(&text)
            {
                if let Ok(json) = serde_json::to_string(&ToBrowser::Pong) {
                    let _ = control_out_tx_for_resend.send(json.into_bytes());
                }
                continue;
            }
            roster_for_ctrl
                .lock()
                .unwrap()
                .feed_control(&viewer_id_for_ctrl, &text);
        }
    });

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let roster_for_shutdown = state.bridge.roster();
    let viewer_id_for_shutdown = viewer_id.clone();
    let link_for_shutdown = link;
    tokio::spawn(async move {
        let _ = shutdown_rx.await;
        roster_for_shutdown
            .lock()
            .unwrap()
            .remove(&viewer_id_for_shutdown);
        link_for_shutdown.send_to_server(ClientToServerMsg::ClientExited);
    });

    state.clients.lock().unwrap().insert(
        client_id,
        RelayVirtualClient {
            web_client_id: viewer_id,
            is_read_only,
            link_id,
            terminal_input_tx,
            control_input_tx,
            control_out_tx,
            shutdown: Some(shutdown_tx),
        },
    );
    let _ = state
        .relay_event_notify
        .send(crate::types::RelayPluginEvent::GuestConnectionsChanged);
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU32;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    use crate::types::{GuestCredential, LinkId, RelayTunnelState, RelayVirtualClient};
    use zellij_browser_bridge::factory::{SessionLinkFactory, SessionSource};
    use zellij_browser_bridge::virtual_client::SessionLink;
    use zellij_browser_bridge::{BrowserBridge, LoopbackLinkFactory};
    use zellij_relay_protocol::crypto::ViewerKeys;
    use zellij_utils::input::{config::Config, options::Options};
    use zellij_utils::pane_size::Size;

    use super::ViewerId;

    #[derive(Debug)]
    pub(crate) struct UnusedOsApiFactory;
    impl SessionLinkFactory for UnusedOsApiFactory {
        fn create(&self) -> Result<Box<dyn SessionLink>, Box<dyn std::error::Error>> {
            Err("unused in these tests".into())
        }
    }

    #[derive(Debug)]
    pub(crate) struct UnusedSessionManager;
    impl SessionSource for UnusedSessionManager {
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
            _os_input: Box<dyn SessionLink>,
            _exists: bool,
            _pipe: &PathBuf,
            _first: zellij_utils::ipc::ClientToServerMsg,
        ) {
        }
    }

    pub(crate) fn single_use_link_map(link_id: LinkId, secret: &[u8]) -> crate::types::CredentialMap {
        use crate::types::{CredentialAccess, CredentialKind};
        let mut map = HashMap::new();
        map.insert(
            link_id,
            Arc::new(GuestCredential {
                link_id,
                secret: secret.to_vec(),
                kind: CredentialKind::Link,
                access: CredentialAccess::ReadWrite,
                label: "link".to_string(),
                enroll: false,
                spent: std::sync::atomic::AtomicBool::new(false),
            }),
        );
        Arc::new(Mutex::new(map))
    }

    pub(crate) fn make_state() -> (Arc<RelayTunnelState>, mpsc::UnboundedReceiver<Vec<u8>>) {
        make_state_with_credentials(single_use_link_map([0u8; 16], b"483921"))
    }

    pub(crate) fn make_state_with_credentials(
        credentials: crate::types::CredentialMap,
    ) -> (Arc<RelayTunnelState>, mpsc::UnboundedReceiver<Vec<u8>>) {
        build_state(credentials, Arc::new(UnusedOsApiFactory))
    }

    pub(crate) fn make_spawnable_state() -> (Arc<RelayTunnelState>, mpsc::UnboundedReceiver<Vec<u8>>)
    {
        let factory = Arc::new(LoopbackLinkFactory::new(Size { rows: 24, cols: 80 }));
        build_state(single_use_link_map([0u8; 16], b"483921"), factory)
    }

    fn build_state(
        credentials: crate::types::CredentialMap,
        os_api_factory: Arc<dyn SessionLinkFactory>,
    ) -> (Arc<RelayTunnelState>, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (control_tunnel_tx, control_tunnel_rx) = mpsc::unbounded_channel();
        let (terminal_tunnel_tx, _terminal_tunnel_rx) = mpsc::unbounded_channel();
        let state = Arc::new(RelayTunnelState {
            next_client_id: AtomicU32::new(1),
            clients: Mutex::new(HashMap::new()),
            control_tunnel_tx,
            terminal_tunnel_tx,
            tunnel_id: "tid-test".to_string(),
            slug: Mutex::new("slug-test".to_string()),
            credentials,
            pending_pake: Mutex::new(HashMap::new()),
            pending_e2e_keys: Mutex::new(HashMap::new()),
            pending_admissions: Mutex::new(HashMap::new()),
            relay_event_notify: mpsc::unbounded_channel().0,
            pending_device_auth: Mutex::new(HashMap::new()),
            pending_enroll: Mutex::new(HashMap::new()),
            session_name: "sess".to_string(),
            zellij_version: env!("CARGO_PKG_VERSION").to_string(),
            bridge: BrowserBridge::new(Arc::new(UnusedSessionManager)),
            os_api_factory,
            config: Arc::new(Mutex::new(Config::default())),
            config_options: Options::default(),
            config_file_path: PathBuf::from("/tmp/zellij-relay-tests"),
        });
        (state, control_tunnel_rx)
    }

    pub(crate) fn spawn_connected_client(
        state: &Arc<RelayTunnelState>,
        client_id: u32,
        link_id: LinkId,
    ) -> ViewerKeys {
        let keys = ViewerKeys {
            terminal_s2v: [1u8; 32],
            terminal_v2s: [2u8; 32],
            control_s2v: [3u8; 32],
            control_v2s: [4u8; 32],
        };
        state
            .pending_e2e_keys
            .lock()
            .unwrap()
            .insert(client_id, keys.clone());
        super::spawn_virtual_client(state, client_id, false, link_id, Vec::new(), 0, None)
            .expect("spawn_virtual_client");
        keys
    }

    pub(crate) fn insert_connected_client(
        state: &Arc<RelayTunnelState>,
        client_id: u32,
        link_id: [u8; 16],
    ) -> mpsc::UnboundedReceiver<Vec<u8>> {
        let (term_tx, _term_rx) = mpsc::unbounded_channel();
        let (ctrl_in_tx, _ctrl_in_rx) = mpsc::unbounded_channel();
        let (ctrl_out_tx, ctrl_out_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        state.clients.lock().unwrap().insert(
            client_id,
            RelayVirtualClient {
                web_client_id: ViewerId(format!("relay-test-{}", client_id)),
                is_read_only: false,
                link_id,
                terminal_input_tx: term_tx,
                control_input_tx: ctrl_in_tx,
                control_out_tx: ctrl_out_tx,
                shutdown: Some(shutdown_tx),
            },
        );
        ctrl_out_rx
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use std::time::Duration;

    use futures_util::StreamExt;
    use tokio::net::TcpListener;
    use tokio::sync::{mpsc, oneshot};
    use tokio_tungstenite::{accept_async, connect_async};

    use super::test_support::make_state;
    use super::{
        run_multiplexer, ControlTunnelSession, MultiplexerExitReason, TerminalTunnelSession,
    };

    #[tokio::test]
    async fn explicit_shutdown_closes_both_peer_websockets() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (control_closed_tx, control_closed_rx) = oneshot::channel();
        let (terminal_closed_tx, terminal_closed_rx) = oneshot::channel();

        let peer = tokio::spawn(async move {
            let (control_tcp, _) = listener.accept().await.unwrap();
            let mut control = accept_async(control_tcp).await.unwrap();
            let control_monitor = tokio::spawn(async move {
                while let Some(frame) = control.next().await {
                    if frame.is_err() {
                        break;
                    }
                }
                let _ = control_closed_tx.send(());
            });

            let (terminal_tcp, _) = listener.accept().await.unwrap();
            let mut terminal = accept_async(terminal_tcp).await.unwrap();
            let terminal_monitor = tokio::spawn(async move {
                while let Some(frame) = terminal.next().await {
                    if frame.is_err() {
                        break;
                    }
                }
                let _ = terminal_closed_tx.send(());
            });

            let _ = tokio::join!(control_monitor, terminal_monitor);
        });

        let (control_ws, _) = connect_async(format!("ws://{addr}/control")).await.unwrap();
        let (control_sink, control_stream) = control_ws.split();
        let control = ControlTunnelSession {
            public_url: "http://localhost/r/test".to_string(),
            slug: "test".to_string(),
            tunnel_id: "tunnel-test".to_string(),
            terminal_binding_secret: "a".repeat(64),
            sink: control_sink,
            stream: control_stream,
        };

        let (terminal_ws, _) = connect_async(format!("ws://{addr}/terminal"))
            .await
            .unwrap();
        let (terminal_sink, terminal_stream) = terminal_ws.split();
        let terminal = TerminalTunnelSession {
            sink: terminal_sink,
            stream: terminal_stream,
        };

        let (state, control_tunnel_rx) = make_state();
        let (_terminal_tunnel_tx, terminal_tunnel_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let multiplexer = tokio::spawn(run_multiplexer(
            state,
            control,
            terminal,
            control_tunnel_rx,
            terminal_tunnel_rx,
            shutdown_rx,
        ));

        shutdown_tx.send(()).unwrap();
        let exit = tokio::time::timeout(Duration::from_secs(1), multiplexer)
            .await
            .expect("multiplexer did not stop promptly")
            .expect("multiplexer task panicked");
        assert_eq!(exit, MultiplexerExitReason::Shutdown);

        tokio::time::timeout(Duration::from_secs(1), control_closed_rx)
            .await
            .expect("control peer did not observe socket closure")
            .expect("control closure notifier dropped");
        tokio::time::timeout(Duration::from_secs(1), terminal_closed_rx)
            .await
            .expect("terminal peer did not observe socket closure")
            .expect("terminal closure notifier dropped");
        peer.await.unwrap();
    }
}

#[cfg(test)]
mod pake_responder_tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    use crate::types::{
        CredentialAccess, GuestCredential, PendingDeviceAuth, PendingPake, RelayTunnelState,
    };
    use zellij_browser_bridge::control_frame::ControlFrame;
    use zellij_browser_bridge::protocol::ToBrowser;
    use zellij_relay_protocol::crypto;
    use zellij_relay_protocol::{decode_control_frame, ControlMessage};

    use super::test_support::{
        insert_connected_client, make_spawnable_state, make_state, make_state_with_credentials,
        spawn_connected_client,
    };
    use super::ViewerId;
    use super::{
        admit_pending, client_id_is_tracked, disconnect_clients_with_link, dispatch_control_message,
        evict_orphan_client_at_relay, finish_pending_enroll, handle_pake_challenge,
        handle_pake_confirm, reject_enroll, relay_virtual_web_client_id, reject_pending,
        reject_pending_with_link, AdmitResult,
    };
    use crate::types::{PendingAdmission, RelayVirtualClient};
    use std::time::Duration;
    use zellij_relay_protocol::crypto::ViewerKeys;

    #[test]
    fn revoke_kicks_connected_client_on_matching_link() {
        let (state, _control_rx) = make_state();
        let link_id = [7u8; 16];
        let other_link = [3u8; 16];
        let (term_tx, _term_rx) = mpsc::unbounded_channel();
        let (ctrl_in_tx, _ctrl_in_rx) = mpsc::unbounded_channel();
        let (ctrl_out_tx, mut ctrl_out_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        state.clients.lock().unwrap().insert(
            42,
            RelayVirtualClient {
                web_client_id: ViewerId("relay-test-42".to_string()),
                is_read_only: false,
                link_id,
                terminal_input_tx: term_tx,
                control_input_tx: ctrl_in_tx,
                control_out_tx: ctrl_out_tx,
                shutdown: Some(shutdown_tx),
            },
        );

        assert_eq!(disconnect_clients_with_link(&state, &other_link), 0);
        assert!(state.clients.lock().unwrap().contains_key(&42));
        assert!(ctrl_out_rx.try_recv().is_err());

        assert_eq!(disconnect_clients_with_link(&state, &link_id), 1);
        assert!(!state.clients.lock().unwrap().contains_key(&42));
        let frame = ctrl_out_rx.try_recv().expect("an exit frame was sent to the viewer");
        let parsed: ToBrowser = serde_json::from_slice(&frame).expect("valid ToBrowser frame");
        assert!(matches!(parsed, ToBrowser::Rejected { .. }));
    }

    #[test]
    fn wrong_confirmation_rejects_viewer() {
        let (state, mut control_rx) = make_state();
        let req = vec![7u8];
        state.pending_pake.lock().unwrap().insert(
            req.clone(),
            PendingPake {
                client_id: 5,
                pake_key: vec![1, 2, 3, 4],
                viewer_msg: vec![10],
                sharer_msg: vec![20],
                link_id: [0u8; 16],
            },
        );
        handle_pake_confirm(&state, req, vec![0u8; 32]);
        let frame = control_rx.try_recv().expect("a frame was sent");
        match decode_control_frame(&frame).unwrap() {
            ControlMessage::PakeResult { accepted, client_id, .. } => {
                assert!(!accepted);
                assert_eq!(client_id, 5);
            },
            other => panic!("expected PakeResult, got {:?}", other),
        }
    }

    #[test]
    fn challenge_produces_response_and_stashes_handshake() {
        // A well-formed viewer SPAKE2 message yields an accepted PakeResponse
        // and leaves a pending handshake to be confirmed.
        let (viewer_state, viewer_msg) =
            zellij_relay_protocol::crypto::pake_start(b"483921", "slug-test");
        drop(viewer_state);
        let (state, mut control_rx) = make_state();
        handle_pake_challenge(&state, vec![1], viewer_msg, Vec::new());
        let frame = control_rx.try_recv().expect("a frame was sent");
        match decode_control_frame(&frame).unwrap() {
            ControlMessage::PakeResponse {
                accepted,
                sharer_msg,
                sharer_confirm,
                ..
            } => {
                assert!(accepted);
                assert!(!sharer_msg.is_empty());
                assert_eq!(sharer_confirm.len(), zellij_relay_protocol::crypto::CONFIRM_LEN);
            },
            other => panic!("expected PakeResponse, got {:?}", other),
        }
        assert_eq!(state.pending_pake.lock().unwrap().len(), 1);
    }

    #[test]
    fn bare_version_request_is_distinguishable_from_browser_frames() {
        use zellij_browser_bridge::protocol::WebClientToWebServerControlMessagePayload as Payload;

        let parsed = serde_json::from_str::<Payload>(r#"{"type":"VersionRequest"}"#);
        assert!(matches!(parsed, Ok(Payload::VersionRequest)));

        let browser_frame = r#"{"web_client_id":"abc","payload":{"type":"ClientReady"}}"#;
        assert!(serde_json::from_str::<Payload>(browser_frame).is_err());
    }

    #[test]
    fn heartbeat_ping_pong_wire_format() {
        use zellij_browser_bridge::protocol::WebClientToWebServerControlMessagePayload as Payload;

        assert_eq!(
            serde_json::to_string(&Payload::Ping).unwrap(),
            r#"{"type":"Ping"}"#
        );
        assert!(matches!(
            serde_json::from_str::<Payload>(r#"{"type":"Ping"}"#),
            Ok(Payload::Ping)
        ));
        assert_eq!(
            serde_json::to_string(&ToBrowser::Pong).unwrap(),
            r#"{"type":"Pong"}"#
        );
        assert!(matches!(
            serde_json::from_str::<ToBrowser>(r#"{"type":"Pong"}"#),
            Ok(ToBrowser::Pong)
        ));
    }

    fn insert_credential(
        state: &Arc<RelayTunnelState>,
        link_id: [u8; 16],
        secret: &[u8],
        spent: bool,
    ) {
        use crate::types::{CredentialAccess, CredentialKind};
        state.credentials.lock().unwrap().insert(
            link_id,
            Arc::new(GuestCredential {
                link_id,
                secret: secret.to_vec(),
                kind: CredentialKind::Link,
                access: CredentialAccess::ReadWrite,
                label: "extra".to_string(),
                enroll: false,
                spent: AtomicBool::new(spent),
            }),
        );
    }

    #[test]
    fn unknown_link_id_rejects_challenge() {
        let (state, mut control_rx) = make_state();
        handle_pake_challenge(&state, vec![1], vec![9, 9, 9], vec![7u8; 16]);
        let frame = control_rx.try_recv().expect("a frame was sent");
        match decode_control_frame(&frame).unwrap() {
            ControlMessage::PakeResponse { accepted, .. } => assert!(!accepted),
            other => panic!("expected PakeResponse, got {:?}", other),
        }
        assert!(state.pending_pake.lock().unwrap().is_empty());
    }

    #[test]
    fn ambiguous_empty_link_id_rejects_when_multiple_credentials() {
        let (state, mut control_rx) = make_state();
        insert_credential(&state, [5u8; 16], b"second-secret", false);
        handle_pake_challenge(&state, vec![1], vec![9, 9, 9], Vec::new());
        let frame = control_rx.try_recv().expect("a frame was sent");
        match decode_control_frame(&frame).unwrap() {
            ControlMessage::PakeResponse { accepted, .. } => assert!(!accepted),
            other => panic!("expected PakeResponse, got {:?}", other),
        }
        assert!(state.pending_pake.lock().unwrap().is_empty());
    }

    #[test]
    fn exhausted_credential_rejects_challenge() {
        let (state, mut control_rx) = make_state();
        let link_id = [3u8; 16];
        insert_credential(&state, link_id, b"second-secret", true);
        handle_pake_challenge(&state, vec![1], vec![9, 9, 9], link_id.to_vec());
        let frame = control_rx.try_recv().expect("a frame was sent");
        match decode_control_frame(&frame).unwrap() {
            ControlMessage::PakeResponse { accepted, .. } => assert!(!accepted),
            other => panic!("expected PakeResponse, got {:?}", other),
        }
        assert!(state.pending_pake.lock().unwrap().is_empty());
    }

    #[test]
    fn valid_link_id_selects_its_credential() {
        let (state, mut control_rx) = make_state();
        let link_id = [5u8; 16];
        insert_credential(&state, link_id, b"second-secret", false);
        let (viewer_state, viewer_msg) =
            zellij_relay_protocol::crypto::pake_start(b"second-secret", "slug-test");
        drop(viewer_state);
        handle_pake_challenge(&state, vec![1], viewer_msg, link_id.to_vec());
        let frame = control_rx.try_recv().expect("a frame was sent");
        match decode_control_frame(&frame).unwrap() {
            ControlMessage::PakeResponse { accepted, .. } => assert!(accepted),
            other => panic!("expected PakeResponse, got {:?}", other),
        }
        let pending = state.pending_pake.lock().unwrap();
        let stashed = pending.get(&vec![1u8]).expect("handshake stashed");
        assert_eq!(stashed.link_id, link_id);
    }

    fn insert_credential_full(
        state: &Arc<RelayTunnelState>,
        link_id: [u8; 16],
        secret: &[u8],
        read_only: bool,
    ) {
        use crate::types::{CredentialAccess, CredentialKind};
        let access = if read_only {
            CredentialAccess::ReadOnly
        } else {
            CredentialAccess::ReadWrite
        };
        state.credentials.lock().unwrap().insert(
            link_id,
            Arc::new(GuestCredential {
                link_id,
                secret: secret.to_vec(),
                kind: CredentialKind::Link,
                access,
                label: "link".to_string(),
                enroll: false,
                spent: AtomicBool::new(false),
            }),
        );
    }

    fn run_full_handshake(
        state: &Arc<RelayTunnelState>,
        control_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
        secret: &[u8],
        link_id: &[u8],
        request_id: Vec<u8>,
    ) -> bool {
        use zellij_relay_protocol::crypto;
        let (pake_state, viewer_msg) = crypto::pake_start(secret, "slug-test");
        handle_pake_challenge(
            state,
            request_id.clone(),
            viewer_msg.clone(),
            link_id.to_vec(),
        );
        let frame = control_rx.try_recv().expect("a challenge frame was sent");
        let (accepted, sharer_msg, client_id) = match decode_control_frame(&frame).unwrap() {
            ControlMessage::PakeResponse {
                accepted,
                sharer_msg,
                client_id,
                ..
            } => (accepted, sharer_msg, client_id),
            other => panic!("expected PakeResponse, got {:?}", other),
        };
        if !accepted {
            return false;
        }
        let pake_key = crypto::pake_finish(pake_state, &sharer_msg).unwrap();
        let viewer_confirm = crypto::confirmation_tag(
            &pake_key,
            crypto::CONFIRM_LABEL_VIEWER,
            &viewer_msg,
            &sharer_msg,
        );
        handle_pake_confirm(state, request_id, viewer_confirm.to_vec());
        let _ = control_rx.try_recv();
        admit_pending(state, client_id, true);
        true
    }

    #[test]
    fn two_single_use_credentials_select_spend_and_access_independently() {
        let (state, mut rx) = make_state();
        let link_a = [0u8; 16];
        let link_b = [2u8; 16];
        insert_credential_full(&state, link_b, b"secretB", true);

        assert!(!state
            .select_credential(&link_a)
            .unwrap()
            .access
            .is_read_only());
        assert!(state
            .select_credential(&link_b)
            .unwrap()
            .access
            .is_read_only());

        assert!(run_full_handshake(&state, &mut rx, b"secretB", &link_b, vec![1]));
        assert!(state.select_credential(&link_b).is_none());
        assert!(!state.select_credential(&link_a).unwrap().is_spent());

        assert!(!run_full_handshake(&state, &mut rx, b"secretB", &link_b, vec![2]));

        assert!(run_full_handshake(&state, &mut rx, b"483921", &link_a, vec![3]));
        assert!(state.select_credential(&link_a).is_none());
        assert!(!run_full_handshake(&state, &mut rx, b"483921", &link_a, vec![4]));
    }

    fn handshake_to_pending(
        state: &Arc<RelayTunnelState>,
        control_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
        secret: &[u8],
        link_id: &[u8],
        request_id: Vec<u8>,
    ) -> u32 {
        use zellij_relay_protocol::crypto;
        let (pake_state, viewer_msg) = crypto::pake_start(secret, "slug-test");
        handle_pake_challenge(state, request_id.clone(), viewer_msg.clone(), link_id.to_vec());
        let frame = control_rx.try_recv().expect("a challenge frame was sent");
        let (sharer_msg, client_id) = match decode_control_frame(&frame).unwrap() {
            ControlMessage::PakeResponse {
                accepted,
                sharer_msg,
                client_id,
                ..
            } => {
                assert!(accepted, "challenge must be accepted");
                (sharer_msg, client_id)
            },
            other => panic!("expected PakeResponse, got {:?}", other),
        };
        let pake_key = crypto::pake_finish(pake_state, &sharer_msg).unwrap();
        let viewer_confirm = crypto::confirmation_tag(
            &pake_key,
            crypto::CONFIRM_LABEL_VIEWER,
            &viewer_msg,
            &sharer_msg,
        );
        handle_pake_confirm(state, request_id, viewer_confirm.to_vec());
        let _ = control_rx.try_recv();
        client_id
    }

    #[test]
    fn confirm_enqueues_pending_without_spawn_or_debit() {
        let (state, mut rx) = make_state();
        let link = [4u8; 16];
        insert_credential_full(&state, link, b"sek", false);
        let client_id = handshake_to_pending(&state, &mut rx, b"sek", &link, vec![1]);
        assert_eq!(state.pending_admissions.lock().unwrap().len(), 1);
        assert!(state.clients.lock().unwrap().is_empty());
        assert!(!state.select_credential(&link).unwrap().is_spent());
        let pending = state.pending_admissions.lock().unwrap();
        let p = pending.get(&client_id).expect("pending recorded");
        assert_eq!(p.sas.len(), 6);
    }

    #[test]
    fn read_write_admit_requires_code_confirmation() {
        let (state, mut rx) = make_state();
        let link = [6u8; 16];
        insert_credential_full(&state, link, b"rw", false);
        let client_id = handshake_to_pending(&state, &mut rx, b"rw", &link, vec![1]);
        assert_eq!(
            admit_pending(&state, client_id, false),
            AdmitResult::NeedsCodeConfirm
        );
        assert!(!state.select_credential(&link).unwrap().is_spent());
        assert!(state.pending_admissions.lock().unwrap().contains_key(&client_id));
        let _ = admit_pending(&state, client_id, true);
        assert!(state.select_credential(&link).is_none());
    }

    #[test]
    fn read_only_admit_does_not_need_code_confirmation() {
        let (state, mut rx) = make_state();
        let link = [7u8; 16];
        insert_credential_full(&state, link, b"ro", true);
        let client_id = handshake_to_pending(&state, &mut rx, b"ro", &link, vec![1]);
        let outcome = admit_pending(&state, client_id, false);
        assert_ne!(outcome, AdmitResult::NeedsCodeConfirm);
        assert!(state.select_credential(&link).is_none());
    }

    #[test]
    fn reject_spends_nothing_and_drops_pending() {
        let (state, mut rx) = make_state();
        let link = [8u8; 16];
        insert_credential_full(&state, link, b"sek", false);
        let client_id = handshake_to_pending(&state, &mut rx, b"sek", &link, vec![1]);
        assert!(reject_pending(&state, client_id, "owner said no"));
        assert!(state.pending_admissions.lock().unwrap().is_empty());
        assert!(!state.select_credential(&link).unwrap().is_spent());
        let frame = rx.try_recv().expect("a rejected control frame was sent");
        assert!(matches!(
            decode_control_frame(&frame).unwrap(),
            ControlMessage::ControlFrameData { .. }
        ));
    }

    #[test]
    fn concurrent_single_use_joins_admit_one_and_reject_other() {
        let (state, mut rx) = make_state();
        let link = [9u8; 16];
        insert_credential_full(&state, link, b"sek", true);
        let first = handshake_to_pending(&state, &mut rx, b"sek", &link, vec![1]);
        let second = handshake_to_pending(&state, &mut rx, b"sek", &link, vec![2]);
        assert_eq!(state.pending_admissions.lock().unwrap().len(), 2);

        let outcome = admit_pending(&state, first, true);
        assert!(matches!(
            outcome,
            AdmitResult::Admitted | AdmitResult::SpawnFailed
        ));
        assert!(state.select_credential(&link).is_none());
        assert!(state.pending_admissions.lock().unwrap().is_empty());
        assert!(!state.pending_admissions.lock().unwrap().contains_key(&second));

        assert_eq!(admit_pending(&state, second, true), AdmitResult::NotFound);
    }

    #[test]
    fn admit_spawn_failure_removes_single_use_credential() {
        let (state, mut rx) = make_state();
        let link = [11u8; 16];
        insert_credential_full(&state, link, b"sek", true);
        let client_id = handshake_to_pending(&state, &mut rx, b"sek", &link, vec![1]);
        assert_eq!(admit_pending(&state, client_id, false), AdmitResult::SpawnFailed);
        assert!(!state.credentials.lock().unwrap().contains_key(&link));
    }

    #[test]
    fn disconnect_gcs_spent_single_use_link() {
        let (state, _rx) = make_state();
        let link = [13u8; 16];
        insert_credential(&state, link, b"sek", true);
        insert_connected_client(&state, 60, link);
        assert!(state.credentials.lock().unwrap().contains_key(&link));
        assert_eq!(disconnect_clients_with_link(&state, &link), 1);
        assert!(state.clients.lock().unwrap().is_empty());
        assert!(!state.credentials.lock().unwrap().contains_key(&link));
    }

    #[test]
    fn disconnect_keeps_device_credential() {
        let (state, _rx) = make_state();
        let link = [14u8; 16];
        state.credentials.lock().unwrap().insert(
            link,
            GuestCredential::device(
                link,
                b"sek".to_vec(),
                CredentialAccess::ReadWrite,
                "device".to_string(),
            ),
        );
        insert_connected_client(&state, 61, link);
        assert_eq!(disconnect_clients_with_link(&state, &link), 1);
        assert!(state.credentials.lock().unwrap().contains_key(&link));
    }

    #[test]
    fn revoke_sweep_rejects_pending_admission() {
        let (state, mut rx) = make_state();
        let link = [21u8; 16];
        insert_credential_full(&state, link, b"sek", false);
        let client_id = handshake_to_pending(&state, &mut rx, b"sek", &link, vec![1]);
        assert_eq!(state.pending_admissions.lock().unwrap().len(), 1);
        let _ = client_id;
        assert_eq!(reject_pending_with_link(&state, &link), 1);
        assert!(state.pending_admissions.lock().unwrap().is_empty());
        let frame = rx.try_recv().expect("a rejected control frame was sent");
        assert!(matches!(
            decode_control_frame(&frame).unwrap(),
            ControlMessage::ControlFrameData { .. }
        ));
    }

    #[test]
    fn revoke_sweep_rejects_pending_device_auth() {
        let (state, mut rx) = make_state();
        let link = [22u8; 16];
        state.pending_device_auth.lock().unwrap().insert(
            7,
            PendingDeviceAuth {
                client_id: 7,
                link_id: link,
                access: CredentialAccess::ReadWrite,
                challenge: vec![0u8; 32],
                control_s2v: [0u8; 32],
                control_v2s: [0u8; 32],
                out_seq: 0,
                last_in_seq: None,
                challenge_sent: false,
                created_at: std::time::Instant::now(),
            },
        );
        assert_eq!(reject_pending_with_link(&state, &link), 1);
        assert!(state.pending_device_auth.lock().unwrap().is_empty());
        let frame = rx.try_recv().expect("a rejected control frame was sent");
        assert!(matches!(
            decode_control_frame(&frame).unwrap(),
            ControlMessage::ControlFrameData { .. }
        ));
    }

    #[test]
    fn revoke_sweep_clears_live_and_pending_on_same_link() {
        let (state, mut rx) = make_state();
        let link = [23u8; 16];
        let (term_tx, _term_rx) = mpsc::unbounded_channel();
        let (ctrl_in_tx, _ctrl_in_rx) = mpsc::unbounded_channel();
        let (ctrl_out_tx, _ctrl_out_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        state.clients.lock().unwrap().insert(
            50,
            RelayVirtualClient {
                web_client_id: ViewerId("relay-test-50".to_string()),
                is_read_only: false,
                link_id: link,
                terminal_input_tx: term_tx,
                control_input_tx: ctrl_in_tx,
                control_out_tx: ctrl_out_tx,
                shutdown: Some(shutdown_tx),
            },
        );
        insert_credential_full(&state, link, b"sek", false);
        let _ = handshake_to_pending(&state, &mut rx, b"sek", &link, vec![1]);
        assert_eq!(disconnect_clients_with_link(&state, &link), 1);
        assert_eq!(reject_pending_with_link(&state, &link), 1);
        assert!(state.clients.lock().unwrap().is_empty());
        assert!(state.pending_admissions.lock().unwrap().is_empty());
    }

    fn zero_viewer_keys() -> ViewerKeys {
        ViewerKeys {
            terminal_s2v: [0u8; 32],
            terminal_v2s: [0u8; 32],
            control_s2v: [0u8; 32],
            control_v2s: [0u8; 32],
        }
    }

    fn insert_pending_admission(state: &Arc<RelayTunnelState>, client_id: u32, link_id: [u8; 16]) {
        state.pending_admissions.lock().unwrap().insert(
            client_id,
            PendingAdmission {
                client_id,
                link_id,
                sas: "000000".to_string(),
                access: CredentialAccess::ReadWrite,
                label: "pending".to_string(),
                claimed_name: None,
                created_at: std::time::Instant::now(),
            },
        );
    }

    fn insert_pending_device_auth(state: &Arc<RelayTunnelState>, client_id: u32, link_id: [u8; 16]) {
        state.pending_device_auth.lock().unwrap().insert(
            client_id,
            PendingDeviceAuth {
                client_id,
                link_id,
                access: CredentialAccess::ReadWrite,
                challenge: vec![0u8; 32],
                control_s2v: [0u8; 32],
                control_v2s: [0u8; 32],
                out_seq: 0,
                last_in_seq: None,
                challenge_sent: false,
                created_at: std::time::Instant::now(),
            },
        );
    }

    #[test]
    fn untracked_client_frame_evicts_at_relay() {
        let (state, mut control_rx) = make_state();
        evict_orphan_client_at_relay(&state, 999, "control");
        let frame = control_rx
            .try_recv()
            .expect("an eviction frame was sent to the relay");
        match decode_control_frame(&frame).unwrap() {
            ControlMessage::ClientDisconnected { client_id } => assert_eq!(client_id, 999),
            other => panic!("expected ClientDisconnected, got {:?}", other),
        }
    }

    #[test]
    fn connected_client_is_not_evicted() {
        let (state, mut control_rx) = make_state();
        let _ctrl_rx = insert_connected_client(&state, 7, [1u8; 16]);
        assert!(client_id_is_tracked(&state, 7));
        evict_orphan_client_at_relay(&state, 7, "control");
        assert!(control_rx.try_recv().is_err());
    }

    #[test]
    fn pending_admission_suppresses_eviction() {
        let (state, mut control_rx) = make_state();
        insert_pending_admission(&state, 11, [2u8; 16]);
        assert!(client_id_is_tracked(&state, 11));
        evict_orphan_client_at_relay(&state, 11, "control");
        assert!(control_rx.try_recv().is_err());
    }

    #[test]
    fn pending_e2e_keys_suppresses_eviction() {
        let (state, mut control_rx) = make_state();
        state
            .pending_e2e_keys
            .lock()
            .unwrap()
            .insert(12, zero_viewer_keys());
        assert!(client_id_is_tracked(&state, 12));
        evict_orphan_client_at_relay(&state, 12, "terminal");
        assert!(control_rx.try_recv().is_err());
    }

    #[test]
    fn pending_device_auth_suppresses_eviction() {
        let (state, mut control_rx) = make_state();
        insert_pending_device_auth(&state, 13, [3u8; 16]);
        assert!(client_id_is_tracked(&state, 13));
        evict_orphan_client_at_relay(&state, 13, "control");
        assert!(control_rx.try_recv().is_err());
    }

    #[test]
    fn to_browser_exit_round_trips() {
        let exit = ToBrowser::Exit {
            reason: "host ended session".to_string(),
        };
        let json = serde_json::to_string(&exit).unwrap();
        assert_eq!(
            json,
            r#"{"type":"Exit","reason":"host ended session"}"#
        );
        match serde_json::from_str::<ToBrowser>(&json).unwrap() {
            ToBrowser::Exit { reason } => assert_eq!(reason, "host ended session"),
            other => panic!("expected Exit, got {:?}", other),
        }
    }

    fn try_decrypt_to_browser(frame: &[u8], control_s2v: &[u8; 32]) -> Option<ToBrowser> {
        let data = match decode_control_frame(frame).ok()? {
            ControlMessage::ControlFrameData { data, .. } => data,
            _ => return None,
        };
        let (_seq, plaintext) = crypto::decrypt_seq(
            control_s2v,
            crypto::FRAME_TYPE_CONTROL,
            crypto::DIRECTION_SHARER_TO_VIEWER,
            &data,
        )
        .ok()?;
        serde_json::from_slice(&plaintext).ok()
    }

    async fn wait_for_browser_frame<F>(
        rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
        control_s2v: &[u8; 32],
        predicate: F,
    ) -> Option<ToBrowser>
    where
        F: Fn(&ToBrowser) -> bool,
    {
        for _ in 0..200 {
            while let Ok(frame) = rx.try_recv() {
                if let Some(parsed) = try_decrypt_to_browser(&frame, control_s2v) {
                    if predicate(&parsed) {
                        return Some(parsed);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        None
    }

    async fn wait_until<F: Fn() -> bool>(cond: F) -> bool {
        for _ in 0..200 {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cond()
    }

    #[tokio::test]
    async fn graceful_close_emits_exit_to_viewer() {
        let (state, mut relay_rx) = make_spawnable_state();
        let keys = spawn_connected_client(&state, 1, [1u8; 16]);
        assert!(state.clients.lock().unwrap().contains_key(&1));

        let viewer = ViewerId(relay_virtual_web_client_id(&state.tunnel_id, 1));
        let close_tx = state
            .bridge
            .roster()
            .lock()
            .unwrap()
            .control_out(&viewer)
            .expect("attached viewer has a control_out sender");
        close_tx
            .send(ControlFrame::Close {
                code: 1000,
                reason: "host ended session".to_string(),
            })
            .expect("close frame queued");

        let exit = wait_for_browser_frame(&mut relay_rx, &keys.control_s2v, |b| {
            matches!(b, ToBrowser::Exit { .. })
        })
        .await;
        match exit {
            Some(ToBrowser::Exit { reason }) => assert_eq!(reason, "host ended session"),
            other => panic!("expected ToBrowser::Exit on the relay leg, got {:?}", other),
        }
        assert!(wait_until(|| !state.clients.lock().unwrap().contains_key(&1)).await);
    }

    #[test]
    fn orphan_frame_after_reconnect_evicts_at_relay() {
        let (state_a, mut relay_rx_a) = make_state();
        let credentials = state_a.credentials.clone();
        let (state_b, mut relay_rx_b) = make_state_with_credentials(credentials);
        let _ctrl_rx = insert_connected_client(&state_a, 7, [2u8; 16]);
        assert!(state_a.clients.lock().unwrap().contains_key(&7));
        assert!(!state_b.clients.lock().unwrap().contains_key(&7));

        dispatch_control_message(
            &state_b,
            ControlMessage::ControlFrameData {
                client_id: 7,
                data: vec![9, 9, 9],
            },
        );

        let frame = relay_rx_b
            .try_recv()
            .expect("state B emits an eviction frame to the relay");
        match decode_control_frame(&frame).unwrap() {
            ControlMessage::ClientDisconnected { client_id } => assert_eq!(client_id, 7),
            other => panic!("expected ClientDisconnected, got {:?}", other),
        }
        assert!(relay_rx_a.try_recv().is_err());
        assert!(state_a.clients.lock().unwrap().contains_key(&7));
    }

    fn insert_enroll_credential(state: &Arc<RelayTunnelState>, link_id: [u8; 16], secret: &[u8]) {
        use crate::types::{CredentialAccess, CredentialKind};
        state.credentials.lock().unwrap().insert(
            link_id,
            Arc::new(GuestCredential {
                link_id,
                secret: secret.to_vec(),
                kind: CredentialKind::Link,
                access: CredentialAccess::ReadWrite,
                label: "dev-link".to_string(),
                enroll: true,
                spent: AtomicBool::new(false),
            }),
        );
    }

    #[tokio::test]
    async fn enroll_admit_defers_spawn_until_complete() {
        let (state, mut rx) = make_spawnable_state();
        let link = [21u8; 16];
        insert_enroll_credential(&state, link, b"sek");
        let client_id = handshake_to_pending(&state, &mut rx, b"sek", &link, vec![1]);

        assert_eq!(admit_pending(&state, client_id, true), AdmitResult::Admitted);
        assert!(state.clients.lock().unwrap().is_empty());
        assert!(state.pending_enroll.lock().unwrap().contains_key(&client_id));

        finish_pending_enroll(&state, client_id);
        assert!(state.pending_enroll.lock().unwrap().is_empty());
        assert!(state.clients.lock().unwrap().contains_key(&client_id));
    }

    #[tokio::test]
    async fn reject_enroll_drops_pending_and_keys() {
        let (state, mut rx) = make_spawnable_state();
        let link = [22u8; 16];
        insert_enroll_credential(&state, link, b"sek");
        let client_id = handshake_to_pending(&state, &mut rx, b"sek", &link, vec![1]);

        assert_eq!(admit_pending(&state, client_id, true), AdmitResult::Admitted);
        assert!(state.pending_e2e_keys.lock().unwrap().contains_key(&client_id));

        assert!(reject_enroll(&state, client_id, "test"));
        assert!(state.pending_enroll.lock().unwrap().is_empty());
        assert!(!state.pending_e2e_keys.lock().unwrap().contains_key(&client_id));
        assert!(!reject_enroll(&state, client_id, "test"));
    }

    #[tokio::test]
    async fn full_path_pake_admit_connect_revoke_kicks() {
        let (state, mut rx) = make_spawnable_state();
        let link = [12u8; 16];
        insert_credential_full(&state, link, b"sek", false);
        let client_id = handshake_to_pending(&state, &mut rx, b"sek", &link, vec![1]);

        assert_eq!(admit_pending(&state, client_id, true), AdmitResult::Admitted);
        assert!(state.clients.lock().unwrap().contains_key(&client_id));

        assert_eq!(disconnect_clients_with_link(&state, &link), 1);
        assert!(wait_until(|| state.clients.lock().unwrap().is_empty()).await);
    }
}
