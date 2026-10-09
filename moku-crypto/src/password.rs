use argon2::{Algorithm, Argon2, Params, Version, password_hash::rand_core::OsRng};
use rand::RngCore;
use secrecy::SecretBox;

use crate::{CryptoError, SafeKey};

pub fn generate_salt(len: usize) -> Vec<u8> {
    let mut salt = vec![0u8; len];
    OsRng.fill_bytes(&mut salt);
    salt
}

/// The one password KDF moku uses: Argon2id, m = 64 MiB, t = 3, p = 4,
/// 32-byte output, version 0x13 (see CLAUDE.md). Synchronous and
/// CPU-heavy — async callers run it on a blocking thread (moku-vault's
/// `SecurityManager::derive_key`).
pub fn argon2id_key(password: &[u8], salt: &[u8]) -> Result<SecretBox<SafeKey>, CryptoError> {
    let params = Params::new(65536, 3, 4, Some(32))
        .map_err(|e| CryptoError::Kdf(format!("invalid Argon2 parameters: {e}")))?;

    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut output_key = [0u8; 32];

    argon2
        .hash_password_into(password, salt, &mut output_key)
        .map_err(|e| CryptoError::Kdf(e.to_string()))?;

    Ok(SecretBox::new(Box::new(SafeKey(output_key))))
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
