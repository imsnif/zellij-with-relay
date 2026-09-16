use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context};
use xshell::Shell;

const RELAY_BIND: &str = "127.0.0.1:8765";

pub fn relay_dev(sh: &Shell, flags: crate::flags::RelayDev) -> anyhow::Result<()> {
    let https_port = flags.https_port.unwrap_or(8443);
    let host = flags.host.clone().unwrap_or_else(|| "localhost".to_string());
    if host.contains('/') || host.contains(':') {
        bail!(
            "--host expects a bare hostname or IPv4 address (no scheme, no port); \
             the port is taken from --https-port"
        );
    }
    let lan_mode = host != "localhost";
    let authority = format!("{}:{}", host, https_port);
    let relay_authority = crate::build::derive_relay_authority(&authority);
    let same_origin_relay = relay_authority == authority;

    if which::which("caddy").is_err() {
        bail!(caddy_missing_message());
    }

    let root = crate::project_root();
    let dev_dir = root.join("target").join("relay-dev");
    let app_dir = dev_dir.join("app");
    let data_dir = dev_dir.join("data");

    let build_release_wasm = true;
    let stage_wasm_to_assets = false;
    crate::build::build_wasm_clip(sh, build_release_wasm, stage_wasm_to_assets)
        .context("failed to build the zellij-ansi-clip wasm blob")?;
    crate::build::stage_app_origin_dev(sh, &app_dir, &authority, None)
        .context("failed to stage the app-origin static site")?;

    if data_dir.exists() {
        let _ = std::fs::remove_dir_all(&data_dir);
    }
    std::fs::create_dir_all(&data_dir)?;
    let token = mint_token(&data_dir).context("failed to mint a relay tunnel-auth token")?;

    let caddyfile = dev_dir.join("Caddyfile");
    std::fs::write(
        &caddyfile,
        caddyfile_contents(&authority, &relay_authority, same_origin_relay, &app_dir),
    )
    .context("failed to write Caddyfile")?;

    let app_origin = format!("https://{}", authority);
    let public_url_template = format!("https://{}/r/{{slug}}", authority);
    let ca_cert_path = caddy_data_home().join("caddy/pki/authorities/local/root.crt");

    clear_stale_leaf_certs(&host, &relay_authority);

    let mut caddy = Command::new("caddy")
        .arg("run")
        .arg("--adapter")
        .arg("caddyfile")
        .arg("--config")
        .arg(&caddyfile)
        .spawn()
        .context("failed to start caddy")?;

    std::thread::sleep(std::time::Duration::from_millis(1500));
    if let Ok(Some(status)) = caddy.try_wait() {
        bail!(caddy_died_message(status, https_port));
    }

    ensure_ca_trusted(&ca_cert_path, &dev_dir);

    print_instructions(
        &app_origin,
        &relay_authority,
        https_port,
        &token,
        lan_mode,
        same_origin_relay,
        &ca_cert_path,
    );

    let relay_status = Command::new(crate::cargo()?)
        .args(["run", "-q", "-p", "zellij-relay-server"])
        .env("RELAY_BIND_ADDR", RELAY_BIND)
        .env("RELAY_ALLOWED_ORIGINS", &app_origin)
        .env("RELAY_PUBLIC_URL_TEMPLATE", &public_url_template)
        .env("RELAY_DATA_DIR", &data_dir)
        .current_dir(&root)
        .status();

    let _ = caddy.kill();
    let _ = caddy.wait();
    relay_status.context("relay process failed to run")?;
    Ok(())
}

fn clear_stale_leaf_certs(host: &str, relay_authority: &str) {
    let relay_host = relay_authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(relay_authority);
    let base = caddy_data_home().join("caddy/certificates/local");
    let mut names = vec![host];
    if relay_host != host {
        names.push(relay_host);
    }
    for name in names {
        let dir = base.join(name);
        if dir.is_dir() && std::fs::remove_dir_all(&dir).is_ok() {
            println!(
                "relay-dev: cleared cached leaf certificate state for '{}' \
                 (caddy re-issues it from the existing local CA on first use)",
                name
            );
        }
    }
}

