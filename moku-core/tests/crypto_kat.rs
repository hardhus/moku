//! Known-answer tests (KATs) pinning moku-core's current cryptographic
//! behavior byte-for-byte.
//!
//! These tests exist so that later refactors (moving crypto into its own
//! crate, adding AAD, keyslots, a new store format) can prove they did NOT
//! silently change how existing data is derived or decrypted. Every
//! expected value below was produced by the code as of moku-bin 0.2.7 and
//! must never be "updated to match" a new implementation: if one of these
//! fails, the change broke compatibility with data already on users' disks.
//!
//! The module-storage-key KAT (K2) lives next to its private function in
//! `src/storage/keys.rs`, since that function isn't exported.

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use moku_core::security::{SafeKey, SecurityManager};
use secrecy::{ExposeSecret, SecretBox};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// K1 — the vault password KDF: Argon2id, m=65536 KiB, t=3, p=4, 32-byte
/// output, version 0x13.
#[tokio::test]
async fn k1_argon2id_vault_key() {
    let key = SecurityManager::derive_key("correct horse", &[0x01; 16])
        .await
        .unwrap();
    assert_eq!(hex(&key.expose_secret().0), K1_EXPECTED);
}
const K1_EXPECTED: &str = "4471256fa58cf203c9c30f88acf8284cb765abf5fdb02f0e300f6bade11793d1";

/// K7a — the storage/canary ciphertext format: `nonce(12) || ct || tag(16)`,
/// AES-256-GCM, no AAD. Built here with a fixed nonce so the exact bytes can
/// be pinned, then opened with the production `SecurityManager::decrypt`.
#[test]
fn k7a_legacy_gcm_format_is_nonce_then_ciphertext_and_tag() {
    let key_bytes = [0x42u8; 32];
    let nonce_bytes = [0x24u8; 12];
    let plaintext = b"foo-kat-plaintext";

    let cipher = Aes256Gcm::new_from_slice(&key_bytes).unwrap();
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_slice())
        .unwrap();
    let mut blob = nonce_bytes.to_vec();
    blob.extend_from_slice(&ct);
    assert_eq!(hex(&blob), K7A_EXPECTED);

    let key = SecretBox::new(Box::new(SafeKey(key_bytes)));
    let opened = SecurityManager::decrypt(&blob, &key).unwrap();
    assert_eq!(opened, plaintext);
}
const K7A_EXPECTED: &str =
    "24242424242424242424242473feab6c82a7b213568443c3d346f6de18382847f6e5c9d4c96977e0ff0e847beb";

/// K7a (cont.) — whatever the production `encrypt` emits must round-trip and
/// keep the same framing: 12-byte nonce prefix + ciphertext + 16-byte tag.
#[test]
fn k7a_production_encrypt_keeps_the_same_framing() {
    let key = SecretBox::new(Box::new(SafeKey([0x42u8; 32])));
    let plaintext = b"foo-kat-plaintext";
    let blob = SecurityManager::encrypt(plaintext, &key).unwrap();
    assert_eq!(blob.len(), 12 + plaintext.len() + 16);
    assert_eq!(SecurityManager::decrypt(&blob, &key).unwrap(), plaintext);
}
