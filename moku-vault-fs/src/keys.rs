use hkdf::Hkdf;
use moku_core::SafeKey;
use secrecy::{ExposeSecret, SecretBox};
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// AES-256-GCM key for block content encryption.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct ContentKey(pub [u8; 32]);

/// AES-256-SIV key pair (two AES-256 keys, 64 bytes) for deterministic
/// filename encryption.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct NameKey(pub [u8; 64]);

pub struct VolumeKeys {
    pub content: SecretBox<ContentKey>,
    pub name: SecretBox<NameKey>,
}

const HKDF_INFO_CONTENT: &[u8] = b"moku-vault-fs/content-key/v1";
const HKDF_INFO_NAME: &[u8] = b"moku-vault-fs/name-key/v1";

/// Derives two independent subkeys from the volume's Argon2id master key —
/// never reuses the raw master key directly for either cipher (plan §1).
pub fn derive_volume_keys(master: &SecretBox<SafeKey>) -> VolumeKeys {
    let hk = Hkdf::<Sha256>::new(None, &master.expose_secret().0);

    let mut content = [0u8; 32];
    hk.expand(HKDF_INFO_CONTENT, &mut content)
        .expect("32 bytes is a valid HKDF-SHA256 output length");

    let mut name = [0u8; 64];
    hk.expand(HKDF_INFO_NAME, &mut name)
        .expect("64 bytes is a valid HKDF-SHA256 output length");

    VolumeKeys {
        content: SecretBox::new(Box::new(ContentKey(content))),
        name: SecretBox::new(Box::new(NameKey(name))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_volume_keys_deterministic_and_distinct() {
        let master = SecretBox::new(Box::new(SafeKey([7u8; 32])));
        let k1 = derive_volume_keys(&master);
        let k2 = derive_volume_keys(&master);
        assert_eq!(k1.content.expose_secret().0, k2.content.expose_secret().0);
        assert_eq!(k1.name.expose_secret().0, k2.name.expose_secret().0);
        assert_ne!(&k1.content.expose_secret().0[..], &k1.name.expose_secret().0[..32]);
    }

    #[test]
    fn test_derive_volume_keys_differs_by_master() {
        let m1 = SecretBox::new(Box::new(SafeKey([1u8; 32])));
        let m2 = SecretBox::new(Box::new(SafeKey([2u8; 32])));
        let k1 = derive_volume_keys(&m1);
        let k2 = derive_volume_keys(&m2);
        assert_ne!(k1.content.expose_secret().0, k2.content.expose_secret().0);
    }
}
