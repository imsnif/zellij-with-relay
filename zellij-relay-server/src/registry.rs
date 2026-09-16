//! In-memory registry of active tunnels. Keyed by slug.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::ws::Message as WsMessage;
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, Notify};
use uuid::Uuid;

/// A fresh, random 32-byte terminal binding secret, lowercase hex-encoded
/// (64 chars) — the shape both the client and `tunnel_terminal.rs` validate.
/// Scope: per tunnel lifetime, single-tunnel (see decision #1); dies with
/// the registry entry. Deliberately not the `RelayTunnelAuthTokenHash`
/// namespace — the terminal path must not import the token module.
pub fn generate_terminal_binding_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn hash_terminal_binding_secret(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{:02x}", byte))
        .collect()
}

/// The sharer's reply to a `PakeChallenge` round-trip.
#[derive(Debug, Clone)]
pub struct PakeResponseResult {
    pub accepted: bool,
    pub client_id: u32,
    pub sharer_msg: Vec<u8>,
    pub sharer_confirm: Vec<u8>,
}

/// A viewer handshake spanning the `/command/login` and `/session` POSTs.
/// Stored under the viewer's session-cookie id between the two PAKE
/// round-trips so the second POST can be correlated with the sharer.
#[derive(Debug, Clone)]
pub struct PendingHandshake {
    pub request_id: Vec<u8>,
    pub client_id: u32,
}

/// Relay-side per-viewer bookkeeping. Keyed by a per-viewer `Uuid` in
/// `TunnelEntry::viewers`.
#[derive(Default)]
pub struct ViewerHandle {
    pub control_sink_tx: Option<mpsc::UnboundedSender<WsMessage>>,
    pub terminal_sink_tx: Option<mpsc::UnboundedSender<WsMessage>>,
    pub terminal_backlog: Vec<WsMessage>,
    pub disconnect_terminal: Option<oneshot::Sender<()>>,
    pub disconnect_control: Option<oneshot::Sender<()>>,
    pub is_read_only: bool,
}

pub const MAX_TERMINAL_BACKLOG_FRAMES: usize = 1024;

/// Per-viewer session, keyed by the random `relay_session` cookie id. Every
/// viewer (read-only or read-write) is 1:1 with a sharer-allocated
/// `client_id`; the per-viewer PAKE key means there is no fan-out.
#[derive(Debug, Clone)]
pub struct ViewerSession {
    pub viewer_id: Uuid,
    pub client_id: u32,
    pub is_read_only: bool,
}

/// Metadata and wiring the relay keeps for an active tunnel.
pub struct TunnelEntry {
    pub tunnel_id: Uuid,
    pub slug: String,
    pub public_url: String,
    pub session_name: String,
    pub zellij_version: String,
    /// Whether the whole tunnel/slug is read-only. The relay drops viewer
    /// stdin frames on a read-only tunnel (input gating). Role is a per-slug
    /// property declared by the sharer in `TunnelAuth`.
    pub read_only: bool,
    pub created_at: Instant,
    /// Fired by the control handler when the terminal tunnel links up, so the
    /// terminal handler can unblock the waiting control-side logic.
    pub terminal_linked: Arc<Notify>,
    pub terminal_linked_flag: Arc<Mutex<bool>>,
    /// Encoded `ControlMessage` bytes → control-tunnel writer task.
    pub control_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Encoded `TerminalMessage` bytes → terminal-tunnel writer task.
    /// `None` until the terminal WS is linked.
    pub terminal_tx: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    /// Outstanding `PakeChallenge` round-trips awaiting `PakeResponse`.
    pub pending_pake_responses: Mutex<HashMap<Vec<u8>, oneshot::Sender<PakeResponseResult>>>,
    /// Outstanding `PakeConfirm` round-trips awaiting `PakeResult`.
    pub pending_pake_results: Mutex<HashMap<Vec<u8>, oneshot::Sender<bool>>>,
    /// Handshakes paused between a viewer's two auth POSTs, keyed by the
    /// viewer's session-cookie id.
    pub pending_handshakes: Mutex<HashMap<Uuid, PendingHandshake>>,
    /// Active viewers, keyed by per-viewer `Uuid`.
    pub viewers: Mutex<HashMap<Uuid, ViewerHandle>>,
    /// Sessions indexed by the random session cookie id.
    pub sessions: Mutex<HashMap<Uuid, ViewerSession>>,
    /// Maps `client_id` → its single viewer's `Uuid` (1:1; no fan-out).
    pub client_id_to_viewer: Mutex<HashMap<u32, Uuid>>,
    /// Set from `TunnelAuthBackend::authorize`'s `AuthDecision` on hosted
    /// (`Online`) tunnels; `None` under standalone `LocalSqlite` auth, which
    /// has no account to attribute events to.
    pub user_id: Option<String>,
    pub credential_id: Option<String>,
    /// SHA-256 hex of the relay-generated terminal binding secret handed to
    /// the client in `TunnelEstablished`. The account credential is never
    /// stored here or re-checked for the terminal socket (see decision #1).
    pub terminal_binding_secret_hash: String,
}

