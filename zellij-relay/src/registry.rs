//! In-memory registry of active tunnels. Keyed by slug.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::Notify;
use uuid::Uuid;

/// Metadata the relay remembers for an active tunnel. Phase 1 does not yet
/// hold the control/terminal WebSocket sinks for forwarding — the control
/// handler owns its socket, and Phase 2 will restructure this once frames
/// need to be routed to viewers.
#[derive(Debug)]
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

    pub fn remove(&self, slug: &str) {
        let mut g = self.inner.lock().unwrap();
        g.remove(slug);
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
        Arc::new(TunnelEntry {
            tunnel_id: Uuid::new_v4(),
            slug: slug.into(),
            public_url: format!("http://localhost/r/{}", slug),
            session_name: "test-session".into(),
            zellij_version: "0.45.0".into(),
            created_at: Instant::now(),
            terminal_linked: Arc::new(Notify::new()),
            terminal_linked_flag: Arc::new(Mutex::new(false)),
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
