pub mod control_tunnel;
pub mod multiplexer;
pub mod terminal_tunnel;
pub mod types;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use zellij_utils::{
    data::ClientId,
    input::{config::Config, options::Options},
};

pub use types::{
    RelayTunnelHandle, RelayTunnelRegistry, RelayTunnelState, RelayTunnelStatus, SharedRegistry,
};

use crate::web_client::types::{ClientOsApiFactory, ConnectionTable, SessionManager};

use control_tunnel::open_control_tunnel;
use multiplexer::{run_multiplexer, MultiplexerExitReason};
use terminal_tunnel::open_terminal_tunnel;

static REGISTRY: OnceLock<SharedRegistry> = OnceLock::new();

fn registry() -> &'static SharedRegistry {
    REGISTRY.get_or_init(RelayTunnelRegistry::new)
}

/// Reconnect backoff schedule in seconds. Matches the Phase 6 spec:
/// `{1, 2, 4, 8, 16, 30, 30, …}s` — doubling capped at 30 s.
const BACKOFF_SCHEDULE_SECS: &[u64] = &[1, 2, 4, 8, 16, 30];
/// After this many consecutive failed reconnects the supervisor gives
/// up and surfaces `Failed(…)` to the share plugin. Operators can
/// re-share with `I` + `i` to kick off a fresh tunnel.
const MAX_RECONNECT_ATTEMPTS: u32 = 12;

