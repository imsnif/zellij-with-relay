//! Subcommands for building.
//!
//! Currently has the following functions:
//!
//! - [`build`]: Builds general cargo projects (i.e. zellij components) with `cargo build`
use crate::{flags, metadata, WorkspaceMember};
use anyhow::Context;
use std::path::{Path, PathBuf};
use xshell::{cmd, Shell};

/// Build members of the zellij workspace.
///
/// Build behavior is controlled by the [`flags`](flags::Build). Calls some variation of `cargo
/// build` under the hood.
pub fn build(sh: &Shell, flags: flags::Build) -> anyhow::Result<()> {
    let _pd = sh.push_dir(crate::project_root());

    let cargo = crate::cargo()?;
    if flags.no_plugins && flags.plugins_only {
        eprintln!("Cannot use both '--no-plugins' and '--plugins-only'");
        std::process::exit(1);
    }

    if flags.wasm_clip {
        // Short-circuit: `cargo x build --wasm-clip` builds the browser wasm
        // blobs (the ansi-clip state machine and the relay E2E crypto shim)
        // AND stages them to the committed assets. This is the path used by
        // the release pipeline (`xtask pipelines::publish`) and by manual
        // invocations that want to refresh the committed blobs.
        build_wasm_clip(sh, flags.release, /* stage_to_assets */ true)?;
        return build_wasm_relay_crypto(sh, flags.release);
    }

    if let Some(out_dir) = flags.app_origin.clone() {
        if flags.release {
            return stage_app_origin(
                sh,
                &out_dir,
                flags.app_host.as_deref(),
                flags.relay_origin.as_deref(),
            );
        }
        build_wasm_clip(sh, true, /* stage_to_assets */ false)?;
        return stage_app_origin_dev(
            sh,
            &out_dir,
            flags.app_host.as_deref().unwrap_or(DEFAULT_APP_HOST),
            flags.relay_origin.as_deref(),
        );
    }

    // zellij-utils requires protobuf definition files to be present. Usually these are
    // auto-generated with `build.rs`-files, but this is currently broken for us.
    // See [this PR][1] for details.
    //
    // [1]: https://github.com/zellij-org/zellij/pull/2711#issuecomment-1695015818
    run_proto_codegen(sh, false);

    // Build all plugins in a single invocation so Cargo can unify transitive dependency
    // features across all of them and compile shared crates (e.g. zellij-utils) only once.
    let build_plugins =
        !flags.no_plugins && (flags.release || plugins_force() || plugin_sources_changed());
    if build_plugins {
        let plugin_members: Vec<&WorkspaceMember> = crate::workspace_members()
            .iter()
            .filter(|m| m.build && m.crate_name.contains("plugins"))
            .collect();

        if !plugin_members.is_empty() {
            eprintln!();
            let msg = ">> Building plugins";
            crate::status(msg);
            eprintln!("{}", msg);

            if flags.release {
                build_plugins_release_into_assets(sh, &plugin_members)?;
            } else {
                let mut base_cmd = cmd!(sh, "{cargo} build --target wasm32-wasip1");
                for member in &plugin_members {
                    base_cmd = base_cmd.args(["-p", plugin_name_of(member)?]);
                }
                base_cmd.run().context("failed to build plugins")?;
                write_plugin_stamp(sh);
            }
        }
    } else if !flags.no_plugins {
        let msg = ">> Plugins unchanged since last build, skipping (set ZELLIJ_FORCE_PLUGINS=1 to rebuild)";
        crate::status(msg);
        eprintln!("{}", msg);
    }

    // Build the ansi-clip wasm blob alongside the plugins when web is enabled.
    // The output lives at `target/wasm32-unknown-unknown/release/zellij_ansi_clip.wasm`,
    // which is where `zellij-web-client-assets/clip_wasm_from_target` (enabled by the
    // root crate's default feature set) reads from at compile time. Release pipelines
    // (`xtask pipelines::publish`) refresh the committed blob via the `wasm_clip: true`
    // early-return path above.
    if !flags.no_plugins && !flags.no_web {
        // Always release mode - `clip_wasm_from_target` hard-codes the `release/`
        // path in its include_bytes!.
        build_wasm_clip(sh, /* release */ true, /* stage_to_assets */ false)?;
    }

    if !flags.no_web {
        crate::assets::assets(sh, crate::flags::Assets { check: false })?;
    }

    // Build non-plugin crates (native target).
    if !flags.plugins_only {
        for WorkspaceMember { crate_name, .. } in crate::workspace_members()
            .iter()
            .filter(|member| member.build && !member.crate_name.contains("plugins"))
        {
            let err_context = || format!("failed to build '{crate_name}'");

            let _pd = sh.push_dir(Path::new(crate_name));
            println!();
            let msg = format!(">> Building '{crate_name}'");
            crate::status(&msg);
            println!("{}", msg);

            let mut base_cmd = cmd!(sh, "{cargo} build");
            if flags.release {
                base_cmd = base_cmd.arg("--release");
            } else {
                base_cmd = base_cmd.args(["--profile", "dev-opt"]);
            }
            if flags.no_web {
                // Check if this crate has web features that need modification
                match metadata::get_no_web_features(sh, crate_name)
                    .context("Failed to check web features")?
                {
                    Some(features) => {
                        base_cmd = base_cmd.arg("--no-default-features");
                        if !features.is_empty() {
                            base_cmd = base_cmd.arg("--features");
                            base_cmd = base_cmd.arg(features);
                        }
                    },
                    None => {
                        // Crate doesn't have web features, build normally
                    },
                }
            }
            base_cmd = base_cmd.args(&flags.args);
            base_cmd.run().with_context(err_context)?;
        }
    }

    Ok(())
}

fn plugins_force() -> bool {
    std::env::var_os("ZELLIJ_FORCE_PLUGINS").is_some()
}

fn plugin_asset_path(plugin_name: &str) -> PathBuf {
    crate::asset_dir()
        .join("plugins")
        .join(plugin_name)
        .with_extension("wasm")
}

fn newest_plugin_source_time() -> Option<std::time::SystemTime> {
    let root = crate::project_root();
    ["default-plugins", "zellij-tile", "zellij-tile-utils"]
        .iter()
        .filter_map(|dir| newest_file_time(&root.join(dir)))
        .max()
}

