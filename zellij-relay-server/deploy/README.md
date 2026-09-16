# zellij-relay testbed deployment

One-command deploy of the relay behind nginx + Let's Encrypt on a VPS, driven
entirely from your laptop via an SSH Docker context.

The deployment supports two authentication modes:

| Mode | Tunnel credential source | Deployment input |
| --- | --- | --- |
| `standalone` (default) | Local SQLite token store on the relay | No additional configuration |
| `hosted` | HTTPS control-plane API | Control-plane URL and service-secret file |

Standalone remains the default so the open-source relay can be self-hosted
without the zellij.online website. Hosted mode is an explicit overlay; it does
not change the standalone image or token store.

This deploy provisions the **relay host only** (`relay.zellij.online`), which
is **transport-only**: it serves `/health`, `/tunnel/*`, and the per-slug
endpoints `/r/<slug>/{info/version,session,command/login,ws/*}` — **no HTML,
no static assets**. The browser viewer is served from a **separate app origin**
(`zellij.online`), provisioned out-of-band and documented in
`zellij-web-client-assets/deploy/README.md`.

> **End-to-end encrypted** — the relay routing your session never has access
> to its contents. The browser viewer is served from a separate origin
> (`zellij.online`); the relay forwards only ciphertext. The native client
> (`zellij attach <url>`) additionally protects against a compromised relay
> cryptographically. The browser viewer is trust-on-delivery against the app
> origin that serves it, not cryptographically protected against a malicious
> app-origin operator. See [`docs/THREAT_MODEL.md`](../../docs/THREAT_MODEL.md)
> for the full model.

---

## Two-origin topology

| Origin | Role | Provisioned by |
| --- | --- | --- |
| `relay.zellij.online` | Transport-only relay: ciphertext routing, token auth, version probe. Serves no document. | This deploy. |
| `zellij.online` | App origin: serves the static browser viewer; points its `/r/<slug>` at the relay. | `zellij-web-client-assets/deploy/` (out-of-band). |

CORS and `OPTIONS` preflight are owned by the **relay binary** (tower-http) via
`RELAY_ALLOWED_ORIGINS` (default `https://zellij.online`), not by nginx. nginx
proxies `OPTIONS` through untouched and passes the relay's upstream
`Access-Control-*` headers transparently — it never adds its own.

---

## Prereqs (laptop)

- `docker` (25+ for SSH-based contexts)
- `ssh`
- `curl`

## Prereqs (VPS)

One manual step, done once:

1. Order an OVHCloud **VPS Starter** with the **Debian 13** image. Paste
   your laptop's SSH public key during provisioning.
2. When the instance email arrives, record the public IPv4.
3. Confirm you can reach it:
   ```
   ssh root@<ip> true
   ```

The deploy script installs Docker on first run; nothing else is needed
on the VPS.

---

## Deploy

### Standalone self-hosting

```sh
cd zellij-relay-server/deploy

./deploy.sh deploy \
    --vps-user  root \
    --le-email  you@zellij.online
```

`deploy` is the default command, so `./deploy.sh --vps-user … --le-email …`
works too.

### Hosted control plane

Hosted mode validates each tunnel credential with an HTTPS control plane and
sends tunnel lifecycle events to it. Put the shared service secret in a
non-empty file **outside the repository/build context**, then run:

```sh
./deploy.sh deploy \
    --mode hosted \
    --control-plane-url https://control-plane.example.com \
    --control-plane-secret-file /secure/path/relay-service-secret \
    --vps-user root \
    --le-email you@example.com
```

The script streams the secret over the Docker SSH connection into the external
`zellij-relay_control-plane-secret` volume. Inside the relay it is mounted
read-only at `/run/secrets/relay-control-plane` and read through
`RELAY_CONTROL_PLANE_SECRET_FILE`. The secret is not placed in the image,
container environment, Compose configuration, command arguments, or logs.

The URL must use HTTPS. Both hosted arguments are mandatory, and partial
configuration fails before the running stack is replaced. Once a hosted relay
is running, an unqualified `deploy` is rejected; pass `--mode hosted` to update
it or explicitly pass `--mode standalone` to switch authentication modes.