/// Establish a relay tunnel, spawn the supervisor (which in turn spawns
/// the multiplexer with heartbeat), and return the first public URL.
/// Subsequent disconnect/reconnect cycles update the shared
/// `RelayTunnelStatus` on the registry handle so the
/// `GetRelayTunnelStatus` IPC poll can surface them to the share plugin.
pub async fn start_relay_tunnel(
    client_id: ClientId,
    relay_url: String,
    session_name: String,
    zellij_version: String,
    relay_tunnel_auth_token: String,
    connection_table: Arc<Mutex<ConnectionTable>>,
    os_api_factory: Arc<dyn ClientOsApiFactory>,
    session_manager: Arc<dyn SessionManager>,
    config: Arc<Mutex<Config>>,
    config_options: Options,
    config_file_path: PathBuf,
) -> Result<String> {
    // First connection is synchronous so the caller sees
    // `RelayTunnelEstablished` → URL immediately. Subsequent reconnects
    // are driven by the supervisor task spawned below.
    let control = open_control_tunnel(
        &relay_url,
        session_name.clone(),
        zellij_version.clone(),
        relay_tunnel_auth_token.clone(),
        String::new(),
    )
    .await?;
    let public_url = control.public_url.clone();
    let slug = control.slug.clone();
    let tunnel_id = control.tunnel_id.clone();
    let terminal = open_terminal_tunnel(&relay_url, &slug, tunnel_id.clone()).await?;

    let (control_tunnel_tx, control_tunnel_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (terminal_tunnel_tx, terminal_tunnel_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    let state = Arc::new(RelayTunnelState {
        next_client_id: AtomicU32::new(1),
        clients: Mutex::new(Default::default()),
        control_tunnel_tx: control_tunnel_tx.clone(),
        terminal_tunnel_tx,
        tunnel_id: tunnel_id.clone(),
        pending_e2e_keys: Mutex::new(Default::default()),
        pending_read_only: Mutex::new(Default::default()),
        token_hash_to_client_id: Mutex::new(Default::default()),
        session_name: session_name.clone(),
        connection_table: connection_table.clone(),
        os_api_factory: os_api_factory.clone(),
        session_manager: session_manager.clone(),
        config: config.clone(),
        config_options: config_options.clone(),
        config_file_path: config_file_path.clone(),
    });

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let status = Arc::new(Mutex::new(RelayTunnelStatus::Connected(public_url.clone())));
    let stop_requested = Arc::new(AtomicBool::new(false));
    let current_iteration_shutdown = Arc::new(Mutex::new(None));
    let control_tx_slot = Arc::new(Mutex::new(Some(control_tunnel_tx)));

    let handle = RelayTunnelHandle {
        public_url: public_url.clone(),
        slug: slug.clone(),
        tunnel_id,
        shutdown_tx: Mutex::new(Some(shutdown_tx)),
        stop_requested: stop_requested.clone(),
        current_iteration_shutdown: current_iteration_shutdown.clone(),
        status: status.clone(),
        control_tx: control_tx_slot.clone(),
    };
    registry().insert(client_id, handle).await;

    tokio::spawn(run_supervisor(
        state,
        control,
        terminal,
        control_tunnel_rx,
        terminal_tunnel_rx,
        shutdown_rx,
        status,
        stop_requested,
        current_iteration_shutdown,
        control_tx_slot,
        SupervisorArgs {
            relay_url,
            session_name,
            zellij_version,
            relay_tunnel_auth_token,
            last_known_slug: slug,
            last_known_url: public_url.clone(),
            connection_table,
            os_api_factory,
            session_manager,
            config,
            config_options,
            config_file_path,
        },
    ));

    Ok(public_url)
}

/// Values threaded into the supervisor so it can rebuild a fresh state +
/// socket pair on each reconnect.
struct SupervisorArgs {
    relay_url: String,
    session_name: String,
    zellij_version: String,
    relay_tunnel_auth_token: String,
    last_known_slug: String,
    last_known_url: String,
    connection_table: Arc<Mutex<ConnectionTable>>,
    os_api_factory: Arc<dyn ClientOsApiFactory>,
    session_manager: Arc<dyn SessionManager>,
    config: Arc<Mutex<Config>>,
    config_options: Options,
    config_file_path: PathBuf,
}

#[allow(clippy::too_many_arguments)]
async fn run_supervisor(
    initial_state: Arc<RelayTunnelState>,
    initial_control: control_tunnel::ControlTunnelSession,
    initial_terminal: terminal_tunnel::TerminalTunnelSession,
    initial_control_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    initial_terminal_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    initial_shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    status: Arc<Mutex<RelayTunnelStatus>>,
    stop_requested: Arc<AtomicBool>,
    current_iteration_shutdown: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    control_tx_slot: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>>,
    mut args: SupervisorArgs,
) {
    // First run: we already have open sockets + a shutdown channel.
    // Later iterations rebuild everything from scratch.
    let first_exit = run_multiplexer(
        initial_state,
        initial_control,
        initial_terminal,
        initial_control_rx,
        initial_terminal_rx,
        initial_shutdown_rx,
    )
    .await;
    if matches!(first_exit, MultiplexerExitReason::Shutdown)
        || stop_requested.load(Ordering::Relaxed)
    {
        log::info!("relay supervisor: shutdown after initial run");
        return;
    }
    log::warn!("relay supervisor: initial run dropped, entering reconnect loop");

    let mut attempt: u32 = 0;
    loop {
        if stop_requested.load(Ordering::Relaxed) {
            log::info!("relay supervisor: stop requested — exiting reconnect loop");
            return;
        }

        attempt += 1;
        if attempt > MAX_RECONNECT_ATTEMPTS {
            log::error!(
                "relay supervisor: giving up after {} reconnect attempts",
                MAX_RECONNECT_ATTEMPTS
            );
            *status.lock().unwrap() = RelayTunnelStatus::Failed(format!(
                "relay unreachable after {} attempts",
                MAX_RECONNECT_ATTEMPTS
            ));
            return;
        }

        *status.lock().unwrap() = RelayTunnelStatus::Reconnecting {
            last_known_url: Some(args.last_known_url.clone()),
            attempt,
        };

        let wait_secs = BACKOFF_SCHEDULE_SECS
            .get((attempt as usize).saturating_sub(1))
            .copied()
            .unwrap_or_else(|| *BACKOFF_SCHEDULE_SECS.last().unwrap());
        log::info!(
            "relay supervisor: reconnect attempt {} in {}s",
            attempt,
            wait_secs
        );
        // Interruptible sleep so an incoming `stop_relay_tunnel` doesn't
        // wait out the full backoff.
        let stopped = tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(wait_secs)) => false,
            _ = interruptible_stop(&stop_requested) => true,
        };
        if stopped || stop_requested.load(Ordering::Relaxed) {
            return;
        }

        let control = match open_control_tunnel(
            &args.relay_url,
            args.session_name.clone(),
            args.zellij_version.clone(),
            args.relay_tunnel_auth_token.clone(),
            args.last_known_slug.clone(),
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                log::warn!(
                    "relay supervisor: control tunnel reconnect failed (attempt {}): {:#}",
                    attempt, e
                );
                continue;
            },
        };

        let new_slug = control.slug.clone();
        let new_tunnel_id = control.tunnel_id.clone();
        let new_url = control.public_url.clone();

        let terminal = match open_terminal_tunnel(
            &args.relay_url,
            &new_slug,
            new_tunnel_id.clone(),
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                log::warn!(
                    "relay supervisor: terminal tunnel reconnect failed (attempt {}): {:#}",
                    attempt, e
                );
                continue;
            },
        };

        // Successful reconnect: reset the attempt counter, update
        // status, and remember the (possibly fresh) slug/URL for the
        // next cycle.
        attempt = 0;
        args.last_known_slug = new_slug.clone();
        args.last_known_url = new_url.clone();
        *status.lock().unwrap() = RelayTunnelStatus::Connected(new_url.clone());

        let (control_tunnel_tx, control_tunnel_rx) =
            tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (terminal_tunnel_tx, terminal_tunnel_rx) =
            tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

        // Refresh the shared control-tx slot so any outside caller (e.g. a
        // `RevokeRelayToken` IPC) reaches the fresh tunnel rather than a
        // closed channel from the previous iteration.
        *control_tx_slot.lock().unwrap() = Some(control_tunnel_tx.clone());

        let state = Arc::new(RelayTunnelState {
            next_client_id: AtomicU32::new(1),
            clients: Mutex::new(Default::default()),
            control_tunnel_tx,
            terminal_tunnel_tx,
            tunnel_id: new_tunnel_id,
            pending_e2e_keys: Mutex::new(Default::default()),
            pending_read_only: Mutex::new(Default::default()),
            token_hash_to_client_id: Mutex::new(Default::default()),
            session_name: args.session_name.clone(),
            connection_table: args.connection_table.clone(),
            os_api_factory: args.os_api_factory.clone(),
            session_manager: args.session_manager.clone(),
            config: args.config.clone(),
            config_options: args.config_options.clone(),
            config_file_path: args.config_file_path.clone(),
        });

        // Per-iteration shutdown wire: `stop_relay_tunnel` drains this
        // and fires the sender to break the run_multiplexer select.
        let (iter_shutdown_tx, iter_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        *current_iteration_shutdown.lock().unwrap() = Some(iter_shutdown_tx);

        let exit = run_multiplexer(
            state,
            control,
            terminal,
            control_tunnel_rx,
            terminal_tunnel_rx,
            iter_shutdown_rx,
        )
        .await;
        // Clear the per-iteration sender slot before the next loop so
        // a late `stop_relay_tunnel` doesn't fire into a closed half.
        *current_iteration_shutdown.lock().unwrap() = None;

        if matches!(exit, MultiplexerExitReason::Shutdown)
            || stop_requested.load(Ordering::Relaxed)
        {
            log::info!("relay supervisor: shutdown after reconnect");
            return;
        }
        log::warn!("relay supervisor: reconnected tunnel dropped again, retrying");
    }
}