fn newest_file_time(dir: &Path) -> Option<std::time::SystemTime> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut newest: Option<std::time::SystemTime> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            if path
                .file_name()
                .map(|name| name == "target")
                .unwrap_or(false)
            {
                continue;
            }
            if let Some(child) = newest_file_time(&path) {
                newest = Some(newest.map_or(child, |current| current.max(child)));
            }
        } else if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
            newest = Some(newest.map_or(modified, |current| current.max(modified)));
        }
    }
    newest
}

pub fn ensure_plugin_assets(sh: &Shell) -> anyhow::Result<()> {
    let plugin_members: Vec<&WorkspaceMember> = crate::workspace_members()
        .iter()
        .filter(|m| m.build && m.crate_name.contains("plugins"))
        .collect();
    if plugin_members.is_empty() {
        return Ok(());
    }

    let newest_source = newest_plugin_source_time();
    let stale = plugins_force()
        || plugin_members.iter().any(|member| {
            let plugin_name = match member.crate_name.rsplit_once('/') {
                Some((_, name)) => name,
                None => return true,
            };
            let asset_time = std::fs::metadata(plugin_asset_path(plugin_name))
                .and_then(|m| m.modified())
                .ok();
            match (asset_time, newest_source) {
                (Some(asset_time), Some(newest_source)) => asset_time < newest_source,
                _ => true,
            }
        });

    if !stale {
        let msg = ">> Plugin assets up to date, skipping plugin build";
        crate::status(msg);
        eprintln!("{}", msg);
        return Ok(());
    }

    let msg = ">> Building plugin assets (release)";
    crate::status(msg);
    eprintln!("{}", msg);

    build_plugins_release_into_assets(sh, &plugin_members)
}

fn build_plugins_release_into_assets(
    sh: &Shell,
    plugin_members: &[&WorkspaceMember],
) -> anyhow::Result<()> {
    let cargo = crate::cargo()?;
    let mut base_cmd = cmd!(sh, "{cargo} build --target wasm32-wasip1 --release");
    for member in plugin_members {
        let plugin_name = plugin_name_of(member)?;
        base_cmd = base_cmd.args(["-p", plugin_name]);
    }
    base_cmd.run().context("failed to build plugin assets")?;

    for member in plugin_members {
        move_plugin_to_assets(sh, plugin_name_of(member)?)?;
    }
    Ok(())
}

fn plugin_name_of(member: &WorkspaceMember) -> anyhow::Result<&'static str> {
    Ok(member
        .crate_name
        .rsplit_once('/')
        .context("Cannot determine plugin name from crate path")?
        .1)
}

fn plugin_stamp_path() -> PathBuf {
    crate::target_dir().join(".xtask-plugins-stamp")
}

fn write_plugin_stamp(sh: &Shell) {
    let _ = sh.write_file(plugin_stamp_path(), b"");
}

fn plugin_sources_changed() -> bool {
    let stamp_time = match std::fs::metadata(plugin_stamp_path()).and_then(|m| m.modified()) {
        Ok(stamp_time) => stamp_time,
        Err(_) => return true,
    };
    match newest_plugin_source_time() {
        Some(newest_source) => newest_source > stamp_time,
        None => true,
    }
}

pub fn proto(sh: &Shell) -> anyhow::Result<()> {
    let msg = ">> Generating protobuffer code";
    crate::status(msg);
    println!("{}", msg);

    run_proto_codegen(sh, true);
    Ok(())
}

fn run_proto_codegen(sh: &Shell, force: bool) {
    // (base_crate_dir, out_subdir, src_subdir, include_file)
    let specs: &[(&str, &str, &str, &str)] = &[
        (
            "zellij-utils",
            "assets/prost",
            "src/plugin_api",
            "generated_plugin_api.rs",
        ),
        (
            "zellij-utils",
            "assets/prost_ipc",
            "src/client_server_contract",
            "generated_client_server_api.rs",
        ),
        (
            "zellij-utils",
            "assets/prost_web_server",
            "src/web_server_contract",
            "generated_web_server_api.rs",
        ),
        (
            "zellij-utils",
            "assets/prost_nested_session",
            "src/nested_session_contract",
            "generated_nested_session_api.rs",
        ),
        (
            "zellij-relay-protocol",
            "assets/prost_relay",
            "src/relay_protocol",
            "generated_relay_protocol.rs",
        ),
    ];

    for (base_crate, out_subdir, src_subdir, include_file) in specs {
        let base_dir = crate::project_root().join(base_crate);
        let _pd = sh.push_dir(&base_dir);

        let out_dir = sh.current_dir().join(out_subdir);
        let src_dir = sh.current_dir().join(src_subdir);
        std::fs::create_dir_all(&out_dir).unwrap();

        let last_generated = out_dir
            .join(include_file)
            .metadata()
            .and_then(|m| m.modified());
        let mut proto_files = vec![];
        let mut needs_regeneration = force;

        for entry in std::fs::read_dir(&src_dir).unwrap() {
            let entry_path = entry.unwrap().path();
            if entry_path.is_file()
                && entry_path
                    .extension()
                    .map(|e| e == "proto")
                    .unwrap_or(false)
            {
                let modified = entry_path.metadata().and_then(|m| m.modified());
                needs_regeneration |= match (&last_generated, modified) {
                    (Ok(last_generated), Ok(modified)) => modified > *last_generated,
                    // Couldn't read some metadata, assume needs update
                    _ => true,
                };
                proto_files.push(entry_path.display().to_string());
            }
        }
        proto_files.sort();

        if needs_regeneration {
            let mut prost = prost_build::Config::new();
            prost.out_dir(&out_dir);
            prost.include_file(include_file);
            prost.compile_protos(&proto_files, &[src_dir]).unwrap();
        }
    }
}

