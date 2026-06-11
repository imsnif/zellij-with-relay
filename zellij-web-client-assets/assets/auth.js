/**
 * Authentication logic and token management
 */

import { getBaseUrl } from "./utils.js";
import { sha256Hex, deriveKey } from "./crypto.js";

/**
 * Hosts that always operate behind an E2E-enforcing relay. Any URL whose
 * hostname (or a subdomain thereof) matches one of these entries has its
 * `expectedE2e` flag forced to `true`, independent of the hidden form
 * field on the challenge page. A compromised relay serving
 * `EXPECTED_E2E=false` is therefore caught before any STDIN is sent.
 */
const KNOWN_RELAY_HOSTS = ["zellij.dev"];

/**
 * Returns true if the current page's URL is a known-relay URL. Exact
 * match or `.<host>` suffix so `relay.zellij.dev` and `my.zellij.dev`
 * are recognised but an unrelated `zellij.dev.evil.com` is not.
 */
function pageIsOnKnownRelay() {
    const host = location.hostname.toLowerCase();
    for (const r of KNOWN_RELAY_HOSTS) {
        if (host === r || host.endsWith("." + r)) {
            return true;
        }
    }
    return false;
}

/** Read the challenge-page `expectedE2e` claim, forcing true on known relays. */
function readExpectedE2e() {
    if (pageIsOnKnownRelay()) {
        return true;
    }
    const el = document.getElementById("zellij-expected-e2e");
    if (!el) return false;
    return el.value === "true";
}

/**
 * Read the server-asserted auth-flow profile from the
 * `zellij-auth-mode` hidden input. Returns "relay" or "local";
 * defaults to "local" when the value is missing or unrecognised.
 */
function getAuthMode() {
    const el = document.getElementById("zellij-auth-mode");
    const v = el && el.value;
    return v === "relay" ? "relay" : "local";
}

/**
 * Wait for user to provide a security token
 * @returns {Promise<{token: string, remember: boolean}>}
 */
async function waitForSecurityToken() {
    let token = null;
    let remember = false;

    while (!token) {
        let result = await getSecurityToken();
        if (result) {
            token = result.token;
            remember = !!result.remember;
        } else {
            await showErrorModal(
                "Error",
                "Must provide security token in order to log in."
            );
        }
    }

    return { token, remember };
}

/**
 * Try to silently fetch a saved credential via the Credential
 * Management API. Returns the token string on success, `null` on
 * unsupported browsers (Safari / Firefox) or when no credential is
 * saved. With `mediation: 'optional'` Chromium / Edge / Brave / Opera
 * may show a credential picker; an empty result means the user
 * dismissed it or no credential matched.
 */
async function tryGetSavedCredential() {
    if (typeof PasswordCredential === "undefined" ||
        !navigator.credentials ||
        !navigator.credentials.get) {
        return null;
    }
    try {
        const cred = await navigator.credentials.get({
            password: true,
            mediation: "optional",
        });
        if (cred && cred.type === "password" && cred.password) {
            return cred.password;
        }
    } catch (_) {
        // Some browsers throw on unrecognised options; fall through.
    }
    return null;
}

/**
 * Persist a successful credential in the browser's password manager.
 * On Chromium this is the imperative path; on Safari / Firefox the
 * form-submit heuristic in `getSecurityToken` produces the same
 * "Save password?" prompt, so this is a no-op there.
 */
async function saveCredential(token) {
    if (typeof PasswordCredential === "undefined" ||
        !navigator.credentials ||
        !navigator.credentials.store) {
        return;
    }
    try {
        const cred = new PasswordCredential({
            id: getCredentialId(),
            password: token,
        });
        await navigator.credentials.store(cred);
    } catch (_) {
        // Best-effort: failures are silent.
    }
}

/**
 * Get client ID from server after authentication
 * @param {string} token - Authentication token
 * @param {boolean} rememberMe - Local-mode Remember-me preference; ignored in relay mode
 * @param {boolean} hasAuthenticationCookie - Whether auth cookie exists
 * @returns {Promise<{webClientId: string, e2e: ?{key: CryptoKey}} | null>} null on failure
 */
