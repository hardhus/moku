use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use anyhow::{Result, anyhow};
use argon2::password_hash::rand_core::OsRng;
use rand::RngCore;
use secrecy::{ExposeSecret, SecretBox};

use crate::SafeKey;

pub fn encrypt(data: &[u8], key: &SecretBox<SafeKey>) -> Result<Vec<u8>> {
    let raw_key = &key.expose_secret().0;
    let cipher = Aes256Gcm::new_from_slice(raw_key).map_err(|_| anyhow!("Invalid key length"))?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_cycle() {
        let raw_key = [1u8; 32];
        let key = SecretBox::new(Box::new(SafeKey(raw_key)));
        let original_data = b"Moku Secret Message";

        let encrypted = encrypt(original_data, &key).expect("Encryption failed");
        let decrypted = decrypt(&encrypted, &key).expect("Decryption failed");

        assert_eq!(original_data, &decrypted[..]);
    }

    #[test]
    fn test_decrypt_with_wrong_key() {
        let key1 = SecretBox::new(Box::new(SafeKey([1u8; 32])));
        let key2 = SecretBox::new(Box::new(SafeKey([2u8; 32])));
        let data = b"Sensitive Info";

        let encrypted = encrypt(data, &key1).unwrap();

        let result = decrypt(&encrypted, &key2);
        assert!(result.is_err());
    }
}
