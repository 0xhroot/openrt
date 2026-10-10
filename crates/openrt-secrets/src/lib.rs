//! Encrypted secrets at rest (M6.3): ChaCha20-Poly1305 AEAD via `ring`.
//!
//! Secrets are never written as plaintext: [`SecretStore`] seals values under
//! a 32-byte master key held outside the ciphertext records (keyfile with
//! 0600 permissions, or an operator-supplied keyring/TPM path when available).
//! Ciphertext lives in the single foundation store under `secrets.data`.
//!
//! Key rotation re-seals every secret under a new key version; the old key id
//! is recorded in the record envelope so rotation is auditable and fail-closed
//! (undecryptable records surface as errors, never silent drops).
//!
//! Redaction: every secret value is also registered with a name that the
//! runtime's [`openrt_core::redaction`] field-name policy already redacts —
//! callers must not put secret *values* into telemetry payloads.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use ring::rand::SecureRandom;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Store namespace for sealed secret records (ADR-006).
pub const SECRETS_NS: &str = "secrets.data";
/// Store namespace for key metadata (ids only — never key material).
pub const SECRETS_KEYS_NS: &str = "secrets.keys";
/// Current AEAD algorithm label recorded in envelopes.
pub const ALGORITHM: &str = "chacha20-poly1305";

/// Errors from the secrets subsystem.
#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("persistence error: {0}")]
    Persistence(#[from] openrt_persistence::PersistenceError),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("secrets store not initialized (no master key)")]
    NotInitialized,
    #[error("secret `{0}` not found")]
    NotFound(String),
    #[error("decryption failed for `{0}` (wrong key or tampered ciphertext)")]
    DecryptFailed(String),
    #[error("key material error: {0}")]
    Key(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("audit error: {0}")]
    Audit(#[from] openrt_audit::AuditError),
    #[error("refusing to store empty secret name")]
    EmptyName,
}

/// Sealed envelope persisted under [`SECRETS_NS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedSecret {
    pub name: String,
    /// Key id used to seal this ciphertext (supports rotation).
    pub key_id: String,
    pub algorithm: String,
    /// base64(nonce || ciphertext || tag).
    pub ciphertext_b64: String,
    /// RFC 3339.
    pub sealed_at: String,
    /// Monotonic rotation counter for this name.
    pub generation: u64,
}

/// Key metadata (no key bytes) under [`SECRETS_KEYS_NS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyMeta {
    pub key_id: String,
    pub created_at: String,
    pub active: bool,
}

/// Master key material held in memory only.
struct MasterKey {
    key_id: String,
    key: LessSafeKey,
}

/// Filesystem keyfile path helper (0600).
pub fn default_keyfile() -> PathBuf {
    let base = std::env::var("OPENRT_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".openrt")
        });
    base.join("secrets.key")
}

/// Generate 32 random bytes using `ring`'s CSPRNG.
pub fn generate_key_bytes() -> [u8; 32] {
    let mut b = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .expect("SystemRandom fill cannot fail");
    b
}

/// Write a keyfile with 0600 permissions (Unix).
pub fn write_keyfile(path: &Path, key: &[u8; 32]) -> Result<(), SecretsError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, key)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(path)?.permissions();
        perm.set_mode(0o600);
        std::fs::set_permissions(path, perm)?;
    }
    Ok(())
}

/// Load or create a keyfile.
pub fn load_or_create_keyfile(path: &Path) -> Result<[u8; 32], SecretsError> {
    if path.exists() {
        let bytes = std::fs::read(path)?;
        if bytes.len() != 32 {
            return Err(SecretsError::Key(format!(
                "keyfile {} must be exactly 32 bytes, got {}",
                path.display(),
                bytes.len()
            )));
        }
        let mut k = [0u8; 32];
        k.copy_from_slice(&bytes);
        Ok(k)
    } else {
        let k = generate_key_bytes();
        write_keyfile(path, &k)?;
        Ok(k)
    }
}

fn unbound(key: &[u8; 32]) -> Result<UnboundKey, SecretsError> {
    UnboundKey::new(&CHACHA20_POLY1305, key).map_err(|e| SecretsError::Key(e.to_string()))
}

fn seal(key: &LessSafeKey, plaintext: &[u8]) -> Result<Vec<u8>, SecretsError> {
    let mut nonce_bytes = [0u8; 12];
    ring::rand::SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|e| SecretsError::Key(e.to_string()))?;
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = plaintext.to_vec();
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| SecretsError::Key("seal failed".into()))?;
    let mut out = nonce_bytes.to_vec();
    out.extend_from_slice(&in_out);
    Ok(out)
}