fn mint_token(data_dir: &Path) -> anyhow::Result<String> {
    let out = Command::new(crate::cargo()?)
        .args([
            "run",
            "-q",
            "-p",
            "zellij-relay-server",
            "--",
            "create-token",
            "relay-dev",
        ])
        .env("RELAY_DATA_DIR", data_dir)
        .current_dir(crate::project_root())
        .stderr(Stdio::inherit())
        .output()?;
    if !out.status.success() {
        bail!("create-token exited with {}", out.status);
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .last()
        .map(|l| l.to_string())
        .context("create-token produced no token on stdout")
}

fn caddyfile_contents(
    authority: &str,
    relay_authority: &str,
    same_origin_relay: bool,
    app_dir: &Path,
) -> String {
    let head = "\
{
	admin off
	auto_https disable_redirects
	log {
		exclude pki.ca.local
	}
}

";
    let split_origin_site = "\
AUTHORITY {
	root * APP_DIR
	try_files {path} /index.html
	file_server
	tls internal
}

RELAY_AUTHORITY {
	reverse_proxy RELAY_BIND_ADDR
	tls internal
}
";
    let same_origin_site = "\
AUTHORITY {
	@relay path_regexp ^/(health$|tunnel/|r/[^/]+/)
	handle @relay {
		reverse_proxy RELAY_BIND_ADDR
	}
	handle {
		root * APP_DIR
		try_files {path} /index.html
		file_server
	}
	tls internal
}
";
    let site = if same_origin_relay {
        same_origin_site
    } else {
        split_origin_site
    };
    let mut tmpl = String::from(head);
    tmpl.push_str(site);
    tmpl.replace("RELAY_AUTHORITY", relay_authority)
        .replace("AUTHORITY", authority)
        .replace("APP_DIR", &app_dir.display().to_string())
        .replace("RELAY_BIND_ADDR", RELAY_BIND)
}

fn print_instructions(
    app_origin: &str,
    relay_authority: &str,
    port: u16,
    token: &str,
    lan_mode: bool,
    same_origin_relay: bool,
    ca_cert_path: &Path,
) {
    let line = "=".repeat(72);
    println!("\n{line}");
    println!("zellij relay-dev is starting");
    println!("{line}");
    println!("App origin (browser + native, ONE url):  {app_origin}");
    println!("Relay (browser, via caddy TLS):          wss://{relay_authority}");
    println!("Relay (sharer, direct):                  ws://{RELAY_BIND}");
    println!();
    println!("1. Start a sharer in another terminal:");
    println!();
    println!("     ZELLIJ_RELAY_TUNNEL_AUTH_TOKEN={token} \\");
    println!("       cargo x run -- options --relay-server-url ws://{RELAY_BIND}");
    println!();
    println!("2. Share it (Ctrl-o \u{2192} share, Online tab). The plugin prints ONE url,");
    println!("   the same shape as production \u{2014} used by both viewers:");
    println!("     {app_origin}/r/<slug>             (PIN share)");
    println!("     {app_origin}/r/<slug>#k=<secret>  (strong-code share)");
    println!();
    println!("3a. Browser: open that url in Firefox.");
    println!("3b. Native:  the SAME url, no flags:");
    println!("     cargo x run -- attach \"{app_origin}/r/<slug>\"");
    if lan_mode {
        println!();
        println!("LAN device access ({app_origin}):");
        println!("  - allow inbound TCP {port} in this machine's firewall");
        println!("  - install caddy's root CA on the device before opening the url:");
        println!("      {}", ca_cert_path.display());
        println!("    Android: Settings \u{2192} Security & privacy \u{2192} Encryption & credentials");
        println!("             \u{2192} Install a certificate \u{2192} CA certificate");
        println!("             (Firefox Android additionally needs 'Use third-party CA");
        println!("             certificates' enabled in its settings)");
        println!("    iOS:     send the file, install the profile, then Settings \u{2192} General");
        println!("             \u{2192} About \u{2192} Certificate Trust Settings \u{2192} enable full trust");
        if same_origin_relay {
            println!("  - IP-literal origin: app and relay share ONE origin; the viewer shows");
            println!("    the same-host security notice \u{2014} expected in this mode");
        }
    }
    println!();
    println!("Ctrl-C stops both the relay and caddy.");
    println!("{line}\n");
}

