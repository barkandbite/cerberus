//! The vault: encrypted-at-rest storage for quarantined cookies, partitioned by
//! `InstanceId`.
//!
//! The key is derived from a passphrase via the injected `Kdf` (Argon2id in the
//! real adapter) and held only in memory inside a zeroizing [`Key`]; it lives
//! nowhere at rest and not in the OS keystore. Locking drops the key.
//!
//! `EncryptedVault` is *our* composition logic over the crypto seams — it
//! implements no primitive itself. The randomized on-disk path and persistent
//! format land at M4; here the ciphertext blobs are kept in memory.

use super::StorageError;
use cerberus_crypto::{Aead, Kdf, Key, Secret};
use cerberus_types::InstanceId;
use std::collections::HashMap;

/// Encrypted, instance-partitioned storage for quarantined secrets.
pub trait Vault: Send {
    /// Whether the vault is locked (no key in memory).
    fn is_locked(&self) -> bool;
    /// Derive the key from `passphrase` and unlock.
    fn unlock(&mut self, passphrase: &Secret) -> Result<(), StorageError>;
    /// Drop the key and lock.
    fn lock(&mut self);
    /// Encrypt and store `plaintext` under `(instance, key)`.
    fn store(
        &mut self,
        instance: InstanceId,
        key: &str,
        plaintext: &[u8],
    ) -> Result<(), StorageError>;
    /// Load and decrypt the value under `(instance, key)`, if present.
    fn load(&mut self, instance: InstanceId, key: &str) -> Result<Option<Vec<u8>>, StorageError>;
    /// Remove the value under `(instance, key)`.
    fn remove(&mut self, instance: InstanceId, key: &str) -> Result<(), StorageError>;
}

/// A vault that holds no key and refuses every operation. Used where quarantine
/// must be impossible (render-only paths) or before a real vault is configured.
#[derive(Debug, Default)]
pub struct NoVault;

impl Vault for NoVault {
    fn is_locked(&self) -> bool {
        true
    }
    fn unlock(&mut self, _passphrase: &Secret) -> Result<(), StorageError> {
        Err(StorageError::VaultLocked)
    }
    fn lock(&mut self) {}
    fn store(
        &mut self,
        _instance: InstanceId,
        _key: &str,
        _plaintext: &[u8],
    ) -> Result<(), StorageError> {
        Err(StorageError::VaultLocked)
    }
    fn load(&mut self, _instance: InstanceId, _key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        Err(StorageError::VaultLocked)
    }
    fn remove(&mut self, _instance: InstanceId, _key: &str) -> Result<(), StorageError> {
        Err(StorageError::VaultLocked)
    }
}

/// One stored ciphertext blob and the nonce it was sealed with.
struct Blob {
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}

/// AEAD-encrypted vault composed over the injected `Aead` + `Kdf`.
pub struct EncryptedVault {
    aead: Box<dyn Aead>,
    kdf: Box<dyn Kdf>,
    salt: [u8; 16],
    key: Option<Key>,
    nonce_counter: u64,
    blobs: HashMap<(InstanceId, String), Blob>,
}

impl EncryptedVault {
    /// Create a locked vault. Call [`Vault::unlock`] before storing.
    pub fn new(aead: Box<dyn Aead>, kdf: Box<dyn Kdf>, salt: [u8; 16]) -> Self {
        Self {
            aead,
            kdf,
            salt,
            key: None,
            nonce_counter: 0,
            blobs: HashMap::new(),
        }
    }

    fn next_nonce(&mut self) -> Vec<u8> {
        let mut nonce = vec![0u8; self.aead.nonce_len()];
        let ctr = self.nonce_counter.to_be_bytes();
        let n = ctr.len().min(nonce.len());
        nonce[..n].copy_from_slice(&ctr[..n]);
        self.nonce_counter += 1;
        nonce
    }
}

/// Bind ciphertext to its `(instance, key)` slot so it cannot be relocated to
/// another instance without failing authentication.
fn aad(instance: InstanceId, key: &str) -> Vec<u8> {
    let mut v = instance.0.as_bytes().to_vec();
    v.extend_from_slice(key.as_bytes());
    v
}

impl Vault for EncryptedVault {
    fn is_locked(&self) -> bool {
        self.key.is_none()
    }

    fn unlock(&mut self, passphrase: &Secret) -> Result<(), StorageError> {
        let key = self
            .kdf
            .derive(passphrase, &self.salt, self.aead.key_len())?;
        self.key = Some(key);
        Ok(())
    }

    fn lock(&mut self) {
        // Dropping the Key zeroizes it.
        self.key = None;
    }

    fn store(
        &mut self,
        instance: InstanceId,
        key: &str,
        plaintext: &[u8],
    ) -> Result<(), StorageError> {
        let nonce = self.next_nonce();
        let secret_key = self.key.as_ref().ok_or(StorageError::VaultLocked)?;
        let ciphertext = self
            .aead
            .seal(secret_key, &nonce, &aad(instance, key), plaintext)?;
        self.blobs
            .insert((instance, key.to_string()), Blob { nonce, ciphertext });
        Ok(())
    }

    fn load(&mut self, instance: InstanceId, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        let secret_key = self.key.as_ref().ok_or(StorageError::VaultLocked)?;
        let Some(blob) = self.blobs.get(&(instance, key.to_string())) else {
            return Ok(None);
        };
        let plaintext = self.aead.open(
            secret_key,
            &blob.nonce,
            &aad(instance, key),
            &blob.ciphertext,
        )?;
        Ok(Some(plaintext))
    }

    fn remove(&mut self, instance: InstanceId, key: &str) -> Result<(), StorageError> {
        self.blobs.remove(&(instance, key.to_string()));
        Ok(())
    }
}
