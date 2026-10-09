//! K9: verifies that today's code can still import the frozen moku-bin
//! 0.2.7 encrypted secrets export (`MSXP` v1) in
//! `test-fixtures/v0.2.7/msxp/` (see the README there). Export files live on
//! users' disks indefinitely, independent of the vault, so this format has
//! to stay importable.

use std::path::PathBuf;

const EXPORT_PASSWORD: &str = "foo-export-password";

fn fixture() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-fixtures/v0.2.7/msxp/export-v1.msxp");
    std::fs::read(path).unwrap()
}

#[tokio::test]
async fn k9_msxp_v1_fixture_imports() {
    let entries = moku_secrets::engine::import_encrypted(&fixture(), &EXPORT_PASSWORD.into())
        .await
        .unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["foo-api", "bar-login"]);
    assert_eq!(entries[0].value.as_str(), "bar-secret-value");
    assert_eq!(entries[1].totp_seed.as_deref(), Some("JBSWY3DPEHPK3PXP"));
}

#[tokio::test]
async fn k9_msxp_v1_fixture_rejects_wrong_password() {
    assert!(
        moku_secrets::engine::import_encrypted(&fixture(), &"not-the-password".into())
            .await
            .is_err()
    );
}
