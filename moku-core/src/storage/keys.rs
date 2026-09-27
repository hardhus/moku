use hkdf::Hkdf;
use secrecy::{ExposeSecret, SecretBox};
use sha2::Sha256;

use crate::security::SafeKey;

const MODULE_KEY_INFO_PREFIX: &[u8] = b"moku-core/storage/";
const MODULE_KEY_INFO_SUFFIX: &[u8] = b"/v1";

/// Derives a module-scoped storage subkey from the vault's raw master key —
/// never uses the master key directly as a cipher key (see
/// moku-volume-fs/src/keys.rs and moku-volume-daemon/src/registry.rs's
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

    /// K2 — known-answer test pinning the exact per-module subkeys (info
    /// string `moku-core/storage/<module_id>/v1`) as produced by moku-bin
    /// 0.2.7. Every encrypted module record on disk is keyed by these
    /// bytes; never "update" this to match a new implementation.
    #[test]
    fn k2_module_storage_key_known_answers() {
        fn hex(bytes: &[u8]) -> String {
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        }
        let master = SecretBox::new(Box::new(SafeKey([0x07; 32])));
        let actual: Vec<(&str, String)> = ["todo", "bookmark", "rss", "secrets", ""]
            .into_iter()
            .map(|m| {
                (
                    m,
                    hex(&derive_module_storage_key(&master, m).expose_secret().0),
                )
            })
            .collect();
        let expected: Vec<(&str, String)> = K2_EXPECTED
            .iter()
            .map(|(k, h)| (*k, h.to_string()))
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(MODULE_KEY_INFO_PREFIX, b"moku-core/storage/");
        assert_eq!(MODULE_KEY_INFO_SUFFIX, b"/v1");
    }
    const K2_EXPECTED: [(&str, &str); 5] = [
        (
            "todo",
            "b79cad5b2e3a4c8ffb01fac50dce1c93b450cbd0001bef01d9190fe384fa90c6",
        ),
        (
            "bookmark",
            "bba36dba9e60652aba66bbcb159cee944fcb3eb0c81397295b16fea9d0831d3d",
        ),
        (
            "rss",
            "038953a11817dc252be6630fe8190d40bd675f846d58e05c28388212287534e2",
        ),
        (
            "secrets",
            "424763cdb988937554b71716743dc14f8eb6d8893f4d161e98e1ca79b9d5ef8d",
        ),
        (
            "",
            "ad074087ca22bf1a316e189c00d3d506be914027b35cb52eec9c1e5eca671990",
        ),
    ];

    #[test]
    fn test_derive_module_storage_key_differs_by_master() {
        let m1 = SecretBox::new(Box::new(SafeKey([1u8; 32])));
        let m2 = SecretBox::new(Box::new(SafeKey([2u8; 32])));
        let k1 = derive_module_storage_key(&m1, "todo");
        let k2 = derive_module_storage_key(&m2, "todo");
        assert_ne!(k1.expose_secret().0, k2.expose_secret().0);
    }
}
