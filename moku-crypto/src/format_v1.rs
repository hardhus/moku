//! The pre-P3 key model and ciphertext format, kept only as long as data
//! in it still exists.
//!
//! - Format: `nonce (12) || ct || tag (16)`, AES-256-GCM, **no AAD**.
//!   Used by V0/V1 storage envelopes, the vault canary and MSXP v1
//!   exports.
//! - These functions accept a key of **any** role. This is the one place
//!   that deliberately bypasses the role system, because V0 records and
//!   the canary are encrypted directly under the root key, and MSXP v1
//!   directly under a raw Argon2 output.
//! - Removal plan: P6 moves the store to `seal`/`open` and deletes the
//!   write path; the read path moves to the migration crate (P4/P6). MSXP
//!   v1 import stays a permanent reader, since export files are the user's
//!   own files outside the data directory.

use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::Aead;
use zeroize::Zeroizing;

use crate::seal::{TAG_LEN, gcm};
use crate::{CryptoError, Key, KeyRole, PasswordKey, RootKey, VolumeMasterKey, random};

const NONCE_LEN: usize = 12;

#[doc(hidden)]
pub fn seal_gcm_no_aad<P: KeyRole>(key: &Key<P>, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let nonce = random::random_array::<NONCE_LEN>();
    let ct = gcm(key.expose())
        .encrypt(GenericArray::from_slice(&nonce), plaintext)
        .map_err(|_| CryptoError::EncryptFailed)?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

#[doc(hidden)]
pub fn open_gcm_no_aad<P: KeyRole>(
    key: &Key<P>,
    data: &[u8],
) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    if data.len() < NONCE_LEN + TAG_LEN {
        return Err(CryptoError::Malformed("shorter than its nonce and tag"));
    }
    let (nonce, ct) = data.split_at(NONCE_LEN);
    let pt = gcm(key.expose())
        .decrypt(GenericArray::from_slice(nonce), ct)
        .map_err(|_| CryptoError::AuthFailed)?;
    Ok(Zeroizing::new(pt))
}

/// The app vault's root key is, today, the Argon2id output of the vault
/// password itself (`meta.json` v1). P5 keeps this value as the DEK.
pub fn adopt_password_key_as_root(key: Key<PasswordKey>) -> Key<RootKey> {
    key.into_role()
}

/// A Custom-mode volume's own vault yields its master key directly (its
/// Argon2id output, via the same `meta.json` v1 scheme as the app vault).
/// Pinned by the v0.2.7 Custom-volume fixture.
pub fn custom_volume_master(key: Key<RootKey>) -> Key<VolumeMasterKey> {
    key.into_role()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_and_layout() {
        let key = Key::<RootKey>::from_slice(&[0x42; 32]).unwrap();
        let sealed = seal_gcm_no_aad(&key, b"foo").unwrap();
        assert_eq!(sealed.len(), NONCE_LEN + 3 + TAG_LEN);
        assert_eq!(&open_gcm_no_aad(&key, &sealed).unwrap()[..], b"foo");
    }

    #[test]
    fn test_wrong_key_and_short_input() {
        let key = Key::<RootKey>::from_slice(&[0x42; 32]).unwrap();
        let other = Key::<RootKey>::from_slice(&[0x43; 32]).unwrap();
        let sealed = seal_gcm_no_aad(&key, b"foo").unwrap();
        assert_eq!(open_gcm_no_aad(&other, &sealed), Err(CryptoError::AuthFailed));
        assert!(matches!(open_gcm_no_aad(&key, &[0u8; 20]), Err(CryptoError::Malformed(_))));
    }

    #[test]
    fn test_role_adoptions_keep_the_bytes() {
        let pk = Key::<PasswordKey>::from_slice(&[5; 32]).unwrap();
        let root = adopt_password_key_as_root(pk);
        assert_eq!(root.expose(), &[5; 32]);
        assert_eq!(custom_volume_master(root).expose(), &[5; 32]);
    }
}
