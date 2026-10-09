use secrecy::SecretBox;

use crate::kdf::{DEFAULT_KDF, argon2id};
use crate::{CryptoError, SafeKey, random};

pub fn generate_salt(len: usize) -> Vec<u8> {
    let mut salt = vec![0u8; len];
    random::fill_random(&mut salt);
    salt
}

/// The one password KDF moku uses: Argon2id, m = 64 MiB, t = 3, p = 4,
/// 32-byte output, version 0x13 (see CLAUDE.md). Synchronous and
/// CPU-heavy — async callers run it on a blocking thread (moku-vault's
/// `SecurityManager::derive_key`).
pub fn argon2id_key(password: &[u8], salt: &[u8]) -> Result<SecretBox<SafeKey>, CryptoError> {
    let key = argon2id(password, salt, DEFAULT_KDF)?;
    Ok(SecretBox::new(Box::new(SafeKey(*key.expose()))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_salt_length() {
        let salt = generate_salt(16);
        assert_eq!(salt.len(), 16);

        let salt2 = generate_salt(32);
        assert_eq!(salt2.len(), 32);
        assert_ne!(salt, generate_salt(16));
    }
}
