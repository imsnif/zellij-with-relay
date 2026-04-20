//! In-memory registry of active tunnels. Keyed by slug.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::ws::Message as WsMessage;
use tokio::sync::{mpsc, oneshot, Notify};
use uuid::Uuid;

/// Result of an AuthChallenge round-trip through the control tunnel.
#[derive(Debug, Clone)]
pub struct AuthResponseResult {
    pub accepted: bool,
    pub client_id: u32,
    pub is_read_only: bool,
    pub session_token_hash: String,
    /// Mirrors the `e2e_encrypted` flag on the Zellij-side `AuthResponse`.
    /// The relay forwards this to the viewer via `/session` so the JS can
    /// cross-check it against the `expectedE2e` challenge-page claim.
    pub e2e_encrypted: bool,
}

/// Relay-side per-viewer bookkeeping. Keyed by the Zellij-allocated
/// `client_id` in `TunnelEntry::viewers`. Each sink is populated
/// independently when the respective WS upgrades; the record itself is
/// inserted when the first WS (either control or terminal) arrives.
#[derive(Default)]
pub struct ViewerHandle {
    pub control_sink_tx: Option<mpsc::UnboundedSender<WsMessage>>,
    pub terminal_sink_tx: Option<mpsc::UnboundedSender<WsMessage>>,
    pub disconnect_terminal: Option<oneshot::Sender<()>>,
    pub disconnect_control: Option<oneshot::Sender<()>>,
    pub is_read_only: bool,
}

/// Relay-side session cookie state: maps an opaque session id stored in the
/// `relay_session` cookie to the `client_id` allocated by Zellij.
#[derive(Debug, Clone)]
pub struct ViewerSession {
    pub client_id: u32,
    pub token_hash: String,
    pub is_read_only: bool,
    /// Mirrors the Zellij-side `AuthResponse.e2e_encrypted` flag. Returned
    /// to the viewer on `/session` so the JS can cross-check it against the
    /// challenge-page claim before transmitting any STDIN.
    pub e2e_encrypted: bool,
}

/// Metadata and wiring the relay keeps for an active tunnel.
pub struct TunnelEntry {
    pub tunnel_id: Uuid,
    pub slug: String,
    pub public_url: String,
    pub session_name: String,
    pub zellij_version: String,
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
    /// Outstanding AuthChallenge round-trips awaiting `AuthResponse`.
    pub pending_auths: Mutex<HashMap<Vec<u8>, oneshot::Sender<AuthResponseResult>>>,
    /// Active viewers, keyed by the `client_id` allocated by Zellij.
    pub viewers: Mutex<HashMap<u32, ViewerHandle>>,
    /// Sessions indexed by the random session cookie id.
    pub sessions: Mutex<HashMap<Uuid, ViewerSession>>,
}

impl std::fmt::Debug for TunnelEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelEntry")
            .field("tunnel_id", &self.tunnel_id)
            .field("slug", &self.slug)
            .field("public_url", &self.public_url)
            .field("session_name", &self.session_name)
            .field("zellij_version", &self.zellij_version)
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

    fn make_entry(slug: &str) -> Arc<TunnelEntry> {
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        Arc::new(TunnelEntry {
            tunnel_id: Uuid::new_v4(),
            slug: slug.into(),
            public_url: format!("http://localhost/r/{}", slug),
            session_name: "test-session".into(),
            zellij_version: "0.45.0".into(),
            created_at: Instant::now(),
            terminal_linked: Arc::new(Notify::new()),
            terminal_linked_flag: Arc::new(Mutex::new(false)),
            control_tx,
            terminal_tx: Mutex::new(None),
            pending_auths: Mutex::new(HashMap::new()),
            viewers: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
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
}
