use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use secrecy::{ExposeSecret, SecretBox};
use serde::{Serialize, de::DeserializeOwned};
use tokio::fs;
use tokio::sync::RwLock;

use crate::dirs;
use crate::security::{SafeKey, SecurityManager, VaultSession};

use super::envelope::{
    CURRENT_SCHEMA_VERSION, EncryptionStatus, KeyScheme, StorageEnvelope, StorageType,
};
use super::keys::derive_module_storage_key;

const EXTERNAL_STORAGE_THRESHOLD: usize = 50 * 1024;

/// Result of `StorageManager::migrate_module_encryption`.
#[derive(Debug, Default)]
pub struct MigrationReport {
    pub migrated: usize,
    pub skipped: usize,
    pub errors: Vec<(String, String)>,
}

/// Hybrid encrypted storage manager.
/// Key resolution is deferred to `VaultSession` to avoid recreating the manager on unlock.
pub struct StorageManager {
    session: Arc<VaultSession>,
    vault_root: PathBuf,
    db_cache: RwLock<HashMap<String, sled::Db>>,
}

impl StorageManager {
    pub async fn new(session: Arc<VaultSession>) -> Result<Self> {
        let data_dir = dirs::get_data_dir()?;
        Self::new_with_root(session, data_dir).await
    }

    pub async fn new_with_root(session: Arc<VaultSession>, root_path: PathBuf) -> Result<Self> {
        let vault_root = root_path.join("vault");
        if !vault_root.exists() {
            fs::create_dir_all(&vault_root)
                .await
                .context("Failed to create vault root directory")?;
        }
        Ok(Self {
            session,
            vault_root,
            db_cache: RwLock::new(HashMap::new()),
        })
    }

    async fn get_or_open_db(&self, module_id: &str) -> Result<sled::Db> {
        {
            let cache = self.db_cache.read().await;
            if let Some(db) = cache.get(module_id) {
                return Ok(db.clone());
            }
        }

        // Hold the write lock across the `spawn_blocking` await (a
        // `tokio::sync::RwLock` guard is designed for this, unlike
        // `std::sync::RwLock`) so concurrent first-time opens of the same
        // path are serialized here rather than racing `sled::open` itself
        // — sled takes an OS-level file lock per DB, so two concurrent
        // opens of a path neither caller has cached yet fail rather than
        // dedupe. Only the actual open() I/O is offloaded to the blocking
        // pool; the lock/cache bookkeeping around it is cheap.
        let mut cache = self.db_cache.write().await;
        if let Some(db) = cache.get(module_id) {
            return Ok(db.clone());
        }
        let db_path = self.vault_root.join(module_id).join("db");
        let opened = tokio::task::spawn_blocking({
            let db_path = db_path.clone();
            move || {
                sled::open(&db_path).context(format!("Failed to open sled DB: {:?}", db_path))
            }
        })
        .await
        .map_err(|e| anyhow!("sled::open task panicked: {e}"))??;
        cache.insert(module_id.to_string(), opened.clone());
        Ok(opened)
    }

    /// Derives the write key for `module_id` from the currently-unlocked
    /// vault master key — always the new per-module HKDF subkey (see
    /// `storage::keys::derive_module_storage_key`), never the raw master
    /// key directly. Every new write uses this, unconditionally.
    fn resolve_write_key(&self, module_id: &str) -> Result<SecretBox<SafeKey>> {
        let master = self
            .session
            .current()
            .ok_or_else(|| anyhow!("Vault locked"))?;
        Ok(derive_module_storage_key(&master, module_id))
    }

    /// Resolves the key to *decrypt* an existing envelope with, based on
    /// which scheme it was written under — `Legacy` envelopes (every
    /// record written before this change, and forever if a user never
    /// explicitly re-migrates) still decrypt with the raw master key,
    /// exactly as before; `PerModuleV1` envelopes use the derived subkey.
    fn resolve_read_key(&self, module_id: &str, scheme: KeyScheme) -> Result<SecretBox<SafeKey>> {
        let master = self
            .session
            .current()
            .ok_or_else(|| anyhow!("Vault locked"))?;
        Ok(match scheme {
            KeyScheme::Legacy => SecretBox::new(Box::new(master.expose_secret().clone())),
            KeyScheme::PerModuleV1 => derive_module_storage_key(&master, module_id),
        })
    }