Both the SSH/deploy target (`--vps-ip`) and the relay host (`--public-host`)
default to `relay.zellij.online` — the relay host clients use by default. Point
a DNS `A` record for `relay.zellij.online` at the VPS **before** deploy, or both
the SSH connection and Let's Encrypt issuance fail.

The app origin (`zellij.online`) is configured separately. The relay derives
its public URL template and CORS allowlist from `APP_ORIGIN` (default
`https://zellij.online`); override it if the viewer is served elsewhere:

```sh
APP_ORIGIN="https://viewer.example.com" ./deploy.sh deploy \
    --vps-user root --le-email you@example.com
```

### Different host or testbed

To deploy by raw IP (e.g. first bring-up before DNS is pointed at the box) or
to serve from a zero-DNS `sslip.io` testbed, pass `--vps-ip` and/or
`--public-host`:

```sh
./deploy.sh deploy \
    --vps-ip       203.0.113.42 \
    --vps-user     root \
    --le-email     you@example.com \
    --public-host  203-0-113-42.sslip.io
```

First run takes a few minutes (Docker install + relay build + cert
issuance). Re-runs are seconds for a no-op, a minute or two when
relay code changes.

The script prints the local Zellij command at the end:

```
zellij options --relay-server-url wss://relay.zellij.online
```

or the equivalent `options { relay_server_url "wss://..."; }` line for
your KDL config. Since `wss://relay.zellij.online` is the built-in default,
this step can be skipped entirely when deploying to the default relay host.

### Tip: shell alias for repeated runs

```sh
alias zr='./deploy.sh --vps-user root'
zr logs
zr ps
zr deploy --le-email you@example.com                 # standalone
zr deploy --mode hosted --control-plane-url https://control-plane.example.com \
  --control-plane-secret-file /secure/relay-secret --le-email you@example.com
```

## Use

In a Zellij session on your laptop:

1. Open the share plugin (`Ctrl-o` → share).
2. `t` → `n` to generate a read/write token. Record it.
3. Press `i` — within ~1 s the plugin shows
   `Public URL: https://zellij.online/r/<slug>`. This points at the **app
   origin** that serves the viewer, not the relay host.
4. Open that URL — `https://zellij.online/r/<slug>#k=<secret>` — in any
   browser. The fragment secret (`#k=…`) never leaves the browser. Paste the
   token. Live session. The viewer's `/ws/*` traffic is routed by the
   transport-only relay host.
5. Press `I` in the plugin to tear the tunnel down.

## Tunnel auth tokens

In standalone mode, the relay rejects any `TunnelAuth` whose token hash is not
in its on-disk store. In hosted mode, it validates the credential with the
configured control plane and fails closed if verification cannot complete.

### Manage tokens on the relay host

Tokens live in `$RELAY_DATA_DIR/relay_tunnel_auth_tokens.db`
(`/var/lib/zellij-relay/relay_tunnel_auth_tokens.db` by default), on
the `relay-data` named volume so the DB survives container
recreation. `deploy.sh` exposes wrapper subcommands that drive the
relay binary inside the running container via the same SSH docker
context as the rest of the script:

```sh
./deploy.sh create-token my-laptop --vps-user root
./deploy.sh list-tokens             --vps-user root
./deploy.sh revoke-token my-laptop  --vps-user root
```

The label is also accepted via `--label <name>` if a positional value
is awkward in your shell.

`create-token` prints the raw token **once** — store it securely. Only
the SHA-256 hash is written to disk.

These three commands intentionally fail with guidance when the running relay
uses hosted mode. Create and revoke hosted credentials through the control
plane instead.

### Configure Zellij to use the token

`relay_server_url` defaults to `wss://relay.zellij.online`, so it only needs to
be set when targeting a different relay (e.g. a `sslip.io` testbed or local
dev).

The token is a static bearer credential — it cannot be set in the KDL config
file or via a CLI flag, keeping it out of cleartext config and out of shell
history / process arguments. Zellij resolves it in this order, first hit wins:

1. `ZELLIJ_RELAY_TUNNEL_AUTH_TOKEN` environment variable.
2. `ZELLIJ_RELAY_TUNNEL_AUTH_TOKEN_FILE` — path to a `0600` file holding the
   token (Zellij warns if the file is readable by other users).
