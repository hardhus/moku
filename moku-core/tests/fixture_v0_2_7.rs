//! Verifies that today's code can still open the frozen moku-bin 0.2.7
//! on-disk fixtures in `test-fixtures/v0.2.7/` (see its README). The
//! fixtures are copied to a temporary directory first, so the committed
//! files are never modified.
//!
//! Later format migrations (audit checklist P4–P6) test their hops against
//! these same fixtures; this test pins what "the old format" means.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use moku_core::StorageManager;
use moku_core::security::{SecurityManager, VaultSession};
use serde_json::Value;
use zeroize::Zeroizing;

const VAULT_PASSWORD: &str = "foo-test-password";

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../test-fixtures/v0.2.7")
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), to).unwrap();
        }
    }
}

/// Raw envelope as stored in sled, to check the fixture really contains the
/// intended mix of formats.
fn raw_envelope(datadir: &Path, module: &str, key: &str) -> Value {
    let db = sled::open(datadir.join("vault").join(module).join("db")).unwrap();
    let bytes = db.get(key).unwrap().expect("record present");
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn v0_2_7_datadir_has_the_intended_format_mix() {
    let tmp = tempfile::tempdir().unwrap();
    let datadir = tmp.path().join("datadir");
    copy_dir(&fixture_root().join("datadir"), &datadir);

    assert!(
        !datadir.join("vault/.key_scheme_version").exists(),
        "fixture models a pre-0.2.1 install with no key-scheme marker"
    );
    for (module, key, status, scheme, storage) in [
        ("todo", "items", "Encrypted", "Legacy", "Embedded"),
        ("secrets", "entries", "Encrypted", "Legacy", "Embedded"),
        (
            "bookmark",
            "bookmarks_encrypted",
            "Encrypted",
            "PerModuleV1",
            "External",
        ),
        ("rss", "feeds", "Plaintext", "Legacy", "Embedded"),
        ("rss", "items", "Plaintext", "Legacy", "Embedded"),
    ] {
        let env = raw_envelope(&datadir, module, key);
        assert_eq!(env["status"], status, "{module}/{key} status");
        assert_eq!(env["key_scheme"], scheme, "{module}/{key} key_scheme");
        assert_eq!(env["storage_type"], storage, "{module}/{key} storage_type");
    }
}

#[tokio::test]
async fn v0_2_7_datadir_opens_and_loads_every_record() {
    let tmp = tempfile::tempdir().unwrap();
    let datadir = tmp.path().join("datadir");
    copy_dir(&fixture_root().join("datadir"), &datadir);

    let security = SecurityManager::new_with_root(datadir.clone());
    let key = security
        .unlock_vault(Zeroizing::new(VAULT_PASSWORD.into()))
        .await
        .expect("fixture vault unlocks with its test password");
    let session = Arc::new(VaultSession::new());
    session.unlock(key);
    let storage = StorageManager::new_with_root(session, datadir.clone())
        .await
        .unwrap();

    assert_eq!(storage.data_dir_key_scheme_version().await, 0);

    let todo: Value = storage.load("todo", "items").await.unwrap();
    let titles: Vec<&str> = todo
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["foo task", "bar subtask", "baz task"]);
    assert_eq!(todo[1]["parent_id"], "0000000000000001");

    let secrets: Value = storage.load("secrets", "entries").await.unwrap();
    assert_eq!(secrets[0]["name"], "foo-api");
    assert_eq!(secrets[0]["value"], "bar-secret-value");
    assert_eq!(secrets[1]["totp_seed"], "JBSWY3DPEHPK3PXP");

    let bookmarks: Value = storage
        .load("bookmark", "bookmarks_encrypted")
        .await
        .unwrap();
    let bookmarks = bookmarks.as_array().unwrap();
    assert_eq!(bookmarks.len(), 3);
    assert_eq!(bookmarks[0]["url"], "https://example.com/foo");
    assert_eq!(
        bookmarks[2]["description"].as_str().unwrap().len(),
        60 * 1024
    );

    let feeds: Value = storage.load("rss", "feeds").await.unwrap();
    assert_eq!(feeds[0]["url"], "https://example.com/feed.xml");
    let items: Value = storage.load("rss", "items").await.unwrap();
    assert_eq!(items[0]["title"], "Foo item 1");
}

#[tokio::test]
async fn v0_2_7_wrong_password_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let datadir = tmp.path().join("datadir");
    copy_dir(&fixture_root().join("datadir"), &datadir);
    let security = SecurityManager::new_with_root(datadir);
    assert!(
        security
            .unlock_vault(Zeroizing::new("not-the-password".into()))
            .await
            .is_err()
    );
}

/// K7b — a real `SecurityManager::encrypt` output (under the K1 key) from
/// 0.2.7 still decrypts.
#[tokio::test]
async fn k7b_legacy_gcm_fixture_decrypts() {
    let blob = std::fs::read(fixture_root().join("blobs/legacy-gcm.bin")).unwrap();
    let key = SecurityManager::derive_key("correct horse", &[0x01; 16])
        .await
        .unwrap();
    assert_eq!(
        SecurityManager::decrypt(&blob, &key).unwrap(),
        b"foo-legacy-plaintext"
    );
}

/// The fixtures are frozen: every file must match `SHA256SUMS`, and no file
/// may be added or removed. If this fails, the committed fixtures were
/// edited or regenerated, which defeats their purpose. Restore them from
/// git instead.
#[test]
fn v0_2_7_fixtures_are_byte_for_byte_frozen() {
    use sha2::{Digest, Sha256};

    let root = fixture_root();
    let manifest = std::fs::read_to_string(root.join("SHA256SUMS")).unwrap();
    let mut expected: Vec<(String, String)> = manifest
        .lines()
        .map(|line| {
            let (hash, path) = line.split_once(" *").expect("`<hash> *<path>` line");
            (path.to_string(), hash.to_string())
        })
        .collect();
    expected.sort();

    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                if rel != "SHA256SUMS" && rel != "README.md" {
                    out.push(rel);
                }
            }
        }
    }
    let mut actual_files = Vec::new();
    walk(&root, &root, &mut actual_files);
    actual_files.sort();
    let expected_files: Vec<&String> = expected.iter().map(|(p, _)| p).collect();
    assert_eq!(actual_files.iter().collect::<Vec<_>>(), expected_files);

    for (path, hash) in &expected {
        let bytes = std::fs::read(root.join(path)).unwrap();
        let actual: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(&actual, hash, "{path} differs from SHA256SUMS");
    }
}
