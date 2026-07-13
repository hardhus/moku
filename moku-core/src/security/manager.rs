use std::{fmt, path::PathBuf};

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use anyhow::{Context, Result, anyhow};
use argon2::{Algorithm, Argon2, Params, Version, password_hash::rand_core::OsRng};
use rand::RngCore;
use secrecy::{ExposeSecret, SecretBox};
use serde::{Deserialize, Serialize};
use tokio::fs;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::dirs;

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SafeKey(pub [u8; 32]);

impl fmt::Debug for SafeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SafeKey(***REDACTED***)")
    }
}

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
        let mut salt = vec![0u8; len];
        OsRng.fill_bytes(&mut salt);
        salt
    }

    pub async fn derive_key(password: &str, salt_bytes: &[u8]) -> Result<SecretBox<SafeKey>> {
        let password = password.to_string();
        let salt = salt_bytes.to_vec();

        tokio::task::spawn_blocking(move || {
            let params = Params::new(65536, 3, 4, Some(32))
                .map_err(|e| anyhow!("Invalid Argon2 parameters: {}", e))?;

            let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
            let mut output_key = [0u8; 32];

            argon2
                .hash_password_into(password.as_bytes(), &salt, &mut output_key)
                .map_err(|e| anyhow!("Key derivation failed: {}", e))?;

            Ok(SecretBox::new(Box::new(SafeKey(output_key))))
        })
        .await?
    }

    pub fn encrypt(data: &[u8], key: &SecretBox<SafeKey>) -> Result<Vec<u8>> {
        let raw_key = &key.expose_secret().0;
        let cipher =
            Aes256Gcm::new_from_slice(raw_key).map_err(|_| anyhow!("Invalid key length"))?;

        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, data)
            .map_err(|e| anyhow!("Encryption failed: {}", e))?;

        let mut combined = Vec::with_capacity(nonce_bytes.len() + ciphertext.len());
        combined.extend_from_slice(&nonce_bytes);
        combined.extend(ciphertext);
        Ok(combined)
    }

    pub fn decrypt(combined_data: &[u8], key: &SecretBox<SafeKey>) -> Result<Vec<u8>> {
        if combined_data.len() < 12 {
            return Err(anyhow!("Payload too short"));
        }
        let raw_key = &key.expose_secret().0;
        let cipher = Aes256Gcm::new_from_slice(raw_key).map_err(|_| anyhow!("Invalid key"))?;

        let (nonce_bytes, ciphertext) = combined_data.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);

        cipher
            .decrypt(nonce, ciphertext)
            .map_err(|_| anyhow!("Decryption failed (Wrong password?)"))
    }

    pub fn is_vault_initialized(&self) -> bool {
        self.get_meta_path().exists()
    }

    pub async fn initialize_vault(&self, password: String) -> Result<SecretBox<SafeKey>> {
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
        fs::write(path, json).await?;

        Ok(key)
    }

    pub async fn unlock_vault(&self, password: String) -> Result<SecretBox<SafeKey>> {
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

    #[test]
    fn test_generate_salt_length() {
        let salt = SecurityManager::generate_salt(16);
        assert_eq!(salt.len(), 16);

        let salt2 = SecurityManager::generate_salt(32);
        assert_eq!(salt2.len(), 32);
        assert_ne!(salt, SecurityManager::generate_salt(16));
    }

    #[tokio::test]
    async fn test_key_derivation_consistency() {
        let password = "test_password";
        let salt = vec![1u8; 16];

        let key1 = SecurityManager::derive_key(password, &salt).await.unwrap();
        let key2 = SecurityManager::derive_key(password, &salt).await.unwrap();

        assert_eq!(key1.expose_secret().0, key2.expose_secret().0);
    }

    #[test]
    fn test_encrypt_decrypt_cycle() {
        let raw_key = [1u8; 32];
        let key = SecretBox::new(Box::new(SafeKey(raw_key)));
        let original_data = b"Moku Secret Message";

        let encrypted = SecurityManager::encrypt(original_data, &key).expect("Encryption failed");
        let decrypted = SecurityManager::decrypt(&encrypted, &key).expect("Decryption failed");

        assert_eq!(original_data, &decrypted[..]);
    }

    #[test]
    fn test_decrypt_with_wrong_key() {
        let key1 = SecretBox::new(Box::new(SafeKey([1u8; 32])));
        let key2 = SecretBox::new(Box::new(SafeKey([2u8; 32])));
        let data = b"Sensitive Info";

        let encrypted = SecurityManager::encrypt(data, &key1).unwrap();

        let result = SecurityManager::decrypt(&encrypted, &key2);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_vault_lifecycle_unit() {
        let temp = tempdir().unwrap();
        let manager = SecurityManager::new_with_root(temp.path().to_path_buf());

        let password = "unit_test_password".to_string();

        assert!(!manager.is_vault_initialized());

        let key = manager
            .initialize_vault(password.clone())
            .await
            .expect("Init failed");
        assert!(manager.is_vault_initialized());

        let unlocked_key = manager.unlock_vault(password).await.expect("Unlock failed");
        assert_eq!(key.expose_secret().0, unlocked_key.expose_secret().0);

        let wrong_result = manager.unlock_vault("wrong".to_string()).await;
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
}
