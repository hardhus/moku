use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use secrecy::SecretBox;
use serde::{Deserialize, Serialize};
use tokio::fs;
use zeroize::Zeroizing;

use moku_sys::dirs;

// Temporary facade while P2 splits moku-core: `SafeKey` and the AES-GCM /
// Argon2 primitives now live in moku-crypto. Removed once every crate
// imports moku_crypto directly.
pub use moku_crypto::SafeKey;

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
        let data_dir =
            dirs::get_data_dir().map_err(|e| anyhow!("Data directory not found: {}", e))?;
        Ok(Self {
            root_path: data_dir.join("vault"),
        })
    }

    pub fn new_with_root(root_path: PathBuf) -> Self {
        Self {
            root_path: root_path.join("vault"),
        }
    }

    pub fn generate_salt(len: usize) -> Vec<u8> {
        moku_crypto::generate_salt(len)
    }

    pub async fn derive_key(password: &str, salt_bytes: &[u8]) -> Result<SecretBox<SafeKey>> {
        let password = password.to_string();
        let salt = salt_bytes.to_vec();

        tokio::task::spawn_blocking(move || moku_crypto::argon2id_key(password.as_bytes(), &salt))
            .await?
    }

    pub fn encrypt(data: &[u8], key: &SecretBox<SafeKey>) -> Result<Vec<u8>> {
        moku_crypto::encrypt(data, key)
    }

    pub fn decrypt(combined_data: &[u8], key: &SecretBox<SafeKey>) -> Result<Vec<u8>> {
        moku_crypto::decrypt(combined_data, key)
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
            return Err(anyhow!(
                "a vault already exists at {}; refusing to overwrite it",
                self.get_meta_path().display()
            ));
        }
        let salt = Self::generate_salt(16);
        let key = Self::derive_key(&password, &salt).await?;

        let canary_text = b"MOKU_VAULT_OK";
        let encrypted_canary = Self::encrypt(canary_text, &key)?;

        let meta = StorageMetadata {
            salt,
            canary: encrypted_canary,
        };
        let path = self.get_meta_path();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let json = serde_json::to_string_pretty(&meta)?;
        moku_sys::util::atomic_write_async(path, json.into_bytes()).await?;

        Ok(key)
    }

    pub async fn unlock_vault(&self, password: Zeroizing<String>) -> Result<SecretBox<SafeKey>> {
        let path = self.get_meta_path();
        let content = fs::read_to_string(&path)
            .await
            .context("Vault metadata file missing or unreadable")?;
        let meta: StorageMetadata = serde_json::from_str(&content)?;

        let key = Self::derive_key(&password, &meta.salt).await?;
        let decrypted = Self::decrypt(&meta.canary, &key);

        match decrypted {
            Ok(bytes) if bytes == b"MOKU_VAULT_OK" => Ok(key),
            _ => Err(anyhow!("Invalid password")),
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
        assert!(wrong_result.is_err());
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
        let salt = SecurityManager::generate_salt(16);

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
        assert!(sm.initialize_vault(Zeroizing::new("second".to_string())).await.is_err());
        assert_eq!(std::fs::read(sm.get_meta_path()).unwrap(), meta_before);
        // The original password still unlocks it.
        sm.unlock_vault(Zeroizing::new("first".to_string())).await.unwrap();
    }
}
