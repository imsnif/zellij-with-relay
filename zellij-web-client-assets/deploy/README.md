# Deploying the Zellij app origin

This directory documents how to deploy the **app origin** — the static site that
serves the browser viewer. It is a plain static site built by:

```sh
cargo x build \
  --app-origin out/ \
  --app-host app.example.com
```

By default the viewer targets `https://relay.<app-host>`. Deployments whose app
and relay do not follow that paired-domain convention can pin a separate relay
origin at build time:

```sh
cargo x build \
  --app-origin out/ \
  --app-host viewer-project.pages.dev \
  --relay-origin https://relay.example.com
```

The app origin serves *only* viewer code (HTML/JS/wasm/css/icons). It performs
no transport and holds no session keys. It MUST live on an origin **distinct
from any relay** (`zellij.online` for the app, `relay.zellij.online` or a
self-hosted host for transport). Keeping code-serving and data-relaying under
separate operators is part of the security model — see
[`docs/THREAT_MODEL.md`](../../docs/THREAT_MODEL.md).

DNS, TLS, and CDN provisioning are out-of-band operations and are intentionally
out of scope of the code in this repo. This doc describes what the host must do;
it does not automate it.

## The artifact

`cargo x build --app-origin out/` produces a self-contained static site split
into an unversioned **handshake core** and a versioned **application bundle**:

- `out/index.html` — the core bootstrap shell. Placeholders resolved
  (`BASE_URL=/`, `AUTH_MODE=relay`, `EXPECTED_E2E=true`), a
  `<meta http-equiv="Content-Security-Policy">` baked in as a fallback, and a
  trusted `<meta name="zellij-relay-origin">` containing the validated
  build-time HTTP origin. The browser derives the matching WebSocket origin
  from this value; share-link query and fragment data cannot override it. The
  file also has `integrity="sha384-…" crossorigin="anonymous"` on every
  `<script src="assets/…">` and stylesheet `<link href="assets/…">`.
- `out/assets/…` — the **handshake core** only: bootstrap/loader, SPAKE2 + AEAD
  crypto (`crypto.js`, `relay_crypto.wasm`, `integrity.js`), URL parsing, modals,
  the relay PAKE flow (`auth.js`), reconnection (`connection.js`),
  `core-control.js` (opens the control channel and reads the authenticated
  `VersionAnnounce`), and base styling. Frozen across releases.
- `out/assets/integrity.js` — generated wasm integrity manifest for the core
  (`relay_crypto.wasm`); the JS loader verifies fetched wasm bytes against it
  before `WebAssembly.instantiate` (SRI cannot ride a `fetch()`).
- `out/v/<version>/assets/app.js` — the consolidated, versioned application
  bundle for this release, accompanied by xterm assets/addons and `clip.wasm`.
  Everything that drifts between releases lives below this versioned path.
- `out/v/<version>/app-manifest.json` — `[{ "name", "integrity" }]` over the
  application assets. The core fetches it, recomputes a rolled-up digest, and
  requires it to equal the digest the sharer attests over the E2E control
  channel before loading any bundle asset (each pinned by its manifest SRI).
- `out/v/<version>/app_bundle_sha384.txt` — the rolled-up SHA-384 over the
  application manifest, equal to the value the release binary attests
  (`zellij_web_client_assets::app_bundle_sha384`); staging aborts on a mismatch.
- `out/v/<version>/assets/integrity.js` — wasm integrity manifest for the
  bundle (`clip.wasm`).
- `out/RELEASE_HASHES.txt` — `sha256sum -c`-compatible manifest of every served
  viewer file (core + `v/<version>/…`). Published as a GitHub release asset;
  used by the verification recipe. Static-host configuration files are
  intentionally excluded because hosts such as Cloudflare Pages consume them
  as configuration rather than serving them at those paths.
- `out/_redirects` and `out/_headers` — generated static-host configuration for
  the `/r/*` SPA rewrite and security headers. Cloudflare Pages and compatible
  hosts consume them; other static hosts may ignore them and configure the
  equivalent behavior separately.

A single `cargo x build --app-origin out/` clears `out/` and emits only the
**current** release's core + `v/<version>/`. Accumulation of older bundles
happens at **deploy time** (overlay, see below), not in one build.

## SPA rewrite

Join URLs have the form `https://<app-origin>/r/<slug>` (with the secret in the
`#k=` fragment, which the browser never sends to the server). The host must:

- serve `index.html` for **every `/r/*` path** (the rewrite is secret-safe
  precisely because the fragment is never transmitted);
- serve `/assets/…` and `/v/<version>/…` literally;
- **never delete** old `/v/<version>/` directories on deploy — overlay the new
  build's tree onto the live site so a viewer whose sharer runs an older Zellij
  can still fetch its matching bundle. The core + `index.html` are replaced;
  the versioned bundles only ever accumulate.

