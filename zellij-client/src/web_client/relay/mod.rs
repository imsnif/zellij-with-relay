pub mod control_tunnel;
pub mod terminal_tunnel;
pub mod types;

use anyhow::Result;
use std::sync::OnceLock;

use zellij_utils::data::ClientId;

pub use types::{RelayTunnelHandle, RelayTunnelRegistry, SharedRegistry};

use control_tunnel::open_control_tunnel;
use terminal_tunnel::{open_terminal_tunnel, run_until_shutdown};

static REGISTRY: OnceLock<SharedRegistry> = OnceLock::new();

fn registry() -> &'static SharedRegistry {
    REGISTRY.get_or_init(RelayTunnelRegistry::new)
}

/// Phase 1: establish a relay tunnel, register the handle, return public URL.
pub async fn start_relay_tunnel(
    client_id: ClientId,
    relay_url: String,
    session_name: String,
    zellij_version: String,
) -> Result<String> {
    let control = open_control_tunnel(&relay_url, session_name, zellij_version).await?;
    let public_url = control.public_url.clone();
    let slug = control.slug.clone();
    let tunnel_id = control.tunnel_id.clone();
    let terminal = open_terminal_tunnel(&relay_url, &slug, tunnel_id.clone()).await?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = RelayTunnelHandle {
        public_url: public_url.clone(),
        slug,
        tunnel_id,
        shutdown_tx,
    };
    registry().insert(client_id, handle).await;

    tokio::spawn(async move {
        run_until_shutdown(control, terminal, shutdown_rx).await;
    });

    Ok(public_url)
}

/// Phase 1: signal the registered tunnel to close. Returns true if a tunnel
/// existed for this client.
pub async fn stop_relay_tunnel(client_id: ClientId) -> bool {
    if let Some(handle) = registry().remove(client_id).await {
        let _ = handle.shutdown_tx.send(());
        true
    } else {
        false
    }
}