/// Build the `zellij-ansi-clip` crate for the `wasm32-unknown-unknown` target.
/// If `stage_to_assets` is true, copies the resulting blob into
/// `zellij-web-client-assets/assets/clip.wasm` (optionally through `wasm-opt`).
/// Otherwise only the raw `target/wasm32-unknown-unknown/<profile>/…wasm` is
/// produced — which is what the `clip_wasm_from_target` feature in
/// `zellij-web-client-assets` `include_bytes!`es from.
///
/// Invoked from:
/// - `cargo x build --wasm-clip`: stage_to_assets=true, release=per-flag.
/// - Normal `cargo x build` / `cargo x run`: stage_to_assets=false, release=true
///   (the `clip_wasm_from_target` feature expects the release path).
/// - Release pipeline (`xtask pipelines::publish`): stage_to_assets=true.
///
/// Requires the `wasm32-unknown-unknown` Rust target.
pub fn build_wasm_clip(
    sh: &Shell,
    release: bool,
    stage_to_assets: bool,
) -> anyhow::Result<()> {
    let _pd = sh.push_dir(crate::project_root());

    println!();
    let msg = ">> Building zellij-ansi-clip wasm blob";
    crate::status(msg);
    println!("{}", msg);

    // Make sure the target is installed; ignore failure (user may have it via
    // rustup components already, or via a toolchain-pinned config).
    let _ = cmd!(sh, "rustup target add wasm32-unknown-unknown")
        .quiet()
        .run();

    let cargo = crate::cargo()?;
    let mut base_cmd = cmd!(sh, "{cargo} build -p zellij-ansi-clip --features wasm --target wasm32-unknown-unknown")
        .env("RUSTFLAGS", "-C strip=symbols -C opt-level=z");
    if release {
        base_cmd = base_cmd.arg("--release");
    }
    base_cmd
        .run()
        .context("failed to build zellij-ansi-clip wasm blob")?;

    let profile = if release { "release" } else { "debug" };
    let target_dir = PathBuf::from(
        std::env::var_os("CARGO_TARGET_DIR")
            .unwrap_or_else(|| crate::project_root().join("target").into_os_string()),
    );
    let wasm_src = target_dir
        .join("wasm32-unknown-unknown")
        .join(profile)
        .join("zellij_ansi_clip.wasm");
    if !wasm_src.is_file() {
        return Err(anyhow::anyhow!(
            "expected wasm artefact at '{}' after build",
            wasm_src.display()
        ));
    }

    if !stage_to_assets {
        println!(
            ">> clip.wasm built at {} (not staged to committed assets)",
            wasm_src.display()
        );
        return Ok(());
    }

    let dst_dir = crate::project_root()
        .join("zellij-web-client-assets")
        .join("assets");
    std::fs::create_dir_all(&dst_dir).context("failed to create assets directory")?;
    let dst = dst_dir.join("clip.wasm");

    // If wasm-opt is available, use it; otherwise a plain copy. `rustc` emits
    // bulk-memory / reference-types / multivalue opcodes by default on stable
    // toolchains — pass the matching `--enable-*` flags so wasm-opt accepts
    // them instead of bailing out of validation.
    let have_wasm_opt = which::which("wasm-opt").is_ok();
    if have_wasm_opt {
        cmd!(
            sh,
            "wasm-opt -Oz --enable-bulk-memory --enable-reference-types --enable-multivalue --enable-mutable-globals --enable-nontrapping-float-to-int --enable-sign-ext -o {dst} {wasm_src}"
        )
        .run()
        .context("wasm-opt optimisation failed")?;
    } else {
        eprintln!("wasm-opt not found; skipping size-optimization pass");
        sh.copy_file(&wasm_src, &dst)
            .context("failed to copy zellij_ansi_clip.wasm into assets")?;
    }

    println!(">> clip.wasm written to {}", dst.display());
    Ok(())
}

/// Build the `zellij-relay-crypto-wasm` crate for `wasm32-unknown-unknown`
/// and stage the blob to `zellij-web-client-assets/assets/relay_crypto.wasm`
/// (the committed asset embedded via `include_dir!`). Mirrors
/// [`build_wasm_clip`] but always stages, since there is no
/// `*_from_target` dev-override feature for this blob.
pub fn build_wasm_relay_crypto(sh: &Shell, release: bool) -> anyhow::Result<()> {
    let _pd = sh.push_dir(crate::project_root());

    let msg = ">> Building zellij-relay-crypto-wasm wasm blob";
    crate::status(msg);
    println!("{}", msg);

    let _ = cmd!(sh, "rustup target add wasm32-unknown-unknown")
        .quiet()
        .run();

    let cargo = crate::cargo()?;
    let mut base_cmd = cmd!(
        sh,
        "{cargo} build -p zellij-relay-crypto-wasm --target wasm32-unknown-unknown"
    )
    .env("RUSTFLAGS", "-C strip=symbols -C opt-level=z");
    if release {
        base_cmd = base_cmd.arg("--release");
    }
    base_cmd
        .run()
        .context("failed to build zellij-relay-crypto-wasm wasm blob")?;

    let profile = if release { "release" } else { "debug" };
    let target_dir = PathBuf::from(
        std::env::var_os("CARGO_TARGET_DIR")
            .unwrap_or_else(|| crate::project_root().join("target").into_os_string()),
    );
    let wasm_src = target_dir
        .join("wasm32-unknown-unknown")
        .join(profile)
        .join("zellij_relay_crypto_wasm.wasm");
    if !wasm_src.is_file() {
        return Err(anyhow::anyhow!(
            "expected wasm artefact at '{}' after build",
            wasm_src.display()
        ));
    }

    let dst = crate::project_root()
        .join("zellij-web-client-assets")
        .join("assets")
        .join("relay_crypto.wasm");

    if which::which("wasm-opt").is_ok() {
        cmd!(
            sh,
            "wasm-opt -Oz --enable-bulk-memory --enable-reference-types --enable-multivalue --enable-mutable-globals --enable-nontrapping-float-to-int --enable-sign-ext -o {dst} {wasm_src}"
        )
        .run()
        .context("wasm-opt optimisation failed")?;
    } else {
        eprintln!("wasm-opt not found; skipping size-optimization pass");
        sh.copy_file(&wasm_src, &dst)
            .context("failed to copy zellij_relay_crypto_wasm.wasm into assets")?;
    }
    println!(">> relay_crypto.wasm written to {}", dst.display());
    Ok(())
}

const DEFAULT_APP_HOST: &str = "zellij.online";

#[derive(Clone, Debug, Eq, PartialEq)]
struct RelayOriginConfig {
    authority: String,
    http_origin: String,
    websocket_origin: String,
}

impl RelayOriginConfig {
    fn derived(app_host: &str) -> anyhow::Result<Self> {
        Self::parse(&format!("https://{}", derive_relay_authority(app_host))).with_context(|| {
            format!("invalid relay authority derived from --app-host '{app_host}'")
        })
    }

