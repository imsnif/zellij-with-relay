use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use zellij_relay_protocol::crypto;

use crate::types::{
    CredentialAccess, CredentialKind, CredentialMap, GuestCredential, LinkId, RelayTunnelState,
};

const LINK_SECRET_LEN: usize = 16;
const LINK_ID_LEN: usize = 16;
const SEED_LINK_ID: LinkId = [0u8; 16];

struct TunnelControl {
    public_url: String,
    credentials: CredentialMap,
    states: Vec<Weak<RelayTunnelState>>,
}

fn live_states(states: &[Weak<RelayTunnelState>]) -> Vec<Arc<RelayTunnelState>> {
    states.iter().filter_map(|w| w.upgrade()).collect()
}

static CONTROL: OnceLock<Mutex<Vec<TunnelControl>>> = OnceLock::new();

fn control() -> &'static Mutex<Vec<TunnelControl>> {
    CONTROL.get_or_init(|| Mutex::new(Vec::new()))
}

#[derive(Debug, Clone)]
pub struct GuestLinkInfo {
    pub link_id: Vec<u8>,
    pub label: String,
    pub read_only: bool,
    pub enroll: bool,
    pub url: String,
    pub spent: bool,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct MintedGuestLink {
    pub link_id: Vec<u8>,
    pub url: String,
    pub label: String,
    pub read_only: bool,
    pub spent: bool,
}

pub fn register_tunnel(public_url: String, credentials: CredentialMap) {
    let mut guard = control().lock().unwrap();
    if let Some(existing) = guard
        .iter_mut()
        .find(|t| Arc::ptr_eq(&t.credentials, &credentials))
    {
        existing.public_url = public_url;
        return;
    }
    guard.push(TunnelControl {
        public_url,
        credentials,
        states: Vec::new(),
    });
}

pub fn register_state(state: &Arc<RelayTunnelState>) {
    let credentials = state.credentials.clone();
    let mut guard = control().lock().unwrap();
    if let Some(existing) = guard
        .iter_mut()
        .find(|t| Arc::ptr_eq(&t.credentials, &credentials))
    {
        existing
            .states
            .retain(|w| w.upgrade().map(|s| !Arc::ptr_eq(&s, state)).unwrap_or(false));
        existing.states.push(Arc::downgrade(state));
        log::info!(
            "guest_links::register_state matched a tunnel; now tracking {} live state(s)",
            existing.states.len()
        );
    } else {
        log::warn!(
            "guest_links::register_state found no tunnel matching this state's credentials map ({} tunnels registered) — revoke will not be able to disconnect its clients",
            guard.len()
        );
    }
}

pub fn update_tunnel_url(credentials: &CredentialMap, public_url: String) {
    let mut guard = control().lock().unwrap();
    if let Some(existing) = guard
        .iter_mut()
        .find(|t| Arc::ptr_eq(&t.credentials, credentials))
    {
        existing.public_url = public_url;
    }
}

pub fn clear() {
    control().lock().unwrap().clear();
}

pub fn mint(read_only: bool, label: String, enroll: bool) -> Result<MintedGuestLink, String> {
    let guard = control().lock().unwrap();
    let target = guard
        .first()
        .ok_or_else(|| "no relay tunnel is available to host this link".to_string())?;

    let link_id = fresh_link_id(&target.credentials);
    let secret = hex_encode(&crypto::random_bytes(LINK_SECRET_LEN));
    let access = if read_only {
        CredentialAccess::ReadOnly
    } else {
        CredentialAccess::ReadWrite
    };

    let credential = Arc::new(GuestCredential {
        link_id,
        secret: secret.clone().into_bytes(),
        kind: CredentialKind::Link,
        access,
        label: label.clone(),
        enroll,
        spent: AtomicBool::new(false),
    });
    target
        .credentials
        .lock()
        .unwrap()
        .insert(link_id, credential);

    let url = build_link_url(&target.public_url, &secret, &link_id);
    Ok(MintedGuestLink {
        link_id: link_id.to_vec(),
        url,
        label,
        read_only,
        spent: false,
    })
}

pub fn revoke(link_id: &[u8]) -> bool {
    let Ok(key) = LinkId::try_from(link_id) else {
        return false;
    };
    if key == SEED_LINK_ID {
        return false;
    }
    let guard = control().lock().unwrap();
    log::info!(
        "guest_links::revoke link_id={} across {} registered tunnel(s)",
        hex_encode(&key),
        guard.len()
    );
    for (idx, tunnel) in guard.iter().enumerate() {
        if tunnel.credentials.lock().unwrap().remove(&key).is_some() {
            let states = live_states(&tunnel.states);
            log::info!(
                "guest_links::revoke removed from tunnel[{}]; {} live state(s) of {} tracked",
                idx,
                states.len(),
                tunnel.states.len()
            );
            for (sidx, state) in states.iter().enumerate() {
                let kicked = crate::multiplexer::disconnect_clients_with_link(state, &key);
                let rejected = crate::multiplexer::reject_pending_with_link(state, &key);
                log::info!(
                    "guest_links::revoke tunnel[{}].state[{}] kicked {} connected, {} pending",
                    idx, sidx, kicked, rejected
                );
            }
            return true;
        }
    }
    log::warn!("guest_links::revoke found no tunnel holding link_id={}", hex_encode(&key));
    false
}

pub fn list() -> Vec<GuestLinkInfo> {
    let guard = control().lock().unwrap();
    let mut out = Vec::new();
    for tunnel in guard.iter() {
        let connected: Vec<LinkId> = live_states(&tunnel.states)
            .iter()
            .flat_map(|state| crate::multiplexer::client_link_ids(state))
            .collect();
        let credentials = tunnel.credentials.lock().unwrap();
        for (id, credential) in credentials.iter() {
            if *id == SEED_LINK_ID {
                continue;
            }
            // This surface lists guest/enrollment *links* only. Enrolled-device
            // credentials (injected copies of roster devices) are owned by the
            // device roster and must never appear here.
            if credential.kind == CredentialKind::Device {
                continue;
            }
            let active = connected.contains(id);
            let spent = credential.spent.load(std::sync::atomic::Ordering::Relaxed);
            if credential.kind == CredentialKind::Link && spent && !active {
                continue;
            }
            let secret = String::from_utf8_lossy(&credential.secret).into_owned();
            out.push(GuestLinkInfo {
                link_id: id.to_vec(),
                label: credential.label.clone(),
                read_only: credential.access.is_read_only(),
                enroll: credential.enroll,
                url: build_link_url(&tunnel.public_url, &secret, id),
                spent,
                active,
            });
        }
    }
    out.sort_by(|a, b| a.label.cmp(&b.label).then(a.link_id.cmp(&b.link_id)));
    out
}

fn fresh_link_id(credentials: &CredentialMap) -> LinkId {
    let existing = credentials.lock().unwrap();
    loop {
        let bytes = crypto::random_bytes(LINK_ID_LEN);
        let mut id = SEED_LINK_ID;
        id.copy_from_slice(&bytes);
        if id != SEED_LINK_ID && !existing.contains_key(&id) {
            return id;
        }
    }
}

fn build_link_url(public_url: &str, secret: &str, link_id: &LinkId) -> String {
    let separator = if public_url.contains('#') { '&' } else { '#' };
    format!(
        "{}{}k={}&l={}",
        public_url,
        separator,
        secret,
        hex_encode(link_id)
    )
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    static TEST_GUARD: Mutex<()> = Mutex::new(());

    fn fresh_registry() -> CredentialMap {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[test]
    fn mint_then_list_then_revoke() {
        let _guard = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let credentials = fresh_registry();
        register_tunnel("https://host/r/slug".to_string(), credentials.clone());

        let minted = mint(false, "Alice".to_string(), false).expect("mint");
        assert!(minted.url.contains("#k="));
        assert!(minted.url.contains("&l="));
        assert_eq!(minted.link_id.len(), 16);

        let links = list();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].label, "Alice");
        assert!(!links[0].read_only);

        assert!(revoke(&minted.link_id));
        assert!(list().is_empty());
        assert!(!revoke(&minted.link_id));
        clear();
    }

