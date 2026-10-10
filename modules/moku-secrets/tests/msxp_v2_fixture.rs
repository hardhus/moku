//! A frozen MSXP v2 export, produced once by the code that introduced v2
//! (P3, moku-bin 0.2.11), so every later version must keep opening it.
//!
//! Synthetic data only (foo/bar/example.com, a fixed test password). Never
//! regenerate or edit `tests/fixtures/msxp-v2.msxp`; if `v2_fixture_*`
//! fails, the change broke users' v2 export files.

use std::path::PathBuf;

use moku_secrets::model::SecretEntry;
use moku_secrets::msxp;

const PASSWORD: &str = "foo-export-password-v2";

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/msxp-v2.msxp")
}

fn sample_entries() -> Vec<SecretEntry> {
    let mut api = SecretEntry::new("foo-api".to_string(), "bar-secret-value");
    api.url = Some("https://example.com".to_string());
    let mut login = SecretEntry::new("bar-login".to_string(), "foo-password");
    login.username = Some("foo@example.com".to_string());
    login.totp_seed = Some("JBSWY3DPEHPK3PXP".into());
    vec![api, login]
}

/// One-shot generator, already run. Refuses to overwrite the fixture.
///   cargo test -p moku-secrets --test msxp_v2_fixture -- --ignored
#[tokio::test]
#[ignore]
async fn generate_v2_fixture_once() {
    let path = fixture_path();
    assert!(!path.exists(), "the v2 fixture is frozen; never regenerate it");
    let bytes = msxp::export(&sample_entries(), &PASSWORD.into()).await.unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
}

#[tokio::test]
async fn v2_fixture_imports() {
    let data = std::fs::read(fixture_path()).unwrap();
    let imported = msxp::import(&data, &PASSWORD.into()).await.unwrap();
    assert_eq!(imported.version, msxp::Version::V2);
    let names: Vec<&str> = imported.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["foo-api", "bar-login"]);
    assert_eq!(imported.entries[0].value.expose(), "bar-secret-value");
    assert_eq!(
        imported.entries[1].totp_seed.as_ref().map(|s| s.expose()),
        Some("JBSWY3DPEHPK3PXP")
    );
}

#[tokio::test]
async fn v2_fixture_rejects_wrong_password() {
    let data = std::fs::read(fixture_path()).unwrap();
    assert!(msxp::import(&data, &"not-the-password".into()).await.is_err());
}

/// The header layout is part of the format.
#[test]
fn v2_fixture_header_layout() {
    let data = std::fs::read(fixture_path()).unwrap();
    assert_eq!(&data[0..4], b"MSXP");
    assert_eq!(data[4], 2, "version");
    assert_eq!(data[5], 1, "kdf id: Argon2id");
    assert_eq!(&data[6..18], &[0, 0, 1, 0, 3, 0, 0, 0, 4, 0, 0, 0], "m=65536 t=3 p=4 (LE)");
    assert_eq!(data[18], 16, "salt length");
    assert_eq!(data[35], 2, "seal alg: XChaCha20-Poly1305");
}
