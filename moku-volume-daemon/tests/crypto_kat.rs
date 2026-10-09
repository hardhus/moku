//! Known-answer tests (KATs) pinning how Default-mode volumes derive their
//! keys from moku's app vault, as produced by the code in moku-bin 0.2.7.
//! Every existing Default-mode volume depends on these exact bytes. If one
//! of these fails, the change broke compatibility with existing volumes.
//! Never "update" the expected values to match a new implementation.

use moku_crypto::{Key, RootKey};
use moku_vault::SecurityManager;
use moku_volume_daemon::registry::derive_default_volume_master_key;
use moku_volume_fs::derive_volume_keys;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// K3 — per-volume master key, HKDF info
/// `moku-vault-daemon/default-volume/v1/<volume_id>`. The "vault" in the info
/// string is intentionally permanent (see src/registry.rs).
#[test]
fn k3_default_volume_master_key_known_answers() {
    let app_master = Key::<RootKey>::from_slice(&[0x07; 32]).unwrap();
    let actual: Vec<(&str, String)> = ["notes", "a", "vol-a"]
        .into_iter()
        .map(|v| {
            let k = derive_default_volume_master_key(&app_master, v);
            (v, hex(k.expose()))
        })
        .collect();
    let expected: Vec<(&str, String)> = K3_EXPECTED
        .iter()
        .map(|(k, h)| (*k, h.to_string()))
        .collect();
    assert_eq!(actual, expected);
}
const K3_EXPECTED: [(&str, &str); 3] = [
    (
        "notes",
        "f2c17bc6e2e028c7b9d09cf4bd014ac4c5ef76ad0e3e10fe2227b6b23b0128e3",
    ),
    (
        "a",
        "8614b6169a7deac3f06e8617fccad21a3fb527e996d3b09f6b488a9112124135",
    ),
    (
        "vol-a",
        "8d8ca249795c8d3c2cd43feb2975418f2f804866f8eb59d0cb84f6b117b47b2c",
    ),
];

/// K5 — the full chain a real Default-mode volume goes through:
/// password --Argon2id--> app master --HKDF--> volume master --HKDF-->
/// content key + name key.
#[tokio::test]
async fn k5_full_default_volume_chain_known_answer() {
    let app_master = SecurityManager::derive_key(&"correct horse".into(), &[0x01; 16])
        .await
        .unwrap();
    let volume_master = derive_default_volume_master_key(&app_master, "notes");
    let keys = derive_volume_keys(&volume_master);
    assert_eq!(hex(keys.content.expose()), K5_CONTENT_EXPECTED);
    assert_eq!(hex(keys.name.expose()), K5_NAME_EXPECTED);
}
const K5_CONTENT_EXPECTED: &str =
    "40373f037d7934d7708a946df9bab5cfe1ccd3031af05c36175d5dda57d8dbd0";
const K5_NAME_EXPECTED: &str = "d344862e0a9d476810e1f28e9f51711a0cba614e6511675fdbc9eaa3bb5a6322cb96c20b826b14eec703221891675c6b9d2347dfab318a86ee670917cb22bc07";
