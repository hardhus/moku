use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use argon2::password_hash::rand_core::OsRng;
use rand::RngCore;
use secrecy::{ExposeSecret, SecretBox};

use crate::{CryptoError, SafeKey};

pub fn encrypt(data: &[u8], key: &SecretBox<SafeKey>) -> Result<Vec<u8>, CryptoError> {
    let raw_key = &key.expose_secret().0;
    let cipher =
        Aes256Gcm::new_from_slice(raw_key).map_err(|_| CryptoError::InvalidKeyLength)?;

    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, data)
        .map_err(|_| CryptoError::EncryptFailed)?;

    let mut combined = Vec::with_capacity(nonce_bytes.len() + ciphertext.len());
    combined.extend_from_slice(&nonce_bytes);
    combined.extend(ciphertext);
    Ok(combined)
}

pub fn decrypt(combined_data: &[u8], key: &SecretBox<SafeKey>) -> Result<Vec<u8>, CryptoError> {
    if combined_data.len() < 12 {
        return Err(CryptoError::Malformed("shorter than its nonce"));
    }
    let raw_key = &key.expose_secret().0;
    let cipher =
        Aes256Gcm::new_from_slice(raw_key).map_err(|_| CryptoError::InvalidKeyLength)?;

    let (nonce_bytes, ciphertext) = combined_data.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| CryptoError::AuthFailed)
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
        assert_eq!(result, Err(CryptoError::AuthFailed));
    }

    #[test]
    fn test_decrypt_rejects_a_payload_shorter_than_the_nonce() {
        let key = SecretBox::new(Box::new(SafeKey([1u8; 32])));
        assert!(matches!(decrypt(&[0u8; 11], &key), Err(CryptoError::Malformed(_))));
    }
}
