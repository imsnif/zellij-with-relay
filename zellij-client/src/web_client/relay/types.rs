use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};

use zellij_relay_protocol::crypto::KEY_LEN;
use zellij_utils::{
    data::ClientId,
    input::{config::Config, options::Options},
};

use crate::web_client::types::{ClientOsApiFactory, ConnectionTable, SessionManager};

/// Handle to a running relay tunnel. Holds the shutdown signal whose firing
/// causes the multiplexer tasks to exit and the sockets to close.
pub struct RelayTunnelHandle {
    #[allow(dead_code)]
    pub public_url: String,
    #[allow(dead_code)]
    pub slug: String,
    #[allow(dead_code)]
    pub tunnel_id: String,
    pub shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

#[derive(Default)]
pub struct RelayTunnelRegistry {
    inner: AsyncMutex<HashMap<ClientId, RelayTunnelHandle>>,
}

impl RelayTunnelRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(RelayTunnelRegistry::default())
    }

    pub async fn insert(&self, client_id: ClientId, handle: RelayTunnelHandle) {
        self.inner.lock().await.insert(client_id, handle);
    }

    pub async fn remove(&self, client_id: ClientId) -> Option<RelayTunnelHandle> {
        self.inner.lock().await.remove(&client_id)
    }
}

pub type SharedRegistry = Arc<RelayTunnelRegistry>;

/// Per-tunnel state owned by the multiplexer task. Each remote viewer that
/// passes authentication becomes a `RelayVirtualClient` entry here.
pub struct RelayTunnelState {
    /// Relay-side client_id allocator. Zellij is authoritative for these ids;
    /// the counter is per-tunnel and starts at 1.
    pub next_client_id: AtomicU32,
    /// Live virtual clients, keyed by the allocated client_id.
    pub clients: Mutex<HashMap<u32, RelayVirtualClient>>,
    /// Writer queue for encoded `ControlMessage` bytes.
    pub control_tunnel_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Writer queue for encoded `TerminalMessage` bytes.
    pub terminal_tunnel_tx: mpsc::UnboundedSender<Vec<u8>>,

    /// Tunnel id returned by the relay in `TunnelEstablished`. Used as the
    /// HKDF `info` parameter when deriving per-client AES keys so a reused
    /// token across reconnections produces a fresh key per tunnel.
    pub tunnel_id: String,

    /// Keys derived during `AuthChallenge` handling, drained when the
    /// matching `ClientConnected` arrives and a virtual client is spawned.
    /// Keyed by the Zellij-allocated `client_id`.
    pub pending_e2e_keys: Mutex<HashMap<u32, [u8; KEY_LEN]>>,

    /// `is_read_only` flag recorded during `AuthChallenge` handling, drained
    /// by `spawn_virtual_client` when it decides whether to attach as a
    /// regular client or a relay-fan-out watcher.
    pub pending_read_only: Mutex<HashMap<u32, bool>>,

    /// Maps the (hex-encoded) `token_hash` of a validated r/o token to the
    /// client_id of the virtual watcher currently backing its fan-out
    /// group. Populated when r/o auth succeeds; cleared when the relay
    /// signals the group has become dormant via
    /// `ReadOnlyViewerUpdate { count: 0 }`.
    pub token_hash_to_client_id: Mutex<HashMap<String, u32>>,

    pub session_name: String,
    pub connection_table: Arc<Mutex<ConnectionTable>>,
    pub os_api_factory: Arc<dyn ClientOsApiFactory>,
    pub session_manager: Arc<dyn SessionManager>,
    pub config: Arc<Mutex<Config>>,
    pub config_options: Options,
    pub config_file_path: PathBuf,
}

/// One viewer on the far side of the tunnel, plumbed into the local
/// `ConnectionTable` as a virtual web client.
pub struct RelayVirtualClient {
    pub web_client_id: String,
    #[allow(dead_code)] // read once Phase 4 threads the flag through
    pub is_read_only: bool,
    /// Bytes from the tunnel terminal-plane → parse_stdin.
    pub terminal_input_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Text frames from the tunnel control-plane → JSON dispatch.
    pub control_input_tx: mpsc::UnboundedSender<String>,
    /// Fires when the virtual client is being torn down.
    pub shutdown: Option<oneshot::Sender<()>>,
}

