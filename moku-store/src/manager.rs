use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Serialize, de::DeserializeOwned};
use tokio::fs;
use tokio::sync::RwLock;
use zeroize::Zeroizing;

use moku_crypto::{CryptoError, Key, ModuleStorageKey, RootKey, format_v1, module_storage_key};
use moku_sys::dirs;
use moku_vault::VaultSession;

use crate::StoreError;
use crate::envelope::{
    CURRENT_KEY_SCHEME, CURRENT_SCHEMA_VERSION, EncryptionStatus, KeyScheme, StorageEnvelope,
    StorageType, data_transform_for_hop,
};

type Result<T> = std::result::Result<T, StoreError>;

/// The key an envelope is (to be) encrypted with, by `KeyScheme`.
enum RecordKey {
    /// `V0`: the raw root key itself (read only; nothing writes V0).
    V0(Key<RootKey>),
    /// `V1`: the module's HKDF subkey.
    V1(Key<ModuleStorageKey>),
}

impl RecordKey {
    // Envelopes are still the pre-P3 format (P6 replaces it).
    fn seal(&self, plaintext: &[u8]) -> std::result::Result<Vec<u8>, CryptoError> {
        match self {
            RecordKey::V0(k) => format_v1::seal_gcm_no_aad(k, plaintext),
            RecordKey::V1(k) => format_v1::seal_gcm_no_aad(k, plaintext),
        }
    }

    fn open(&self, data: &[u8]) -> std::result::Result<Zeroizing<Vec<u8>>, CryptoError> {
        match self {
            RecordKey::V0(k) => format_v1::open_gcm_no_aad(k, data),
            RecordKey::V1(k) => format_v1::open_gcm_no_aad(k, data),
        }
    }
}

const EXTERNAL_STORAGE_THRESHOLD: usize = 50 * 1024;

/// Data-directory-level record of which `KeyScheme` version the vault is
/// on — independent of any in-app config or per-record envelope tag, so
/// a bare copy of the data directory (a backup, a move to another
/// machine) is enough on its own to know its version without opening a
/// single sled DB. Lives at the vault root, sibling to every module's
/// own subdirectory.
const KEY_SCHEME_MARKER_FILENAME: &str = ".key_scheme_version";