## Two-stage trust

- **Handshake core** (`index.html` + `/assets/…`): irreducibly
  trust-on-delivery — something must run before E2E exists. Mitigated by being
  small, frozen, URL-pinned, SRI-pinned, reproducible, and verifiable with
  `scripts/verify-app-origin.sh`.
- **Application bundle** (`/v/<version>/…`): verified against a digest the
  **sharer attests over the E2E control channel** (`VersionAnnounce`), so a
  compromised app origin cannot backdoor it undetected — a byte mismatch fails
  the rolled-up-digest check or the per-asset SRI and the viewer refuses.

## Required response headers

Even though the artifact bakes a CSP `<meta>` fallback into `index.html`, the
host **should also send these as real HTTP response headers**. Some directives
(`frame-ancestors`, HSTS) cannot be set via `<meta>` and therefore **must** be
headers:

```
Content-Security-Policy: default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self' https://relay.example.com wss://relay.example.com; manifest-src 'self'; frame-ancestors 'none'; base-uri 'self'; object-src 'none'
Strict-Transport-Security: max-age=63072000; includeSubDomains
X-Content-Type-Options: nosniff
X-Frame-Options: DENY
Referrer-Policy: no-referrer
```

Notes:
- `'wasm-unsafe-eval'` is required to instantiate the SPAKE2/clip wasm.
- `style-src 'unsafe-inline'` is required by xterm.js.
- `base-uri 'self'` permits the generated `<base href="/">` needed when the
  viewer shell is served from `/r/<slug>`, while still rejecting cross-origin
  base URLs.
- `connect-src` names only the app origin and the deployment's relay host.
  The baked CSP and browser runtime metadata are generated from the same
  validated relay configuration. Without `--relay-origin`, the relay is
  derived from the host passed at stage time (`--app-host <your-host>`, default
  `zellij.online`). Self-hosted origins **must** stage with their own app host
  or explicit relay origin.
- Remote explicit relay origins must use `https://` or `wss://`. HTTP/WS is
  accepted only for loopback development. Credentials, non-root paths,
  queries, fragments, malformed hosts, and ambiguous ports are rejected.

## Per-release pinning and verification

Pin the app origin to a specific release tag. Publish that release's
`RELEASE_HASHES.txt` (it is a release asset) and point users at:

- [`docs/THREAT_MODEL.md`](../../docs/THREAT_MODEL.md) — what is and is not
  protected, including the trust-on-delivery caveat for the browser tier.
- [`scripts/verify-app-origin.sh`](../../scripts/verify-app-origin.sh) — one-step
  check that the live origin serves exactly the published bytes:

  ```sh
  scripts/verify-app-origin.sh <tag>
  ```

## Cloudflare Pages recipe (reference)

This is the reference deployment.

1. **Build the artifact:**

   ```sh
   cargo x build \
     --app-origin out/ \
     --app-host viewer-project.pages.dev \
     --relay-origin https://relay.example.com
   ```

2. **Publish the generated host config.** Cloudflare Pages reads `_redirects`
   and `_headers` directly from the output directory. The header-level
   `connect-src`, baked CSP, and trusted runtime target all come from the same
   build-time relay configuration. A crafted share link cannot change them.
   These two configuration files are intentionally absent from
   `RELEASE_HASHES.txt`; verify them through deployed behavior instead: request
   `/r/test-slug`, confirm it serves `index.html`, and inspect the response for
   the generated CSP, HSTS, `nosniff`, frame-denial, and referrer-policy
   headers.

3. **Deploy:**

   ```sh
   wrangler pages deploy out/ --project-name zellij-app
   ```

   Cloudflare provisions TLS automatically.

4. **Mandatory: disable all content rewriting on the zone.** Turn OFF
   **Rocket Loader**, **Auto Minify** (JS/CSS/HTML), **Email Obfuscation**, and
   **Mirage**. Each mutates served bytes, which breaks Subresource Integrity and
   the verification recipe. After deploy, verify:

   ```sh
   scripts/verify-app-origin.sh <tag>
   ```

5. **Keep code and transport separate.** Host the app origin on a **different
   Cloudflare account/zone than the relay** — or keep the relay off Cloudflare
   entirely. One entity fronting both code-serving and data-relaying erodes the
   origin separation the model depends on.

### Trust note

A CDN that terminates TLS for the app origin (Cloudflare here) can see and
rewrite the bytes it serves, and therefore **joins the app-origin trust set**
(trust-on-delivery). The mitigations are **Subresource Integrity** on every
asset and the **verification recipe** — they make a tampered or coerced CDN
*detectable*, not impossible. The **native client** (`zellij attach`) does not
load any app-origin code and is unaffected, and **terminal-plaintext
confidentiality** (end-to-end AES-256-GCM) is unaffected regardless of who
fronts the app origin.