    /// Drops the cached handle for `module_id`, releasing sled's exclusive
    /// process-level file lock on that module's DB. sled only allows one
    /// process to hold a given DB open at a time, so a long-lived process
    /// (the daemon) that doesn't need continuous access to a module should
    /// call this after each use — otherwise any other process (e.g. a TUI)
    /// trying to open the same module's DB fails with "Failed to open sled
    /// DB" for as long as the first process keeps it cached.
    pub async fn close_db(&self, module_id: &str) {
        self.db_cache.write().await.remove(module_id);
    }

    pub async fn save<T: Serialize + Send + Sync + 'static>(
        &self,
        module_id: &str,
        key: &str,
        data: &T,
        is_encryption_enabled: bool,
    ) -> Result<()> {
        self.save_impl(module_id, key, data, is_encryption_enabled, true)
            .await
    }

    /// `flush`: whether to fsync-equivalent this write immediately. Regular
    /// callers always want `true` (durability per call); the bulk
    /// migration path (`migrate_module_encryption`) passes `false` for
    /// every individual key and flushes once after the whole batch instead
    /// of once per key.
    async fn save_impl<T: Serialize + Send + Sync + 'static>(
        &self,
        module_id: &str,
        key: &str,
        data: &T,
        is_encryption_enabled: bool,
        flush: bool,
    ) -> Result<()> {
        let (_, blobs_path) = self.prepare_module_paths(module_id).await?;
        let db = self.get_or_open_db(module_id).await?;
        let raw_bytes = serde_json::to_vec(data).context("Serialization failed")?;

        // Every new write uses the derived per-module subkey — never the
        // raw master key directly (see storage::keys::derive_module_storage_key).
        let session_key = if is_encryption_enabled {
            Some(self.resolve_write_key(module_id)?)
        } else {
            None
        };

        let key_string = key.to_string();
        let module_id_str = module_id.to_string();

        let (final_payload, storage_type) = tokio::task::spawn_blocking(move || {
            let (processed_payload, status) = if let Some(k) = session_key {
                (
                    SecurityManager::encrypt(&raw_bytes, &k)?,
                    EncryptionStatus::Encrypted,
                )
            } else {
                (raw_bytes, EncryptionStatus::Plaintext)
            };

            let storage_type = if processed_payload.len() > EXTERNAL_STORAGE_THRESHOLD {
                StorageType::External
            } else {
                StorageType::Embedded
            };

            let envelope = StorageEnvelope {
                schema_version: CURRENT_SCHEMA_VERSION,
                status: status.clone(),
                storage_type: storage_type.clone(),
                payload: if storage_type == StorageType::External {
                    Vec::new()
                } else {
                    processed_payload.clone()
                },
                hash: None,
                key_scheme: if status == EncryptionStatus::Encrypted {
                    KeyScheme::PerModuleV1
                } else {
                    KeyScheme::Legacy
                },
            };

            let envelope_bytes = serde_json::to_vec(&envelope)?;
            db.insert(key_string, envelope_bytes)?;
            if flush {
                db.flush()?;
            }

            Ok::<_, anyhow::Error>((processed_payload, storage_type))
        })
        .await??;

        if storage_type == StorageType::External {
            let file_name = format!("{}_{}.blob", module_id_str, key);
            fs::write(blobs_path.join(file_name), &final_payload)
                .await
                .context("Failed to write external blob")?;
        }

        Ok(())
    }

    /// Reads and parses `key`'s envelope, resolving external-blob payloads
    /// — but does not decrypt. Split out from `load` so the migration path
    /// can reuse the already-fetched envelope/ciphertext instead of a
    /// second full read (`load` does its own second `db.get` today just to
    /// re-check "is this already migrated?").
    async fn fetch_envelope(
        &self,
        module_id: &str,
        key: &str,
    ) -> Result<(StorageEnvelope, Vec<u8>)> {
        let (_, blobs_path) = self.prepare_module_paths(module_id).await?;
        let db = self.get_or_open_db(module_id).await?;
        let key_string = key.to_string();

        let iv = tokio::task::spawn_blocking(move || db.get(&key_string))
            .await??
            .ok_or_else(|| anyhow!("Key not found: {}", key))?;

        let envelope: StorageEnvelope =
            serde_json::from_slice(&iv).context("Envelope corrupted")?;

        let raw_data = match envelope.storage_type {
            StorageType::Embedded => envelope.payload.clone(),
            StorageType::External => {
                let file_name = format!("{}_{}.blob", module_id, key);
                fs::read(blobs_path.join(file_name))
                    .await
                    .context("External blob file missing")?
            }
        };

        Ok((envelope, raw_data))
    }

    /// Decrypts (if needed) and deserializes an already-fetched envelope's
    /// raw payload, picking the key by `envelope.key_scheme` — `Legacy`
    /// envelopes (every record written before per-module subkeys existed)
    /// still decrypt with the raw master key, exactly as before.
    async fn decrypt_payload<T: DeserializeOwned>(
        &self,
        module_id: &str,
        envelope: &StorageEnvelope,
        raw_data: Vec<u8>,
    ) -> Result<T> {
        let session_key = if envelope.status == EncryptionStatus::Encrypted {
            Some(self.resolve_read_key(module_id, envelope.key_scheme)?)
        } else {
            None
        };

        let decrypted_data = tokio::task::spawn_blocking(move || {
            if let Some(k) = session_key {
                SecurityManager::decrypt(&raw_data, &k)
            } else {
                Ok(raw_data)
            }
        })
        .await??;

        Ok(serde_json::from_slice(&decrypted_data)?)
    }

    pub async fn load<T: DeserializeOwned>(&self, module_id: &str, key: &str) -> Result<T> {
        let (envelope, raw_data) = self.fetch_envelope(module_id, key).await?;
        self.decrypt_payload(module_id, &envelope, raw_data).await
    }

    /// Re-saves every record under `module_id` so its on-disk
    /// `EncryptionStatus` matches `target_encrypted`. Records already in
    /// the target state are left untouched — idempotent, safe to call
    /// repeatedly (e.g. after a config change, to reconcile drift).
    pub async fn migrate_module_encryption(
        &self,
        module_id: &str,
        target_encrypted: bool,
    ) -> Result<MigrationReport> {
        if target_encrypted && self.session.current().is_none() {
            anyhow::bail!("Vault must be unlocked to migrate '{module_id}' to encrypted");
        }

        let db = self.get_or_open_db(module_id).await?;
        let keys: Vec<String> = tokio::task::spawn_blocking({
            let db = db.clone();
            move || {
                db.iter()
                    .filter_map(|entry| entry.ok())
                    .map(|(k, _)| String::from_utf8_lossy(&k).to_string())
                    .collect::<Vec<_>>()
            }
        })
        .await?;

        let mut report = MigrationReport {
            migrated: 0,
            skipped: 0,
            errors: Vec::new(),
        };

        for key in keys {
            match self
                .migrate_one_key(module_id, &key, target_encrypted)
                .await
            {
                Ok(true) => report.migrated += 1,
                Ok(false) => report.skipped += 1,
                Err(e) => report.errors.push((key, e.to_string())),
            }
        }

        // `migrate_one_key` defers its own flush (see `save_impl`'s `flush`
        // param) so a migration touching many keys does one fsync-
        // equivalent for the whole batch instead of one per key.
        tokio::task::spawn_blocking(move || db.flush()).await??;

        Ok(report)
    }

    /// Returns `Ok(true)` if `key` was re-saved, `Ok(false)` if it already
    /// matched `target_encrypted` AND was already on the current key
    /// scheme. A record that's already `Encrypted` under `target_encrypted
    /// == true` but still on `KeyScheme::Legacy` also counts as needing
    /// migration — this is what lets re-running "migrate to encrypted"
    /// after upgrading also transparently upgrade the key scheme, with no
    /// separate migration action needed.
    async fn migrate_one_key(
        &self,
        module_id: &str,
        key: &str,
        target_encrypted: bool,
    ) -> Result<bool> {
        let (envelope, raw_data) = self.fetch_envelope(module_id, key).await?;

        let currently_encrypted = envelope.status == EncryptionStatus::Encrypted;
        let stale_key_scheme =
            currently_encrypted && envelope.key_scheme != KeyScheme::PerModuleV1;
        if currently_encrypted == target_encrypted && !stale_key_scheme {
            return Ok(false);
        }

        let value: serde_json::Value =
            self.decrypt_payload(module_id, &envelope, raw_data).await?;
        self.save_impl(module_id, key, &value, target_encrypted, false)
            .await?;
        Ok(true)
    }

    async fn prepare_module_paths(&self, module_id: &str) -> Result<(PathBuf, PathBuf)> {
        let module_root = self.vault_root.join(module_id);
        let db_path = module_root.join("db");
        let blobs_path = module_root.join("blobs");
        if !db_path.exists() {
            fs::create_dir_all(&db_path).await?;
        }
        if !blobs_path.exists() {
            fs::create_dir_all(&blobs_path).await?;
        }
        Ok((db_path, blobs_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::VaultSession;
    use tempfile::tempdir;

    fn locked_session() -> Arc<VaultSession> {
        Arc::new(VaultSession::new())
    }

    async fn unlocked_session() -> Arc<VaultSession> {
        let session = VaultSession::new();
        let key = SecurityManager::derive_key("test_pass", &[7u8; 16])
            .await
            .unwrap();
        session.unlock(key);
        Arc::new(session)
    }

    #[tokio::test]
    async fn test_db_cache_mechanism() {
        let temp = tempdir().unwrap();
        let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
            .await
            .unwrap();

        let _db1 = manager
            .get_or_open_db("test_mod")
            .await
            .expect("First open failed");
        let _db2 = manager
            .get_or_open_db("test_mod")
            .await
            .expect("Second open failed");

        assert_eq!(
            manager.db_cache.read().await.len(),
            1,
            "Cache must contain exactly 1 DB instance"
        );
    }

    #[tokio::test]
    async fn test_storage_envelope_logic() {
        let temp = tempdir().unwrap();
        let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
            .await
            .unwrap();

        let small_data = "Moku".to_string();
        manager
            .save("mod1", "key1", &small_data, false)
            .await
            .unwrap();
        let loaded: String = manager.load("mod1", "key1").await.unwrap();
        assert_eq!(small_data, loaded);

        let large_data = "A".repeat(EXTERNAL_STORAGE_THRESHOLD + 1024);
        manager
            .save("mod1", "large_key", &large_data, false)
            .await
            .unwrap();
        let loaded_large: String = manager.load("mod1", "large_key").await.unwrap();
        assert_eq!(large_data, loaded_large);
    }

    #[tokio::test]
    async fn test_encryption_error_without_unlocked_session() {
        let temp = tempdir().unwrap();
        let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
            .await
            .unwrap();

        let data = "secret".to_string();
        let result = manager.save("mod1", "key1", &data, true).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Vault locked"));
    }

    #[tokio::test]
    async fn test_encrypted_roundtrip_with_unlocked_session() {
        let temp = tempdir().unwrap();
        let manager =
            StorageManager::new_with_root(unlocked_session().await, temp.path().to_path_buf())
                .await
                .unwrap();

        let data = "secret".to_string();
        manager.save("mod1", "key1", &data, true).await.unwrap();
        let loaded: String = manager.load("mod1", "key1").await.unwrap();
        assert_eq!(data, loaded);
    }

    /// REGRESSION TEST: Verifies vault can be unlocked mid-lifetime via `VaultSession`
    /// without recreating the `StorageManager` or wiping the internal database cache.
    #[tokio::test]
    async fn test_session_unlock_mid_lifetime_without_recreating_manager() {
        let temp = tempdir().unwrap();
        let session = Arc::new(VaultSession::new());
        let manager =
            StorageManager::new_with_root(Arc::clone(&session), temp.path().to_path_buf())
                .await
                .unwrap();

        let data = "secret".to_string();
        assert!(manager.save("mod1", "key1", &data, true).await.is_err());

        let key = SecurityManager::derive_key("test_pass", &[3u8; 16])
            .await
            .unwrap();
        session.unlock(key);

        manager.save("mod1", "key1", &data, true).await.unwrap();
        let loaded: String = manager.load("mod1", "key1").await.unwrap();
        assert_eq!(data, loaded);
    }

    #[tokio::test]
    async fn test_concurrent_access() {
        let temp = tempdir().unwrap();
        let manager = Arc::new(
            StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
                .await
                .unwrap(),
        );

        let mut handles = vec![];
        for i in 0..10 {
            let m = Arc::clone(&manager);
            handles.push(tokio::spawn(async move {
                m.save("shared_mod", &format!("key_{}", i), &i, false).await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }

        for i in 0..10 {
            let val: i32 = manager
                .load("shared_mod", &format!("key_{}", i))
                .await
                .unwrap();
            assert_eq!(val, i);
        }
    }

    #[tokio::test]
    async fn test_storage_atomic_integrity_simulation() {
        let temp = tempdir().unwrap();
        let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
            .await
            .unwrap();

        let initial_data = "Original Safe Data".to_string();
        manager
            .save("atomic_test", "key1", &initial_data, false)
            .await
            .unwrap();

        let corrupted_attempt = "New Corrupted Data".to_string();
        let result = manager
            .save("atomic_test", "key1", &corrupted_attempt, true)
            .await;
        assert!(result.is_err());

        let final_check: String = manager.load("atomic_test", "key1").await.unwrap();
        assert_eq!(final_check, initial_data);
    }

    #[tokio::test]
    async fn test_migrate_plaintext_to_encrypted() {
        let temp = tempdir().unwrap();
        let manager =
            StorageManager::new_with_root(unlocked_session().await, temp.path().to_path_buf())
                .await
                .unwrap();

        manager
            .save("mod1", "a", &"one".to_string(), false)
            .await
            .unwrap();
        manager
            .save("mod1", "b", &"two".to_string(), false)
            .await
            .unwrap();

        let report = manager
            .migrate_module_encryption("mod1", true)
            .await
            .unwrap();
        assert_eq!(report.migrated, 2);
        assert_eq!(report.skipped, 0);
        assert!(report.errors.is_empty());

        // Data survives the round trip and reads back as still-encrypted.
        let a: String = manager.load("mod1", "a").await.unwrap();
        assert_eq!(a, "one");
    }

    #[tokio::test]
    async fn test_migrate_requires_unlocked_vault_for_encrypted_target() {
        let temp = tempdir().unwrap();
        let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
            .await
            .unwrap();

        manager
            .save("mod1", "a", &"one".to_string(), false)
            .await
            .unwrap();

        let result = manager.migrate_module_encryption("mod1", true).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unlocked"));
    }

    #[tokio::test]
    async fn test_migrate_is_idempotent() {
        let temp = tempdir().unwrap();
        let manager =
            StorageManager::new_with_root(unlocked_session().await, temp.path().to_path_buf())
                .await
                .unwrap();

        manager
            .save("mod1", "a", &"one".to_string(), true)
            .await
            .unwrap();

        // Already encrypted, target is encrypted: nothing to do.
        let report = manager
            .migrate_module_encryption("mod1", true)
            .await
            .unwrap();
        assert_eq!(report.migrated, 0);
        assert_eq!(report.skipped, 1);
    }

    #[tokio::test]
    async fn test_migrate_encrypted_to_plaintext_readable_after_relock() {
        let temp = tempdir().unwrap();
        let session = Arc::new(VaultSession::new());
        let key = SecurityManager::derive_key("test_pass", &[9u8; 16])
            .await
            .unwrap();
        session.unlock(key);
        let manager =
            StorageManager::new_with_root(Arc::clone(&session), temp.path().to_path_buf())
                .await
                .unwrap();

        manager
            .save("mod1", "a", &"one".to_string(), true)
            .await
            .unwrap();

        let report = manager
            .migrate_module_encryption("mod1", false)
            .await
            .unwrap();
        assert_eq!(report.migrated, 1);

        // Once migrated to plaintext, the record must be readable even
        // with the vault locked again — proving it's genuinely plaintext
        // on disk now, not still encrypted.
        session.lock();
        let a: String = manager.load("mod1", "a").await.unwrap();
        assert_eq!(a, "one");
    }

    /// Simulates a record written before per-module HKDF subkeys existed
    /// (directly encrypted under the raw master key, `key_scheme: Legacy`,
    /// no `save()` call involved) — `load()` must still transparently read
    /// it correctly, with zero action required, forever.
    #[tokio::test]
    async fn test_legacy_key_scheme_record_still_loads() {
        let temp = tempdir().unwrap();
        let session = unlocked_session().await;
        let manager =
            StorageManager::new_with_root(Arc::clone(&session), temp.path().to_path_buf())
                .await
                .unwrap();

        let master = session.current().unwrap();
        let raw_bytes = serde_json::to_vec(&"legacy value".to_string()).unwrap();
        let ciphertext = SecurityManager::encrypt(&raw_bytes, &master).unwrap();
        let envelope = StorageEnvelope {
            schema_version: 1,
            status: EncryptionStatus::Encrypted,
            storage_type: StorageType::Embedded,
            payload: ciphertext,
            hash: None,
            key_scheme: KeyScheme::Legacy,
        };
        let envelope_bytes = serde_json::to_vec(&envelope).unwrap();

        let db = manager.get_or_open_db("mod1").await.unwrap();
        db.insert("a", envelope_bytes).unwrap();
        db.flush().unwrap();

        let loaded: String = manager.load("mod1", "a").await.unwrap();
        assert_eq!(loaded, "legacy value");
    }

    /// Every new encrypted write must land as `KeyScheme::PerModuleV1`, not
    /// the legacy raw-master-key scheme.
    #[tokio::test]
    async fn test_save_always_writes_per_module_key_scheme() {
        let temp = tempdir().unwrap();
        let manager =
            StorageManager::new_with_root(unlocked_session().await, temp.path().to_path_buf())
                .await
                .unwrap();

        manager
            .save("mod1", "a", &"one".to_string(), true)
            .await
            .unwrap();

        let db = manager.get_or_open_db("mod1").await.unwrap();
        let raw = db.get("a").unwrap().unwrap();
        let envelope: StorageEnvelope = serde_json::from_slice(&raw).unwrap();
        assert_eq!(envelope.key_scheme, KeyScheme::PerModuleV1);
    }

    /// Real domain separation: the per-module subkey differs from the raw
    /// master key, and ciphertext encrypted under one cannot be decrypted
    /// with the other (AEAD auth failure, not silent garbage).
    #[tokio::test]
    async fn test_per_module_key_differs_from_master_and_cannot_cross_decrypt() {
        let master = SecretBox::new(Box::new(SafeKey([5u8; 32])));
        let derived = derive_module_storage_key(&master, "mod1");
        assert_ne!(master.expose_secret().0, derived.expose_secret().0);

        let plaintext = b"hello".to_vec();
        let ciphertext = SecurityManager::encrypt(&plaintext, &master).unwrap();
        assert!(SecurityManager::decrypt(&ciphertext, &derived).is_err());
    }

    /// Re-running "migrate to encrypted" on a module that already has a
    /// Legacy-scheme Encrypted record must upgrade it to PerModuleV1 (not
    /// skip it just because `EncryptionStatus` already matches) — this is
    /// the whole mechanism a real user re-triggers to migrate existing
    /// data after upgrading, with no new UI/command needed.
    #[tokio::test]
    async fn test_migrate_upgrades_legacy_key_scheme_to_per_module_v1() {
        let temp = tempdir().unwrap();
        let session = unlocked_session().await;
        let manager =
            StorageManager::new_with_root(Arc::clone(&session), temp.path().to_path_buf())
                .await
                .unwrap();

        let master = session.current().unwrap();
        let raw_bytes = serde_json::to_vec(&"legacy value".to_string()).unwrap();
        let ciphertext = SecurityManager::encrypt(&raw_bytes, &master).unwrap();
        let envelope = StorageEnvelope {
            schema_version: 1,
            status: EncryptionStatus::Encrypted,
            storage_type: StorageType::Embedded,
            payload: ciphertext,
            hash: None,
            key_scheme: KeyScheme::Legacy,
        };
        let envelope_bytes = serde_json::to_vec(&envelope).unwrap();
        let db = manager.get_or_open_db("mod1").await.unwrap();
        db.insert("a", envelope_bytes).unwrap();
        db.flush().unwrap();

        let report = manager
            .migrate_module_encryption("mod1", true)
            .await
            .unwrap();
        assert_eq!(
            report.migrated, 1,
            "stale key scheme must count as needing migration"
        );
        assert_eq!(report.skipped, 0);

        let db = manager.get_or_open_db("mod1").await.unwrap();
        let raw = db.get("a").unwrap().unwrap();
        let envelope: StorageEnvelope = serde_json::from_slice(&raw).unwrap();
        assert_eq!(envelope.key_scheme, KeyScheme::PerModuleV1);

        let loaded: String = manager.load("mod1", "a").await.unwrap();
        assert_eq!(loaded, "legacy value");

        // Idempotent: running it again on already-upgraded data is a no-op.
        let report2 = manager
            .migrate_module_encryption("mod1", true)
            .await
            .unwrap();
        assert_eq!(report2.migrated, 0);
        assert_eq!(report2.skipped, 1);
    }
}
