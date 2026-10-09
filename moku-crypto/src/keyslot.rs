//! Keyslot primitives for P5's DEK + keyslot vault: wrapping the DEK under
//! a key-encryption key (KEK) derived from a password or recovery secret.
//!
//! The slot's own metadata (vault id, slot id, kind, KDF parameters) goes
//! into the `Aad`, so swapping slots or downgrading a slot's KDF
//! parameters makes unwrapping fail. A successful unwrap also proves the
//! password, so a keyslot needs no separate canary.

use crate::purpose::{Purpose, SlotKind, derive};
use crate::{
    Aad, Alg, CryptoError, KdfParams, Kek, Key, RecoverySecret, RootKey, SecretString, open,
    password_key, seal,
};

/// Encrypts the DEK under `kek`: a self-describing XChaCha20-Poly1305
/// `seal` output.
pub fn wrap_dek(kek: &Key<Kek>, dek: &Key<RootKey>, slot_aad: &Aad) -> Result<Vec<u8>, CryptoError> {
    seal(kek, Alg::XChaCha20Poly1305, slot_aad, dek.expose())
}

/// Reverses `wrap_dek`. A wrong KEK (wrong password) or any changed slot
/// metadata is `AuthFailed`.
pub fn unwrap_dek(kek: &Key<Kek>, wrapped: &[u8], slot_aad: &Aad) -> Result<Key<RootKey>, CryptoError> {
    let dek = open(kek, slot_aad, wrapped)?;
    Key::from_slice(&dek)
}

/// Password slot KEK: Argon2id(password) -> HKDF `KeyslotKek{Password}`.
/// Synchronous and CPU-heavy, like `password_key`.
pub fn password_kek(
    password: &SecretString,
    salt: &[u8],
    params: KdfParams,
) -> Result<Key<Kek>, CryptoError> {
    let pk = password_key(password, salt, params)?;
    Ok(derive(
        pk.expose(),
        Purpose::KeyslotKek {
            kind: SlotKind::Password,
        },
    ))
}

/// Recovery slot KEK: HKDF `KeyslotKek{Recovery}` over the recovery
/// secret's entropy.
pub fn kek_from_recovery_entropy(entropy: &Key<RecoverySecret>) -> Key<Kek> {
    derive(
        entropy.expose(),
        Purpose::KeyslotKek {
            kind: SlotKind::Recovery,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn slot_aad(slot_id: &[u8], m_kib: u32) -> Aad {
        Aad::new("moku/keyslot/v1")
            .field(b"vault-1")
            .field(slot_id)
            .field(b"password")
            .field(&m_kib.to_le_bytes())
    }

    /// Small, fast parameters: these tests are about the slot logic.
    const TEST_KDF: KdfParams = KdfParams { m_kib: 64, t: 1, p: 1 };

    #[test]
    fn test_wrap_unwrap_roundtrip() {
        let kek = password_kek(&SecretString::from("pw".to_string()), &[1; 16], TEST_KDF).unwrap();
        let dek = Key::<RootKey>::random();
        let wrapped = wrap_dek(&kek, &dek, &slot_aad(b"s1", 64)).unwrap();
        let back = unwrap_dek(&kek, &wrapped, &slot_aad(b"s1", 64)).unwrap();
        assert_eq!(back.expose(), dek.expose());
    }

    #[test]
    fn test_wrong_password_or_changed_slot_metadata_fails() {
        let kek = password_kek(&SecretString::from("pw".to_string()), &[1; 16], TEST_KDF).unwrap();
        let dek = Key::<RootKey>::random();
        let wrapped = wrap_dek(&kek, &dek, &slot_aad(b"s1", 64)).unwrap();

        let wrong =
            password_kek(&SecretString::from("pW".to_string()), &[1; 16], TEST_KDF).unwrap();
        assert_eq!(
            unwrap_dek(&wrong, &wrapped, &slot_aad(b"s1", 64)).unwrap_err(),
            CryptoError::AuthFailed
        );
        // Slot swap and KDF downgrade.
        assert_eq!(
            unwrap_dek(&kek, &wrapped, &slot_aad(b"s2", 64)).unwrap_err(),
            CryptoError::AuthFailed
        );
        assert_eq!(
            unwrap_dek(&kek, &wrapped, &slot_aad(b"s1", 8)).unwrap_err(),
            CryptoError::AuthFailed
        );
    }

    /// New derivations, pinned from their first implementation (P3).
    #[test]
    fn test_kek_known_answers() {
        let kek = password_kek(&SecretString::from("correct horse".to_string()), &[1; 16], TEST_KDF)
            .unwrap();
        assert_eq!(hex(kek.expose()), PASSWORD_KEK_EXPECTED);
        let entropy = Key::<RecoverySecret>::from_slice(&[0x07; 32]).unwrap();
        assert_eq!(hex(kek_from_recovery_entropy(&entropy).expose()), RECOVERY_KEK_EXPECTED);
    }
    const PASSWORD_KEK_EXPECTED: &str =
        "b136708f05c3ae2511bd7615ab584243b0834ea5cef48295ecb06a6d54a5ac87";
    // Cross-checked against an independent HKDF (Python hmac/hashlib).
    const RECOVERY_KEK_EXPECTED: &str =
        "0ff5b2c4dc9ef50dd1f1d93c92a7c9c1d6f2b2af696fd5807abd59722997b43a";
}
