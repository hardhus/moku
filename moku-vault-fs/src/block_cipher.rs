use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use anyhow::{Result, anyhow};
use rand::RngCore;
use rand::rngs::OsRng;
use secrecy::{ExposeSecret, SecretBox};

use crate::keys::ContentKey;

pub const BLOCK_SIZE: usize = 4096;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
pub const FULL_BLOCK_DISK_SIZE: u64 = (NONCE_LEN + BLOCK_SIZE + TAG_LEN) as u64;

pub struct BlockCipher<'a> {
    key: &'a SecretBox<ContentKey>,
}

impl<'a> BlockCipher<'a> {
    pub fn new(key: &'a SecretBox<ContentKey>) -> Self {
        Self { key }
    }

    fn aad(file_id: &[u8; 16], block_idx: u64) -> [u8; 24] {
        let mut aad = [0u8; 24];
        aad[..16].copy_from_slice(file_id);
        aad[16..].copy_from_slice(&block_idx.to_be_bytes());
        aad
    }

    /// Encrypts one block's plaintext (<=4096 bytes) into its on-disk form:
    /// `nonce(12) || ciphertext+tag`. AAD binds the block to its owning
    /// file and position, so a block can never be spliced into another
    /// file or moved to a different offset undetected.
    pub fn encrypt_block(&self, file_id: &[u8; 16], block_idx: u64, plaintext: &[u8]) -> Result<Vec<u8>> {
        debug_assert!(plaintext.len() <= BLOCK_SIZE);
        let cipher = Aes256Gcm::new_from_slice(&self.key.expose_secret().0)
            .map_err(|_| anyhow!("invalid content key length"))?;

        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let aad = Self::aad(file_id, block_idx);

        let ciphertext = cipher
            .encrypt(nonce, Payload { msg: plaintext, aad: &aad })
            .map_err(|e| anyhow!("block encryption failed: {e}"))?;

        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend(ciphertext);
        Ok(out)
    }

    /// Decrypts one on-disk block back into plaintext.
    pub fn decrypt_block(&self, file_id: &[u8; 16], block_idx: u64, disk_bytes: &[u8]) -> Result<Vec<u8>> {
        if disk_bytes.len() < NONCE_LEN + TAG_LEN {
            return Err(anyhow!("corrupt block: too short"));
        }
        let cipher = Aes256Gcm::new_from_slice(&self.key.expose_secret().0)
            .map_err(|_| anyhow!("invalid content key length"))?;

        let (nonce_bytes, ciphertext) = disk_bytes.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);
        let aad = Self::aad(file_id, block_idx);

        cipher
            .decrypt(nonce, Payload { msg: ciphertext, aad: &aad })
            .map_err(|_| anyhow!("block decryption failed (corrupt data or wrong key)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SecretBox<ContentKey> {
        SecretBox::new(Box::new(ContentKey([9u8; 32])))
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let k = key();
        let cipher = BlockCipher::new(&k);
        let file_id = [1u8; 16];
        let plaintext = b"hello moku vault";

        let disk = cipher.encrypt_block(&file_id, 0, plaintext).unwrap();
        assert_eq!(disk.len(), NONCE_LEN + plaintext.len() + TAG_LEN);

        let back = cipher.decrypt_block(&file_id, 0, &disk).unwrap();
        assert_eq!(back, plaintext);
    }

    #[test]
    fn test_wrong_block_idx_fails_aad_check() {
        let k = key();
        let cipher = BlockCipher::new(&k);
        let file_id = [1u8; 16];
        let disk = cipher.encrypt_block(&file_id, 3, b"data").unwrap();
        assert!(cipher.decrypt_block(&file_id, 4, &disk).is_err());
    }

    #[test]
    fn test_wrong_file_id_fails_aad_check() {
        let k = key();
        let cipher = BlockCipher::new(&k);
        let disk = cipher.encrypt_block(&[1u8; 16], 0, b"data").unwrap();
        assert!(cipher.decrypt_block(&[2u8; 16], 0, &disk).is_err());
    }

    #[test]
    fn test_two_encryptions_use_different_nonces() {
        let k = key();
        let cipher = BlockCipher::new(&k);
        let file_id = [1u8; 16];
        let a = cipher.encrypt_block(&file_id, 0, b"same plaintext!!").unwrap();
        let b = cipher.encrypt_block(&file_id, 0, b"same plaintext!!").unwrap();
        assert_ne!(a, b, "nonce reuse would make ciphertexts identical");
    }
}
