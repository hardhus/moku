//! Known-answer tests (KATs) pinning moku-volume-fs's current key
//! derivation and filename encryption byte-for-byte, as produced by the code
//! in moku-bin 0.2.7. Every encrypted volume on disk depends on these exact
//! bytes. If one of these fails, the change broke compatibility with
//! existing volumes. Never "update" the expected values to match a new
//! implementation.

use moku_core::SafeKey;
use moku_volume_fs::derive_volume_keys;
use moku_volume_fs::keys::NameKey;
use moku_volume_fs::names::NameCipher;
use secrecy::{ExposeSecret, SecretBox};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// K4 — content key (HKDF info `moku-vault-fs/content-key/v1`, 32 B) and
/// name key (HKDF info `moku-vault-fs/name-key/v1`, 64 B). The "vault" in
/// the info strings is intentionally permanent (see src/keys.rs).
#[test]
fn k4_volume_keys_known_answers() {
    let master = SecretBox::new(Box::new(SafeKey([0x07; 32])));
    let keys = derive_volume_keys(&master);
    assert_eq!(hex(&keys.content.expose_secret().0), K4_CONTENT_EXPECTED);
    assert_eq!(hex(&keys.name.expose_secret().0), K4_NAME_EXPECTED);
}
const K4_CONTENT_EXPECTED: &str =
    "d0a162181535eea5958394f7f84ddb15ec4065110431855540953f01d29dcba0";
const K4_NAME_EXPECTED: &str = "0d6d3a721d2ccae933a2140f69955bed3328a896e14cef15a76d5be769c6f87e62816061a58339e38edac6498d17cf520e0f2a68d0de1eaaf037ce73402f9430";

/// K6 — deterministic AES-SIV filename encryption (parent dir IV as AD,
/// URL-safe base64 without padding).
#[test]
fn k6_name_cipher_known_answer() {
    let name_key = SecretBox::new(Box::new(NameKey([5u8; 64])));
    let cipher = NameCipher::new(&name_key);
    let encrypted = cipher.encrypt_name(&[1u8; 16], "todo.md").unwrap();
    assert_eq!(encrypted, K6_EXPECTED);
    assert_eq!(
        cipher.decrypt_name(&[1u8; 16], &encrypted).unwrap(),
        "todo.md"
    );
}
const K6_EXPECTED: &str = "UJ1gnd9b36kqfhWnlT1Rx1ZwwEodVTw";