    fn parse(input: &str) -> anyhow::Result<Self> {
        use anyhow::bail;
        use url::Host;

        if input.is_empty() || input.chars().any(char::is_whitespace) || input.contains('\\') {
            bail!("--relay-origin must be one unambiguous HTTP(S) or WebSocket origin");
        }

        let scheme_end = input
            .find("://")
            .ok_or_else(|| anyhow::anyhow!("--relay-origin must include a scheme"))?;
        let after_scheme = &input[scheme_end + 3..];
        let authority_end = after_scheme
            .find(['/', '?', '#'])
            .unwrap_or(after_scheme.len());
        let raw_authority = &after_scheme[..authority_end];
        let raw_path = after_scheme[authority_end..]
            .split(['?', '#'])
            .next()
            .unwrap_or_default();
        if raw_authority.is_empty() || raw_authority.ends_with(':') || raw_authority.contains('%') {
            bail!("--relay-origin contains an invalid or ambiguous host/port");
        }
        if !raw_path.chars().all(|c| c == '/') {
            bail!("--relay-origin must not contain a path");
        }

        let parsed = url::Url::parse(input).context("failed to parse --relay-origin")?;
        if !parsed.username().is_empty() || parsed.password().is_some() {
            bail!("--relay-origin must not contain credentials");
        }
        if !parsed.path().chars().all(|c| c == '/') {
            bail!("--relay-origin must not contain a path");
        }
        if parsed.query().is_some() {
            bail!("--relay-origin must not contain a query string");
        }
        if parsed.fragment().is_some() {
            bail!("--relay-origin must not contain a fragment");
        }
        if parsed.port() == Some(0) {
            bail!("--relay-origin port must be between 1 and 65535");
        }

        let host = parsed
            .host()
            .ok_or_else(|| anyhow::anyhow!("--relay-origin must contain a host"))?;
        let is_loopback = match host {
            Host::Domain(domain) => {
                validate_domain_name(domain)?;
                domain == "localhost"
            },
            Host::Ipv4(address) => address.is_loopback(),
            Host::Ipv6(address) => address.is_loopback(),
        };
        let secure = match parsed.scheme() {
            "https" | "wss" => true,
            "http" | "ws" if is_loopback => false,
            "http" | "ws" => {
                bail!("remote --relay-origin values must use https:// or wss://")
            },
            _ => bail!("--relay-origin must use https://, wss://, or a loopback HTTP/WS origin"),
        };

        let host = match host {
            Host::Domain(domain) => domain.to_string(),
            Host::Ipv4(address) => address.to_string(),
            Host::Ipv6(address) => format!("[{address}]"),
        };
        let authority = match parsed.port() {
            Some(port) => format!("{host}:{port}"),
            None => host,
        };
        let (http_scheme, websocket_scheme) = if secure {
            ("https", "wss")
        } else {
            ("http", "ws")
        };

        Ok(Self {
            http_origin: format!("{http_scheme}://{authority}"),
            websocket_origin: format!("{websocket_scheme}://{authority}"),
            authority,
        })
    }

    fn csp(&self) -> String {
        format!(
            "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self' {} {}; manifest-src 'self'; frame-ancestors 'none'; base-uri 'self'; object-src 'none'",
            self.http_origin, self.websocket_origin
        )
    }
}

fn validate_domain_name(domain: &str) -> anyhow::Result<()> {
    use anyhow::bail;

    if domain.len() > 253 || domain.ends_with('.') {
        bail!("--relay-origin contains an invalid domain name");
    }
    for label in domain.split('.') {
        if label.is_empty()
            || label.len() > 63
            || !label
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            || !label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            || !label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
        {
            bail!("--relay-origin contains an invalid domain name");
        }
    }
    Ok(())
}

pub(crate) fn derive_relay_authority(app_host: &str) -> String {
    let (host, port) = match app_host.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h, Some(p)),
        _ => (app_host, None),
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let relay_host = if bare.parse::<std::net::IpAddr>().is_ok() || host.starts_with("relay.") {
        host.to_string()
    } else {
        format!("relay.{}", host)
    };
    match port {
        Some(p) => format!("{}:{}", relay_host, p),
        None => relay_host,
    }
}

use zellij_web_client_assets::manifest;

pub fn stage_app_origin(
    sh: &Shell,
    out_dir: &Path,
    app_host: Option<&str>,
    relay_origin: Option<&str>,
) -> anyhow::Result<()> {
    stage_app_origin_inner(
        sh,
        out_dir,
        false,
        app_host.unwrap_or(DEFAULT_APP_HOST),
        relay_origin,
    )
}

pub fn stage_app_origin_dev(
    sh: &Shell,
    out_dir: &Path,
    app_authority: &str,
    relay_origin: Option<&str>,
) -> anyhow::Result<()> {
    stage_app_origin_inner(sh, out_dir, true, app_authority, relay_origin)
}

