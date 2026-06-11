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
use crate::disk::{self, RecordReader, RecordWriter, KIND_VAULT};
use cerberus_crypto::{Aead, Kdf, Key, Secret};
use cerberus_types::InstanceId;
use std::collections::HashMap;
use std::path::Path;

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

    /// Persist ciphertext blobs to `path` (no-op for memory-only vaults).
    /// Ciphertext only — the key never touches disk.
    fn save_to(&self, _path: &Path) -> std::io::Result<()> {
        Ok(())
    }
    /// Load ciphertext blobs from `path` if it exists. Works while locked.
    fn load_from(&mut self, _path: &Path) -> std::io::Result<()> {
        Ok(())
    }
    /// Whether there are unsaved changes.
    fn is_dirty(&self) -> bool {
        false
    }
    /// Mark changes as saved.
    fn clear_dirty(&mut self) {}
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

/// AAD binding the unlock-check sentinel (see [`EncryptedVault::unlock`]).
const CHECK_AAD: &[u8] = b"cerberus-vault-check-v1";

/// AEAD-encrypted vault composed over the injected `Aead` + `Kdf`.
pub struct EncryptedVault {
    aead: Box<dyn Aead>,
    kdf: Box<dyn Kdf>,
    salt: [u8; 16],
    key: Option<Key>,
    /// Sentinel sealed at first unlock; verifying it makes a wrong passphrase
    /// fail *at unlock* instead of at the first cookie release.
    check: Option<Blob>,
    blobs: HashMap<(InstanceId, String), Blob>,
    dirty: bool,
}

impl EncryptedVault {
    /// Create a locked vault. Call [`Vault::unlock`] before storing.
    pub fn new(aead: Box<dyn Aead>, kdf: Box<dyn Kdf>, salt: [u8; 16]) -> Self {
        Self {
            aead,
            kdf,
            salt,
            key: None,
            check: None,
            blobs: HashMap::new(),
            dirty: false,
        }
    }

    /// A fresh *random* nonce per sealed blob. Never a counter: a persisted
    /// counter would reset across restarts and reuse nonces, which breaks the
    /// AEAD outright. XChaCha20's 24-byte nonce makes random draws safe.
    fn fresh_nonce(&self) -> Vec<u8> {
        crate::rand::random_bytes(self.aead.nonce_len())
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
        match &self.check {
            // A wrong passphrase fails here — the sentinel won't authenticate —
            // and the vault stays locked.
            Some(blob) => {
                self.aead
                    .open(&key, &blob.nonce, CHECK_AAD, &blob.ciphertext)?;
            }
            // First unlock: seal the sentinel under the fresh key.
            None => {
                let nonce = self.fresh_nonce();
                let ciphertext = self.aead.seal(&key, &nonce, CHECK_AAD, b"cerberus")?;
                self.check = Some(Blob { nonce, ciphertext });
                self.dirty = true;
            }
        }
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
        let nonce = self.fresh_nonce();
        let secret_key = self.key.as_ref().ok_or(StorageError::VaultLocked)?;
        let ciphertext = self
            .aead
            .seal(secret_key, &nonce, &aad(instance, key), plaintext)?;
        self.blobs
            .insert((instance, key.to_string()), Blob { nonce, ciphertext });
        self.dirty = true;
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
        if self.blobs.remove(&(instance, key.to_string())).is_some() {
            self.dirty = true;
        }
        Ok(())
    }

    /// On-disk layout (KIND_VAULT records): `["C", nonce, ct]` for the check
    /// sentinel, `["B", instance, key, nonce, ct]` per blob. Ciphertext only.
    fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let mut w = RecordWriter::new(KIND_VAULT);
        if let Some(check) = &self.check {
            w.record(&[b"C", &check.nonce, &check.ciphertext]);
        }
        for ((instance, key), blob) in &self.blobs {
            w.record(&[
                b"B",
                instance.0.as_bytes(),
                key.as_bytes(),
                &blob.nonce,
                &blob.ciphertext,
            ]);
        }
        disk::atomic_write(path, &w.finish())
    }

    fn load_from(&mut self, path: &Path) -> std::io::Result<()> {
        let Some(bytes) = disk::read_if_exists(path)? else {
            return Ok(());
        };
        let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
        let mut reader = RecordReader::new(&bytes, KIND_VAULT)?;
        while let Some(fields) = reader.next_record()? {
            match fields.first().map(Vec::as_slice) {
                Some(b"C") if fields.len() == 3 => {
                    self.check = Some(Blob {
                        nonce: fields[1].clone(),
                        ciphertext: fields[2].clone(),
                    });
                }
                Some(b"B") if fields.len() == 5 => {
                    let id: [u8; 16] = fields[1]
                        .as_slice()
                        .try_into()
                        .map_err(|_| bad("bad instance id"))?;
                    let key =
                        String::from_utf8(fields[2].clone()).map_err(|_| bad("bad vault key"))?;
                    self.blobs.insert(
                        (InstanceId(cerberus_types::Id128::from_bytes(id)), key),
                        Blob {
                            nonce: fields[3].clone(),
                            ciphertext: fields[4].clone(),
                        },
                    );
                }
                _ => return Err(bad("unknown vault record")),
            }
        }
        Ok(())
    }

    fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn clear_dirty(&mut self) {
        self.dirty = false;
    }
}