3. The in-memory runtime slot (the share plugin's inline `A` / `i` prompt),
   which lives only in the running session and is never written to disk.

The environment variable is the cross-platform secret-store integration point:
populate it from your OS keychain at launch via command substitution, so the
secret lives only in the keychain and the process environment.

```sh
# macOS Keychain
ZELLIJ_RELAY_TUNNEL_AUTH_TOKEN="$(security find-generic-password -s zellij -a relay-token -w)" zellij
# Linux Secret Service
ZELLIJ_RELAY_TUNNEL_AUTH_TOKEN="$(secret-tool lookup service zellij key relay-token)" zellij
# any secret manager: pass / op / bw / vault, etc.
ZELLIJ_RELAY_TUNNEL_AUTH_TOKEN="$(pass show zellij/relay-token)" zellij
```

This works on headless servers too and adds no in-binary keyring dependency.

### Inline prompt flow in the share plugin

If Zellij has no token configured (or the relay rejects the configured
one), pressing `i` surfaces the `<relay rejected auth token>` row.
Pressing `i` again opens an inline prompt — paste the token, press
`Enter`, and the tunnel opens in one keystroke flow. `A` rotates the
token at any time without waiting for a rejection.

### Revocation propagation

Revoking a viewer token via the share plugin's `x` / `Ctrl-X` path
emits a `RevokeToken` control frame to every active relay tunnel.
The relay force-disconnects every viewer whose session was keyed on
that hash and drops the r/o fan-out group so a subsequent viewer with
the same (now-revoked) raw token fails at the auth step.

## TLS key pinning (SPKI / TOFU)

The native attach client (`zellij attach <url>`) pins the relay's TLS public key
on first connect (trust-on-first-use, SSH `known_hosts` style) and refuses to
connect if the key later changes without an explicit re-pin. The pin is over the
**SubjectPublicKeyInfo (SPKI)**, not the whole cert, so it **survives Let's
Encrypt's ~90-day renewals as long as the key is reused** — only a real key
rotation (or an actual MITM) trips the warning.

Key reuse is made automatic here:

- `bootstrap-cert.sh` runs `certonly … --reuse-key`, which persists
  `reuse_key = True` into the renewal config on the `letsencrypt` volume.
- `compose.yml`'s certbot loop runs `certbot renew --reuse-key …` as a
  belt-and-suspenders default even if that renewal config is ever wiped.

**Ship this reuse-key config before clients start pinning** (or simultaneously),
so the first post-pin renewal keeps the key and never false-alarms.

### Read the current pin

`print-spki.sh` reads the pin from the live cert for out-of-band publishing or
verification:

```sh
PUBLIC_HOST=relay.zellij.online ./print-spki.sh --vps-user root
# sha256//BASE64…
```

(The value matches the `sha256//…` fingerprint the client stores and prints.)

### Rotation runbook

When you deliberately rotate the relay's key (`certbot … --force-renewal` without
`--reuse-key`, or a new keypair), the SPKI changes and every pinned client
refuses the next connect with an old/new fingerprint warning. To recover:

1. Publish the new pin (`./print-spki.sh`) so operators can verify out-of-band.
2. Each client re-pins with `zellij attach <url> --repin` (validates the cert
   normally, then replaces the stored pin). `--forget` also clears the pin.

## Operate

All operational commands need `--vps-user` (and `--vps-ip` only when the
target is not the default `relay.zellij.online`); together they drive the SSH
docker context:

```sh
./deploy.sh logs    --vps-user root
./deploy.sh logs    --vps-user root --service relay
./deploy.sh ps      --vps-user root
./deploy.sh restart --vps-user root
./deploy.sh destroy --vps-user root   # prompts
```

Under the hood every command runs via `DOCKER_HOST=ssh://…` — no shell
sessions on the VPS. The compose project name is fixed at `zellij-relay`.

## Redeploying after code changes

Standalone:

```sh
./deploy.sh --vps-user root --le-email you@zellij.online
```

Hosted:

```sh
./deploy.sh --mode hosted \
    --control-plane-url https://control-plane.example.com \
    --control-plane-secret-file /secure/relay-service-secret \
    --vps-user root --le-email you@zellij.online
```

Both commands rebuild the current local checkout and recreate the containers.
Certificate bootstrap is a no-op when the certificate already exists. Active
tunnels disconnect during the recreation.

