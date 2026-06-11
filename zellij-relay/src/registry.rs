//! In-memory registry of active tunnels. Keyed by slug.

use std::collections::{HashMap, HashSet};
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

/// How the relay dispatches tunnel frames to a viewer.
///
/// - `Rw(client_id)` — the Zellij-allocated `client_id` is 1:1 with the
///   viewer. Frames arriving on the tunnel for this id are forwarded to
///   exactly one viewer.
/// - `Ro(token_hash)` — the viewer is part of a fan-out group. Frames
///   arriving on the tunnel for the virtual watcher's `client_id` are
///   broadcast to every viewer whose `token_hash` matches.
#[derive(Debug, Clone)]
pub enum ViewerRouting {
    Rw(u32),
    Ro(String),
}

/// Relay-side per-viewer bookkeeping. Keyed by a per-viewer `Uuid` in
/// `TunnelEntry::viewers` so r/o fan-out groups (which share a single
/// Zellij-allocated `client_id`) can still be tracked individually.
#[derive(Default)]
pub struct ViewerHandle {
    pub control_sink_tx: Option<mpsc::UnboundedSender<WsMessage>>,
    pub terminal_sink_tx: Option<mpsc::UnboundedSender<WsMessage>>,
    pub disconnect_terminal: Option<oneshot::Sender<()>>,
    pub disconnect_control: Option<oneshot::Sender<()>>,
    pub is_read_only: bool,
}

/// Relay-side session cookie state: maps an opaque session id stored in the
/// `relay_session` cookie to the viewer's routing + identity.
#[derive(Debug, Clone)]
pub struct ViewerSession {
    /// Per-viewer identity. For r/w, this is also the viewers-map key; for
    /// r/o, every viewer in a fan-out group has a distinct id.
    pub viewer_id: Uuid,
    /// How to dispatch tunnel frames for this viewer.
    pub routing: ViewerRouting,
    pub token_hash: String,
    pub is_read_only: bool,
    /// Mirrors the Zellij-side `AuthResponse.e2e_encrypted` flag. Returned
    /// to the viewer on `/session` so the JS can cross-check it against the
    /// challenge-page claim before transmitting any STDIN.
    pub e2e_encrypted: bool,
}

/// A cached r/o token's fan-out group. Populated on the first accepted
/// r/o `AuthResponse` for a `token_hash`; lives for the life of the tunnel.
///
/// `active_client_id == Some(_)` means a Zellij-side virtual watcher is
/// live and the relay can forward new viewers into the group with no
/// round-trip. `None` means the group went dormant (last viewer left) — a
/// fresh `AuthChallenge` must allocate a new virtual watcher, but the
/// cached validation proves the token is still an r/o token so
/// re-challenging is a formality.
#[derive(Debug, Clone)]
pub struct RoGroup {
    pub validated: bool,
    pub active_client_id: Option<u32>,
    pub e2e_encrypted: bool,
    pub viewer_ids: HashSet<Uuid>,
    /// Most recent session viewport size pushed by the Zellij tunnel peer.
    /// `None` until the first `SessionSize` arrives; `/session` stamps `0`
    /// sentinels so the browser can lazy-update on the first
    /// `SessionSizeChanged` JSON push.
    pub session_size: Option<(u32, u32)>,
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
    /// Active viewers, keyed by per-viewer `Uuid`. Each r/w viewer has a
    /// single entry; each r/o viewer in a fan-out group has its own entry.
    pub viewers: Mutex<HashMap<Uuid, ViewerHandle>>,
    /// Sessions indexed by the random session cookie id.
    pub sessions: Mutex<HashMap<Uuid, ViewerSession>>,
    /// Maps r/w `client_id` → its single viewer's `Uuid`. Used by the
    /// tunnel reader to route inbound frames to the correct viewer.
    pub client_id_to_viewer: Mutex<HashMap<u32, Uuid>>,
    /// Cached r/o token fan-out groups, keyed by hex `token_hash`.
    pub ro_groups: Mutex<HashMap<String, RoGroup>>,
    /// Maps the active virtual-watcher `client_id` back to its
    /// `token_hash`. Present iff the group's `active_client_id` is `Some`.
    pub client_id_to_token_hash: Mutex<HashMap<u32, String>>,
}

impl TunnelEntry {
    /// Helper used by the tunnel readers: resolve a Zellij-allocated
    /// `client_id` into the list of viewer `Uuid`s that should receive the
    /// frame. For r/w this yields a single-element list; for r/o it yields
    /// every viewer in the fan-out group.
    pub fn viewers_for_client_id(&self, client_id: u32) -> Vec<Uuid> {
        if let Some(token_hash) = self
            .client_id_to_token_hash
            .lock()
            .unwrap()
            .get(&client_id)
            .cloned()
        {
            if let Some(group) = self.ro_groups.lock().unwrap().get(&token_hash) {
                return group.viewer_ids.iter().copied().collect();
            }
            return Vec::new();
        }
        self.client_id_to_viewer
            .lock()
            .unwrap()
            .get(&client_id)
            .copied()
            .map(|id| vec![id])
            .unwrap_or_default()
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
            client_id_to_viewer: Mutex::new(HashMap::new()),
            ro_groups: Mutex::new(HashMap::new()),
            client_id_to_token_hash: Mutex::new(HashMap::new()),
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
    fn viewers_for_client_id_rw_and_ro() {
        let entry = make_entry("lookup");
        let ro_viewer_a = Uuid::new_v4();
        let ro_viewer_b = Uuid::new_v4();
        let rw_viewer = Uuid::new_v4();
        let ro_group_client_id = 7u32;
        let rw_client_id = 9u32;
        let token_hash = "deadbeef".to_string();

        entry.client_id_to_token_hash.lock().unwrap().insert(
            ro_group_client_id,
            token_hash.clone(),
        );
        entry.ro_groups.lock().unwrap().insert(
            token_hash.clone(),
            RoGroup {
                validated: true,
                active_client_id: Some(ro_group_client_id),
                e2e_encrypted: true,
                viewer_ids: [ro_viewer_a, ro_viewer_b].into_iter().collect(),
                session_size: None,
            },
        );
        entry
            .client_id_to_viewer
            .lock()
            .unwrap()
            .insert(rw_client_id, rw_viewer);

        let rw = entry.viewers_for_client_id(rw_client_id);
        assert_eq!(rw, vec![rw_viewer]);

        let mut ro = entry.viewers_for_client_id(ro_group_client_id);
        ro.sort();
        let mut expected = vec![ro_viewer_a, ro_viewer_b];
        expected.sort();
        assert_eq!(ro, expected);

        assert_eq!(entry.viewers_for_client_id(999).len(), 0);
    }
}
