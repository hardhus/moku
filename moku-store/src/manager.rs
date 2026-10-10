use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Serialize, de::DeserializeOwned};
use tokio::fs;
use tokio::sync::RwLock;
use zeroize::Zeroizing;

use moku_crypto::{CryptoError, Key, ModuleStorageKey, format_v1, module_storage_key};
use moku_sys::dirs;
use moku_vault::VaultSession;

use crate::StoreError;
use crate::envelope::{
    CURRENT_KEY_SCHEME, CURRENT_SCHEMA_VERSION, EncryptionStatus, KeyScheme, StorageEnvelope,
    StorageType,
};

type Result<T> = std::result::Result<T, StoreError>;

const EXTERNAL_STORAGE_THRESHOLD: usize = 50 * 1024;

/// The pre-`FORMAT` key-scheme marker. A fresh `vault/` still gets one
/// (value 1) so moku-bin 0.2.1-0.2.11 opened on it see an up-to-date
/// directory; nothing in this crate reads it (moku-migrate infers the
/// format of a directory without `FORMAT` from it).
const KEY_SCHEME_MARKER_FILENAME: &str = ".key_scheme_version";

/// Result of `StorageManager::migrate_module_encryption`: per-record
/// failures are collected
/// (record key + typed error) instead of aborting the whole module.
#[derive(Debug, Default)]
pub struct MigrationReport {
    pub migrated: usize,
    pub skipped: usize,
    pub errors: Vec<(String, StoreError)>,
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
        let is_fresh = !vault_root.exists();
        if is_fresh {
            fs::create_dir_all(&vault_root)
                .await
                .map_err(StoreError::io("create the vault root directory", &vault_root))?;
        }
        let manager = Self {
            session,
            vault_root,
            db_cache: RwLock::new(HashMap::new()),
        };
        if is_fresh {
            // A brand-new data directory has no pre-existing records: it
            // starts on the latest key scheme. An existing `vault_root` is
            // left untouched.
            manager
                .write_key_scheme_marker()
                .await?;
        }
        Ok(manager)
    }

    fn key_scheme_marker_path(&self) -> PathBuf {
        self.vault_root.join(KEY_SCHEME_MARKER_FILENAME)
    }

    async fn write_key_scheme_marker(&self) -> Result<()> {
        let path = self.key_scheme_marker_path();
        moku_sys::util::atomic_write_async(path, b"1".to_vec()).await?;
        Ok(())
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
        let opened = tokio::task::spawn_blocking(move || sled::open(&db_path))
            .await?
            .map_err(|e| StoreError::from_open(module_id, e))?;
        cache.insert(module_id.to_string(), opened.clone());
        Ok(opened)
    }

    /// The module's storage key, `module_storage_key(root, module_id)`
    /// (`KeyScheme::V1`), used for every read and write. The root key is
    /// only borrowed long enough to derive it.
    fn module_key(&self, module_id: &str) -> Result<Key<ModuleStorageKey>> {
        self.session
            .with(|root| module_storage_key(root, module_id))
            .map_err(|_| StoreError::Locked)
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

    /// Drops every cached DB handle, releasing all of this process's sled
    /// locks. The data-format upgrade (`moku-migrate`) opens the module
    /// DBs itself, so the TUI calls this right before running it.
    pub async fn close_all_dbs(&self) {
        self.db_cache.write().await.clear();
    }

    pub async fn save<T: Serialize + Send + Sync + ?Sized + 'static>(
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
    async fn save_impl<T: Serialize + Send + Sync + ?Sized + 'static>(
        &self,
        module_id: &str,
        key: &str,
        data: &T,
        is_encryption_enabled: bool,
        flush: bool,
    ) -> Result<()> {
        let (_, blobs_path) = self.prepare_module_paths(module_id).await?;
        let db = self.get_or_open_db(module_id).await?;
        // The plaintext JSON is wiped once it has been encrypted (or, for a
        // plaintext module, written).
        let raw_bytes = Zeroizing::new(serde_json::to_vec(data).map_err(StoreError::Encode)?);

        // Every new write uses the derived per-module subkey — never the
        // raw master key directly (see moku_crypto::module_storage_key).
        let session_key = if is_encryption_enabled {
            Some(self.module_key(module_id)?)
        } else {
            None
        };

        let key_string = key.to_string();
        let module_id_str = module_id.to_string();

        let (final_payload, storage_type) = tokio::task::spawn_blocking(move || {
            let (processed_payload, status) = if let Some(k) = session_key {
                // Envelopes are still the pre-P3 format (P6 replaces it).
                (format_v1::seal_gcm_no_aad(&k, &raw_bytes)?, EncryptionStatus::Encrypted)
            } else {
                (raw_bytes.to_vec(), EncryptionStatus::Plaintext)
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
                    CURRENT_KEY_SCHEME
                } else {
                    KeyScheme::default()
                },
            };

            let envelope_bytes = serde_json::to_vec(&envelope).map_err(StoreError::Encode)?;
            db.insert(key_string, envelope_bytes)?;
            if flush {
                db.flush()?;
            }

            Ok::<_, StoreError>((processed_payload, storage_type))
        })
        .await??;

        if storage_type == StorageType::External {
            let blob_path = blobs_path.join(format!("{}_{}.blob", module_id_str, key));
            fs::write(&blob_path, &final_payload)
                .await
                .map_err(StoreError::io("write an external blob", blob_path))?;
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
        self.fetch_envelope_opt(module_id, key)
            .await?
            .ok_or_else(|| StoreError::NotFound {
                module: module_id.to_string(),
                key: key.to_string(),
            })
    }

    /// Like `fetch_envelope`, but a key that simply doesn't exist yet is
    /// `Ok(None)`: the one case that genuinely means "nothing stored",
    /// kept separate from every real failure (unreadable DB, corrupt
    /// envelope, missing blob), which stays an `Err`.
    async fn fetch_envelope_opt(
        &self,
        module_id: &str,
        key: &str,
    ) -> Result<Option<(StorageEnvelope, Vec<u8>)>> {
        let (_, blobs_path) = self.prepare_module_paths(module_id).await?;
        let db = self.get_or_open_db(module_id).await?;
        let key_string = key.to_string();

        let Some(iv) = tokio::task::spawn_blocking(move || db.get(&key_string)).await?? else {
            return Ok(None);
        };

        let envelope: StorageEnvelope =
            serde_json::from_slice(&iv).map_err(|e| StoreError::Corrupt {
                module: module_id.to_string(),
                key: key.to_string(),
                source: Box::new(e),
            })?;

        let raw_data = match envelope.storage_type {
            StorageType::Embedded => envelope.payload.clone(),
            StorageType::External => {
                let blob_path = blobs_path.join(format!("{}_{}.blob", module_id, key));
                match fs::read(&blob_path).await {
                    Ok(bytes) => bytes,
                    Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                        return Err(StoreError::MissingBlob {
                            module: module_id.to_string(),
                            key: key.to_string(),
                            source,
                        });
                    }
                    Err(source) => {
                        return Err(StoreError::Io {
                            what: "read an external blob",
                            path: blob_path,
                            source,
                        });
                    }
                }
            }
        };

        Ok(Some((envelope, raw_data)))
    }

    /// Decrypts (if needed) and deserializes an already-fetched envelope's
    /// raw payload. An encrypted `V0` envelope (data format 0, raw root
    /// key) is `NeedsUpgrade`: only moku-migrate reads that format.
    async fn decrypt_payload<T: DeserializeOwned>(
        &self,
        module_id: &str,
        key: &str,
        envelope: &StorageEnvelope,
        raw_data: Vec<u8>,
    ) -> Result<T> {
        let session_key = if envelope.status == EncryptionStatus::Encrypted {
            if envelope.key_scheme != CURRENT_KEY_SCHEME {
                return Err(StoreError::NeedsUpgrade {
                    module: module_id.to_string(),
                    key: key.to_string(),
                });
            }
            Some(self.module_key(module_id)?)
        } else {
            None
        };

        let decrypted = tokio::task::spawn_blocking(move || {
            if let Some(k) = session_key {
                format_v1::open_gcm_no_aad(&k, &raw_data)
            } else {
                Ok(Zeroizing::new(raw_data))
            }
        })
        .await?;
        let decrypted_data = match decrypted {
            Ok(bytes) => bytes,
            Err(CryptoError::AuthFailed) => {
                return Err(StoreError::Tampered {
                    module: module_id.to_string(),
                    key: key.to_string(),
                });
            }
            Err(e @ CryptoError::Malformed(_)) => {
                return Err(StoreError::Corrupt {
                    module: module_id.to_string(),
                    key: key.to_string(),
                    source: Box::new(e),
                });
            }
            Err(e) => return Err(e.into()),
        };

        serde_json::from_slice(&decrypted_data).map_err(|source| StoreError::SchemaMismatch {
            module: module_id.to_string(),
            key: key.to_string(),
            source,
        })
    }

    pub async fn load<T: DeserializeOwned>(&self, module_id: &str, key: &str) -> Result<T> {
        let (envelope, raw_data) = self.fetch_envelope(module_id, key).await?;
        self.decrypt_payload(module_id, key, &envelope, raw_data).await
    }

    /// Loads `key`, distinguishing "nothing stored yet" (`Ok(None)`) from
    /// every real failure: locked vault, wrong key, corrupt envelope,
    /// missing blob, or a shape change (`Err`).
    ///
    /// Modules must use this rather than treating any `load` error as an
    /// empty collection: a module that shows "empty" after a failed load
    /// and then saves overwrites the user's real data with that empty (or
    /// newly-edited, partial) list.
    pub async fn load_opt<T: DeserializeOwned>(
        &self,
        module_id: &str,
        key: &str,
    ) -> Result<Option<T>> {
        match self.fetch_envelope_opt(module_id, key).await? {
            None => Ok(None),
            Some((envelope, raw_data)) => self
                .decrypt_payload(module_id, key, &envelope, raw_data)
                .await
                .map(Some),
        }
    }

    /// Whether any module has at least one ENCRYPTED record on disk. The
    /// lock screen uses this when `vault/meta.json` is missing: if encrypted
    /// data exists, that data was sealed under the lost meta's key, and
    /// setting up a fresh vault would orphan it for good, so the user must
    /// be told to restore `meta.json` instead. Plaintext-only modules (e.g.
    /// RSS by default, which can be written before any vault exists) don't
    /// count. Only scans module directories that already exist.
    pub async fn has_encrypted_records(&self) -> Result<bool> {
        let mut entries = match fs::read_dir(&self.vault_root).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(StoreError::io("list the vault directory", &self.vault_root)(e)),
        };
        let mut modules = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(StoreError::io("list the vault directory", &self.vault_root))?
        {
            if entry.path().join("db").is_dir() {
                modules.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        for module in modules {
            let db = self.get_or_open_db(&module).await?;
            let found = tokio::task::spawn_blocking(move || {
                db.iter().filter_map(|entry| entry.ok()).any(|(_, value)| {
                    serde_json::from_slice::<StorageEnvelope>(&value)
                        .map(|env| env.status == EncryptionStatus::Encrypted)
                        // An unparseable record may well be encrypted data:
                        // err on the side of "don't orphan it".
                        .unwrap_or(true)
                })
            })
            .await?;
            if found {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Lists every key currently stored for `module_id`, via a
    /// `spawn_blocking` sled scan.
    async fn list_keys(&self, module_id: &str) -> Result<(sled::Db, Vec<String>)> {
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
        Ok((db, keys))
    }

    /// Re-saves every record under `module_id` so its on-disk
    /// `EncryptionStatus` matches `target_encrypted`. Records already in
    /// the target state are left untouched — idempotent, safe to call
    /// repeatedly (e.g. after a config change, to reconcile drift). Purely
    /// about the encrypted/plaintext toggle, a user preference; the data
    /// format (key scheme) is moku-migrate's.
    pub async fn migrate_module_encryption(
        &self,
        module_id: &str,
        target_encrypted: bool,
    ) -> Result<MigrationReport> {
        if target_encrypted && !self.session.is_unlocked() {
            return Err(StoreError::Locked);
        }

        let (db, keys) = self.list_keys(module_id).await?;
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
                Err(e) => report.errors.push((key, e)),
            }
        }

        // `migrate_one_key` defers its own flush (see `save_impl`'s `flush`
        // param) so a migration touching many keys does one fsync-
        // equivalent for the whole batch instead of one per key.
        tokio::task::spawn_blocking(move || db.flush()).await??;

        Ok(report)
    }

    /// Returns `Ok(true)` if `key` was re-saved, `Ok(false)` if its
    /// `EncryptionStatus` already matched `target_encrypted`.
    async fn migrate_one_key(
        &self,
        module_id: &str,
        key: &str,
        target_encrypted: bool,
    ) -> Result<bool> {
        let (envelope, raw_data) = self.fetch_envelope(module_id, key).await?;

        let currently_encrypted = envelope.status == EncryptionStatus::Encrypted;
        if currently_encrypted == target_encrypted {
            return Ok(false);
        }

        let value: serde_json::Value = self
            .decrypt_payload(module_id, key, &envelope, raw_data)
            .await?;
        self.save_impl(module_id, key, &value, target_encrypted, false)
            .await?;
        Ok(true)
    }

    async fn prepare_module_paths(&self, module_id: &str) -> Result<(PathBuf, PathBuf)> {
        let module_root = self.vault_root.join(module_id);
        let db_path = module_root.join("db");
        let blobs_path = module_root.join("blobs");
        if !db_path.exists() {
            fs::create_dir_all(&db_path)
                .await
                .map_err(StoreError::io("create a module directory", &db_path))?;
        }
        if !blobs_path.exists() {
            fs::create_dir_all(&blobs_path)
                .await
                .map_err(StoreError::io("create a module directory", &blobs_path))?;
        }
        Ok((db_path, blobs_path))
    }
}

#[cfg(test)]
mod tests;