### Hosted service-secret rotation

1. Configure the control plane to accept both the old and new service secrets.
2. Store the new secret in a protected local file outside this repository.
3. Redeploy in hosted mode using the new file.
4. Verify tunnel creation, rejection, revocation, and lifecycle events.
5. Remove the old secret from the control plane.

The volume update is atomic: an interrupted write leaves the previously
installed secret available. Ordinary redeploys preserve the volume.

### Rollback

Check out the previously working commit and rerun the same deployment mode.
The certificate, standalone token database, and hosted secret volumes survive
container recreation. Do not use `destroy` for rollback: it deliberately
deletes all three after an explicit confirmation.

## Hostname / IP changes

When the relay host stays `relay.zellij.online`, moving to a new box only
requires re-pointing the DNS `A` record, then rerunning `deploy.sh` (the
default SSH target follows DNS automatically). The nginx image bakes the relay
hostname in at build time (because Let's Encrypt cert paths must be literal),
so a change to `--public-host` triggers an nginx rebuild and a fresh cert via
`bootstrap-cert.sh`. Changing the app origin is independent: set `APP_ORIGIN`
and redeploy — no cert change on the relay host.

## Without a domain (sslip.io)

To test without owning a domain, pass `--public-host <ip-with-dashes>.sslip.io`.
`sslip.io` resolves any hostname of that form to the embedded IP and is a real
DNS name from Let's Encrypt's perspective, so a valid CA-signed cert issues
without a registration. If the shared rate limit on `sslip.io` bites, `nip.io`
and `traefik.me` are drop-in alternatives.

## Files

| File | Role |
| --- | --- |
| `Dockerfile` | Multi-stage build of the `zellij-relay-server` binary |
| `Dockerfile.dockerignore` | Keeps the SSH context small and excludes local secret material |
| `nginx/Dockerfile` | Bakes the relay host (`PUBLIC_HOST`) into `nginx.conf` via `envsubst` |
| `nginx/nginx.conf.template` | TLS termination, WS upgrade, transport-only rate limits |
| `nginx/proxy-ws.conf` | Shared proxy + upgrade snippet |
| `compose.yml` | Standalone `relay`, `nginx`, and `certbot` stack |
| `compose.hosted.yml` | Hosted-only control-plane environment and secret-volume overlay |
| `deploy.sh` | One-click standalone/hosted deploy, logs, restart, token management, destroy |
| `bootstrap-cert.sh` | Idempotent LetsEncrypt bootstrap (used by `deploy.sh`) |
| `print-spki.sh` | Print the SPKI SHA-256 pin (`sha256//…`) from the live cert |

## End-to-end verification

After `deploy.sh` reports healthy:

1. `curl -v https://relay.zellij.online/health` — LE cert chain valid, body
   `ok`.
2. `curl -i https://relay.zellij.online/r/<slug>` — the relay serves **no**
   document at the bare slug; only `/info/version`, `/session`,
   `/command/login`, and `/ws/*` are routed under a slug.
3. Browser at the **app origin** on a different network
   (`https://zellij.online/r/<slug>#k=<secret>`):
   - Wrong token → identical 401 to an unknown slug (enumeration-safe).
   - Correct token → interactive session, typing works, resize reflows.
   - CORS preflight (`OPTIONS`) to the relay succeeds and carries the relay's
     `Access-Control-Allow-Origin: https://zellij.online` (no nginx
     duplication).
4. `./deploy.sh logs --service relay --vps-user root` during the session:
   - `ClientConnected` on tab open.
   - `ClientDisconnected` on tab close.
5. Press `I` in the plugin → both tunnel WS close cleanly in the relay log.
6. Confirm ciphertext on the wire: `websocat wss://relay.zellij.online/...`
   shows opaque bytes, not session contents.

Authentication-mode checks:

- Standalone: create a local token, open a tunnel with it, revoke it, and
  confirm the next tunnel attempt is rejected.
- Hosted: create a control-plane credential, confirm the tunnel appears and
  disappears in the control plane, revoke it, and confirm the next attempt is
  rejected. Temporarily make the control plane unavailable and confirm tunnel
  creation fails closed.
- Hosted secret hygiene: inspect the relay container environment, image
  history, and logs and confirm the service-secret value is absent.
