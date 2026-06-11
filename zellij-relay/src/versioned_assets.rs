//! Versioned web-client asset lookup.
//!
//! The relay serves web-client assets to browser viewers keyed by the
//! tunnel's declared `zellij_version`. Two sources are consulted:
//!
//! * The **embedded bundle** (`zellij_web_client_assets`) — the one
//!   currently in development at this workspace's HEAD. Served when the
//!   tunnel's version matches the relay's own `CARGO_PKG_VERSION`.
//!
//! * A **disk-committed snapshot** under `zellij-relay/assets/<version>/`
//!   captured at release time by `xtask pipelines::publish` — served for
//!   any previously-released version still carried in-tree.
//!
//! Anything else produces a 404 with an upgrade hint.

use include_dir::{include_dir, Dir};
use zellij_web_client_assets::{lookup as lookup_embedded, mime_type_for_extension, AssetResponse};

/// On-disk historical bundles, bundled into the relay binary at compile
/// time. Each top-level entry is a directory named after a Zellij
/// version string (matching `TunnelAuth.zellij_version`). Files within
/// are served verbatim.
static VERSIONED_ASSETS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/assets");

/// The relay's own crate version. A tunnel announcing exactly this
/// version is served from the live embedded bundle in
/// `zellij_web_client_assets`.
pub const RELAY_VERSION: &str = env!("CARGO_PKG_VERSION");

/// True when `version` is supported — either by being the live relay
/// version or by having a committed on-disk snapshot.
pub fn has_version(version: &str) -> bool {
    version == RELAY_VERSION || VERSIONED_ASSETS_DIR.get_dir(version).is_some()
}

/// Resolve an asset for a specific Zellij version. Returns `None` when
/// the version is unknown or the file does not exist within its bundle.
/// Callers should distinguish the two cases via `has_version` when the
/// response wording matters.
pub fn lookup(version: &str, path: &str) -> Option<AssetResponse> {
    if version == RELAY_VERSION {
        return lookup_embedded(path);
    }
    let trimmed = path.trim_start_matches('/');
    let joined = format!("{}/{}", version, trimmed);
    let file = VERSIONED_ASSETS_DIR.get_file(&joined)?;
    let ext = file.path().extension().and_then(|e| e.to_str());
    Some(AssetResponse {
        content_type: mime_type_for_extension(ext),
        contents: file.contents(),
    })
}
