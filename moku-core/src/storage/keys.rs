use hkdf::Hkdf;
use secrecy::{ExposeSecret, SecretBox};
use sha2::Sha256;

use crate::security::SafeKey;

const MODULE_KEY_INFO_PREFIX: &[u8] = b"moku-core/storage/";
const MODULE_KEY_INFO_SUFFIX: &[u8] = b"/v1";

/// Derives a module-scoped storage subkey from the vault's raw master key —
/// never uses the master key directly as a cipher key (see
/// moku-vault-fs/src/keys.rs and moku-vault-daemon/src/registry.rs's
/// derive_default_volume_master_key for the established convention this
/// follows). Info string: `moku-core/storage/<module_id>/v1`.
pub fn derive_module_storage_key(
    master: &SecretBox<SafeKey>,
    module_id: &str,
) -> SecretBox<SafeKey> {
    let hk = Hkdf::<Sha256>::new(None, &master.expose_secret().0);
    let mut info = Vec::with_capacity(
        MODULE_KEY_INFO_PREFIX.len() + module_id.len() + MODULE_KEY_INFO_SUFFIX.len(),
    );
    info.extend_from_slice(MODULE_KEY_INFO_PREFIX);
    info.extend_from_slice(module_id.as_bytes());
    info.extend_from_slice(MODULE_KEY_INFO_SUFFIX);

    let mut out = [0u8; 32];
    hk.expand(&info, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    SecretBox::new(Box::new(SafeKey(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_module_storage_key_deterministic_and_distinct_per_module() {
        let master = SecretBox::new(Box::new(SafeKey([7u8; 32])));
        let k1 = derive_module_storage_key(&master, "todo");
        let k2 = derive_module_storage_key(&master, "todo");
        assert_eq!(k1.expose_secret().0, k2.expose_secret().0);

        let k3 = derive_module_storage_key(&master, "secrets");
        assert_ne!(k1.expose_secret().0, k3.expose_secret().0);
    }

    #[test]
    fn test_derive_module_storage_key_differs_from_master() {
        let master = SecretBox::new(Box::new(SafeKey([9u8; 32])));
        let derived = derive_module_storage_key(&master, "todo");
        assert_ne!(master.expose_secret().0, derived.expose_secret().0);
    }

    #[test]
    fn test_derive_module_storage_key_differs_by_master() {
        let m1 = SecretBox::new(Box::new(SafeKey([1u8; 32])));
        let m2 = SecretBox::new(Box::new(SafeKey([2u8; 32])));
        let k1 = derive_module_storage_key(&m1, "todo");
        let k2 = derive_module_storage_key(&m2, "todo");
        assert_ne!(k1.expose_secret().0, k2.expose_secret().0);
    }
}
