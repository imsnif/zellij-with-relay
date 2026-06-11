/**
 * End-to-end crypto helpers mirroring `zellij-relay-protocol/src/crypto.rs`.
 *
 * Both sides must produce identical AES-256-GCM keys: the Zellij server
 * derives `HKDF(auth_token_hash, salt="zellij-e2e-v1", info=tunnel_id)`;
 * the browser does the same, taking the SHA-256 of the raw auth token the
 * user typed so it never transmits the raw token to the server after
 * login.
 *
 * Nonces are 12 bytes random per encrypt; payload wire format is
 * `nonce || ciphertext`.
 */

const HKDF_SALT = new TextEncoder().encode("zellij-e2e-v1");
const NONCE_LEN = 12;
const KEY_LEN = 32;

/** Convert a hex string to a Uint8Array. */
function hexToBytes(hex) {
    const out = new Uint8Array(hex.length / 2);
    for (let i = 0; i < out.length; i++) {
        out[i] = parseInt(hex.substr(i * 2, 2), 16);
    }
    return out;
}

/** Convert a Uint8Array to a lowercase hex string. */
function bytesToHex(bytes) {
    let out = "";
    for (const b of bytes) {
        out += b.toString(16).padStart(2, "0");
    }
    return out;
}

/** SHA-256 of a UTF-8 string, returned as lowercase hex. */
export async function sha256Hex(utf8) {
    const buf = new TextEncoder().encode(utf8);
    const digest = await crypto.subtle.digest("SHA-256", buf);
    return bytesToHex(new Uint8Array(digest));
}

/**
 * Derive a 32-byte AES-256 key via HKDF-SHA256.
 *
 * `keyMaterial` is the hex string Zellij stores for the token
 * (SHA-256(raw_token) lowercased). `tunnelId` is the value the server
 * returned on /session — used as the HKDF `info` parameter so reused
 * tokens across reconnections produce a fresh key per tunnel.
 */
export async function deriveKey(keyMaterialHex, tunnelId) {
    // HKDF input is the token-hash bytes (match Zellij's
    // `derive_key(token_hash.as_bytes(), tunnel_id)`).
    const ikm = new TextEncoder().encode(keyMaterialHex);
    const ikmKey = await crypto.subtle.importKey(
        "raw",
        ikm,
        { name: "HKDF" },
        false,
        ["deriveKey"]
    );
    return crypto.subtle.deriveKey(
        {
            name: "HKDF",
            hash: "SHA-256",
            salt: HKDF_SALT,
            info: new TextEncoder().encode(tunnelId),
        },
        ikmKey,
        { name: "AES-GCM", length: 256 },
        false,
        ["encrypt", "decrypt"]
    );
}

/**
 * Encrypt a Uint8Array or ArrayBuffer with the derived key. Output
 * layout: `nonce(12) || ciphertext`.
 */
export async function encrypt(key, plaintext) {
    const pt = plaintext instanceof Uint8Array
        ? plaintext
        : new Uint8Array(plaintext);
    const nonce = crypto.getRandomValues(new Uint8Array(NONCE_LEN));
    const ctBuf = await crypto.subtle.encrypt(
        { name: "AES-GCM", iv: nonce },
        key,
        pt
    );
    const ct = new Uint8Array(ctBuf);
    const out = new Uint8Array(NONCE_LEN + ct.length);
    out.set(nonce, 0);
    out.set(ct, NONCE_LEN);
    return out;
}

/**
 * Decrypt a `nonce || ciphertext` payload. Throws on AEAD tag mismatch.
 */
export async function decrypt(key, nonceAndCt) {
    const buf = nonceAndCt instanceof Uint8Array
        ? nonceAndCt
        : new Uint8Array(nonceAndCt);
    if (buf.length < NONCE_LEN) {
        throw new Error("ciphertext too short");
    }
    const nonce = buf.subarray(0, NONCE_LEN);
    const ct = buf.subarray(NONCE_LEN);
    const ptBuf = await crypto.subtle.decrypt(
        { name: "AES-GCM", iv: nonce },
        key,
        ct
    );
    return new Uint8Array(ptBuf);
}

export { NONCE_LEN, KEY_LEN };
