//! End-to-end crypto helper shared between the Zellij tunnel client, the
//! browser viewer (mirrored in JS), and the Rust attach client.
//!
//! The auth token doubles as key material: viewers type the same token they
//! use to authenticate, and both sides independently derive the AES-GCM key
//! via HKDF. The relay only ever sees `SHA-256(token)` plus the ciphertext
//! — it cannot derive the key.
//!
//! # Primitives
//!
//! * HKDF-SHA256 with a fixed salt of `b"zellij-e2e-v1"` and an `info`
//!   parameter bound to the tunnel's `tunnel_id` (returned by the relay in
//!   `TunnelEstablished`). Different tunnels on the same token therefore
//!   derive distinct keys.
//! * AES-256-GCM with a 12-byte random nonce generated via `rand::rngs::OsRng`.
//!   `encrypt` returns `nonce || ciphertext` as a single `Vec<u8>`; `decrypt`
//!   expects the same layout.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hkdf::Hkdf;
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::Sha256;
use thiserror::Error;

/// Fixed HKDF salt. A version suffix is baked in so a future migration can
/// rotate the salt while keeping the old path decryptable during a
/// transition window.
pub const HKDF_SALT: &[u8] = b"zellij-e2e-v1";

/// AES-GCM nonce length in bytes.
pub const NONCE_LEN: usize = 12;

/// AES-256 key length in bytes.
pub const KEY_LEN: usize = 32;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("ciphertext too short (expected >= {expected} bytes, got {actual})")]
    TooShort { expected: usize, actual: usize },
    #[error("aead decryption failed")]
    Decrypt,
    #[error("aead encryption failed")]
    Encrypt,
}

/// Derive a 32-byte AES-256 key from the raw auth token plus the tunnel id.
///
/// `tunnel_id` is included as the HKDF `info` parameter so a single token
/// reused across reconnections produces a fresh key per tunnel — mitigating
/// nonce-reuse risk if a token is ever reused against a different relay
/// allocation.
pub fn derive_key(raw_token: &str, tunnel_id: &str) -> [u8; KEY_LEN] {
    let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), raw_token.as_bytes());
    let mut okm = [0u8; KEY_LEN];
    hk.expand(tunnel_id.as_bytes(), &mut okm)
        .expect("HKDF output length is valid");
    okm
}

/// Encrypt `plaintext` with AES-256-GCM, returning `nonce || ciphertext`.
///
/// A fresh 12-byte random nonce is generated per call. `OsRng` is the
/// cryptographically-secure system RNG; a panic here would indicate an OS
/// RNG failure and is treated as unrecoverable.
pub fn encrypt(key: &[u8; KEY_LEN], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| CryptoError::Encrypt)?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypt a `nonce || ciphertext` payload produced by [`encrypt`].
///
/// Returns `CryptoError::TooShort` when `nonce_and_ct` is shorter than a
/// single nonce, and `CryptoError::Decrypt` on AEAD auth-tag mismatch
/// (tampering, wrong key, or truncation).
pub fn decrypt(key: &[u8; KEY_LEN], nonce_and_ct: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if nonce_and_ct.len() < NONCE_LEN {
        return Err(CryptoError::TooShort {
            expected: NONCE_LEN,
            actual: nonce_and_ct.len(),
        });
    }
    let (nonce_bytes, ct) = nonce_and_ct.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher.decrypt(nonce, ct).map_err(|_| CryptoError::Decrypt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn derive_key_is_deterministic() {
        let k1 = derive_key("some-token", "tunnel-abc");
        let k2 = derive_key("some-token", "tunnel-abc");
        assert_eq!(k1, k2);
    }

    #[test]
    fn derive_key_depends_on_token() {
        let k1 = derive_key("token-a", "tunnel-abc");
        let k2 = derive_key("token-b", "tunnel-abc");
        assert_ne!(k1, k2);
    }

    #[test]
    fn derive_key_depends_on_tunnel_id() {
        let k1 = derive_key("same-token", "tunnel-1");
        let k2 = derive_key("same-token", "tunnel-2");
        assert_ne!(k1, k2);
    }

    #[test]
    fn roundtrip_preserves_plaintext() {
        let key = derive_key("my-token", "t-123");
        let plaintext = b"hello, end-to-end world";
        let encrypted = encrypt(&key, plaintext).unwrap();
        let decrypted = decrypt(&key, &encrypted).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn roundtrip_preserves_empty_plaintext() {
        let key = derive_key("tok", "t");
        let encrypted = encrypt(&key, b"").unwrap();
        let decrypted = decrypt(&key, &encrypted).unwrap();
        assert_eq!(decrypted, b"");
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = derive_key("my-token", "t-123");
        let mut encrypted = encrypt(&key, b"confidential").unwrap();
        let tamper_idx = encrypted.len() - 1;
        encrypted[tamper_idx] ^= 0x01;
        let err = decrypt(&key, &encrypted).expect_err("should fail");
        assert!(matches!(err, CryptoError::Decrypt));
    }

    #[test]
    fn wrong_key_fails() {
        let key_a = derive_key("token-a", "t");
        let key_b = derive_key("token-b", "t");
        let encrypted = encrypt(&key_a, b"secret").unwrap();
        let err = decrypt(&key_b, &encrypted).expect_err("should fail");
        assert!(matches!(err, CryptoError::Decrypt));
    }

    #[test]
    fn truncated_ciphertext_fails() {
        let key = derive_key("tok", "t");
        let encrypted = encrypt(&key, b"secret").unwrap();
        let err = decrypt(&key, &encrypted[..5]).expect_err("should fail");
        assert!(matches!(err, CryptoError::TooShort { .. }));
    }

    #[test]
    fn nonces_are_unique_across_many_encrypts() {
        // Sample a moderate number of encrypts; we care about catching a
        // constant-nonce bug, not about exhaustively validating OsRng.
        let key = derive_key("tok", "t");
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            let encrypted = encrypt(&key, b"x").unwrap();
            let nonce = encrypted[..NONCE_LEN].to_vec();
            assert!(seen.insert(nonce), "duplicate nonce");
        }
    }
}
