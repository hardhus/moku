use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, anyhow};
use serde::{Serialize, de::DeserializeOwned};
use tokio::fs;

use crate::dirs;
use crate::security::{SecurityManager, VaultSession};

use super::envelope::{CURRENT_SCHEMA_VERSION, EncryptionStatus, StorageEnvelope, StorageType};

const EXTERNAL_STORAGE_THRESHOLD: usize = 50 * 1024;

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

    fn get_or_open_db(&self, module_id: &str) -> Result<sled::Db> {
        {
            let cache = self
                .db_cache
                .read()
                .map_err(|_| anyhow!("RWLock Read Error"))?;
            if let Some(db) = cache.get(module_id) {
                return Ok(db.clone());
            }
        }
        let mut cache = self
            .db_cache
            .write()
            .map_err(|_| anyhow!("RWLock Write Error"))?;
        if let Some(db) = cache.get(module_id) {
            return Ok(db.clone());
        }

        let db_path = self.vault_root.join(module_id).join("db");
        let db = sled::open(&db_path).context(format!("Failed to open sled DB: {:?}", db_path))?;
        cache.insert(module_id.to_string(), db.clone());
        Ok(db)
    }

    /// Drops the cached handle for `module_id`, releasing sled's exclusive
    /// process-level file lock on that module's DB. sled only allows one
    /// process to hold a given DB open at a time, so a long-lived process
    /// (the daemon) that doesn't need continuous access to a module should
    /// call this after each use — otherwise any other process (e.g. a TUI)
    /// trying to open the same module's DB fails with "Failed to open sled
    /// DB" for as long as the first process keeps it cached.
    pub fn close_db(&self, module_id: &str) -> Result<()> {
        let mut cache = self
            .db_cache
            .write()
            .map_err(|_| anyhow!("RWLock Write Error"))?;
        cache.remove(module_id);
        Ok(())
    }

    pub async fn save<T: Serialize + Send + Sync + 'static>(
        &self,
        module_id: &str,
        key: &str,
        data: &T,
        is_encryption_enabled: bool,
    ) -> Result<()> {
        let (_, blobs_path) = self.prepare_module_paths(module_id).await?;
        let db = self.get_or_open_db(module_id)?;
        let raw_bytes = serde_json::to_vec(data).context("Serialization failed")?;

        let session_key = if is_encryption_enabled {
            Some(
                self.session
                    .current()
                    .ok_or_else(|| anyhow!("Vault locked"))?,
            )
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
                status,
                storage_type: storage_type.clone(),
                payload: if storage_type == StorageType::External {
                    Vec::new()
                } else {
                    processed_payload.clone()
                },
                hash: None,
            };

            let envelope_bytes = serde_json::to_vec(&envelope)?;
            db.insert(key_string, envelope_bytes)?;
            db.flush()?;

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

    pub async fn load<T: DeserializeOwned>(&self, module_id: &str, key: &str) -> Result<T> {
        let (_, blobs_path) = self.prepare_module_paths(module_id).await?;
        let db = self.get_or_open_db(module_id)?;
        let key_string = key.to_string();

        let iv = tokio::task::spawn_blocking(move || db.get(&key_string))
            .await??
            .ok_or_else(|| anyhow!("Key not found: {}", key))?;

        let envelope: StorageEnvelope =
            serde_json::from_slice(&iv).context("Envelope corrupted")?;

        let raw_data = match envelope.storage_type {
            StorageType::Embedded => envelope.payload,
            StorageType::External => {
                let file_name = format!("{}_{}.blob", module_id, key);
                fs::read(blobs_path.join(file_name))
                    .await
                    .context("External blob file missing")?
            }
        };

        let session_key = if envelope.status == EncryptionStatus::Encrypted {
            Some(
                self.session
                    .current()
                    .ok_or_else(|| anyhow!("Vault locked"))?,
            )
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
            .expect("First open failed");
        let _db2 = manager
            .get_or_open_db("test_mod")
            .expect("Second open failed");

        assert_eq!(
            manager.db_cache.read().unwrap().len(),
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
}