export async function getClientId(token, rememberMe, hasAuthenticationCookie, expectedE2e) {
    const baseUrl = getBaseUrl();

    if (!hasAuthenticationCookie) {
        // In relay mode `remember_me` has no server-side effect (no
        // persistent cookie path) and the field is not part of the relay
        // login contract, so it is omitted entirely. In local mode the
        // flag is forwarded so the standalone web-client can issue a
        // persistent cookie when requested.
        const loginBody = getAuthMode() === "local"
            ? { auth_token: token, remember_me: !!rememberMe }
            : { auth_token: token };
        let login_res = await fetch(`${baseUrl}/command/login`, {
            method: "POST",
            headers: {
                "Content-Type": "application/json",
            },
            body: JSON.stringify(loginBody),
            credentials: "include",
        });

        if (login_res.status === 401) {
            await showErrorModal(
                "Error",
                "Unauthorized or revoked login token."
            );
            return null;
        } else if (!login_res.ok) {
            await showErrorModal(
                "Error",
                `Error ${login_res.status} connecting to server.`
            );
            return null;
        }
    }

    let data = await fetch(`${baseUrl}/session`, {
        method: "POST",
        headers: {
            "Content-Type": "application/json",
        },
        body: JSON.stringify({}),
    });

    if (data.status === 401) {
        await showErrorModal("Error", "Unauthorized or revoked login token.");
        return null;
    } else if (!data.ok) {
        await showErrorModal(
            "Error",
            `Error ${data.status} connecting to server.`
        );
        return null;
    }

    let body = await data.json();
    const serverE2e = body.e2e_encrypted === true;

    // Cross-check: the server's claim must not be weaker than what the
    // page (or known-relay override) advertised. Stronger is allowed so
    // clients can opportunistically upgrade.
    if (expectedE2e && !serverE2e) {
        await showErrorModal(
            "Refused",
            "This session was advertised as end-to-end encrypted, but the server does not confirm it. Refusing to connect."
        );
        return null;
    }

    let e2e = null;
    if (serverE2e) {
        // Derive the same key the server derived: HKDF over the
        // hex-encoded SHA-256 of the raw token, with info=tunnel_id.
        if (!body.tunnel_id || typeof body.tunnel_id !== "string") {
            await showErrorModal(
                "Error",
                "Server did not return a tunnel id; cannot enable encryption."
            );
            return null;
        }
        const tokenHashHex = await sha256Hex(token);
        const key = await deriveKey(tokenHashHex, body.tunnel_id);
        e2e = { key };
    }

    return {
        webClientId: body.web_client_id,
        e2e,
        isReadOnly: body.is_read_only === true,
        sessionRows: Number(body.session_rows) || 0,
        sessionCols: Number(body.session_cols) || 0,
    };
}

/**
 * Initialize authentication flow and return client ID
 * @returns {Promise<{webClientId: string, e2e: ?{key: CryptoKey}}>}
 */
export async function initAuthentication() {
    let token = null;
    let remember = false;
    let hasAuthenticationCookie = document.body.dataset.authenticated === "true";
    const expectedE2e = readExpectedE2e();
    // Gates the imperative `navigator.credentials.store` call so the
    // password manager is only asked to save tokens that came from a
    // real user gesture, not from a silent autofill.
    let tokenFromUserEntry = false;

    if (!hasAuthenticationCookie) {
        // Silent autofill via the Credential Management API is only
        // used in relay mode, where the password manager is the
        // primary persistence layer. In local mode the canonical
        // persistence layer is the server-side Remember-me cookie,
        // which depends on the user interacting with the checkbox;
        // skipping the modal here would silently downgrade that
        // decision on every return visit. The form's `autocomplete`
        // attributes still let the password manager autofill the
        // modal once it is visible.
        const saved = getAuthMode() === "relay"
            ? await tryGetSavedCredential()
            : null;
        if (saved) {
            token = saved;
        } else {
            const tokenResult = await waitForSecurityToken();
            token = tokenResult.token;
            remember = tokenResult.remember;
            tokenFromUserEntry = true;
        }
    }

    let session;

    while (!session) {
        session = await getClientId(
            token,
            remember,
            hasAuthenticationCookie,
            expectedE2e,
        );
        if (!session) {
            // Login rejected (revoked / wrong token) — drop any cookie
            // assumption and prompt the user manually. The modal's
            // form-submit then lets the password manager offer to
            // update the saved credential.
            hasAuthenticationCookie = false;
            const tokenResult = await waitForSecurityToken();
            token = tokenResult.token;
            remember = tokenResult.remember;
            tokenFromUserEntry = true;
        }
    }

    if (token && tokenFromUserEntry) {
        await saveCredential(token);
    }

    return session;
}
