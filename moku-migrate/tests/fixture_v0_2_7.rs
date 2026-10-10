//! Hop 0 → 1 against the frozen moku-bin 0.2.7 data directory
//! (`test-fixtures/v0.2.7/`, copied to a temp dir first) and against
//! synthetic format-0 states the fixture doesn't contain.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use moku_crypto::{Key, RootKey, format_v1, module_storage_key};
use moku_migrate::{DataFormat, DataState, MigrateError, detect_format, read_format, upgrade};
use moku_store::{
    CURRENT_SCHEMA_VERSION, EncryptionStatus, KeyScheme, StorageEnvelope, StorageManager,
    StorageType,
};
use moku_vault::{SecurityManager, VaultSession};
use serde_json::{Value, json};

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

/// Every file under `dir`, as (relative path, bytes), sorted.
fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn raw_envelope(datadir: &Path, module: &str, key: &str) -> Value {
    let db = sled::open(datadir.join("vault").join(module).join("db")).unwrap();
    let value = serde_json::from_slice(&db.get(key).unwrap().expect("record present")).unwrap();
    drop(db);
    value
}

async fn unlocked_fixture() -> (tempfile::TempDir, PathBuf, Arc<VaultSession>) {
    let tmp = tempfile::tempdir().unwrap();
    let datadir = tmp.path().join("datadir");
    copy_dir(&fixture_root().join("datadir"), &datadir);
    let key = SecurityManager::new_with_root(datadir.clone())
        .unlock_vault(moku_crypto::SecretString::from(VAULT_PASSWORD))
        .await
        .expect("fixture vault unlocks with its test password");
    let session = Arc::new(VaultSession::new());
    session.unlock(key);
    (tmp, datadir, session)
}

