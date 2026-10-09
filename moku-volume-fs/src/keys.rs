use moku_crypto::{Key, VolumeContentKey, VolumeMasterKey, VolumeNameKey};

/// A volume's two data keys. The HKDF info strings behind them
/// (`moku-vault-fs/content-key/v1`, `moku-vault-fs/name-key/v1`) are
/// permanent and still say "vault"; see `moku_crypto::purpose`.
pub struct VolumeKeys {
    /// AES-256-GCM key for block content encryption.
    pub content: Key<VolumeContentKey>,
    /// AES-256-SIV key pair (64 bytes) for deterministic filename
    /// encryption.
    pub name: Key<VolumeNameKey, 64>,
}

/// Derives two independent subkeys from the volume's master key — never
/// reuses the master key directly for either cipher (plan §1).
pub fn derive_volume_keys(master: &Key<VolumeMasterKey>) -> VolumeKeys {
    let (content, name) = moku_crypto::volume_keys(master);
    VolumeKeys { content, name }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn master(b: u8) -> Key<VolumeMasterKey> {
        Key::from_slice(&[b; 32]).unwrap()
    }

    #[test]
    fn test_derive_volume_keys_deterministic_and_distinct() {
        let k1 = derive_volume_keys(&master(7));
        let k2 = derive_volume_keys(&master(7));
        assert_eq!(k1.content.expose(), k2.content.expose());
        assert_eq!(k1.name.expose(), k2.name.expose());
        assert_ne!(&k1.content.expose()[..], &k1.name.expose()[..32]);
    }

    #[test]
    fn test_derive_volume_keys_differs_by_master() {
        let k1 = derive_volume_keys(&master(1));
        let k2 = derive_volume_keys(&master(2));
        assert_ne!(k1.content.expose(), k2.content.expose());
    }
}
