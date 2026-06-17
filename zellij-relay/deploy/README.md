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
   ssh root@<ip> true
   ```

The deploy script installs Docker on first run; nothing else is needed
on the VPS.

---

## Deploy

```sh
cd zellij-relay/deploy

./deploy.sh deploy \
    --vps-user  root \
    --le-email  you@zellij.online
```

`deploy` is the default command, so `./deploy.sh --vps-user … --le-email …`
works too.

Both the SSH/deploy target (`--vps-ip`) and the public hostname
(`--public-host`) default to `zellij.online` — the same host clients use by
default (see below), so a relay deployed there needs no client
`relay_server_url` configuration at all. Point a DNS `A` record for
`zellij.online` at the VPS **before** deploy, or both the SSH connection and
Let's Encrypt issuance fail.

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
zellij options --relay-server-url wss://zellij.online
```

or the equivalent `options { relay_server_url "wss://..."; }` line for
your KDL config. Since `wss://zellij.online` is the built-in default, this
step can be skipped entirely when deploying to the default host.

### Tip: shell alias for repeated runs

```sh
alias zr='./deploy.sh --vps-user root'
zr logs
zr ps
zr deploy --le-email you@example.com
```

## Use

In a Zellij session on your laptop:

1. Open the share plugin (`Ctrl-o` → share).
2. `t` → `n` to generate a read/write token. Record it.
3. Press `i` — within ~1 s the plugin shows
   `Public URL: https://zellij.online/r/<slug>`.
4. Open that URL in any browser. Paste the token. Live session.
5. Press `I` in the plugin to tear the tunnel down.

> Phase 6 Session C tightens tunnel establishment with a shared-secret
> auth token; see [Tunnel auth tokens](#tunnel-auth-tokens-phase-6-session-c)
> below before expecting `i` to open the tunnel cleanly.

## Tunnel auth tokens (Phase 6 Session C)

The relay rejects any `TunnelAuth` whose `token` hash is not in its
on-disk store, so operators must mint one on the relay host and
configure the sharer's Zellij to send it.

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

### Configure Zellij to use the token

`relay_server_url` defaults to `wss://zellij.online`, so it only needs to be
set when targeting a different relay (e.g. a `sslip.io` testbed or local dev).

Either persist in KDL:

```kdl
options {
    relay_server_url "wss://<ip-dashes>.sslip.io"
    relay_tunnel_auth_token "paste-token-here"
}
```

…or set via `zellij options`:

```sh
zellij options --relay-tunnel-auth-token "paste-token-here"
```

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

## Operate

All operational commands need `--vps-user` (and `--vps-ip` only when the
target is not the default `zellij.online`); together they drive the SSH
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

```sh
./deploy.sh --vps-user root --le-email you@zellij.online
```

(Rebuilds images, rolls containers. Cert bootstrap is a no-op when the
cert already exists.)

## Hostname / IP changes

When the public host stays `zellij.online`, moving to a new box only requires
re-pointing the DNS `A` record, then rerunning `deploy.sh` (the default SSH
target follows DNS automatically). The nginx image bakes the hostname in
at build time (because Let's Encrypt cert paths must be literal), so a change
to `--public-host` triggers an nginx rebuild and a fresh cert via
`bootstrap-cert.sh`.

## Without a domain (sslip.io)

To test without owning a domain, pass `--public-host <ip-with-dashes>.sslip.io`.
`sslip.io` resolves any hostname of that form to the embedded IP and is a real
DNS name from Let's Encrypt's perspective, so a valid CA-signed cert issues
without a registration. If the shared rate limit on `sslip.io` bites, `nip.io`
and `traefik.me` are drop-in alternatives.

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