/// Poll-loop that resolves when `stop_requested` becomes true. Lets the
/// supervisor's backoff sleep be broken cleanly by a stop request.
async fn interruptible_stop(flag: &AtomicBool) {
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    loop {
        ticker.tick().await;
        if flag.load(Ordering::Relaxed) {
            return;
        }
    }
}

/// Signal the registered tunnel to close. Returns true if a tunnel
/// existed for this client.
pub async fn stop_relay_tunnel(client_id: ClientId) -> bool {
    if let Some(handle) = registry().remove(client_id).await {
        handle.stop_requested.store(true, Ordering::Relaxed);
        if let Some(tx) = handle.shutdown_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
        if let Some(tx) = handle.current_iteration_shutdown.lock().unwrap().take() {
            let _ = tx.send(());
        }
        *handle.status.lock().unwrap() =
            RelayTunnelStatus::Failed("tunnel stopped by user".into());
        true
    } else {
        false
    }
}

/// Phase 6 (Session A): read the current status for a client's relay
/// tunnel and return it encoded as a sentinel-or-URL string. Empty
/// string means no tunnel is registered for this client.
pub async fn get_relay_tunnel_status_sentinel(client_id: ClientId) -> String {
    registry()
        .with_handle(client_id, |handle| {
            handle
                .status
                .lock()
                .unwrap()
                .to_mode_info_sentinel()
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default()
}

/// Phase 6 Session C: broadcast a token revocation onto every active
/// relay tunnel's control channel. The relay tears down any fan-out
/// group or r/w client whose session is keyed on the given hash.
/// Fire-and-forget: a closed control channel (tunnel between reconnect
/// iterations, or already shut down) is silently skipped.
pub async fn broadcast_revoke_token(token_hash: String) {
    use zellij_relay_protocol::ControlMessage;
    let frame = ControlMessage::RevokeToken {
        token_hash: token_hash.clone(),
    }
    .encode();
    let mut senders: Vec<tokio::sync::mpsc::UnboundedSender<Vec<u8>>> = Vec::new();
    registry()
        .for_each_handle(|handle| {
            if let Some(tx) = handle.control_tx.lock().unwrap().clone() {
                senders.push(tx);
            }
        })
        .await;
    for tx in senders {
        let _ = tx.send(frame.clone());
    }
    log::debug!(
        "relay: broadcast RevokeToken for token_hash={}",
        &token_hash
    );
}
