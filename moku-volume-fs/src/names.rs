use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use moku_crypto::{CryptoError, Key, NameSiv, VolumeNameKey};

use crate::types::{VResult as Result, VolumeFsError};

/// Cap on the *virtual* (plaintext) file/directory name length. AES-SIV +
/// base64url inflates by roughly 1.33x on top of a fixed 16-byte SIV tag,
/// so this keeps every encrypted backing name comfortably under common
/// 255-byte filesystem limits. Longer names are rejected (NameTooLong) —
/// see the plan's v1 scope note; a gocryptfs-style long-name overflow
/// scheme is a possible future hardening pass.
pub const MAX_VIRTUAL_NAME_LEN: usize = 140;

/// Volume file-name encryption: AES-256-SIV (moku-crypto's `NameSiv`)
/// plus the MKV1 name rules (length cap, URL-safe base64 without padding).
pub struct NameCipher {
    siv: NameSiv,
}

impl NameCipher {
    pub fn new(key: Key<VolumeNameKey, 64>) -> Self {
        Self {
            siv: NameSiv::new(key),
        }
    }

    /// Encrypts one path segment name, deterministically, scoped to its
    /// parent directory's IV — identical names in different directories
    /// encrypt to different backing names, and renaming/moving a directory
    /// never requires re-encrypting its children (plan §1).
    pub fn encrypt_name(&self, dir_iv: &[u8; 16], name: &str) -> Result<String> {
        if name.len() > MAX_VIRTUAL_NAME_LEN {
            return Err(VolumeFsError::NameTooLong);
        }
        let ciphertext = self.siv.seal(dir_iv, name.as_bytes())?;
        Ok(URL_SAFE_NO_PAD.encode(ciphertext))
    }

    pub fn decrypt_name(&self, dir_iv: &[u8; 16], encoded: &str) -> Result<String> {
        let ciphertext = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|e| VolumeFsError::Corrupt(format!("backing name is not base64: {e}")))?;
        let plaintext = self.siv.open(dir_iv, &ciphertext).map_err(|e| match e {
            CryptoError::AuthFailed => VolumeFsError::Integrity("file name"),
            other => other.into(),
        })?;
        std::str::from_utf8(&plaintext)
            .map(str::to_owned)
            .map_err(|_| VolumeFsError::Corrupt("decrypted name is not valid UTF-8".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher() -> NameCipher {
        NameCipher::new(Key::from_slice(&[5u8; 64]).unwrap())
    }

    #[test]
    fn test_name_roundtrip() {
        let cipher = cipher();
        let dir_iv = [1u8; 16];
        let encrypted = cipher.encrypt_name(&dir_iv, "todo.md").unwrap();
        let decrypted = cipher.decrypt_name(&dir_iv, &encrypted).unwrap();
        assert_eq!(decrypted, "todo.md");
    }

    #[test]
    fn test_same_name_different_dir_iv_differs() {
        let cipher = cipher();
        let a = cipher.encrypt_name(&[1u8; 16], "todo.md").unwrap();
        let b = cipher.encrypt_name(&[2u8; 16], "todo.md").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn test_deterministic_same_dir_same_name() {
        let cipher = cipher();
        let dir_iv = [3u8; 16];
        let a = cipher.encrypt_name(&dir_iv, "notes.md").unwrap();
        let b = cipher.encrypt_name(&dir_iv, "notes.md").unwrap();
        assert_eq!(a, b, "SIV mode is deterministic by design");
    }

    #[test]
    fn test_name_too_long_rejected() {
        let cipher = cipher();
        let long_name = "a".repeat(MAX_VIRTUAL_NAME_LEN + 1);
        assert!(matches!(
            cipher.encrypt_name(&[0u8; 16], &long_name),
            Err(VolumeFsError::NameTooLong)
        ));
    }

    #[test]
    fn test_wrong_dir_iv_fails_to_decrypt() {
        let cipher = cipher();
        let encrypted = cipher.encrypt_name(&[1u8; 16], "secret.txt").unwrap();
        assert!(matches!(
            cipher.decrypt_name(&[2u8; 16], &encrypted),
            Err(VolumeFsError::Integrity(_))
        ));
    }

    #[test]
    fn test_encrypted_name_fits_typical_filesystem_limits() {
        let cipher = cipher();
        let name = "a".repeat(MAX_VIRTUAL_NAME_LEN);
        let encrypted = cipher.encrypt_name(&[0u8; 16], &name).unwrap();
        assert!(encrypted.len() < 255, "encrypted name must fit under common FS limits");
    }
}
