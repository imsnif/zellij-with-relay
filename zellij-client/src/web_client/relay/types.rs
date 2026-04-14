use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use zellij_utils::data::ClientId;

/// Handle to a running relay tunnel. Phase 1: holds metadata + a shutdown
/// signal that, when fired, causes the websocket tasks to drop their sockets.
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
    inner: Mutex<HashMap<ClientId, RelayTunnelHandle>>,
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
