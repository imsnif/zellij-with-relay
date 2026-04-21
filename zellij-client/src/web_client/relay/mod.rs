pub mod control_tunnel;
pub mod multiplexer;
pub mod terminal_tunnel;
pub mod types;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex, OnceLock};

use zellij_utils::{
    data::ClientId,
    input::{config::Config, options::Options},
};

pub use types::{RelayTunnelHandle, RelayTunnelRegistry, RelayTunnelState, SharedRegistry};

use crate::web_client::types::{ClientOsApiFactory, ConnectionTable, SessionManager};

use control_tunnel::open_control_tunnel;
use multiplexer::run_multiplexer;
use terminal_tunnel::open_terminal_tunnel;

static REGISTRY: OnceLock<SharedRegistry> = OnceLock::new();

fn registry() -> &'static SharedRegistry {
    REGISTRY.get_or_init(RelayTunnelRegistry::new)
}

/// Establish a relay tunnel, spawn the multiplexer task, and return the
/// public URL. Phase 2: virtual remote clients are plumbed into the shared
/// `ConnectionTable`; each `ClientConnected` from the relay becomes a
/// standard web-client entry.
pub async fn start_relay_tunnel(
    client_id: ClientId,
    relay_url: String,
    session_name: String,
    zellij_version: String,
    connection_table: Arc<Mutex<ConnectionTable>>,
    os_api_factory: Arc<dyn ClientOsApiFactory>,
    session_manager: Arc<dyn SessionManager>,
    config: Arc<Mutex<Config>>,
    config_options: Options,
    config_file_path: PathBuf,
) -> Result<String> {
    let control = open_control_tunnel(&relay_url, session_name.clone(), zellij_version).await?;
    let public_url = control.public_url.clone();
    let slug = control.slug.clone();
    let tunnel_id = control.tunnel_id.clone();
    let terminal = open_terminal_tunnel(&relay_url, &slug, tunnel_id.clone()).await?;

    let (control_tunnel_tx, control_tunnel_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (terminal_tunnel_tx, terminal_tunnel_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    let state = Arc::new(RelayTunnelState {
        next_client_id: AtomicU32::new(1),
        clients: Mutex::new(Default::default()),
        control_tunnel_tx,
        terminal_tunnel_tx,
        tunnel_id: tunnel_id.clone(),
        pending_e2e_keys: Mutex::new(Default::default()),
        pending_read_only: Mutex::new(Default::default()),
        token_hash_to_client_id: Mutex::new(Default::default()),
        session_name,
        connection_table,
        os_api_factory,
        session_manager,
        config,
        config_options,
        config_file_path,
    });

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = RelayTunnelHandle {
        public_url: public_url.clone(),
        slug,
        tunnel_id,
        shutdown_tx,
    };
    registry().insert(client_id, handle).await;

    tokio::spawn(async move {
        run_multiplexer(
            state,
            control,
            terminal,
            control_tunnel_rx,
            terminal_tunnel_rx,
            shutdown_rx,
        )
        .await;
    });

    Ok(public_url)
}

/// Signal the registered tunnel to close. Returns true if a tunnel existed
/// for this client.
pub async fn stop_relay_tunnel(client_id: ClientId) -> bool {
    if let Some(handle) = registry().remove(client_id).await {
        let _ = handle.shutdown_tx.send(());
        true
    } else {
        false
    }
}
