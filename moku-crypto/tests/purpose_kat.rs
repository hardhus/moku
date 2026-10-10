//! Known-answer tests for `Purpose` and the named HKDF derivations.
//!
//! The expected outputs of the permanent derivations are the same K2–K4
//! values pinned in 0.2.7 (K2 lived in moku-crypto's subkey.rs, K3/K4 in
//! moku-volume-daemon and moku-volume-fs tests), checked through the P3
//! API. K2's only home is now here. Never "update" an expected value to match new code: a failure
//! means existing data would no longer decrypt.

use moku_crypto::purpose::{
    Purpose, SlotKind, default_volume_master, module_storage_key, secrets_export_key, volume_keys,
};
use moku_crypto::{Key, PasswordKey, RootKey, VolumeMasterKey};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn every_info_string_is_pinned() {
    let cases: [(Purpose, &[u8]); 7] = [
        (
            Purpose::ModuleStorageV1 { module_id: "todo" },
            b"moku-core/storage/todo/v1",
        ),
        (
            Purpose::DefaultVolumeV1 { volume_id: "notes" },
            b"moku-vault-daemon/default-volume/v1/notes",
        ),
        (Purpose::VolumeContentV1, b"moku-vault-fs/content-key/v1"),
        (Purpose::VolumeNameV1, b"moku-vault-fs/name-key/v1"),
        (
            Purpose::KeyslotKek { kind: SlotKind::Password },
            b"moku/keyslot/kek/v1\x00\x08password",
        ),
        (
            Purpose::KeyslotKek { kind: SlotKind::Recovery },
            b"moku/keyslot/kek/v1\x00\x08recovery",
        ),
        (Purpose::SecretsExportV2, b"moku/secrets/export/v2"),
    ];
    for (purpose, expected) in cases {
        assert_eq!(purpose.info(), expected, "{purpose:?}");
    }
}

/// K2 through the P3 API.
#[test]
fn k2_module_storage_key_via_purpose() {
    let root = Key::<RootKey>::from_slice(&[0x07; 32]).unwrap();
    let expected = [
        ("todo", "b79cad5b2e3a4c8ffb01fac50dce1c93b450cbd0001bef01d9190fe384fa90c6"),
        ("bookmark", "bba36dba9e60652aba66bbcb159cee944fcb3eb0c81397295b16fea9d0831d3d"),
        ("rss", "038953a11817dc252be6630fe8190d40bd675f846d58e05c28388212287534e2"),
        ("secrets", "424763cdb988937554b71716743dc14f8eb6d8893f4d161e98e1ca79b9d5ef8d"),
        ("", "ad074087ca22bf1a316e189c00d3d506be914027b35cb52eec9c1e5eca671990"),
    ];
    for (module, hex_expected) in expected {
        assert_eq!(hex(module_storage_key(&root, module).expose()), hex_expected, "{module}");
    }
}

/// K3 through the P3 API.
#[test]
fn k3_default_volume_master_via_purpose() {
    let root = Key::<RootKey>::from_slice(&[0x07; 32]).unwrap();
    let expected = [
        ("notes", "f2c17bc6e2e028c7b9d09cf4bd014ac4c5ef76ad0e3e10fe2227b6b23b0128e3"),
        ("a", "8614b6169a7deac3f06e8617fccad21a3fb527e996d3b09f6b488a9112124135"),
        ("vol-a", "8d8ca249795c8d3c2cd43feb2975418f2f804866f8eb59d0cb84f6b117b47b2c"),
    ];
    for (volume, hex_expected) in expected {
        assert_eq!(hex(default_volume_master(&root, volume).expose()), hex_expected, "{volume}");
    }
}

/// K4 through the P3 API.
#[test]
fn k4_volume_keys_via_purpose() {
    let master = Key::<VolumeMasterKey>::from_slice(&[0x07; 32]).unwrap();
    let (content, name) = volume_keys(&master);
    assert_eq!(
        hex(content.expose()),
        "d0a162181535eea5958394f7f84ddb15ec4065110431855540953f01d29dcba0"
    );
    assert_eq!(
        hex(name.expose()),
        "0d6d3a721d2ccae933a2140f69955bed3328a896e14cef15a76d5be769c6f87e62816061a58339e38edac6498d17cf520e0f2a68d0de1eaaf037ce73402f9430"
    );
}

/// MSXP v2 export key, pinned from its first implementation (P3) and
/// cross-checked against an independent HKDF (Python hmac/hashlib).
#[test]
fn secrets_export_key_known_answer() {
    let pk = Key::<PasswordKey>::from_slice(&[0x07; 32]).unwrap();
    assert_eq!(hex(secrets_export_key(&pk).expose()), SECRETS_EXPORT_EXPECTED);
}
const SECRETS_EXPORT_EXPECTED: &str =
    "a3b76b4eac60fe7d573e52e578050ef711cab5f706864b8f10a577f3a7beed7d";

/// RFC 5869 Test Case 1, straight against the hkdf crate: guards the
/// library itself across upgrades (P10).
#[test]
fn rfc5869_test_case_1() {
    let ikm = [0x0bu8; 22];
    let salt: Vec<u8> = (0x00..=0x0c).collect();
    let info: Vec<u8> = (0xf0..=0xf9).collect();
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(&salt), &ikm);
    let mut okm = [0u8; 42];
    hk.expand(&info, &mut okm).unwrap();
    assert_eq!(
        hex(&okm),
        "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
    );
}
