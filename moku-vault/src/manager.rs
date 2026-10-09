use std::path::PathBuf;

use secrecy::SecretBox;
use serde::{Deserialize, Serialize};
use tokio::fs;
use zeroize::Zeroizing;

use moku_sys::dirs;

use crate::VaultError;

type Result<T> = std::result::Result<T, VaultError>;

use moku_crypto::{CryptoError, SafeKey};

#[derive(Serialize, Deserialize)]
pub struct StorageMetadata {
    pub salt: Vec<u8>,
    pub canary: Vec<u8>,
}

pub struct SecurityManager {
    root_path: PathBuf,
}

impl SecurityManager {
    pub fn new() -> Result<Self> {
        let data_dir = dirs::get_data_dir()?;
        Ok(Self {
            root_path: data_dir.join("vault"),
        })
    }

    pub fn new_with_root(root_path: PathBuf) -> Self {
        Self {
            root_path: root_path.join("vault"),
        }
    }

    pub async fn derive_key(password: &str, salt_bytes: &[u8]) -> Result<SecretBox<SafeKey>> {
        // The copy handed to the blocking thread is wiped when it drops.
        let password = Zeroizing::new(password.as_bytes().to_vec());
        let salt = salt_bytes.to_vec();

        let key = tokio::task::spawn_blocking(move || moku_crypto::argon2id_key(&password, &salt))
            .await
            .map_err(VaultError::Task)??;
        Ok(key)
    }

    pub fn is_vault_initialized(&self) -> bool {
        self.get_meta_path().exists()
    }