fn stage_app_origin_inner(
    sh: &Shell,
    out_dir: &Path,
    dev_clip_from_target: bool,
    app_host: &str,
    relay_origin: Option<&str>,
) -> anyhow::Result<()> {
    let relay = match relay_origin {
        Some(origin) => RelayOriginConfig::parse(origin)?,
        None => RelayOriginConfig::derived(app_host)?,
    };
    crate::assets::assets(sh, crate::flags::Assets { check: false })?;

    let root = crate::project_root();
    let src = root.join("zellij-web-client-assets").join("assets");
    if !src.is_dir() {
        return Err(anyhow::anyhow!(
            "expected source asset directory at '{}'",
            src.display()
        ));
    }
    if out_dir.exists() {
        std::fs::remove_dir_all(out_dir)
            .with_context(|| format!("failed to clear existing output at {}", out_dir.display()))?;
    }

    let core_assets = out_dir.join("assets");
    std::fs::create_dir_all(&core_assets)
        .with_context(|| format!("failed to create {}", core_assets.display()))?;
    stage_named_assets(sh, &src, &core_assets, manifest::HANDSHAKE_CORE_ASSET_NAMES)?;
    write_wasm_integrity_module(&core_assets, &["relay_crypto.wasm"], !dev_clip_from_target)?;

    let index_src = std::fs::read_to_string(src.join("index.html"))
        .with_context(|| "failed to read source index.html")?;
    let index_out = render_app_origin_index(&index_src, &core_assets, &relay)?;
    verify_app_origin_index(&index_out, &relay)?;
    std::fs::write(out_dir.join("index.html"), index_out)
        .with_context(|| "failed to write staged index.html")?;
    write_static_host_config(out_dir, &relay.csp())?;

    let version = zellij_web_client_assets::VERSION;
    let bundle_dir = out_dir.join("v").join(version);
    let bundle_assets = bundle_dir.join("assets");
    std::fs::create_dir_all(&bundle_assets)
        .with_context(|| format!("failed to create {}", bundle_assets.display()))?;
    stage_named_assets(sh, &src, &bundle_assets, manifest::APPLICATION_ASSET_NAMES)?;
    if dev_clip_from_target {
        let target_clip = root
            .join("target/wasm32-unknown-unknown/release/zellij_ansi_clip.wasm");
        if !target_clip.is_file() {
            return Err(anyhow::anyhow!(
                "expected development clip.wasm at '{}'",
                target_clip.display()
            ));
        }
        sh.copy_file(&target_clip, &bundle_assets.join("clip.wasm"))?;
    }
    write_wasm_integrity_module(&bundle_assets, &["clip.wasm"], true)?;

    let (manifest_json, rolled_up) = build_app_manifest(&bundle_assets)?;
    std::fs::write(bundle_dir.join("app-manifest.json"), manifest_json)
        .with_context(|| "failed to write app-manifest.json")?;
    let mut digest_file = rolled_up.clone();
    digest_file.push(char::from(10));
    std::fs::write(bundle_dir.join("app_bundle_sha384.txt"), digest_file)
        .with_context(|| "failed to write app_bundle_sha384.txt")?;

    if !dev_clip_from_target {
        let attested = zellij_web_client_assets::app_bundle_sha384();
        if rolled_up != attested {
            return Err(anyhow::anyhow!(
                "staged app bundle digest {} does not match the binary-attested digest {} — \
                 the committed assets and the embedded assets disagree",
                rolled_up,
                attested
            ));
        }
    }

    write_release_hashes(out_dir)?;

    println!(">> app-origin static site staged to {}", out_dir.display());
    println!(
        ">> browser runtime and connect-src pinned to {} ({})",
        relay.http_origin, relay.websocket_origin
    );
    println!(
        ">> versioned application bundle staged at v/{}/ ({})",
        version, rolled_up
    );
    Ok(())
}

fn stage_named_assets(
    sh: &Shell,
    src: &Path,
    dest: &Path,
    names: &[&str],
) -> anyhow::Result<()> {
    for name in names {
        let from = src.join(name);
        if !from.is_file() {
            return Err(anyhow::anyhow!(
                "expected source asset '{}' at {}",
                name,
                from.display()
            ));
        }
        sh.copy_file(&from, &dest.join(name))?;
    }
    Ok(())
}