#[tokio::test]
async fn fixture_upgrades_to_format_1_and_every_record_still_loads() {
    let (_tmp, datadir, session) = unlocked_fixture().await;
    let original_vault = snapshot(&datadir.join("vault"));
    assert_eq!(
        detect_format(&datadir).unwrap(),
        DataState::Behind {
            from: DataFormat(0),
            stamped: false
        }
    );

    let report = upgrade(&datadir, &session, "test", &|_| {}).unwrap();
    assert_eq!(report.from, Some(DataFormat(0)));
    assert_eq!(report.to, Some(DataFormat(1)));
    let (_, hop) = &report.hops[0];
    assert_eq!(
        hop.per_module,
        [
            ("bookmark".to_string(), 0),
            ("rss".to_string(), 0),
            ("secrets".to_string(), 1),
            ("todo".to_string(), 1),
        ]
    );
    assert_eq!((hop.migrated, hop.skipped), (2, 3));

    assert_eq!(read_format(&datadir).unwrap().unwrap().data_format, 1);
    assert_eq!(
        std::fs::read_to_string(datadir.join("vault/.key_scheme_version")).unwrap(),
        "1"
    );
    for (module, key, scheme) in [
        ("todo", "items", "PerModuleV1"),
        ("secrets", "entries", "PerModuleV1"),
        ("bookmark", "bookmarks_encrypted", "PerModuleV1"),
        // Plaintext records keep their tag (the runtime reads it as "no key").
        ("rss", "feeds", "Legacy"),
        ("rss", "items", "Legacy"),
    ] {
        assert_eq!(
            raw_envelope(&datadir, module, key)["key_scheme"],
            scheme,
            "{module}/{key}"
        );
    }

    // The backup is the untouched original vault/, byte for byte.
    let backup = report.backup.clone().expect("a backup was taken");
    assert!(backup.starts_with(datadir.join("backups")));
    assert_eq!(snapshot(&backup.join("vault")), original_vault);

    // A second run has nothing to do.
    let again = upgrade(&datadir, &session, "test", &|_| {}).unwrap();
    assert!(again.hops.is_empty());
    assert_eq!(moku_migrate::list_backups(&datadir).unwrap().len(), 1);

    // Today's store reads every record, with the same values as before.
    let storage = StorageManager::new_with_root(Arc::clone(&session), datadir.clone())
        .await
        .unwrap();
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
    assert_eq!(bookmarks.as_array().unwrap().len(), 3);
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
async fn upgrade_needs_the_unlocked_vault() {
    let tmp = tempfile::tempdir().unwrap();
    let datadir = tmp.path().join("datadir");
    copy_dir(&fixture_root().join("datadir"), &datadir);
    let err = upgrade(&datadir, &VaultSession::new(), "test", &|_| {}).unwrap_err();
    assert!(matches!(err, MigrateError::Locked), "{err}");
    assert!(read_format(&datadir).unwrap().is_none());
    assert!(!datadir.join("backups").exists());
}

#[tokio::test]
async fn a_module_db_held_open_fails_the_hop_and_keeps_the_format() {
    let (_tmp, datadir, session) = unlocked_fixture().await;
    let held = sled::open(datadir.join("vault/todo/db")).unwrap();
    let err = upgrade(&datadir, &session, "test", &|_| {}).unwrap_err();
    assert!(
        matches!(err, MigrateError::InUse { ref module, .. } if module == "todo"),
        "{err}"
    );
    drop(held);
    assert!(read_format(&datadir).unwrap().is_none());
    assert!(
        !datadir.join("backups").exists(),
        "refused before the backup"
    );

    // Once the other handle is gone, the same upgrade finishes.
    upgrade(&datadir, &session, "test", &|_| {}).unwrap();
    assert_eq!(read_format(&datadir).unwrap().unwrap().data_format, 1);
}

// --- synthetic format-0 states -------------------------------------------

async fn test_key() -> Key<RootKey> {
    SecurityManager::derive_key(&"synthetic".into(), &[0x07; 16])
        .await
        .unwrap()
}

/// Writes one Encrypted record the way 0.2.x did: sealed under `cipher_key`
/// (raw bytes), tagged `scheme`, Embedded or External.
fn put_record(
    datadir: &Path,
    module: &str,
    key: &str,
    ciphertext: Vec<u8>,
    external: bool,
    scheme: KeyScheme,
) {
    let module_dir = datadir.join("vault").join(module);
    std::fs::create_dir_all(module_dir.join("blobs")).unwrap();
    let envelope = StorageEnvelope {
        schema_version: CURRENT_SCHEMA_VERSION,
        status: EncryptionStatus::Encrypted,
        storage_type: if external {
            StorageType::External
        } else {
            StorageType::Embedded
        },
        payload: if external {
            Vec::new()
        } else {
            ciphertext.clone()
        },
        hash: None,
        key_scheme: scheme,
    };
    if external {
        std::fs::write(
            module_dir
                .join("blobs")
                .join(format!("{module}_{key}.blob")),
            &ciphertext,
        )
        .unwrap();
    }
    let db = sled::open(module_dir.join("db")).unwrap();
    db.insert(key, serde_json::to_vec(&envelope).unwrap())
        .unwrap();
    db.flush().unwrap();
}

async fn load(datadir: &Path, session: &Arc<VaultSession>, module: &str, key: &str) -> Value {
    let storage = StorageManager::new_with_root(Arc::clone(session), datadir.to_path_buf())
        .await
        .unwrap();
    let value = storage.load(module, key).await.unwrap();
    storage.close_all_dbs().await;
    value
}

#[tokio::test]
async fn an_external_v0_record_is_migrated() {
    let tmp = tempfile::tempdir().unwrap();
    let datadir = tmp.path().to_path_buf();
    let root = test_key().await;
    let plaintext = serde_json::to_vec(&json!({"big": "x".repeat(70 * 1024)})).unwrap();
    put_record(
        &datadir,
        "foo",
        "bar",
        format_v1::seal_gcm_no_aad(&root, &plaintext).unwrap(),
        true,
        KeyScheme::V0,
    );
    let blob = datadir.join("vault/foo/blobs/foo_bar.blob");
    let old_blob = std::fs::read(&blob).unwrap();

    let session = Arc::new(VaultSession::new());
    session.unlock(root);
    let report = upgrade(&datadir, &session, "test", &|_| {}).unwrap();
    assert_eq!(report.hops[0].1.per_module, [("foo".to_string(), 1)]);
    assert_ne!(
        std::fs::read(&blob).unwrap(),
        old_blob,
        "the blob was re-encrypted"
    );
    let env = raw_envelope(&datadir, "foo", "bar");
    assert_eq!(env["key_scheme"], "PerModuleV1");
    assert_eq!(env["storage_type"], "External");

    assert_eq!(
        load(&datadir, &session, "foo", "bar").await["big"]
            .as_str()
            .unwrap()
            .len(),
        70 * 1024
    );
}

/// A crash after the blob was rewritten but before the envelope was: the
/// envelope still says Legacy, the blob is already under the module key.
#[tokio::test]
async fn an_interrupted_external_rewrite_is_recovered() {
    let tmp = tempfile::tempdir().unwrap();
    let datadir = tmp.path().to_path_buf();
    let root = test_key().await;
    let module_key = module_storage_key(&root, "foo");
    let plaintext = serde_json::to_vec(&json!({"n": 42})).unwrap();
    put_record(
        &datadir,
        "foo",
        "bar",
        format_v1::seal_gcm_no_aad(&module_key, &plaintext).unwrap(),
        true,
        KeyScheme::V0,
    );

    let session = Arc::new(VaultSession::new());
    session.unlock(root);
    let report = upgrade(&datadir, &session, "test", &|_| {}).unwrap();
    assert_eq!(report.hops[0].1.migrated, 1);
    assert_eq!(
        raw_envelope(&datadir, "foo", "bar")["key_scheme"],
        "PerModuleV1"
    );
    assert_eq!(load(&datadir, &session, "foo", "bar").await["n"], 42);
}

#[tokio::test]
async fn an_unreadable_record_fails_the_hop_but_the_rest_is_converted() {
    let tmp = tempfile::tempdir().unwrap();
    let datadir = tmp.path().to_path_buf();
    let root = test_key().await;
    let good = serde_json::to_vec(&json!(["ok"])).unwrap();
    put_record(
        &datadir,
        "aaa",
        "good",
        format_v1::seal_gcm_no_aad(&root, &good).unwrap(),
        false,
        KeyScheme::V0,
    );
    put_record(&datadir, "bbb", "bad", vec![0u8; 64], false, KeyScheme::V0);

    let session = Arc::new(VaultSession::new());
    session.unlock(root);
    let err = upgrade(&datadir, &session, "test", &|_| {}).unwrap_err();
    let MigrateError::HopFailed { source, .. } = &err else {
        panic!("unexpected error: {err}");
    };
    let MigrateError::Records { failures } = &**source else {
        panic!("unexpected error: {source}");
    };
    assert_eq!(failures.len(), 1);
    assert_eq!(
        (failures[0].module.as_str(), failures[0].key.as_str()),
        ("bbb", "bad")
    );
    assert!(
        read_format(&datadir).unwrap().is_none(),
        "the marker did not advance"
    );
    assert_eq!(
        raw_envelope(&datadir, "aaa", "good")["key_scheme"],
        "PerModuleV1"
    );
    assert_eq!(raw_envelope(&datadir, "bbb", "bad")["key_scheme"], "Legacy");
}
