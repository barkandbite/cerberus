//! Cryptographic seams for the vault.
//!
//! This crate defines the `Aead` and `Kdf` traits and zeroizing key material.
//! It deliberately implements **no** primitives: we do not hand-roll AEAD or a
//! KDF. The real adapters wrap approved, audited crates (RustCrypto AEAD +
//! `argon2` for Argon2id; see ADR-0003) and land at M4. Test code supplies its
//! own throwaway impls of these traits — never shipped.

use zeroize::Zeroize;

/// A secret byte string (passphrase or derived key) that is zeroized on drop
/// (volatile writes via the audited `zeroize` crate, ADR-0003) and never
/// prints its contents.
///
/// Scope note: pages holding key material are *not* `mlock`ed in v1 — that
/// would require `unsafe`/libc, and the workspace denies unsafe outside
/// vetted adapters. The trade-off (swap exposure on memory pressure) is
/// documented in the threat model; OS-level swap encryption is the
/// compensating control.
pub struct Secret(Vec<u8>);

impl Secret {
    /// Wrap owned bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Wrap a passphrase string's bytes.
    pub fn from_passphrase(s: &str) -> Self {
        Self(s.as_bytes().to_vec())
    }

    /// Borrow the secret bytes. Keep the borrow's lifetime short.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the secret is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret(<{} bytes redacted>)", self.0.len())
    }
}

/// A symmetric key. Same zeroizing/redaction policy as [`Secret`].
#[derive(Debug)]
pub struct Key(Secret);

impl Key {
    /// Wrap raw key bytes.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(Secret::new(bytes))
    }

    /// Borrow the key bytes.
    pub fn expose(&self) -> &[u8] {
        self.0.expose()
    }

    /// Key length in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the key is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Errors from cryptographic operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CryptoError {
    /// Decryption/authentication failed (wrong key, tampered ciphertext, ...).
    OpenFailed,
    /// A parameter (key/nonce length) was invalid.
    InvalidParameter(&'static str),
    /// Key derivation failed.
    Kdf(String),
}

/// Authenticated encryption with associated data. The seam for an approved
/// RustCrypto AEAD (ADR-0003); wired at M4.
pub trait Aead: Send + Sync {
    /// Required key length in bytes.
    fn key_len(&self) -> usize;
    /// Required nonce length in bytes.
    fn nonce_len(&self) -> usize;
    /// Encrypt and authenticate `plaintext` (+ `aad`); returns ciphertext||tag.
    fn seal(
        &self,
        key: &Key,
        nonce: &[u8],
        aad: &[u8],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, CryptoError>;
    /// Verify and decrypt.
    fn open(
        &self,
        key: &Key,
        nonce: &[u8],
        aad: &[u8],
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, CryptoError>;
}

/// A password-based key derivation function. The seam for Argon2id (ADR-0003);
/// wired at M4.
pub trait Kdf: Send + Sync {
    /// Derive an `out_len`-byte key from `passphrase` and `salt`.
    fn derive(&self, passphrase: &Secret, salt: &[u8], out_len: usize) -> Result<Key, CryptoError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_redacts_in_debug() {
        let s = Secret::from_passphrase("hunter2");
        assert_eq!(format!("{s:?}"), "Secret(<7 bytes redacted>)");
        assert_eq!(s.expose(), b"hunter2");
    }
}
