/**
 * Utility functions for the terminal web client
 */

/**
 * Check if the current page is served over HTTPS
 * @returns {boolean} true if protocol is https:, false otherwise
 */
export function is_https() {
    return document.location.protocol === "https:";
}

export function isMac() {
    if (navigator.userAgentData && navigator.userAgentData.platform) {
        return navigator.userAgentData.platform === "macOS";
    }
    return navigator.platform.toUpperCase().includes("MAC");
}

/**
 * Get the base URL from the base href tag
 * @returns {string} Base URL
 */
export function getBaseUrl() {
    const baseElement = document.querySelector("base");
    if (baseElement && baseElement.href) {
        return baseElement.href.replace(/\/$/, ""); // Remove trailing slash
    }
    // Fallback to current origin if no base href
    return window.location.origin;
}

export function isCurrentLocation(target) {
    try {
        const targetUrl = new URL(target, window.location.href);
        const stripTrailingSlash = (path) => path.replace(/\/$/, "");
        return (
            targetUrl.origin === window.location.origin &&
            stripTrailingSlash(targetUrl.pathname) ===
                stripTrailingSlash(window.location.pathname)
        );
    } catch (_) {
        return false;
    }
}

export function isMobileViewport() {
    return (
        (window.matchMedia &&
            window.matchMedia("(pointer: coarse)").matches &&
            window.innerWidth < 600) ||
        /Mobi|Android|iPhone|iPad/i.test(navigator.userAgent)
    );
}

/**
 * Get the base URL from the base href tag and convert to WebSocket URL
 * @returns {string} WebSocket base URL
 */
export function getWebSocketBaseUrl() {
    const baseElement = document.querySelector("base");
    if (baseElement && baseElement.href) {
        const baseUrl = baseElement.href.replace(/\/$/, ""); // Remove trailing slash
        // Convert http/https to ws/wss for WebSocket
        return baseUrl.replace(/^https?/, is_https() ? "wss" : "ws");
    }
    // Fallback to current origin if no base href
    return window.location.origin.replace(/^https?/, is_https() ? "wss" : "ws");
}

/** True when the page is the standalone app-origin relay viewer. */
export function isRelayMode() {
    const el = document.getElementById("zellij-auth-mode");
    return !!el && el.value === "relay";
}

/** Relay slug from the path (`/r/<slug>`); empty string if absent. */
export function getRelaySlug() {
    const m = location.pathname.match(/\/r\/([^/]+)/);
    return m ? m[1] : "";
}

export function getFragmentParams() {
    const raw = location.hash.startsWith("#") ? location.hash.slice(1) : location.hash;
    const out = {};
    for (const part of raw.split("&")) {
        if (!part) continue;
        const eq = part.indexOf("=");
        if (eq === -1) continue;
        out[decodeURIComponent(part.slice(0, eq))] = decodeURIComponent(part.slice(eq + 1));
    }
    return out;
}

/** Decode a lowercase/uppercase hex string into an array of byte values. */
export function hexToBytes(hex) {
    if (typeof hex !== "string" || hex.length === 0 || hex.length % 2 !== 0) return null;
    const out = new Array(hex.length / 2);
    for (let i = 0; i < out.length; i++) {
        const byte = parseInt(hex.substr(i * 2, 2), 16);
        if (Number.isNaN(byte)) return null;
        out[i] = byte;
    }
    return out;
}

export function deriveRelayHost(appHost) {
    if (isIpLiteralHost(appHost)) return appHost;
    if (appHost.startsWith("relay.")) return appHost;
    return "relay." + appHost;
}

function isIpLiteralHost(host) {
    return /^\d+(\.\d+){3}$/.test(host) || host.startsWith("[");
}

/**
 * Return the relay HTTP origin pinned into a staged app-origin artifact.
 * This value comes from build-generated HTML, never from share-link data.
 */
export function getConfiguredRelayOrigin() {
    const element = document.querySelector('meta[name="zellij-relay-origin"]');
    if (!element) return null;

    const configured = element.getAttribute("content");
    try {
        const parsed = new URL(configured);
        if (
            !["https:", "http:"].includes(parsed.protocol) ||
            parsed.username ||
            parsed.password ||
            parsed.pathname !== "/" ||
            parsed.search ||
            parsed.hash
        ) {
            throw new Error("invalid relay origin");
        }
        const hostname = parsed.hostname.toLowerCase();
        const isLoopback =
            hostname === "localhost" ||
            hostname === "[::1]" ||
            /^127(?:\.\d{1,3}){3}$/.test(hostname);
        if (parsed.protocol === "http:" && !isLoopback) {
            throw new Error("insecure remote relay origin");
        }
        return parsed.origin;
    } catch (_) {
        throw new Error("The app artifact contains an invalid relay origin");
    }
}

export function getRelayTarget() {
    const params = getFragmentParams();
    const slug = getRelaySlug();
    const configuredOrigin = getConfiguredRelayOrigin();
    let relayHost;
    let httpOrigin;
    let websocketOrigin;
    if (configuredOrigin) {
        const relayUrl = new URL(configuredOrigin);
        relayHost = relayUrl.host;
        httpOrigin = relayUrl.origin;
        websocketOrigin = relayUrl.origin.replace(
            /^https?/,
            relayUrl.protocol === "https:" ? "wss" : "ws"
        );
    } else {
        relayHost = deriveRelayHost(location.hostname);
        if (location.port) {
            relayHost += ":" + location.port;
        }
        const httpScheme = is_https() ? "https" : "http";
        const wsScheme = is_https() ? "wss" : "ws";
        httpOrigin = `${httpScheme}://${relayHost}`;
        websocketOrigin = `${wsScheme}://${relayHost}`;
    }
    return {
        slug,
        secret: params.k || null,
        linkId: params.l ? hexToBytes(params.l) : null,
        relayHost,
        httpBase: `${httpOrigin}/r/${slug}`,
        wsBase: `${websocketOrigin}/r/${slug}`,
    };
}

export function relayHostCollidesWithAppHost(appHost, relayHost) {
    const stripPort = (host) => host.replace(/:\d+$/, "").toLowerCase();
    return stripPort(appHost) === stripPort(relayHost);
}

export function getRelayIsolation() {
    const appHost = location.host;
    const relayHost = getRelayTarget().relayHost;
    return {
        appHost,
        relayHost,
        sameHost: relayHostCollidesWithAppHost(appHost, relayHost),
    };
}

/** API base for HTTP calls: the relay in relay mode, else the page origin. */
export function getApiBase() {
    return isRelayMode() ? getRelayTarget().httpBase : getBaseUrl();
}

/** API base for WebSocket calls: the relay in relay mode, else the page origin. */
export function getWsApiBase() {
    return isRelayMode() ? getRelayTarget().wsBase : getWebSocketBaseUrl();
}
