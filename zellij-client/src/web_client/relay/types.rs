use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};

use zellij_relay_protocol::crypto::KEY_LEN;
use zellij_utils::{
    data::ClientId,
    input::{config::Config, options::Options},
};

use crate::web_client::types::{ClientOsApiFactory, ConnectionTable, SessionManager};

/// Phase 6 structured status surfaced to the sharer through the share
/// plugin. Encoded into `ModeInfo.remote_share_url: Option<String>` via
/// sentinel strings (see `RelayTunnelStatus::to_mode_info_sentinel`) so
/// no protobuf churn is required to reach the plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayTunnelStatus {
    /// Tunnel is live; the carried URL matches what the relay handed back.
    Connected(String),
    /// Last known URL was `url`, reconnect is in progress; `attempt`
    /// counts consecutive reconnect cycles (1-based).
    Reconnecting {
        last_known_url: Option<String>,
        attempt: u32,
    },
    /// Reconnect budget exhausted or non-retryable error; plugin surface
    /// renders the carried diagnostic verbatim.
    Failed(String),
}

impl RelayTunnelStatus {
    /// Sentinel prefix identifying a `Reconnecting` status in the wire
    /// `Option<String>`. Chosen to be unambiguously distinct from any
    /// valid URL (no `://`, leading underscores).
    pub const RECONNECTING_SENTINEL: &'static str = "__RELAY_RECONNECTING__:";
    /// Sentinel prefix for `Failed` status.
    pub const FAILED_SENTINEL: &'static str = "__RELAY_FAILED__:";

    /// Encode to the string form shipped through `ModeInfo.remote_share_url`.
    /// `None` means "no tunnel active" — caller fast-paths to `None` there.
    pub fn to_mode_info_sentinel(&self) -> Option<String> {
        match self {
            RelayTunnelStatus::Connected(url) => Some(url.clone()),
            RelayTunnelStatus::Reconnecting { attempt, .. } => Some(format!(
                "{}{}",
                Self::RECONNECTING_SENTINEL,
                attempt
            )),
            RelayTunnelStatus::Failed(msg) => {
                Some(format!("{}{}", Self::FAILED_SENTINEL, msg))
            },
        }
    }
}

/// Handle to a running relay tunnel. Holds the shutdown signal whose firing
/// causes the multiplexer tasks to exit and the sockets to close.
pub struct RelayTunnelHandle {
    #[allow(dead_code)]
    pub public_url: String,
    #[allow(dead_code)]
    pub slug: String,
    #[allow(dead_code)]
    pub tunnel_id: String,
    /// Fires the initial multiplexer shutdown. Only consumed once by
    /// `run_supervisor`; reconnect iterations rely on `stop_requested`
    /// + a per-iteration oneshot instead.
    pub shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
    /// Cooperative stop flag checked by the supervisor around every
    /// reconnect boundary. `stop_relay_tunnel` flips it to `true`.
    pub stop_requested: Arc<AtomicBool>,
    /// Per-iteration shutdown signaller: the supervisor stores the
    /// current `oneshot::Sender<()>` here each time it kicks off a new
    /// `run_multiplexer`. `stop_relay_tunnel` drains it to forcibly
    /// break a live reconnected run.
    pub current_iteration_shutdown: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    /// Phase 6: latest known status for this tunnel, shared with the
    /// supervisor task. The server-side poll reads this and translates
    /// transitions into `RemoteShareUrlChange` screen instructions.
    pub status: Arc<Mutex<RelayTunnelStatus>>,
    /// Phase 6 Session C: shared handle to the control-tunnel writer of
    /// the current (first or reconnected) iteration. Refreshed by the
    /// supervisor on every reconnect. Consumers send encoded
    /// `ControlMessage` bytes into this to reach the relay — currently
    /// the `RevokeRelayToken` IPC. `None` between reconnect boundaries.
    pub control_tx: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>>,
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

    /// Phase 6: read-only access to a still-registered tunnel handle.
    /// Returns `None` if no tunnel exists for `client_id`. Kept narrow
    /// on purpose — callers that want to *remove* should use `remove`.
    pub async fn with_handle<R>(
        &self,
        client_id: ClientId,
        f: impl FnOnce(&RelayTunnelHandle) -> R,
    ) -> Option<R> {
        let guard = self.inner.lock().await;
        guard.get(&client_id).map(f)
    }

    /// Phase 6 Session C: run a closure against every registered handle.
    /// Used by `broadcast_revoke_token` to fan a control-plane frame out
    /// to every live tunnel.
    pub async fn for_each_handle<F: FnMut(&RelayTunnelHandle)>(&self, mut f: F) {
        let guard = self.inner.lock().await;
        for handle in guard.values() {
            f(handle);
        }
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

