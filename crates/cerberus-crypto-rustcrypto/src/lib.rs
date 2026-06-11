//! RustCrypto adapters behind our `Aead`/`Kdf` seams (ADR-0003, approved M4).
//!
//! This crate is the *only* place the `chacha20poly1305` and `argon2` crates
//! are named; callers see `cerberus_crypto::{Aead, Kdf}` and nothing else.
//! Swapping primitives (e.g. to `aes-gcm`) is a new adapter crate, not a
//! caller change.

use argon2::{Algorithm, Argon2, Params, Version};
use cerberus_crypto::{Aead, CryptoError, Kdf, Key, Secret};
use chacha20poly1305::aead::{Aead as _, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};

/// XChaCha20-Poly1305 (RFC 8439 construction with the 24-byte extended nonce).
/// The extended nonce is the point: blobs at rest get independent *random*
/// nonces with no birthday-bound anxiety at our scale.
#[derive(Debug, Default)]
pub struct XChaCha20Poly1305Aead;

impl XChaCha20Poly1305Aead {
    pub fn new() -> Self {
        Self
    }
}

impl Aead for XChaCha20Poly1305Aead {
    fn key_len(&self) -> usize {
        32
    }

    fn nonce_len(&self) -> usize {
        24
    }

    fn seal(
        &self,
        key: &Key,
        nonce: &[u8],
        aad: &[u8],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let cipher = XChaCha20Poly1305::new_from_slice(key.expose())
            .map_err(|_| CryptoError::InvalidParameter("key length"))?;
        if nonce.len() != self.nonce_len() {
            return Err(CryptoError::InvalidParameter("nonce length"));
        }
        cipher
            .encrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| CryptoError::OpenFailed)
    }

    fn open(
        &self,
        key: &Key,
        nonce: &[u8],
        aad: &[u8],
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let cipher = XChaCha20Poly1305::new_from_slice(key.expose())
            .map_err(|_| CryptoError::InvalidParameter("key length"))?;
        if nonce.len() != self.nonce_len() {
            return Err(CryptoError::InvalidParameter("nonce length"));
        }
        cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| CryptoError::OpenFailed)
    }
}

/// Argon2id with parameters in the OWASP-recommended band. The memory cost is
/// a deliberate trade-off documented in PLAN §5: ~19 MiB is transient during
/// unlock only (the mem-gate scenario never unlocks, so the budget gate is
/// unaffected; an unlock on a loaded page can briefly add this on top).
#[derive(Debug, Default)]
pub struct Argon2idKdf;

/// Argon2id memory cost in KiB (19 MiB), time cost, and lanes.
const M_COST_KIB: u32 = 19 * 1024;
const T_COST: u32 = 2;
const P_COST: u32 = 1;

impl Argon2idKdf {
    pub fn new() -> Self {
        Self
    }
}

impl Kdf for Argon2idKdf {
    fn derive(&self, passphrase: &Secret, salt: &[u8], out_len: usize) -> Result<Key, CryptoError> {
        let params = Params::new(M_COST_KIB, T_COST, P_COST, Some(out_len))
            .map_err(|e| CryptoError::Kdf(e.to_string()))?;
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut out = vec![0u8; out_len];
        argon
            .hash_password_into(passphrase.expose(), salt, &mut out)
            .map_err(|e| CryptoError::Kdf(e.to_string()))?;
        Ok(Key::from_bytes(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key32() -> Key {
        Key::from_bytes((0u8..32).collect())
    }

    #[test]
    fn seal_open_round_trip_with_aad() {
        let aead = XChaCha20Poly1305Aead::new();
        let nonce = [7u8; 24];
        let ct = aead
            .seal(&key32(), &nonce, b"slot-1", b"cookie value")
            .unwrap();
        assert_ne!(&ct[..12], b"cookie value");
        let pt = aead.open(&key32(), &nonce, b"slot-1", &ct).unwrap();
        assert_eq!(pt, b"cookie value");
    }

    #[test]
    fn tampered_ciphertext_or_wrong_aad_fails() {
        let aead = XChaCha20Poly1305Aead::new();
        let nonce = [7u8; 24];
        let mut ct = aead.seal(&key32(), &nonce, b"slot-1", b"secret").unwrap();
        // Wrong AAD (blob moved to another slot) must fail authentication.
        assert_eq!(
            aead.open(&key32(), &nonce, b"slot-2", &ct),
            Err(CryptoError::OpenFailed)
        );
        // Bit-flip must fail authentication.
        ct[0] ^= 1;
        assert_eq!(
            aead.open(&key32(), &nonce, b"slot-1", &ct),
            Err(CryptoError::OpenFailed)
        );
    }

    #[test]
    fn wrong_key_fails() {
        let aead = XChaCha20Poly1305Aead::new();
        let nonce = [7u8; 24];
        let ct = aead.seal(&key32(), &nonce, b"", b"secret").unwrap();
        let other = Key::from_bytes(vec![0xFF; 32]);
        assert_eq!(
            aead.open(&other, &nonce, b"", &ct),
            Err(CryptoError::OpenFailed)
        );
    }

    #[test]
    fn bad_parameter_lengths_are_rejected() {
        let aead = XChaCha20Poly1305Aead::new();
        let short_key = Key::from_bytes(vec![0u8; 16]);
        assert!(matches!(
            aead.seal(&short_key, &[0u8; 24], b"", b"x"),
            Err(CryptoError::InvalidParameter(_))
        ));
        assert!(matches!(
            aead.seal(&key32(), &[0u8; 12], b"", b"x"),
            Err(CryptoError::InvalidParameter(_))
        ));
    }

    #[test]
    fn argon2id_is_deterministic_per_salt_and_differs_across_salts() {
        let kdf = Argon2idKdf::new();
        let pass = Secret::from_passphrase("correct horse battery staple");
        let a1 = kdf.derive(&pass, b"salt-aaaa-bbbb-cc", 32).unwrap();
        let a2 = kdf.derive(&pass, b"salt-aaaa-bbbb-cc", 32).unwrap();
        let b = kdf.derive(&pass, b"salt-xxxx-yyyy-zz", 32).unwrap();
        assert_eq!(a1.expose(), a2.expose());
        assert_ne!(a1.expose(), b.expose());
        assert_eq!(a1.len(), 32);
    }

    #[test]
    fn kdf_feeds_aead_end_to_end() {
        let kdf = Argon2idKdf::new();
        let aead = XChaCha20Poly1305Aead::new();
        let key = kdf
            .derive(
                &Secret::from_passphrase("pass"),
                b"0123456789abcdef",
                aead.key_len(),
            )
            .unwrap();
        let nonce = [3u8; 24];
        let ct = aead.seal(&key, &nonce, b"aad", b"payload").unwrap();
        assert_eq!(aead.open(&key, &nonce, b"aad", &ct).unwrap(), b"payload");
    }
}