/// Result of `StorageManager::migrate_module_encryption` /
/// `migrate_key_scheme_to_latest`: per-record failures are collected
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
            // A brand-new data directory has no pre-existing records, so
            // it starts life already on the latest key scheme — nothing
            // for `migrate_key_scheme_to_latest` to ever do here. An
            // already-existing `vault_root` (from before this marker
            // file existed) is left untouched: `data_dir_key_scheme_
            // version` correctly reads that as version 0.
            manager
                .write_key_scheme_marker(CURRENT_KEY_SCHEME.version())
                .await?;
        }
        Ok(manager)
    }

    fn key_scheme_marker_path(&self) -> PathBuf {
        self.vault_root.join(KEY_SCHEME_MARKER_FILENAME)
    }

    async fn write_key_scheme_marker(&self, version: u16) -> Result<()> {
        let path = self.key_scheme_marker_path();
        fs::write(&path, version.to_string())
            .await
            .map_err(StoreError::io("write the key-scheme version marker", path))
    }

    /// Reads the data directory's own record of which `KeyScheme` version
    /// it's on. Missing file means "predates this marker" — i.e. version
    /// `0`, exactly `KeyScheme::default()`'s meaning. Never touches sled
    /// or the vault lock state — safe to call before the vault is
    /// unlocked, purely to decide whether to prompt for a migration.
    pub async fn data_dir_key_scheme_version(&self) -> u16 {
        match fs::read_to_string(self.key_scheme_marker_path()).await {
            Ok(s) => s.trim().parse().unwrap_or(0),
            Err(_) => 0,
        }
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

    /// Maps a `KeyScheme` to how its key is actually computed from the
    /// unlocked vault master key — the one place that knows this, so
    /// both the write path (always `CURRENT_KEY_SCHEME`) and the read
    /// path (whatever scheme an existing envelope recorded) share it;
    /// adding a future `V2` means adding one match arm here.
    fn resolve_key_for_scheme(
        &self,
        module_id: &str,
        scheme: KeyScheme,
    ) -> Result<RecordKey> {
        // The root key is only borrowed long enough to derive (or, for a
        // V0 read, copy) the record key.
        self.session
            .with(|root| match scheme {
                KeyScheme::V0 => RecordKey::V0(root.duplicate()),
                KeyScheme::V1 => RecordKey::V1(module_storage_key(root, module_id)),
            })
            .map_err(|_| StoreError::Locked)
    }

    /// Derives the write key for `module_id` — always under
    /// `CURRENT_KEY_SCHEME`, never an older scheme. Every new write uses
    /// this, unconditionally.
    fn resolve_write_key(&self, module_id: &str) -> Result<RecordKey> {
        self.resolve_key_for_scheme(module_id, CURRENT_KEY_SCHEME)
    }

    /// Resolves the key to *decrypt* an existing envelope with, based on
    /// which scheme it was written under — a `V0` envelope (every record
    /// written before per-module subkeys existed, and forever if a user
    /// never explicitly migrates) still decrypts with the raw master
    /// key, exactly as before; `V1` envelopes use the derived subkey.
    fn resolve_read_key(&self, module_id: &str, scheme: KeyScheme) -> Result<RecordKey> {
        self.resolve_key_for_scheme(module_id, scheme)
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
        // The plaintext JSON is wiped once it has been encrypted (or, for a
        // plaintext module, written).
        let raw_bytes = Zeroizing::new(serde_json::to_vec(data).map_err(StoreError::Encode)?);

        // Every new write uses the derived per-module subkey — never the
        // raw master key directly (see moku_crypto::module_storage_key).
        let session_key = if is_encryption_enabled {
            Some(self.resolve_write_key(module_id)?)
        } else {
            None
        };

        let key_string = key.to_string();
        let module_id_str = module_id.to_string();

        let (final_payload, storage_type) = tokio::task::spawn_blocking(move || {
            let (processed_payload, status) = if let Some(k) = session_key {
                (k.seal(&raw_bytes)?, EncryptionStatus::Encrypted)
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
    /// raw payload, picking the key by `envelope.key_scheme` — a `V0`
    /// envelope (every record written before per-module subkeys existed)
    /// still decrypts with the raw master key, exactly as before.
    async fn decrypt_payload<T: DeserializeOwned>(
        &self,
        module_id: &str,
        key: &str,
        envelope: &StorageEnvelope,
        raw_data: Vec<u8>,
    ) -> Result<T> {
        let session_key = if envelope.status == EncryptionStatus::Encrypted {
            Some(self.resolve_read_key(module_id, envelope.key_scheme)?)
        } else {
            None
        };

        let decrypted = tokio::task::spawn_blocking(move || {
            if let Some(k) = session_key {
                k.open(&raw_data)
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
    /// `spawn_blocking` sled scan — shared by both migration passes below
    /// so they don't each reimplement the same scan.
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
    /// about the encrypted/plaintext toggle — never touches `key_scheme`;
    /// see `migrate_key_scheme_to_latest` for that, a separate concern.
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

    /// Re-encrypts every `Encrypted` record under `module_id` that's
    /// still on an older `KeyScheme` than `CURRENT_KEY_SCHEME`, walking
    /// straight to the latest scheme regardless of how many versions
    /// behind it is — a caller never needs to know or care about
    /// intermediate hops. Independent of `migrate_module_encryption`'s
    /// encrypted/plaintext toggle: this only ever moves an already-
    /// encrypted record forward to a newer key scheme. Idempotent — a
    /// record already on `CURRENT_KEY_SCHEME` is left untouched.
    pub async fn migrate_key_scheme_to_latest(&self, module_id: &str) -> Result<MigrationReport> {
        if !self.session.is_unlocked() {
            return Err(StoreError::Locked);
        }

        let (db, keys) = self.list_keys(module_id).await?;
        let mut report = MigrationReport {
            migrated: 0,
            skipped: 0,
            errors: Vec::new(),
        };

        for key in keys {
            match self.migrate_one_key_scheme(module_id, &key).await {
                Ok(true) => report.migrated += 1,
                Ok(false) => report.skipped += 1,
                Err(e) => report.errors.push((key, e)),
            }
        }

        tokio::task::spawn_blocking(move || db.flush()).await??;

        Ok(report)
    }

    /// Returns `Ok(true)` if `key` was upgraded to `CURRENT_KEY_SCHEME`,
    /// `Ok(false)` if it's `Plaintext` (key scheme is meaningless there)
    /// or already on the latest scheme.
    async fn migrate_one_key_scheme(&self, module_id: &str, key: &str) -> Result<bool> {
        let (envelope, raw_data) = self.fetch_envelope(module_id, key).await?;
        if envelope.status != EncryptionStatus::Encrypted
            || envelope.key_scheme.version() >= CURRENT_KEY_SCHEME.version()
        {
            return Ok(false);
        }

        let mut value: serde_json::Value = self
            .decrypt_payload(module_id, key, &envelope, raw_data)
            .await?;
        for from_version in envelope.key_scheme.version()..CURRENT_KEY_SCHEME.version() {
            if let Some(transform) = data_transform_for_hop(from_version) {
                value = transform(value).map_err(|message| StoreError::Transform {
                    module: module_id.to_string(),
                    key: key.to_string(),
                    message,
                })?;
            }
        }
        self.save_impl(module_id, key, &value, true, false).await?;
        Ok(true)
    }

    /// Runs `migrate_key_scheme_to_latest` for every module id in
    /// `module_ids` and, only if all of them finish with zero errors,
    /// advances the data directory's `.key_scheme_version` marker to
    /// `CURRENT_KEY_SCHEME` — a partially-failed migration is never
    /// marked as fully done, so the next launch will retry it.
    pub async fn migrate_all_key_schemes(
        &self,
        module_ids: &[&str],
    ) -> Result<Vec<(String, MigrationReport)>> {
        let mut reports = Vec::with_capacity(module_ids.len());
        let mut all_ok = true;
        for &module_id in module_ids {
            let report = self.migrate_key_scheme_to_latest(module_id).await?;
            if !report.errors.is_empty() {
                all_ok = false;
            }
            reports.push((module_id.to_string(), report));
        }
        if all_ok {
            self.write_key_scheme_marker(CURRENT_KEY_SCHEME.version())
                .await?;
        }
        Ok(reports)
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
