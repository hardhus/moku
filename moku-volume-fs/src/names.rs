use aes_siv::KeyInit;
use aes_siv::siv::Aes256Siv;
use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use secrecy::{ExposeSecret, SecretBox};

use crate::keys::NameKey;

/// Cap on the *virtual* (plaintext) file/directory name length. AES-SIV +
/// base64url inflates by roughly 1.33x on top of a fixed 16-byte SIV tag,
/// so this keeps every encrypted backing name comfortably under common
/// 255-byte filesystem limits. Longer names are rejected (NameTooLong) —
/// see the plan's v1 scope note; a gocryptfs-style long-name overflow
/// scheme is a possible future hardening pass.
pub const MAX_VIRTUAL_NAME_LEN: usize = 140;

pub struct NameCipher<'a> {
    key: &'a SecretBox<NameKey>,
}

impl<'a> NameCipher<'a> {
    pub fn new(key: &'a SecretBox<NameKey>) -> Self {
        Self { key }
    }

    fn cipher(&self) -> Result<Aes256Siv> {
        Aes256Siv::new_from_slice(&self.key.expose_secret().0)
            .map_err(|_| anyhow!("invalid name key length"))
    }

    /// Encrypts one path segment name, deterministically, scoped to its
    /// parent directory's IV — identical names in different directories
    /// encrypt to different backing names, and renaming/moving a directory
    /// never requires re-encrypting its children (plan §1).
    pub fn encrypt_name(&self, dir_iv: &[u8; 16], name: &str) -> Result<String> {
        if name.len() > MAX_VIRTUAL_NAME_LEN {
            return Err(anyhow!("name too long"));
        }
        let mut cipher = self.cipher()?;
        let ciphertext = cipher
            .encrypt([dir_iv.as_slice()], name.as_bytes())
            .map_err(|e| anyhow!("name encryption failed: {e}"))?;
        Ok(URL_SAFE_NO_PAD.encode(ciphertext))
    }

    pub fn decrypt_name(&self, dir_iv: &[u8; 16], encoded: &str) -> Result<String> {
        let ciphertext = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|e| anyhow!("corrupt backing name (base64): {e}"))?;
        let mut cipher = self.cipher()?;
        let plaintext = cipher
            .decrypt([dir_iv.as_slice()], &ciphertext)
            .map_err(|_| anyhow!("name decryption failed (corrupt data or wrong key)"))?;
        String::from_utf8(plaintext).map_err(|_| anyhow!("decrypted name is not valid UTF-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SecretBox<NameKey> {
        SecretBox::new(Box::new(NameKey([5u8; 64])))
    }

    #[test]
    fn test_name_roundtrip() {
        let k = key();
        let cipher = NameCipher::new(&k);
        let dir_iv = [1u8; 16];
        let encrypted = cipher.encrypt_name(&dir_iv, "todo.md").unwrap();
        let decrypted = cipher.decrypt_name(&dir_iv, &encrypted).unwrap();
        assert_eq!(decrypted, "todo.md");
    }

    #[test]
    fn test_same_name_different_dir_iv_differs() {
        let k = key();
        let cipher = NameCipher::new(&k);
        let a = cipher.encrypt_name(&[1u8; 16], "todo.md").unwrap();
        let b = cipher.encrypt_name(&[2u8; 16], "todo.md").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn test_deterministic_same_dir_same_name() {
        let k = key();
        let cipher = NameCipher::new(&k);
        let dir_iv = [3u8; 16];
        let a = cipher.encrypt_name(&dir_iv, "notes.md").unwrap();
        let b = cipher.encrypt_name(&dir_iv, "notes.md").unwrap();
        assert_eq!(a, b, "SIV mode is deterministic by design");
    }

    #[test]
    fn test_name_too_long_rejected() {
        let k = key();
        let cipher = NameCipher::new(&k);
        let long_name = "a".repeat(MAX_VIRTUAL_NAME_LEN + 1);
        assert!(cipher.encrypt_name(&[0u8; 16], &long_name).is_err());
    }

    #[test]
    fn test_wrong_dir_iv_fails_to_decrypt() {
        let k = key();
        let cipher = NameCipher::new(&k);
        let encrypted = cipher.encrypt_name(&[1u8; 16], "secret.txt").unwrap();
        assert!(cipher.decrypt_name(&[2u8; 16], &encrypted).is_err());
    }

    #[test]
    fn test_encrypted_name_fits_typical_filesystem_limits() {
        let k = key();
        let cipher = NameCipher::new(&k);
        let name = "a".repeat(MAX_VIRTUAL_NAME_LEN);
        let encrypted = cipher.encrypt_name(&[0u8; 16], &name).unwrap();
        assert!(encrypted.len() < 255, "encrypted name must fit under common FS limits");
    }
}
