# zellij-relay testbed deployment

One-command deploy of the relay behind nginx + Let's Encrypt on an OVHCloud
VPS, driven entirely from your laptop via an SSH docker context.

**Status:** interim testbed for Phase 1–2 of the remote-share design
(`initial_remote_share.md`). Phase 6 will replace `nginx/nginx.conf.template`
with the production reference config; the layout here anticipates that.

> **⚠️  Phase 3 (E2E encryption) is not yet delivered.** Terminal content
> traverses the relay in cleartext. Only share with parties you trust to
> read the shared session's contents — treat the VPS as trusted infra
> until Phase 3 lands.

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
   ssh debian@<ip> true
   ```

The deploy script installs Docker on first run; nothing else is needed
on the VPS.

---

## Deploy

```sh
cd zellij-relay/deploy

./deploy.sh deploy \
    --vps-ip    203.0.113.42 \
    --vps-user  debian \
    --le-email  you@example.com
```

`deploy` is the default command, so `./deploy.sh --vps-ip … --vps-user … --le-email …`
works too.

First run takes a few minutes (Docker install + relay build + cert
issuance). Re-runs are seconds for a no-op, a minute or two when
relay code changes.

The script prints the local Zellij command at the end:

```
zellij options --relay-server-url wss://<ip-dashes>.sslip.io
```

or the equivalent `options { relay_server_url "wss://..."; }` line for
your KDL config.

### Tip: shell alias for repeated runs

```sh
alias zr='./deploy.sh --vps-ip 203.0.113.42 --vps-user debian'
zr logs
zr ps
zr deploy --le-email you@example.com
```

## Use

In a Zellij session on your laptop:

1. Open the share plugin (`Ctrl-o` → share).
2. `t` → `n` to generate a read/write token. Record it.
3. Press `i` — within ~1 s the plugin shows
   `Public URL: https://<ip-dashes>.sslip.io/r/<slug>`.
4. Open that URL in any browser. Paste the token. Live session.
5. Press `I` in the plugin to tear the tunnel down.

## Operate

All operational commands need `--vps-ip` and `--vps-user` (they drive the
SSH docker context):

```sh
./deploy.sh logs    --vps-ip 203.0.113.42 --vps-user debian
./deploy.sh logs    --vps-ip 203.0.113.42 --vps-user debian --service relay
./deploy.sh ps      --vps-ip 203.0.113.42 --vps-user debian
./deploy.sh restart --vps-ip 203.0.113.42 --vps-user debian
./deploy.sh destroy --vps-ip 203.0.113.42 --vps-user debian   # prompts
```

Under the hood every command runs via `DOCKER_HOST=ssh://…` — no shell
sessions on the VPS. The compose project name is fixed at `zellij-relay`.

## Redeploying after code changes

```sh
./deploy.sh --vps-ip 203.0.113.42 --vps-user debian --le-email you@example.com
```

(Rebuilds images, rolls containers. Cert bootstrap is a no-op when the
cert already exists.)

## Hostname / IP changes

If the VPS IP changes, update `VPS_IP` in `.env` and rerun. The nginx
image bakes the hostname in at build time (because Let's Encrypt cert
paths must be literal), so `deploy.sh` rebuilds nginx and
`bootstrap-cert.sh` issues a fresh cert for the new hostname.

## Why sslip.io?

The plan avoids registering a domain. `sslip.io` resolves any hostname
of the form `<ip-with-dashes>.sslip.io` to the embedded IP. It is a
real DNS name from Let's Encrypt's perspective, so you get a valid
CA-signed cert without owning a domain. If the shared rate limit on
`sslip.io` bites, `nip.io` and `traefik.me` are drop-in alternatives —
edit the `PUBLIC_HOST` derivation in `deploy.sh`.

## Files

| File | Role |
| --- | --- |
| `Dockerfile` | Multi-stage build of the `zellij-relay` binary |
| `Dockerfile.dockerignore` | Keeps the SSH context upload small |
| `nginx/Dockerfile` | Bakes `PUBLIC_HOST` into `nginx.conf` via `envsubst` |
| `nginx/nginx.conf.template` | TLS termination, WS upgrade, Phase-1 rate limit, Phase-6 TODOs |
| `nginx/proxy-ws.conf` | Shared proxy + upgrade snippet |
| `compose.yml` | `relay`, `nginx`, `certbot` services + two named volumes |
| `deploy.sh` | One-click deploy, logs, restart, destroy |
| `bootstrap-cert.sh` | Idempotent LetsEncrypt bootstrap (used by `deploy.sh`) |

## End-to-end verification (Phase 1–2)

After `deploy.sh` reports healthy:

1. `curl -v https://<host>/health` — LE cert chain valid, body `ok`.
2. Browser on a different network:
   - Wrong token → identical 401 to an unknown slug (enumeration-safe).
   - Correct token → interactive session, typing works, resize reflows.
3. `./deploy.sh logs relay` during the session:
   - `ClientConnected` on tab open.
   - `ClientDisconnected` on tab close.
4. Press `I` in the plugin → both tunnel WS close cleanly in the relay log.
5. Open two concurrent sessions with two different tokens — ids stay routed
   correctly (Zellij is authoritative for client ids by design).

## Forward path

- **Phase 3 (E2E)**: rebuild + redeploy the relay; verify with
  `websocat wss://<host>/...` that bytes on the wire are ciphertext.
- **Phase 4 (r/o fan-out)**: no infra change; test with N tabs and an r/o token.
- **Phase 5 (`zellij attach <url>`)**: no infra change; run it from a
  second machine.
- **Phase 6 (hardening)**: fill in the `TODO(phase-6)` markers in
  `nginx/nginx.conf.template`; tighten `proxy_read_timeout` once the
  heartbeat is in place; redeploy.