    #[test]
    fn both_access_links_mint_on_the_same_tunnel() {
        let _guard = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let credentials = fresh_registry();
        register_tunnel("https://host/r/rw".to_string(), credentials.clone());
        assert!(mint(false, "rw".to_string(), false).is_ok());
        assert!(mint(true, "ro".to_string(), false).is_ok());
        clear();
    }

    #[test]
    fn device_credentials_are_not_listed_as_guest_links() {
        let _guard = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let credentials = fresh_registry();
        register_tunnel("https://host/r/slug".to_string(), credentials.clone());

        // A real guest link plus an injected enrolled-device credential
        // (kind = Device) sharing the same tunnel's credential map.
        mint(false, "Alice".to_string(), false).expect("mint");
        let device_id: LinkId = [9u8; 16];
        credentials.lock().unwrap().insert(
            device_id,
            GuestCredential::device(
                device_id,
                b"device-secret".to_vec(),
                CredentialAccess::ReadWrite,
                "my-laptop".to_string(),
            ),
        );

        let links = list();
        assert_eq!(links.len(), 1, "device credential must not appear as a link");
        assert_eq!(links[0].label, "Alice");
        clear();
    }

    #[test]
    fn spent_link_with_connected_client_is_active() {
        let _guard = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let (state, _ctrl_rx) = crate::multiplexer::test_support::make_state();
        let link_id = [11u8; 16];
        state.credentials.lock().unwrap().insert(
            link_id,
            Arc::new(GuestCredential {
                link_id,
                secret: b"483921".to_vec(),
                kind: CredentialKind::Link,
                access: CredentialAccess::ReadWrite,
                label: "Bob".to_string(),
                enroll: false,
                spent: AtomicBool::new(true),
            }),
        );
        register_tunnel("https://host/r/slug".to_string(), state.credentials.clone());
        register_state(&state);
        let _client_rx =
            crate::multiplexer::test_support::insert_connected_client(&state, 1, link_id);

        let links = list();
        assert_eq!(links.len(), 1);
        assert!(links[0].active);
        assert!(links[0].spent);
        assert!(state.credentials.lock().unwrap().contains_key(&link_id));
        clear();
    }

