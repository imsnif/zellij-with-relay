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
 * Wait for user to provide a security token
 * @returns {Promise<{token: string, remember: boolean}>}
 */
async function waitForSecurityToken() {
    let token = null;
    let remember = null;

    while (!token) {
        let result = await getSecurityToken();
        if (result) {
            token = result.token;
            remember = result.remember;
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
 * Get client ID from server after authentication
 * @param {string} token - Authentication token
 * @param {boolean} rememberMe - Remember login preference
 * @param {boolean} hasAuthenticationCookie - Whether auth cookie exists
 * @returns {Promise<{webClientId: string, e2e: ?{key: CryptoKey}} | null>} null on failure
 */
export async function getClientId(token, rememberMe, hasAuthenticationCookie, expectedE2e) {
    const baseUrl = getBaseUrl();

    if (!hasAuthenticationCookie) {
        let login_res = await fetch(`${baseUrl}/command/login`, {
            method: "POST",
            headers: {
                "Content-Type": "application/json",
            },
            body: JSON.stringify({
                auth_token: token,
                remember_me: rememberMe ? true : false,
            }),
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
    };
}

/**
 * Initialize authentication flow and return client ID
 * @returns {Promise<{webClientId: string, e2e: ?{key: CryptoKey}}>}
 */
export async function initAuthentication() {
    let token = null;
    let remember = null;
    let hasAuthenticationCookie = document.body.dataset.authenticated === "true";
    const expectedE2e = readExpectedE2e();

    if (!hasAuthenticationCookie) {
        const tokenResult = await waitForSecurityToken();
        token = tokenResult.token;
        remember = tokenResult.remember;
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
            hasAuthenticationCookie = false;
            const tokenResult = await waitForSecurityToken();
            token = tokenResult.token;
            remember = tokenResult.remember;
        }
    }

    return session;
}