    /// Creates a brand-new vault. Refuses if a `meta.json` already exists:
    /// overwriting it would replace the salt, silently orphaning every
    /// record encrypted under the old key. The file is written atomically,
    /// because a torn `meta.json` would lose the salt (and with it all
    /// data) just the same.
    pub async fn initialize_vault(
        &self,
        password: Zeroizing<String>,
    ) -> Result<SecretBox<SafeKey>> {
        if self.is_vault_initialized() {
            return Err(VaultError::AlreadyInitialized(self.get_meta_path()));
        }
        let salt = moku_crypto::generate_salt(16);
        let key = Self::derive_key(&password, &salt).await?;

        let canary_text = b"MOKU_VAULT_OK";
        let encrypted_canary = moku_crypto::encrypt(canary_text, &key)?;

        let meta = StorageMetadata {
            salt,
            canary: encrypted_canary,
        };
        let path = self.get_meta_path();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|source| VaultError::Io {
                    path: path.clone(),
                    source,
                })?;
        }

        let json = serde_json::to_string_pretty(&meta).map_err(|e| VaultError::MetadataCorrupt {
            path: path.clone(),
            source: Some(e),
        })?;
        moku_sys::util::atomic_write_async(path, json.into_bytes()).await?;

        Ok(key)
    }

    /// Derives the key from `password` and checks it against the canary.
    /// Only a canary that fails authentication means `WrongPassword`; a
    /// missing, unreadable or malformed `meta.json` (or a canary that
    /// decrypts to the wrong bytes) is reported as what it is, so the user
    /// isn't told to retype a password when the real problem is on disk.
    pub async fn unlock_vault(&self, password: Zeroizing<String>) -> Result<SecretBox<SafeKey>> {
        let path = self.get_meta_path();
        let content = match fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(VaultError::NotInitialized);
            }
            Err(source) => return Err(VaultError::Io { path, source }),
        };
        let meta: StorageMetadata =
            serde_json::from_str(&content).map_err(|e| VaultError::MetadataCorrupt {
                path: path.clone(),
                source: Some(e),
            })?;

        let key = Self::derive_key(&password, &meta.salt).await?;
        match moku_crypto::decrypt(&meta.canary, &key) {
            Ok(bytes) if bytes == b"MOKU_VAULT_OK" => Ok(key),
            Ok(_) | Err(CryptoError::Malformed(_)) => {
                Err(VaultError::MetadataCorrupt { path, source: None })
            }
            Err(CryptoError::AuthFailed) => Err(VaultError::WrongPassword),
            Err(e) => Err(e.into()),
        }
    }

    fn get_meta_path(&self) -> PathBuf {
        self.root_path.join("meta.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_key_derivation_consistency() {
        let password = "test_password";
        let salt = vec![1u8; 16];

        let key1 = SecurityManager::derive_key(password, &salt).await.unwrap();
        let key2 = SecurityManager::derive_key(password, &salt).await.unwrap();

        assert_eq!(key1.expose_secret().0, key2.expose_secret().0);
    }

    #[tokio::test]
    async fn test_vault_lifecycle_unit() {
        let temp = tempdir().unwrap();
        let manager = SecurityManager::new_with_root(temp.path().to_path_buf());

        let password = Zeroizing::new("unit_test_password".to_string());

        assert!(!manager.is_vault_initialized());

        let key = manager
            .initialize_vault(password.clone())
            .await
            .expect("Init failed");
        assert!(manager.is_vault_initialized());

        let unlocked_key = manager.unlock_vault(password).await.expect("Unlock failed");
        assert_eq!(key.expose_secret().0, unlocked_key.expose_secret().0);

        let wrong_result = manager
            .unlock_vault(Zeroizing::new("wrong".to_string()))
            .await;
        assert!(matches!(wrong_result, Err(VaultError::WrongPassword)));
    }

    #[test]
    fn test_get_meta_path_sandbox() {
        let temp_path = PathBuf::from("/tmp/moku_test");
        let manager = SecurityManager::new_with_root(temp_path.clone());

        let meta_path = manager.get_meta_path();
        assert!(meta_path.starts_with(temp_path));
        assert!(meta_path.ends_with("meta.json"));
    }

    #[tokio::test]
    async fn test_argon2_resource_exhaustion_resilience() {
        let massive_password = "A".repeat(1024 * 1024);
        let salt = moku_crypto::generate_salt(16);

        let result = SecurityManager::derive_key(&massive_password, &salt).await;
        assert!(
            result.is_ok(),
            "System must not crash on oversized passwords"
        );
    }

    #[tokio::test]
    async fn test_initialize_vault_refuses_to_overwrite_an_existing_vault() {
        let temp = tempfile::tempdir().unwrap();
        let sm = SecurityManager::new_with_root(temp.path().to_path_buf());
        sm.initialize_vault(Zeroizing::new("first".to_string())).await.unwrap();
        let meta_before = std::fs::read(sm.get_meta_path()).unwrap();
        assert!(matches!(
            sm.initialize_vault(Zeroizing::new("second".to_string())).await,
            Err(VaultError::AlreadyInitialized(_))
        ));
        assert_eq!(std::fs::read(sm.get_meta_path()).unwrap(), meta_before);
        // The original password still unlocks it.
        sm.unlock_vault(Zeroizing::new("first".to_string())).await.unwrap();
    }

    #[tokio::test]
    async fn test_unlock_without_meta_is_not_initialized() {
        let temp = tempdir().unwrap();
        let sm = SecurityManager::new_with_root(temp.path().to_path_buf());
        let err = sm.unlock_vault(Zeroizing::new("pw".to_string())).await.unwrap_err();
        assert!(matches!(err, VaultError::NotInitialized), "got {err:?}");
    }

    #[tokio::test]
    async fn test_unlock_with_unparseable_meta_is_corrupt_not_wrong_password() {
        let temp = tempdir().unwrap();
        let sm = SecurityManager::new_with_root(temp.path().to_path_buf());
        sm.initialize_vault(Zeroizing::new("pw".to_string())).await.unwrap();
        std::fs::write(sm.get_meta_path(), b"{ not json").unwrap();
        let err = sm.unlock_vault(Zeroizing::new("pw".to_string())).await.unwrap_err();
        assert!(
            matches!(err, VaultError::MetadataCorrupt { source: Some(_), .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn test_unlock_with_a_truncated_canary_is_corrupt_not_wrong_password() {
        let temp = tempdir().unwrap();
        let sm = SecurityManager::new_with_root(temp.path().to_path_buf());
        sm.initialize_vault(Zeroizing::new("pw".to_string())).await.unwrap();
        let mut meta: StorageMetadata =
            serde_json::from_slice(&std::fs::read(sm.get_meta_path()).unwrap()).unwrap();
        meta.canary.truncate(5);
        std::fs::write(sm.get_meta_path(), serde_json::to_vec(&meta).unwrap()).unwrap();
        let err = sm.unlock_vault(Zeroizing::new("pw".to_string())).await.unwrap_err();
        assert!(
            matches!(err, VaultError::MetadataCorrupt { source: None, .. }),
            "got {err:?}"
        );
    }
}