    #[test]
    fn spent_link_is_not_destroyed_by_a_query_before_its_client_registers() {
        let _guard = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let (state, _ctrl_rx) = crate::multiplexer::test_support::make_state();
        let link_id = [13u8; 16];
        state.credentials.lock().unwrap().insert(
            link_id,
            Arc::new(GuestCredential {
                link_id,
                secret: b"483921".to_vec(),
                kind: CredentialKind::Link,
                access: CredentialAccess::ReadWrite,
                label: "Dave".to_string(),
                enroll: false,
                spent: AtomicBool::new(true),
            }),
        );
        register_tunnel("https://host/r/slug".to_string(), state.credentials.clone());
        register_state(&state);

        assert!(list().is_empty());
        assert!(
            state.credentials.lock().unwrap().contains_key(&link_id),
            "a query must not delete a spent credential whose client has not registered yet"
        );

        let _client_rx =
            crate::multiplexer::test_support::insert_connected_client(&state, 1, link_id);
        let links = list();
        assert_eq!(links.len(), 1, "the link must reappear once its guest connects");
        assert!(links[0].active);
        assert!(links[0].spent);
        clear();
    }

    #[test]
    fn spent_link_without_connected_client_is_not_listed() {
        let _guard = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let (state, _ctrl_rx) = crate::multiplexer::test_support::make_state();
        let link_id = [12u8; 16];
        state.credentials.lock().unwrap().insert(
            link_id,
            Arc::new(GuestCredential {
                link_id,
                secret: b"483921".to_vec(),
                kind: CredentialKind::Link,
                access: CredentialAccess::ReadWrite,
                label: "Carol".to_string(),
                enroll: false,
                spent: AtomicBool::new(true),
            }),
        );
        register_tunnel("https://host/r/slug".to_string(), state.credentials.clone());
        register_state(&state);

        assert!(list().is_empty());
        crate::multiplexer::test_support::gc_spent_link(&state, &link_id);
        assert!(!state.credentials.lock().unwrap().contains_key(&link_id));
        clear();
    }

    #[test]
    fn reserved_zero_link_id_cannot_be_revoked_or_listed() {
        let _guard = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let credentials = fresh_registry();
        credentials.lock().unwrap().insert(
            SEED_LINK_ID,
            GuestCredential::device(
                SEED_LINK_ID,
                b"secret".to_vec(),
                CredentialAccess::ReadWrite,
                "reserved".to_string(),
            ),
        );
        register_tunnel("https://host/r/s".to_string(), credentials.clone());
        assert!(list().is_empty());
        assert!(!revoke(&SEED_LINK_ID.to_vec()));
        clear();
    }
}
