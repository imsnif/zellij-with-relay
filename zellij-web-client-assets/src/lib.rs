//! Bundled Zellij web-client assets.
//!
//! The same static bundle is served by both the local web server in
//! `zellij-client` and the `zellij-relay` binary. Extracted into its own
//! workspace crate so the relay does not have to pull in the full
//! `zellij-client` dependency tree.

use include_dir::{include_dir, Dir};

pub static ASSETS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/assets");

/// The raw HTML shell served by the web client. Consumers are expected to
/// substitute `BASE_URL` and `IS_AUTHENTICATED` placeholders before
/// returning to the browser.
pub static INDEX_HTML: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/index.html"));

/// Result of a successful asset lookup.
pub struct AssetResponse {
    pub content_type: &'static str,
    pub contents: &'static [u8],
}

/// When the `clip_wasm_from_target` feature is enabled, bundle the live
/// `zellij-ansi-clip` build artifact instead of the committed blob so edits
/// to the crate are picked up without a commit. Release builds + CI always
/// use the committed `assets/clip.wasm`.
#[cfg(feature = "clip_wasm_from_target")]
static CLIP_WASM_FROM_TARGET: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/wasm32-unknown-unknown/release/zellij_ansi_clip.wasm"
));

fn clip_wasm_override() -> Option<&'static [u8]> {
    #[cfg(feature = "clip_wasm_from_target")]
    {
        Some(CLIP_WASM_FROM_TARGET)
    }
    #[cfg(not(feature = "clip_wasm_from_target"))]
    {
        None
    }
}

/// Resolve an asset by relative path (e.g. `"index.js"` or `"xterm.css"`).
/// Returns the bytes alongside the resolved MIME type. Returns `None` when
/// the path does not match a bundled asset.
pub fn lookup(path: &str) -> Option<AssetResponse> {
    let trimmed = path.trim_start_matches('/');
    if trimmed == "clip.wasm" {
        if let Some(bytes) = clip_wasm_override() {
            return Some(AssetResponse {
                content_type: "application/wasm",
                contents: bytes,
            });
        }
    }
    let file = ASSETS_DIR.get_file(trimmed)?;
    let ext = file.path().extension().and_then(|ext| ext.to_str());
    Some(AssetResponse {
        content_type: mime_type_for_extension(ext),
        contents: file.contents(),
    })
}

/// Resolve the MIME type for a file extension. Matches the small set of
/// content types served by the local web server; everything else falls
/// back to `text/plain`.
pub fn mime_type_for_extension(ext: Option<&str>) -> &'static str {
    match ext {
        None => "text/plain",
        Some(ext) => match ext {
            "html" => "text/html",
            "css" => "text/css",
            "js" => "application/javascript",
            "wasm" => "application/wasm",
            "png" => "image/png",
            "ico" => "image/x-icon",
            "svg" => "image/svg+xml",
            _ => "text/plain",
        },
    }
}