fn build_app_manifest(bundle_assets: &Path) -> anyhow::Result<(String, String)> {
    let mut entries: Vec<(String, String)> = Vec::new();
    for name in manifest::APPLICATION_ASSET_NAMES {
        let bytes = std::fs::read(bundle_assets.join(name))
            .with_context(|| format!("missing staged application asset: {}", name))?;
        entries.push((name.to_string(), manifest::per_asset_integrity_b64(&bytes)));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let json_items: Vec<String> = entries
        .iter()
        .map(|(name, integrity)| {
            format!("  {{\"name\": {:?}, \"integrity\": {:?}}}", name, integrity)
        })
        .collect();
    let mut manifest_json = format!("[\n{}\n]", json_items.join(",\n"));
    manifest_json.push(char::from(10));

    let mut lines: Vec<String> = entries
        .iter()
        .map(|(name, integrity)| manifest::manifest_line(name, integrity))
        .collect();
    lines.sort();
    let rolled_up = manifest::rolled_up_from_lines(&lines);

    Ok((manifest_json, rolled_up))
}

fn render_app_origin_index(
    index_src: &str,
    assets_out: &Path,
    relay: &RelayOriginConfig,
) -> anyhow::Result<String> {
    let resolved = index_src
        .replace("BASE_URL", "/")
        .replace("IS_AUTHENTICATED", "false")
        .replace("EXPECTED_E2E", "true")
        .replace("IS_READ_ONLY", "false")
        .replace("SESSION_ROWS", "0")
        .replace("SESSION_COLS", "0")
        .replace("AUTH_MODE", "relay");

    let mut out_lines: Vec<String> = Vec::new();
    let mut csp_injected = false;
    for line in resolved.lines() {
        if !csp_injected && line.contains("</title>") {
            out_lines.push(line.to_string());
            out_lines.push(format!(
                "        <meta http-equiv=\"Content-Security-Policy\" content=\"{}\" />",
                relay.csp()
            ));
            out_lines.push(format!(
                "        <meta name=\"zellij-relay-origin\" content=\"{}\" />",
                relay.http_origin
            ));
            csp_injected = true;
            continue;
        }
        out_lines.push(inject_sri(line, assets_out)?);
    }
    if !csp_injected {
        return Err(anyhow::anyhow!(
            "index.html has no </title> anchor to attach the CSP meta to"
        ));
    }
    let mut joined = out_lines.join("\n");
    joined.push(char::from(10));
    Ok(joined)
}

fn staged_asset_name(line: &str, attr: &str) -> Option<String> {
    let start = line.find(attr)? + attr.len();
    let rest = &line[start..];
    let end = rest.find(char::from(34))?;
    Some(rest[..end].to_string())
}

fn require_pinned_tag(line: &str, kind: &str, name: &str) -> anyhow::Result<()> {
    if !line.contains("integrity=\"sha384-") || !line.contains("crossorigin=\"anonymous\"") {
        return Err(anyhow::anyhow!(
            "staged index.html {} tag for '{}' lacks integrity/crossorigin pinning: {}",
            kind,
            name,
            line.trim()
        ));
    }
    Ok(())
}

fn assert_name_set_matches(
    kind: &str,
    mut found: Vec<String>,
    expected: &[&str],
) -> anyhow::Result<()> {
    found.sort();
    let mut wanted: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    wanted.sort();
    if found != wanted {
        return Err(anyhow::anyhow!(
            "staged index.html {} set {:?} does not match the handshake manifest {:?}",
            kind,
            found,
            wanted
        ));
    }
    Ok(())
}

fn verify_app_origin_index(index_html: &str, relay: &RelayOriginConfig) -> anyhow::Result<()> {
    let mut classic_scripts: Vec<String> = Vec::new();
    let mut module_scripts: Vec<String> = Vec::new();
    let mut stylesheets: Vec<String> = Vec::new();
    let expected_csp = format!(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"{}\" />",
        relay.csp()
    );
    let expected_runtime = format!(
        "<meta name=\"zellij-relay-origin\" content=\"{}\" />",
        relay.http_origin
    );
    if index_html.matches("<base href=\"/\" />").count() != 1 {
        return Err(anyhow::anyhow!(
            "staged index.html must contain exactly one same-origin root base URL"
        ));
    }
    if index_html.matches(&expected_csp).count() != 1
        || index_html.matches(&expected_runtime).count() != 1
    {
        return Err(anyhow::anyhow!(
            "staged index.html runtime relay target and CSP do not match the validated relay origin"
        ));
    }
    for line in index_html.lines() {
        if line.contains("<style") {
            return Err(anyhow::anyhow!(
                "staged index.html must not contain a <style> element: {}",
                line.trim()
            ));
        }
        if line.contains("<script") {
            let name = staged_asset_name(line, "src=\"assets/").ok_or_else(|| {
                anyhow::anyhow!(
                    "staged index.html contains a script that is not a pinned core asset: {}",
                    line.trim()
                )
            })?;
            require_pinned_tag(line, "script", &name)?;
            if line.contains("type=\"module\"") {
                module_scripts.push(name);
            } else {
                classic_scripts.push(name);
            }
        } else if line.contains("<link") && line.contains("rel=\"stylesheet\"") {
            let name = staged_asset_name(line, "href=\"assets/").ok_or_else(|| {
                anyhow::anyhow!(
                    "staged index.html contains a stylesheet outside assets/: {}",
                    line.trim()
                )
            })?;
            require_pinned_tag(line, "stylesheet", &name)?;
            stylesheets.push(name);
        }
    }
    assert_name_set_matches(
        "classic script",
        classic_scripts,
        manifest::HANDSHAKE_CLASSIC_SCRIPT_NAMES,
    )?;
    assert_name_set_matches(
        "module script",
        module_scripts,
        manifest::HANDSHAKE_MODULE_SCRIPT_NAMES,
    )?;
    assert_name_set_matches(
        "stylesheet",
        stylesheets,
        manifest::HANDSHAKE_STYLESHEET_NAMES,
    )?;
    Ok(())
}

fn write_static_host_config(out_dir: &Path, csp: &str) -> anyhow::Result<()> {
    std::fs::write(out_dir.join("_redirects"), "/r/*  /index.html  200\n")
        .with_context(|| "failed to write _redirects")?;
    let headers = format!(
        "/*\n  Content-Security-Policy: {csp}\n  Strict-Transport-Security: max-age=63072000; includeSubDomains\n  X-Content-Type-Options: nosniff\n  X-Frame-Options: DENY\n  Referrer-Policy: no-referrer\n"
    );
    std::fs::write(out_dir.join("_headers"), headers)
        .with_context(|| "failed to write _headers")?;
    Ok(())
}

fn inject_sri(line: &str, assets_out: &Path) -> anyhow::Result<String> {
    let trimmed = line.trim_start();
    let is_script = trimmed.starts_with("<script") && line.contains("src=\"assets/");
    let is_style = trimmed.starts_with("<link")
        && line.contains("rel=\"stylesheet\"")
        && line.contains("href=\"assets/");
    if !is_script && !is_style {
        return Ok(line.to_string());
    }
    let attr = if is_script { "src=\"" } else { "href=\"" };
    let start = line.find(attr).expect("attribute present") + attr.len();
    let rel_len = line[start..].find(char::from(34)).expect("closing quote");
    let url_path = &line[start..start + rel_len];
    let asset_rel = url_path
        .strip_prefix("assets/")
        .expect("asset url under assets/");
    let bytes = std::fs::read(assets_out.join(asset_rel))
        .with_context(|| format!("missing staged asset for SRI: {}", asset_rel))?;
    let integrity = sri_sha384(&bytes);
    let close = start + rel_len + 1;
    let injected = format!(
        " integrity=\"{}\" crossorigin=\"anonymous\"",
        integrity
    );
    Ok(format!("{}{}{}", &line[..close], injected, &line[close..]))
}

fn write_wasm_integrity_module(
    assets_out: &Path,
    names: &[&str],
    require_all: bool,
) -> anyhow::Result<()> {
    let mut entries: Vec<String> = Vec::new();
    for name in names {
        let path = assets_out.join(name);
        if !path.is_file() {
            if require_all {
                return Err(anyhow::anyhow!(
                    "staged bundle would ship an integrity manifest missing '{}' — every wasm must be pinned",
                    name
                ));
            }
            continue;
        }
        let bytes = std::fs::read(&path)?;
        entries.push(format!("    \"{}\": \"{}\",", name, sri_sha384(&bytes)));
    }
    let module = format!(
        "export const WASM_INTEGRITY = {{\n{}\n}};\n",
        entries.join("\n")
    );
    std::fs::write(assets_out.join("integrity.js"), module)
        .with_context(|| "failed to write integrity.js")?;
    Ok(())
}

fn write_release_hashes(out_dir: &Path) -> anyhow::Result<()> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_files(out_dir, &mut files)?;
    files.sort();
    let mut lines: Vec<String> = Vec::new();
    for path in &files {
        let rel = path.strip_prefix(out_dir).expect("within out_dir");
        let rel = rel.to_string_lossy().replace(char::from(92), "/");
        if matches!(
            rel.as_str(),
            "RELEASE_HASHES.txt" | "_headers" | "_redirects"
        ) {
            continue;
        }
        let bytes = std::fs::read(path)?;
        lines.push(format!("{}  {}", sha256_hex(&bytes), rel));
    }
    let mut body = lines.join("\n");
    body.push(char::from(10));
    std::fs::write(out_dir.join("RELEASE_HASHES.txt"), body)
        .with_context(|| "failed to write RELEASE_HASHES.txt")?;
    Ok(())
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

fn sri_sha384(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha384};
    let digest = Sha384::digest(bytes);
    format!("sha384-{}", base64_encode(&digest))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

fn base64_symbol(value: usize) -> u8 {
    match value {
        0..=25 => b'A' + value as u8,
        26..=51 => b'a' + (value - 26) as u8,
        52..=61 => b'0' + (value - 52) as u8,
        62 => 43,
        _ => 47,
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const PAD: u8 = 61;
    let mut out: Vec<u8> = Vec::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
        out.push(base64_symbol(b0 >> 2));
        out.push(base64_symbol(((b0 & 0x03) << 4) | (b1 >> 4)));
        out.push(if chunk.len() > 1 {
            base64_symbol(((b1 & 0x0f) << 2) | (b2 >> 6))
        } else {
            PAD
        });
        out.push(if chunk.len() > 2 {
            base64_symbol(b2 & 0x3f)
        } else {
            PAD
        });
    }
    String::from_utf8(out).expect("base64 alphabet is ascii")
}

fn move_plugin_to_assets(sh: &Shell, plugin_name: &str) -> anyhow::Result<()> {
    let err_context = || format!("failed to move plugin '{plugin_name}' to assets folder");

    // Get asset path
    let asset_name = crate::asset_dir()
        .join("plugins")
        .join(plugin_name)
        .with_extension("wasm");

    // Get plugin path
    let plugin = crate::target_dir()
        .join("wasm32-wasip1")
        .join("release")
        .join(plugin_name)
        .with_extension("wasm");

    if !plugin.is_file() {
        return Err(anyhow::anyhow!("No plugin found at '{}'", plugin.display()))
            .with_context(err_context);
    }

    // This is a plugin we want to move
    let from = plugin.as_path();
    let to = asset_name.as_path();
    sh.copy_file(from, to).with_context(err_context)
}
#[cfg(test)]
mod app_origin_tests {
    use super::*;

    #[test]
    fn stages_core_and_versioned_bundle_with_matching_digest() {
        let sh = Shell::new().unwrap();
        let out = crate::project_root()
            .join("target")
            .join("app-origin-stage-test");
        stage_app_origin(&sh, &out, None, None).unwrap();

        let index = std::fs::read_to_string(out.join("index.html")).unwrap();
        assert!(index.contains("core-bootstrap.js"));
        assert!(!index.contains("\"assets/index.js\"") && !index.contains("/index.js"));
        assert!(!index.contains("assets/websockets.js"));
        assert!(!index.contains("assets/xterm.js"));
        assert!(index
            .contains("connect-src 'self' https://relay.zellij.online wss://relay.zellij.online"));

        let version = zellij_web_client_assets::VERSION;
        let bundle = out.join("v").join(version);

        let manifest_json =
            std::fs::read_to_string(bundle.join("app-manifest.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&manifest_json).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), manifest::APPLICATION_ASSET_NAMES.len());
        for entry in arr {
            assert!(entry["integrity"].as_str().unwrap().starts_with("sha384-"));
            let name = entry["name"].as_str().unwrap();
            assert!(manifest::APPLICATION_ASSET_NAMES.contains(&name));
        }

        let digest =
            std::fs::read_to_string(bundle.join("app_bundle_sha384.txt")).unwrap();
        assert_eq!(digest.trim(), zellij_web_client_assets::app_bundle_sha384());

        let hashes = std::fs::read_to_string(out.join("RELEASE_HASHES.txt")).unwrap();
        assert!(hashes.contains(&format!("v/{}/assets/app.js", version)));
        assert!(!hashes.lines().any(|line| line.ends_with("  _headers")));
        assert!(!hashes.lines().any(|line| line.ends_with("  _redirects")));

        let _ = std::fs::remove_dir_all(&out);
    }

    fn valid_index_fixture() -> String {
        let relay = RelayOriginConfig::parse("https://relay.example.com").unwrap();
        let mut lines: Vec<String> = vec![
            "<html>".to_string(),
            "<base href=\"/\" />".to_string(),
            format!(
                "<meta http-equiv=\"Content-Security-Policy\" content=\"{}\" />",
                relay.csp()
            ),
            format!(
                "<meta name=\"zellij-relay-origin\" content=\"{}\" />",
                relay.http_origin
            ),
            "<link rel=\"stylesheet\" href=\"assets/style.css\" integrity=\"sha384-x\" crossorigin=\"anonymous\">".to_string(),
            "<script src=\"assets/modals.js\" integrity=\"sha384-x\" crossorigin=\"anonymous\"></script>".to_string(),
        ];
        for name in manifest::HANDSHAKE_MODULE_SCRIPT_NAMES {
            lines.push(format!(
                "<script type=\"module\" src=\"assets/{}\" integrity=\"sha384-x\" crossorigin=\"anonymous\"></script>",
                name
            ));
        }
        lines.push("</html>".to_string());
        lines.join("\n")
    }

    fn verify_test_index(index: &str) -> anyhow::Result<()> {
        let relay = RelayOriginConfig::parse("https://relay.example.com").unwrap();
        verify_app_origin_index(index, &relay)
    }

    #[test]
    fn verify_accepts_the_valid_fixture() {
        verify_test_index(&valid_index_fixture()).unwrap();
    }

    #[test]
    fn verify_rejects_inline_script() {
        let doctored = valid_index_fixture().replace(
            "</html>",
            "<script>alert(1)</script>\n</html>",
        );
        assert!(verify_test_index(&doctored).is_err());
    }

    #[test]
    fn verify_rejects_unlisted_script() {
        let doctored = valid_index_fixture().replace(
            "</html>",
            "<script type=\"module\" src=\"assets/evil.js\" integrity=\"sha384-x\" crossorigin=\"anonymous\"></script>\n</html>",
        );
        assert!(verify_test_index(&doctored).is_err());
    }

    #[test]
    fn verify_rejects_missing_core_script() {
        let fixture = valid_index_fixture();
        let doctored: String = fixture
            .lines()
            .filter(|line| !line.contains("assets/auth.js"))
            .collect::<Vec<&str>>()
            .join("\n");
        assert!(verify_test_index(&doctored).is_err());
    }

    #[test]
    fn verify_rejects_style_element() {
        let doctored = valid_index_fixture().replace(
            "</html>",
            "<style>body { display: none; }</style>\n</html>",
        );
        assert!(verify_test_index(&doctored).is_err());
    }

    #[test]
    fn verify_rejects_script_without_integrity() {
        let doctored = valid_index_fixture().replace(
            "<script src=\"assets/modals.js\" integrity=\"sha384-x\" crossorigin=\"anonymous\"></script>",
            "<script src=\"assets/modals.js\"></script>",
        );
        assert!(verify_test_index(&doctored).is_err());
    }

    #[test]
    fn verify_rejects_unpinned_connect_src() {
        let doctored = valid_index_fixture().replace(
            "connect-src 'self' https://relay.example.com wss://relay.example.com",
            "connect-src 'self' https: wss:",
        );
        assert!(verify_test_index(&doctored).is_err());
    }

    #[test]
    fn derive_relay_authority_rule() {
        assert_eq!(derive_relay_authority("zellij.online"), "relay.zellij.online");
        assert_eq!(
            derive_relay_authority("relay.zellij.online"),
            "relay.zellij.online"
        );
        assert_eq!(
            derive_relay_authority("localhost:8443"),
            "relay.localhost:8443"
        );
        assert_eq!(derive_relay_authority("127.0.0.1:9000"), "127.0.0.1:9000");
        assert_eq!(derive_relay_authority("[::1]:9000"), "[::1]:9000");
    }

    #[test]
    fn staged_index_pins_connect_src_and_carries_sri() {
        let sh = Shell::new().unwrap();
        let out = crate::project_root()
            .join("target")
            .join("app-origin-stage-test-csp");
        stage_app_origin(&sh, &out, Some("example.com"), None).unwrap();

        let index = std::fs::read_to_string(out.join("index.html")).unwrap();
        assert!(index
            .contains("connect-src 'self' https://relay.example.com wss://relay.example.com"));
        assert!(!index.contains("https: wss:"));
        let pinned_tags = index.matches("integrity=\"sha384-").count();
        assert_eq!(pinned_tags, 9);
        assert!(index.contains("crossorigin=\"anonymous\""));

        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn explicit_pages_relay_origin_drives_runtime_csp_and_host_files() {
        let sh = Shell::new().unwrap();
        let out = crate::project_root()
            .join("target")
            .join("app-origin-stage-test-explicit-relay");
        stage_app_origin(
            &sh,
            &out,
            Some("viewer-project.pages.dev"),
            Some("https://relay.zellij.online/"),
        )
        .unwrap();

        let index = std::fs::read_to_string(out.join("index.html")).unwrap();
        let headers = std::fs::read_to_string(out.join("_headers")).unwrap();
        let redirects = std::fs::read_to_string(out.join("_redirects")).unwrap();
        let relay = RelayOriginConfig::parse("https://relay.zellij.online").unwrap();
        let expected_connect =
            "connect-src 'self' https://relay.zellij.online wss://relay.zellij.online";
        let expected_headers = format!(
            "/*\n  Content-Security-Policy: {}\n  Strict-Transport-Security: max-age=63072000; includeSubDomains\n  X-Content-Type-Options: nosniff\n  X-Frame-Options: DENY\n  Referrer-Policy: no-referrer\n",
            relay.csp()
        );
        assert!(index.contains(
            "<meta name=\"zellij-relay-origin\" content=\"https://relay.zellij.online\" />"
        ));
        assert!(index.contains(expected_connect));
        assert!(index.contains("<base href=\"/\" />"));
        assert!(index.contains("base-uri 'self'"));
        assert_eq!(headers, expected_headers);
        assert_eq!(redirects, "/r/*  /index.html  200\n");
        assert!(!index.contains("relay.viewer-project.pages.dev"));

        let hashes = std::fs::read_to_string(out.join("RELEASE_HASHES.txt")).unwrap();
        assert!(hashes.contains("index.html"));
        assert!(hashes.contains(&format!(
            "v/{}/assets/app.js",
            zellij_web_client_assets::VERSION
        )));
        assert!(!hashes.lines().any(|line| line.ends_with("  _headers")));
        assert!(!hashes.lines().any(|line| line.ends_with("  _redirects")));

        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn relay_origin_normalizes_secure_and_loopback_origins() {
        let secure = RelayOriginConfig::parse("wss://relay.example.test:8443///").unwrap();
        assert_eq!(secure.authority, "relay.example.test:8443");
        assert_eq!(secure.http_origin, "https://relay.example.test:8443");
        assert_eq!(secure.websocket_origin, "wss://relay.example.test:8443");

        let default_port = RelayOriginConfig::parse("https://relay.example.test:443/").unwrap();
        assert_eq!(default_port.http_origin, "https://relay.example.test");

        let localhost = RelayOriginConfig::parse("http://localhost:8765/").unwrap();
        assert_eq!(localhost.http_origin, "http://localhost:8765");
        assert_eq!(localhost.websocket_origin, "ws://localhost:8765");

        let ipv4 = RelayOriginConfig::parse("ws://127.0.0.1:9000").unwrap();
        assert_eq!(ipv4.http_origin, "http://127.0.0.1:9000");
        let ipv6 = RelayOriginConfig::parse("http://[::1]:9000").unwrap();
        assert_eq!(ipv6.websocket_origin, "ws://[::1]:9000");
    }

    #[test]
    fn relay_origin_rejects_insecure_or_ambiguous_inputs() {
        for invalid in [
            "relay.example.test",
            "http://relay.example.test",
            "ws://192.0.2.1:9000",
            "https://user@relay.example.test",
            "https://relay.example.test/path",
            "https://relay.example.test/path/..",
            "https://relay.example.test/%2e%2e",
            "https://relay.example.test?target=other",
            "https://relay.example.test#fragment",
            "https://relay.example.test:",
            "https://relay..example.test",
            "https://-relay.example.test",
            "https://relay.example.test:0",
            "ftp://relay.example.test",
            " https://relay.example.test",
            "https:\\relay.example.test",
        ] {
            assert!(
                RelayOriginConfig::parse(invalid).is_err(),
                "accepted invalid relay origin: {invalid}"
            );
        }
    }

    #[test]
    fn verifier_rejects_runtime_and_csp_disagreement() {
        let doctored = valid_index_fixture().replace(
            "content=\"https://relay.example.com\"",
            "content=\"https://other.example.com\"",
        );
        assert!(verify_test_index(&doctored).is_err());
    }

    #[test]
    fn wasm_integrity_module_fails_hard_when_a_release_wasm_is_missing() {
        let dir = crate::project_root()
            .join("target")
            .join("wasm-integrity-require-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = write_wasm_integrity_module(&dir, &["relay_crypto.wasm"], true).unwrap_err();
        assert!(err.to_string().contains("relay_crypto.wasm"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wasm_integrity_module_tolerates_a_missing_wasm_in_dev() {
        let dir = crate::project_root()
            .join("target")
            .join("wasm-integrity-dev-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_wasm_integrity_module(&dir, &["relay_crypto.wasm"], false).unwrap();
        let module = std::fs::read_to_string(dir.join("integrity.js")).unwrap();
        assert!(module.contains("WASM_INTEGRITY"));
        assert!(!module.contains("sha384-"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