fn ensure_ca_trusted(root_crt: &Path, dev_dir: &Path) {
    for _ in 0..30 {
        if root_crt.is_file() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let Ok(ca_bytes) = std::fs::read(root_crt) else {
        eprintln!(
            "relay-dev: caddy CA not found at {} yet; native `attach` will need \
             --ca-cert <that path> until it is trusted.",
            root_crt.display()
        );
        return;
    };

    let fingerprint = sha256_hex(&ca_bytes);
    let marker = dev_dir.join(".ca-trusted");
    if std::fs::read_to_string(&marker).map(|m| m.trim() == fingerprint).unwrap_or(false) {
        return;
    }

    if Path::new("/etc/NIXOS").exists() {
        print_nixos_trust_guidance(root_crt);
        return;
    }

    println!();
    println!("relay-dev: installing caddy's local dev CA into the system trust store.");
    println!("This lets the native client (zellij attach) trust wss://relay.localhost");
    println!("over TLS \u{2014} the same way a public CA is trusted in production. It adds");
    println!("only caddy's root CA ({}) and will prompt for sudo.", root_crt.display());
    println!("Undo later with your platform's trust tool (or delete that cert + refresh).");
    println!();

    if install_ca_systemwide(root_crt) {
        let _ = std::fs::write(&marker, fingerprint);
    } else {
        let p = root_crt.display();
        eprintln!("relay-dev: no supported system trust tool found, CA not installed.");
        eprintln!("Trust it once with the command for your system, then re-run:");
        eprintln!("  Debian/Ubuntu: sudo cp '{p}' /usr/local/share/ca-certificates/zellij-relay-dev.crt && sudo update-ca-certificates");
        eprintln!("  Fedora/RHEL:   sudo cp '{p}' /etc/pki/ca-trust/source/anchors/zellij-relay-dev.crt && sudo update-ca-trust");
        eprintln!("  Arch/p11-kit:  sudo trust anchor --store '{p}'");
        eprintln!("  NixOS:         add to security.pki.certificateFiles in configuration.nix, then nixos-rebuild switch");
        eprintln!("Until then, native `attach` works with: --ca-cert {p}");
    }
}

fn print_nixos_trust_guidance(root_crt: &Path) {
    let p = root_crt.display();
    println!();
    println!("relay-dev: NixOS detected \u{2014} CA trust is declarative, so it cannot be");
    println!("installed imperatively. For a persistent, flag-free native client (the");
    println!("NixOS equivalent of trusting a production CA), add to configuration.nix:");
    println!();
    println!("  security.pki.certificateFiles = [ \"{p}\" ];");
    println!();
    println!("then `sudo nixos-rebuild switch`. After that, native `attach` works with");
    println!("the bare url, same as the browser.");
    println!();
    println!("Without a rebuild, pass the CA per-invocation instead:");
    println!("  cargo x run -- attach --ca-cert {p} \"<url>\"");
    println!("  (or: SSL_CERT_FILE={p} cargo x run -- attach \"<url>\")");
    println!();
}

fn install_ca_systemwide(root_crt: &Path) -> bool {
    let src = root_crt.display().to_string();

    if cfg!(target_os = "macos") {
        return sudo(&[
            "security",
            "add-trusted-cert",
            "-d",
            "-r",
            "trustRoot",
            "-k",
            "/Library/Keychains/System.keychain",
            &src,
        ]);
    }
    let with_sbin = "PATH=\"/usr/sbin:/usr/bin:/sbin:/bin:$PATH\"";
    if Path::new("/usr/local/share/ca-certificates").is_dir() {
        let dest = "/usr/local/share/ca-certificates/zellij-relay-dev.crt";
        return sudo(&[
            "sh",
            "-c",
            &format!("cp '{}' '{}' && {} update-ca-certificates", src, dest, with_sbin),
        ]);
    }
    if Path::new("/etc/pki/ca-trust/source/anchors").is_dir() {
        let dest = "/etc/pki/ca-trust/source/anchors/zellij-relay-dev.crt";
        return sudo(&[
            "sh",
            "-c",
            &format!("cp '{}' '{}' && {} update-ca-trust", src, dest, with_sbin),
        ]);
    }
    if which::which("trust").is_ok() {
        return sudo(&["trust", "anchor", "--store", &src]);
    }
    false
}

fn sudo(args: &[&str]) -> bool {
    Command::new("sudo")
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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

/// The data-home base directory caddy stores its state under, mirroring
/// caddy's own `AppDataDir()` precedence so we find its auto-generated local
/// CA. Caddy checks `XDG_DATA_HOME` first on every platform, then falls back
/// to the OS default — which on macOS is `~/Library/Application Support`, NOT
/// `~/.local/share`. Getting this wrong makes `ensure_ca_trusted` silently
/// no-op on macOS (CA "not found"), so the CA never lands in the system trust
/// store. The caller appends `caddy/pki/authorities/local/root.crt`.
fn caddy_data_home() -> std::path::PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return std::path::PathBuf::from(xdg);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if cfg!(target_os = "macos") {
        return std::path::PathBuf::from(home)
            .join("Library")
            .join("Application Support");
    }
    std::path::PathBuf::from(home).join(".local").join("share")
}

fn caddy_died_message(status: std::process::ExitStatus, port: u16) -> String {
    format!(
        "caddy exited immediately ({status}) \u{2014} nothing is serving the app origin.

Most likely another caddy already holds a port. The system `caddy` package
starts a service on boot; stop it and re-run:

  sudo systemctl stop caddy

Then check nothing else is bound:

  ss -ltnp | grep -E ':{port}|:2019'

`relay-dev` already runs caddy with `admin off`, so the default admin-port
(2019) clash should not recur once any stray caddy is stopped. The caddy
output above this line shows the exact bind error.

To test without caddy at all, use the native viewer (plain ws to the relay):
  cargo x run -- attach \"http://127.0.0.1:8765/r/<slug>#k=<secret>\""
    )
}

fn caddy_missing_message() -> String {
    "\
`caddy` was not found on PATH.

`cargo x relay-dev` uses caddy to serve the app origin and the relay over
local HTTPS (so the viewer's CSP and wss:// connections work without manual
TLS setup). Install it, then re-run:

  macOS:          brew install caddy
  Debian/Ubuntu:  sudo apt install caddy
  Arch:           sudo pacman -S caddy
  other:          https://caddyserver.com/docs/install

After installing, run `cargo x relay-dev` again."
        .to_string()
}

#[cfg(test)]
mod caddyfile_tests {
    use super::*;

    #[test]
    fn split_origin_uses_two_vhosts() {
        let out = caddyfile_contents(
            "zj.lan:8443",
            "relay.zj.lan:8443",
            false,
            Path::new("/tmp/app"),
        );
        assert!(out.contains("zj.lan:8443 {"));
        assert!(out.contains("relay.zj.lan:8443 {"));
        assert!(out.contains(&format!("reverse_proxy {}", RELAY_BIND)));
        assert!(!out.contains("AUTHORITY"));
    }

    #[test]
    fn same_origin_routes_relay_paths() {
        let out = caddyfile_contents(
            "192.168.1.50:8443",
            "192.168.1.50:8443",
            true,
            Path::new("/tmp/app"),
        );
        assert_eq!(out.matches("192.168.1.50:8443 {").count(), 1);
        assert!(out.contains("@relay path_regexp ^/(health$|tunnel/|r/[^/]+/)"));
        assert!(out.contains(&format!("reverse_proxy {}", RELAY_BIND)));
        assert!(out.contains("try_files {path} /index.html"));
        assert!(!out.contains("AUTHORITY"));
    }
}