fn open(key: &LessSafeKey, sealed: &[u8]) -> Result<Vec<u8>, SecretsError> {
    if sealed.len() < 12 + 16 {
        return Err(SecretsError::Key("ciphertext too short".into()));
    }
    let (n, rest) = sealed.split_at(12);
    let mut buf = rest.to_vec();
    let nonce =
        Nonce::try_assume_unique_for_key(n).map_err(|_| SecretsError::Key("bad nonce".into()))?;
    let plain = key
        .open_in_place(nonce, Aad::empty(), &mut buf)
        .map_err(|_| SecretsError::Key("open failed".into()))?;
    Ok(plain.to_vec())
}

fn key_id_for(key: &[u8; 32]) -> String {
    let mut h = Sha256::new();
    h.update(b"openrt.secrets.keyid.v1");
    h.update(key);
    hex::encode(&h.finalize()[..8])
}

/// Encrypted secret store over the foundation store + an in-memory master key.
pub struct SecretStore {
    store: Arc<dyn openrt_persistence::PersistenceStore>,
    master: MasterKey,
    audit: Option<Arc<openrt_audit::AuditLog>>,
    lock: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretStore")
            .field("key_id", &self.master.key_id)
            .finish_non_exhaustive()
    }
}

impl SecretStore {
    /// Open with an explicit 32-byte master key (keyring/TPM path supplies
    /// these bytes; never derive from a password alone without KDF).
    pub fn with_key(
        store: Arc<dyn openrt_persistence::PersistenceStore>,
        key: [u8; 32],
    ) -> Result<Self, SecretsError> {
        let key_id = key_id_for(&key);
        let ub = unbound(&key)?;
        Ok(Self {
            store,
            master: MasterKey {
                key_id,
                key: LessSafeKey::new(ub),
            },
            audit: None,
            lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Open using a keyfile path (creates the file 0600 if absent).
    pub async fn open_keyfile(
        store: Arc<dyn openrt_persistence::PersistenceStore>,
        keyfile: &Path,
    ) -> Result<Self, SecretsError> {
        let key = load_or_create_keyfile(keyfile)?;
        let s = Self::with_key(store, key)?;
        s.ensure_key_meta().await?;
        Ok(s)
    }

    /// Attach the shared audit log for operator-visible seal/open/rotate.
    pub fn with_audit(mut self, audit: Arc<openrt_audit::AuditLog>) -> Self {
        self.audit = Some(audit);
        self
    }

    pub fn key_id(&self) -> &str {
        &self.master.key_id
    }

    async fn ensure_key_meta(&self) -> Result<(), SecretsError> {
        let meta = KeyMeta {
            key_id: self.master.key_id.clone(),
            created_at: openrt_core::now().to_rfc3339(),
            active: true,
        };
        let key = format!("k.{}", self.master.key_id);
        if self
            .store
            .get_record(SECRETS_KEYS_NS, &key)
            .await?
            .is_none()
        {
            self.store
                .put_record(SECRETS_KEYS_NS, &key, &serde_json::to_value(&meta)?)
                .await?;
        }
        Ok(())
    }

    /// Seal and persist a secret. Fail-closed on store errors.
    pub async fn put(&self, name: &str, value: &[u8]) -> Result<(), SecretsError> {
        if name.trim().is_empty() {
            return Err(SecretsError::EmptyName);
        }
        let _g = self.lock.lock().await;
        let generation = match self.store.get_record(SECRETS_NS, name).await? {
            Some(v) => {
                let prev: SealedSecret = serde_json::from_value(v)?;
                prev.generation + 1
            }
            None => 1,
        };
        let sealed = seal(&self.master.key, value)?;
        let rec = SealedSecret {
            name: name.to_string(),
            key_id: self.master.key_id.clone(),
            algorithm: ALGORITHM.to_string(),
            ciphertext_b64: B64.encode(sealed),
            sealed_at: openrt_core::now().to_rfc3339(),
            generation,
        };
        self.store
            .put_record(SECRETS_NS, name, &serde_json::to_value(&rec)?)
            .await?;
        if let Some(audit) = &self.audit {
            audit
                .append(
                    "operator",
                    "secret.seal",
                    name,
                    "sealed",
                    serde_json::json!({
                        "key_id": rec.key_id,
                        "generation": rec.generation,
                        "algorithm": rec.algorithm,
                    }),
                )
                .await?;
        }
        Ok(())
    }

    /// Open a secret by name (returns plaintext only to the caller).
    pub async fn get(&self, name: &str) -> Result<Vec<u8>, SecretsError> {
        let v = self
            .store
            .get_record(SECRETS_NS, name)
            .await?
            .ok_or_else(|| SecretsError::NotFound(name.to_string()))?;
        let rec: SealedSecret = serde_json::from_value(v)?;
        if rec.key_id != self.master.key_id {
            return Err(SecretsError::DecryptFailed(format!(
                "{name}: sealed under key {}, active key is {}",
                rec.key_id, self.master.key_id
            )));
        }
        let raw = B64
            .decode(&rec.ciphertext_b64)
            .map_err(|e| SecretsError::Key(e.to_string()))?;
        open(&self.master.key, &raw).map_err(|_| SecretsError::DecryptFailed(name.to_string()))
    }

    /// Whether a secret exists (no plaintext involved).
    pub async fn contains(&self, name: &str) -> Result<bool, SecretsError> {
        Ok(self.store.get_record(SECRETS_NS, name).await?.is_some())
    }

    /// List secret names and metadata only (never ciphertext).
    pub async fn list(&self) -> Result<Vec<SealedSecret>, SecretsError> {
        let rows = self.store.list_records(SECRETS_NS).await?;
        let mut out = Vec::new();
        for (k, v) in rows {
            let mut rec: SealedSecret = serde_json::from_value(v)?;
            rec.name = k;
            // Never leak ciphertext in list views.
            rec.ciphertext_b64 = String::new();
            out.push(rec);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Delete a secret (operator recovery). Audited when a log is attached.
    pub async fn delete(&self, name: &str) -> Result<(), SecretsError> {
        let _g = self.lock.lock().await;
        if self.store.get_record(SECRETS_NS, name).await?.is_none() {
            return Err(SecretsError::NotFound(name.to_string()));
        }
        self.store.delete_record(SECRETS_NS, name).await?;
        if let Some(audit) = &self.audit {
            audit
                .append(
                    "operator",
                    "secret.delete",
                    name,
                    "deleted",
                    serde_json::json!({}),
                )
                .await?;
        }
        Ok(())
    }

    /// Rotate the master key: re-seal every secret under `new_key`.
    /// Fail-closed: if any reseal fails, previously re-sealed entries remain
    /// under the new key and the error is returned (no plaintext is written).
    pub async fn rotate(&self, new_key: [u8; 32]) -> Result<String, SecretsError> {
        let _g = self.lock.lock().await;
        let new_id = key_id_for(&new_key);
        if new_id == self.master.key_id {
            return Ok(new_id);
        }
        let ub = unbound(&new_key)?;
        let new_master = LessSafeKey::new(ub);
        let rows = self.store.list_records(SECRETS_NS).await?;
        for (name, v) in rows {
            let rec: SealedSecret = serde_json::from_value(v)?;
            if rec.key_id == new_id {
                continue;
            }
            // Decrypt under current, reseal under new.
            let raw = B64
                .decode(&rec.ciphertext_b64)
                .map_err(|e| SecretsError::Key(e.to_string()))?;
            let plain = open(&self.master.key, &raw)
                .map_err(|_| SecretsError::DecryptFailed(name.clone()))?;
            let sealed = seal(&new_master, &plain)?;
            // Best-effort wipe of the temporary plaintext buffer.
            let mut wipe = plain;
            for b in wipe.iter_mut() {
                *b = 0;
            }
            let next = SealedSecret {
                name: rec.name.clone(),
                key_id: new_id.clone(),
                algorithm: ALGORITHM.to_string(),
                ciphertext_b64: B64.encode(sealed),
                sealed_at: openrt_core::now().to_rfc3339(),
                generation: rec.generation + 1,
            };
            self.store
                .put_record(SECRETS_NS, &name, &serde_json::to_value(&next)?)
                .await?;
        }
        // Mark old key inactive, new active (metadata only).
        let old_meta = KeyMeta {
            key_id: self.master.key_id.clone(),
            created_at: openrt_core::now().to_rfc3339(),
            active: false,
        };
        self.store
            .put_record(
                SECRETS_KEYS_NS,
                &format!("k.{}", old_meta.key_id),
                &serde_json::to_value(&old_meta)?,
            )
            .await?;
        let new_meta = KeyMeta {
            key_id: new_id.clone(),
            created_at: openrt_core::now().to_rfc3339(),
            active: true,
        };
        self.store
            .put_record(
                SECRETS_KEYS_NS,
                &format!("k.{new_id}"),
                &serde_json::to_value(&new_meta)?,
            )
            .await?;
        if let Some(audit) = &self.audit {
            audit
                .append(
                    "operator",
                    "secret.rotate",
                    "master",
                    "rotated",
                    serde_json::json!({
                        "from": self.master.key_id,
                        "to": new_id,
                    }),
                )
                .await?;
        }
        // Note: in-process master is not swapped (open callers rebuild); the
        // rotation path is primarily for re-sealing at rest before restart.
        // Production Runtime rebuilds SecretStore after rotate.
        Ok(new_id)
    }
}

/// Best-effort check that a byte string does not appear as plaintext in a
/// file (secrets-at-rest leakage test helper; no network, no canaries).
pub fn file_contains_plaintext(path: &Path, secret: &[u8]) -> Result<bool, SecretsError> {
    if !path.exists() {
        return Ok(false);
    }
    let hay = std::fs::read(path)?;
    Ok(hay.windows(secret.len()).any(|w| w == secret))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    fn store() -> SecretStore {
        SecretStore::with_key(Arc::new(MemoryStore::new()), [7u8; 32]).unwrap()
    }

    #[tokio::test]
    async fn roundtrip_seal_open() {
        let s = store();
        s.put("api_token", b"sk-super-secret").await.unwrap();
        let out = s.get("api_token").await.unwrap();
        assert_eq!(out, b"sk-super-secret");
        // Ciphertext at rest ≠ plaintext.
        let rows = s.store.list_records(SECRETS_NS).await.unwrap();
        let (_, v) = &rows[0];
        let rec: SealedSecret = serde_json::from_value(v.clone()).unwrap();
        assert!(!rec.ciphertext_b64.contains("sk-super-secret"));
        assert_eq!(rec.algorithm, ALGORITHM);
        assert_eq!(rec.generation, 1);
    }

    #[tokio::test]
    async fn missing_secret_not_found() {
        let s = store();
        assert!(matches!(
            s.get("nope").await,
            Err(SecretsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn wrong_key_fails_closed() {
        let s = store();
        s.put("k", b"v").await.unwrap();
        let other = SecretStore::with_key(Arc::new(MemoryStore::new()), [9u8; 32]).unwrap();
        // Different store — not found. Same store, wrong key:
        let wrong = SecretStore::with_key(s.store.clone(), [9u8; 32]).unwrap();
        let err = wrong.get("k").await.unwrap_err();
        assert!(matches!(err, SecretsError::DecryptFailed(_)));
        let _ = other;
    }

    #[tokio::test]
    async fn rotation_preserves_plaintext() {
        let s = store();
        s.put("a", b"alpha").await.unwrap();
        s.put("b", b"beta").await.unwrap();
        let new_id = s.rotate([3u8; 32]).await.unwrap();
        assert_ne!(new_id, s.key_id());
        // After rotation, records use new key id; old in-process master can no
        // longer open them (fail-closed until rebuild).
        assert!(matches!(
            s.get("a").await,
            Err(SecretsError::DecryptFailed(_))
        ));
        // Rebuild with new key: plaintext preserved.
        let rebuilt = SecretStore::with_key(s.store.clone(), [3u8; 32]).unwrap();
        assert_eq!(rebuilt.get("a").await.unwrap(), b"alpha");
        assert_eq!(rebuilt.get("b").await.unwrap(), b"beta");
        let list = rebuilt.list().await.unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|r| r.ciphertext_b64.is_empty()));
        assert!(list.iter().all(|r| r.key_id == new_id));
    }

    #[tokio::test]
    async fn keyfile_0600_roundtrip() {
        let dir = std::env::temp_dir().join(format!("openrt-sec-{}", uuid::Uuid::new_v4()));
        let path = dir.join("secrets.key");
        let s = SecretStore::open_keyfile(Arc::new(MemoryStore::new()), &path)
            .await
            .unwrap();
        s.put("x", b"y").await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Reopen same keyfile: decrypt works.
        let s2 = SecretStore::open_keyfile(s.store.clone(), &path)
            .await
            .unwrap();
        assert_eq!(s2.get("x").await.unwrap(), b"y");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn empty_name_rejected() {
        let s = store();
        assert!(matches!(
            s.put("", b"v").await,
            Err(SecretsError::EmptyName)
        ));
    }
}
