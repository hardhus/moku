use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::fs;

use moku_sys::dirs;

use crate::VaultError;

type Result<T> = std::result::Result<T, VaultError>;

use moku_crypto::kdf::DEFAULT_KDF;
use moku_crypto::{CryptoError, Key, RootKey, SecretString, format_v1};

/// What the canary decrypts to when the password is right.
const CANARY: &[u8] = b"MOKU_VAULT_OK";

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

    /// The vault's root key from its password: Argon2id with the default
    /// parameters (`meta.json` v1 records none), run on a blocking thread.
    pub async fn derive_key(password: &SecretString, salt_bytes: &[u8]) -> Result<Key<RootKey>> {
        // The copy handed to the blocking thread is wiped when it drops.
        let password = password.duplicate();
        let salt = salt_bytes.to_vec();

        let key = tokio::task::spawn_blocking(move || {
            moku_crypto::password_key(&password, &salt, DEFAULT_KDF)
        })
        .await
        .map_err(VaultError::Task)??;
        Ok(format_v1::adopt_password_key_as_root(key))
    }

    pub fn is_vault_initialized(&self) -> bool {
        self.get_meta_path().exists()
    }

    /// Creates a brand-new vault. Refuses if a `meta.json` already exists:
    /// overwriting it would replace the salt, silently orphaning every
    /// record encrypted under the old key. The file is written atomically,
    /// because a torn `meta.json` would lose the salt (and with it all
    /// data) just the same.
    pub async fn initialize_vault(&self, password: SecretString) -> Result<Key<RootKey>> {
        if self.is_vault_initialized() {
            return Err(VaultError::AlreadyInitialized(self.get_meta_path()));
        }
        let salt = moku_crypto::kdf::generate_salt().to_vec();
        let key = Self::derive_key(&password, &salt).await?;

        let encrypted_canary = format_v1::seal_gcm_no_aad(&key, CANARY)?;

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
    pub async fn unlock_vault(&self, password: SecretString) -> Result<Key<RootKey>> {
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
        match format_v1::open_gcm_no_aad(&key, &meta.canary) {
            Ok(bytes) if bytes[..] == *CANARY => Ok(key),
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
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_key_derivation_consistency() {
        let password = SecretString::from("test_password");
        let salt = vec![1u8; 16];

        let key1 = SecurityManager::derive_key(&password, &salt).await.unwrap();
        let key2 = SecurityManager::derive_key(&password, &salt).await.unwrap();

        assert_eq!(key1.expose(), key2.expose());
    }

    #[tokio::test]
    async fn test_vault_lifecycle_unit() {
        let temp = tempdir().unwrap();
        let manager = SecurityManager::new_with_root(temp.path().to_path_buf());

        let password = SecretString::from("unit_test_password");

        assert!(!manager.is_vault_initialized());

        let key = manager
            .initialize_vault(password.duplicate())
            .await
            .expect("Init failed");
        assert!(manager.is_vault_initialized());

        let unlocked_key = manager.unlock_vault(password).await.expect("Unlock failed");
        assert_eq!(key.expose(), unlocked_key.expose());

        let wrong_result = manager
            .unlock_vault(SecretString::from("wrong"))
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
        let massive_password = SecretString::from("A".repeat(1024 * 1024));
        let salt = moku_crypto::kdf::generate_salt();

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
        sm.initialize_vault(SecretString::from("first")).await.unwrap();
        let meta_before = std::fs::read(sm.get_meta_path()).unwrap();
        assert!(matches!(
            sm.initialize_vault(SecretString::from("second")).await,
            Err(VaultError::AlreadyInitialized(_))
        ));
        assert_eq!(std::fs::read(sm.get_meta_path()).unwrap(), meta_before);
        // The original password still unlocks it.
        sm.unlock_vault(SecretString::from("first")).await.unwrap();
    }

    #[tokio::test]
    async fn test_unlock_without_meta_is_not_initialized() {
        let temp = tempdir().unwrap();
        let sm = SecurityManager::new_with_root(temp.path().to_path_buf());
        let err = sm.unlock_vault(SecretString::from("pw")).await.unwrap_err();
        assert!(matches!(err, VaultError::NotInitialized), "got {err:?}");
    }

    #[tokio::test]
    async fn test_unlock_with_unparseable_meta_is_corrupt_not_wrong_password() {
        let temp = tempdir().unwrap();
        let sm = SecurityManager::new_with_root(temp.path().to_path_buf());
        sm.initialize_vault(SecretString::from("pw")).await.unwrap();
        std::fs::write(sm.get_meta_path(), b"{ not json").unwrap();
        let err = sm.unlock_vault(SecretString::from("pw")).await.unwrap_err();
        assert!(
            matches!(err, VaultError::MetadataCorrupt { source: Some(_), .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn test_unlock_with_a_truncated_canary_is_corrupt_not_wrong_password() {
        let temp = tempdir().unwrap();
        let sm = SecurityManager::new_with_root(temp.path().to_path_buf());
        sm.initialize_vault(SecretString::from("pw")).await.unwrap();
        let mut meta: StorageMetadata =
            serde_json::from_slice(&std::fs::read(sm.get_meta_path()).unwrap()).unwrap();
        meta.canary.truncate(5);
        std::fs::write(sm.get_meta_path(), serde_json::to_vec(&meta).unwrap()).unwrap();
        let err = sm.unlock_vault(SecretString::from("pw")).await.unwrap_err();
        assert!(
            matches!(err, VaultError::MetadataCorrupt { source: None, .. }),
            "got {err:?}"
        );
    }
}
