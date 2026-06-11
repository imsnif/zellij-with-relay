//! Subcommands for building.
//!
//! Currently has the following functions:
//!
//! - [`build`]: Builds general cargo projects (i.e. zellij components) with `cargo build`
//! - [`manpage`]: Builds the manpage with `mandown`
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
        // Short-circuit: `cargo x build --wasm-clip` only builds the clip wasm
        // blob AND stages it to the committed assets. This is the path used
        // by the release pipeline (`xtask pipelines::publish`) and by manual
        // invocations that want to refresh the committed blob.
        return build_wasm_clip(sh, flags.release, /* stage_to_assets */ true);
    }

    // zellij-utils requires protobuf definition files to be present. Usually these are
    // auto-generated with `build.rs`-files, but this is currently broken for us.
    // See [this PR][1] for details.
    //
    // [1]: https://github.com/zellij-org/zellij/pull/2711#issuecomment-1695015818
    run_proto_codegen(sh);

    // Build all plugins in a single invocation so Cargo can unify transitive dependency
    // features across all of them and compile shared crates (e.g. zellij-utils) only once.
    if !flags.no_plugins {
        let plugin_members: Vec<&WorkspaceMember> = crate::workspace_members()
            .iter()
            .filter(|m| m.build && m.crate_name.contains("plugins"))
            .collect();

        if !plugin_members.is_empty() {
            println!();
            let msg = ">> Building plugins";
            crate::status(msg);
            println!("{}", msg);

            let mut base_cmd = cmd!(sh, "{cargo} build --target wasm32-wasip1");
            if flags.release {
                base_cmd = base_cmd.arg("--release");
            }
            for member in &plugin_members {
                let plugin_name = member
                    .crate_name
                    .rsplit_once('/')
                    .context("Cannot determine plugin name from crate path")?
                    .1;
                base_cmd = base_cmd.args(["-p", plugin_name]);
            }
            base_cmd.run().context("failed to build plugins")?;

            if flags.release {
                for member in &plugin_members {
                    let plugin_name = member
                        .crate_name
                        .rsplit_once('/')
                        .context("Cannot determine plugin name from crate path")?
                        .1;
                    move_plugin_to_assets(sh, plugin_name)?;
                }
            }
        }

        // Build the ansi-clip wasm blob alongside the plugins when web is
        // enabled. The output lives at
        // `target/wasm32-unknown-unknown/release/zellij_ansi_clip.wasm`, which
        // is where `zellij-web-client-assets/clip_wasm_from_target` (enabled
        // by the root crate's default feature set) reads from at compile
        // time. Release pipelines (`xtask pipelines::publish`) refresh the
        // committed blob via the `wasm_clip: true` early-return path above.
        // This mirrors the plugins flow: dev builds stream through `target/`,
        // release pipelines stage to `assets/`.
        if !flags.no_web {
            // Always release mode — `clip_wasm_from_target` hard-codes the
            // `release/` path in its include_bytes!.
            build_wasm_clip(sh, /* release */ true, /* stage_to_assets */ false)?;
        }
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
            base_cmd.run().with_context(err_context)?;
        }
    }

    Ok(())
}

fn run_proto_codegen(sh: &Shell) {
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
        let mut needs_regeneration = false;

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

/// Copy `zellij-web-client-assets/assets/` into
/// `zellij-relay/assets/<version>/`. Called from `pipelines::publish`
/// right after the `wasm_clip` build, so the fresh `clip.wasm` is part
/// of the snapshot. The resulting files are picked up by the release
/// commit alongside the other regenerated assets.
pub fn snapshot_web_assets_for_relay(sh: &Shell, version: &str) -> anyhow::Result<()> {
    let root = crate::project_root();
    let src = root.join("zellij-web-client-assets").join("assets");
    let dst = root.join("zellij-relay").join("assets").join(version);

    if !src.is_dir() {
        return Err(anyhow::anyhow!(
            "expected source asset directory at '{}'",
            src.display()
        ));
    }

    // Rewrite-from-scratch so a re-run at the same version does not
    // retain stale files (e.g. an asset removed upstream).
    if dst.exists() {
        std::fs::remove_dir_all(&dst)
            .with_context(|| format!("failed to clear existing snapshot at {}", dst.display()))?;
    }
    std::fs::create_dir_all(&dst)
        .with_context(|| format!("failed to create {}", dst.display()))?;

    copy_dir_recursive(sh, &src, &dst)
        .with_context(|| format!("failed to snapshot {} -> {}", src.display(), dst.display()))?;

    println!(
        ">> relay web-client asset snapshot written to {}",
        dst.display()
    );
    Ok(())
}

fn copy_dir_recursive(sh: &Shell, src: &Path, dst: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(src)
            .expect("directory walk stays inside src");
        let target = dst.join(relative);
        if path.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_dir_recursive(sh, &path, &target)?;
        } else if path.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            sh.copy_file(&path, &target)?;
        }
    }
    Ok(())
}

fn move_plugin_to_assets(sh: &Shell, plugin_name: &str) -> anyhow::Result<()> {
    let err_context = || format!("failed to move plugin '{plugin_name}' to assets folder");

    // Get asset path
    let asset_name = crate::asset_dir()
        .join("plugins")
        .join(plugin_name)
        .with_extension("wasm");

    // Get plugin path
    let plugin = PathBuf::from(
        std::env::var_os("CARGO_TARGET_DIR")
            .unwrap_or(crate::project_root().join("target").into_os_string()),
    )
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

/// Build the manpage with `mandown`.
//      mkdir -p ${root_dir}/assets/man
//      mandown ${root_dir}/docs/MANPAGE.md 1 > ${root_dir}/assets/man/zellij.1
pub fn manpage(sh: &Shell) -> anyhow::Result<()> {
    let err_context = "failed to generate manpage";

    let mandown = mandown(sh).context(err_context)?;

    let project_root = crate::project_root();
    let asset_dir = &project_root.join("assets").join("man");
    sh.create_dir(asset_dir).context(err_context)?;
    let _pd = sh.push_dir(asset_dir);

    cmd!(sh, "{mandown} {project_root}/docs/MANPAGE.md 1")
        .read()
        .and_then(|text| sh.write_file("zellij.1", text))
        .context(err_context)
}

/// Get the path to a `mandown` executable.
///
/// If the executable isn't found, an error is returned instead.
fn mandown(_sh: &Shell) -> anyhow::Result<PathBuf> {
    match which::which("mandown") {
        Ok(path) => Ok(path),
        Err(e) => {
            eprintln!("!! 'mandown' wasn't found but is needed for this build step.");
            eprintln!("!! Please install it with: `cargo install mandown`");
            Err(e).context("Couldn't find 'mandown' executable")
        },
    }
}