impl TunnelEntry {
    /// Resolve a sharer-allocated `client_id` to its single viewer `Uuid`.
    pub fn viewer_for_client_id(&self, client_id: u32) -> Option<Uuid> {
        self.client_id_to_viewer
            .lock()
            .unwrap()
            .get(&client_id)
            .copied()
    }
}

impl std::fmt::Debug for TunnelEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelEntry")
            .field("tunnel_id", &self.tunnel_id)
            .field("slug", &self.slug)
            .field("public_url", &self.public_url)
            .field("session_name", &self.session_name)
            .field("zellij_version", &self.zellij_version)
            .field("read_only", &self.read_only)
            .field("created_at", &self.created_at)
            .finish()
    }
}

#[derive(Debug, Default, Clone)]
pub struct Registry {
    inner: Arc<Mutex<HashMap<String, Arc<TunnelEntry>>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, entry: Arc<TunnelEntry>) {
        let mut g = self.inner.lock().unwrap();
        g.insert(entry.slug.clone(), entry);
    }

    pub fn get(&self, slug: &str) -> Option<Arc<TunnelEntry>> {
        let g = self.inner.lock().unwrap();
        g.get(slug).cloned()
    }

    pub fn remove(&self, slug: &str) -> Option<Arc<TunnelEntry>> {
        let mut g = self.inner.lock().unwrap();
        g.remove(slug)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn make_entry(slug: &str) -> Arc<TunnelEntry> {
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        Arc::new(TunnelEntry {
            tunnel_id: Uuid::new_v4(),
            slug: slug.into(),
            public_url: format!("http://localhost/r/{}", slug),
            session_name: "test-session".into(),
            zellij_version: "0.45.0".into(),
            read_only: false,
            created_at: Instant::now(),
            terminal_linked: Arc::new(Notify::new()),
            terminal_linked_flag: Arc::new(Mutex::new(false)),
            control_tx,
            terminal_tx: Mutex::new(None),
            pending_pake_responses: Mutex::new(HashMap::new()),
            pending_pake_results: Mutex::new(HashMap::new()),
            pending_handshakes: Mutex::new(HashMap::new()),
            viewers: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            client_id_to_viewer: Mutex::new(HashMap::new()),
            user_id: None,
            credential_id: None,
            terminal_binding_secret_hash: String::new(),
        })
    }

    #[test]
    fn insert_lookup_remove() {
        let registry = Registry::new();
        let entry = make_entry("abc");
        let tunnel_id = entry.tunnel_id;
        registry.insert(entry);
        let fetched = registry.get("abc").expect("entry present");
        assert_eq!(fetched.tunnel_id, tunnel_id);
        assert_eq!(fetched.slug, "abc");
        registry.remove("abc");
        assert!(registry.get("abc").is_none());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn duplicate_slug_insert_overwrites() {
        let registry = Registry::new();
        let first = make_entry("dup");
        let first_id = first.tunnel_id;
        registry.insert(first);

        let second = make_entry("dup");
        let second_id = second.tunnel_id;
        assert_ne!(first_id, second_id);
        registry.insert(second);

        let fetched = registry.get("dup").expect("entry present");
        assert_eq!(fetched.tunnel_id, second_id);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn generated_binding_secret_is_64_lowercase_hex() {
        let secret = generate_terminal_binding_secret();
        assert_eq!(secret.len(), 64);
        assert!(secret.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    }

    #[test]
    fn binding_secret_hash_is_deterministic_and_distinguishes_input() {
        let a = hash_terminal_binding_secret("abc");
        let b = hash_terminal_binding_secret("abc");
        let c = hash_terminal_binding_secret("abd");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn viewer_for_client_id_lookup() {
        let entry = make_entry("lookup");
        let viewer = Uuid::new_v4();
        entry.client_id_to_viewer.lock().unwrap().insert(9, viewer);
        assert_eq!(entry.viewer_for_client_id(9), Some(viewer));
        assert_eq!(entry.viewer_for_client_id(999), None);
    }
}
