//! The one password KDF: Argon2id, version 0x13.

use argon2::{Algorithm, Argon2, Block, Params, Version};
use zeroize::Zeroizing;

use crate::{CryptoError, Key, PasswordKey, SecretString, random};

/// Memory cost in KiB (64 MiB).
pub const ARGON2_M_KIB: u32 = 65_536;
/// Iterations.
pub const ARGON2_T: u32 = 3;
/// Parallelism (lanes).
pub const ARGON2_P: u32 = 4;
/// Output length in bytes.
pub const ARGON2_OUT: usize = 32;
/// Salt length moku generates.
pub const SALT_LEN: usize = 16;

/// Argon2id cost parameters. Stored next to anything derived from a
/// password in a new format (keyslots, MSXP v2), so they can be raised
/// later without breaking old data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

/// The parameters every moku password key uses today (see CLAUDE.md).
/// Formats that don't record parameters (`meta.json` v1, MSXP v1) imply
/// these.
pub const DEFAULT_KDF: KdfParams = KdfParams {
    m_kib: ARGON2_M_KIB,
    t: ARGON2_T,
    p: ARGON2_P,
};

/// A fresh random salt.
pub fn generate_salt() -> [u8; SALT_LEN] {
    random::random_array()
}

/// Derives a password key with Argon2id. Synchronous and CPU-heavy:
/// async callers run it on a blocking thread.
pub fn password_key(
    password: &SecretString,
    salt: &[u8],
    params: KdfParams,
) -> Result<Key<PasswordKey>, CryptoError> {
    argon2id(password.expose().as_bytes(), salt, params)
}

/// The block memory is our own `Zeroizing` buffer, not the one argon2's
/// `hash_password_into` would allocate: that one is freed unwiped, even
/// with argon2's `zeroize` feature (proven in tests/zeroize_on_free.rs).
pub(crate) fn argon2id(
    password: &[u8],
    salt: &[u8],
    params: KdfParams,
) -> Result<Key<PasswordKey>, CryptoError> {
    let argon_params = Params::new(params.m_kib, params.t, params.p, Some(ARGON2_OUT))
        .map_err(|e| CryptoError::Kdf(format!("invalid Argon2 parameters: {e}")))?;
    let mut memory = Zeroizing::new(vec![Block::default(); argon_params.block_count()]);
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params);
    Key::from_fn(|out| {
        argon2
            .hash_password_into_with_memory(password, salt, out, memory.as_mut_slice())
            .map_err(|e| CryptoError::Kdf(e.to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_kdf_matches_the_documented_parameters() {
        assert_eq!(
            DEFAULT_KDF,
            KdfParams {
                m_kib: 65_536,
                t: 3,
                p: 4
            }
        );
        assert_eq!(ARGON2_OUT, 32);
        assert_eq!(SALT_LEN, 16);
    }

    #[test]
    fn test_invalid_parameters_are_a_kdf_error() {
        let pw = SecretString::from("pw".to_string());
        let bad = KdfParams { m_kib: 1, t: 1, p: 1 };
        assert!(matches!(
            password_key(&pw, &[0u8; 16], bad),
            Err(CryptoError::Kdf(_))
        ));
    }

    #[test]
    fn test_generate_salt_is_random() {
        assert_ne!(generate_salt(), generate_salt());
    }
}
