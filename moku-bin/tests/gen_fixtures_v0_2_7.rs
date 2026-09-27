//! One-shot generator for `test-fixtures/v0.2.7/`.
//!
//! This file is a historical record of HOW the committed fixtures were
//! produced by the moku-bin 0.2.7 code. It is `#[ignore]`d and must never be
//! re-run to "refresh" them. The fixtures' whole purpose is to preserve
//! bytes written by the old code, so that later format migrations (audit
//! checklist P4–P6) can be tested against real old-format data. The
//! generator refuses to run if the output directory already exists.
//!
//! Everything here is synthetic test data only: foo/bar/example.com values,
//! fixed test passwords, fixed timestamps. It is built in a temporary
//! directory and never touches the real data dir, the global volume index,
//! or `moku_dev`.
//!
//! Run (once, already done for v0.2.7):
//!   cargo test -p moku-bin --test gen_fixtures_v0_2_7 -- --ignored --nocapture

use std::path::{Path, PathBuf};
use std::sync::Arc;

use moku_bookmark::model::Bookmark;
use moku_core::StorageManager;
use moku_core::security::{SecurityManager, VaultSession};
use moku_core::storage::{
    CURRENT_SCHEMA_VERSION, EncryptionStatus, KeyScheme, StorageEnvelope, StorageType,
};
use moku_rss::engine::{FeedItem, FeedSubscription};
use moku_secrets::model::SecretEntry;
use moku_todo::model::Task;
use moku_volume_daemon::PasswordMode;
use moku_volume_daemon::registry::{VolumeConfig, derive_default_volume_master_key};
use moku_volume_fs::{VirtualPath, VolumeEngine, derive_volume_keys};
use secrecy::SecretBox;
use zeroize::Zeroizing;

pub const VAULT_PASSWORD: &str = "foo-test-password";
pub const CUSTOM_VOLUME_PASSWORD: &str = "bar-volume-password";
pub const EXPORT_PASSWORD: &str = "foo-export-password";
const TS: u64 = 1_700_000_000;

fn out_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("test-fixtures")
        .join("v0.2.7")
}

fn tasks() -> Vec<Task> {
    vec![
        Task {
            id: "0000000000000001".into(),
            title: "foo task".into(),
            completed: false,
            parent_id: None,
            created_at: TS,
            updated_at: TS,
        },
        Task {
            id: "0000000000000002".into(),
            title: "bar subtask".into(),
            completed: true,
            parent_id: Some("0000000000000001".into()),
            created_at: TS + 1,
            updated_at: TS + 2,
        },
        Task {
            id: "0000000000000003".into(),
            title: "baz task".into(),
            completed: false,
            parent_id: None,
            created_at: TS + 3,
            updated_at: TS + 3,
        },
    ]
}

/// Big enough (with a long synthetic description) that the encrypted
/// envelope crosses the 50 KiB External-blob threshold.
fn bookmarks() -> Vec<Bookmark> {
    let mut out = vec![
        Bookmark {
            url: "https://example.com/foo".into(),
            domain: "example.com".into(),
            name: Some("Foo".into()),
            description: Some("foo bookmark".into()),
            tags: vec!["foo".into()],
            created_at: TS,
        },
        Bookmark {
            url: "https://example.org/bar".into(),
            domain: "example.org".into(),
            name: Some("Bar".into()),
            description: None,
            tags: vec!["bar".into(), "baz".into()],
            created_at: TS + 1,
        },
    ];
    out.push(Bookmark {
        url: "https://example.net/filler".into(),
        domain: "example.net".into(),
        name: Some("Filler".into()),
        description: Some("A".repeat(60 * 1024)),
        tags: vec![],
        created_at: TS + 2,
    });
    out
}

fn secret(id: &str, name: &str, value: &str, totp: Option<&str>) -> SecretEntry {
    SecretEntry {
        id: id.into(),
        name: name.into(),
        value: Zeroizing::new(value.into()),
        category: Some("foo".into()),
        username: Some("foo-user".into()),
        url: Some("https://example.com".into()),
        totp_seed: totp.map(Into::into),
        notes: None,
        created_at: TS,
        updated_at: TS,
    }
}

fn secrets() -> Vec<SecretEntry> {
    vec![
        secret("00000000000000a1", "foo-api", "bar-secret-value", None),
        secret(
            "00000000000000a2",
            "bar-login",
            "baz-secret-value",
            Some("JBSWY3DPEHPK3PXP"),
        ),
    ]
}

fn feeds() -> Vec<FeedSubscription> {
    vec![FeedSubscription {
        url: "https://example.com/feed.xml".into(),
        title: Some("Foo Feed".into()),
        favorite: true,
    }]
}

fn feed_items() -> Vec<FeedItem> {
    vec![FeedItem {
        id: "https://example.com/foo-item-1".into(),
        feed_title: "Foo Feed".into(),
        title: "Foo item 1".into(),
        link: "https://example.com/foo-item-1".into(),
        published_at: TS,
        summary: Some("bar summary".into()),
    }]
}

/// Writes a pre-0.2.0-style record: encrypted directly with the raw vault
/// master key (`KeyScheme::V0`), Embedded, straight into the module's sled
/// DB. `StorageManager::save` can no longer produce these.
fn write_v0_record(
    root: &Path,
    module: &str,
    key: &str,
    master: &SecretBox<moku_core::SafeKey>,
    value: &serde_json::Value,
) {
    let raw = serde_json::to_vec(value).unwrap();
    let envelope = StorageEnvelope {
        schema_version: CURRENT_SCHEMA_VERSION,
        status: EncryptionStatus::Encrypted,
        storage_type: StorageType::Embedded,
        payload: SecurityManager::encrypt(&raw, master).unwrap(),
        hash: None,
        key_scheme: KeyScheme::V0,
    };
    let db = sled::open(root.join("vault").join(module).join("db")).unwrap();
    db.insert(key, serde_json::to_vec(&envelope).unwrap())
        .unwrap();
    db.flush().unwrap();
}

fn write_volume_tree(engine: &VolumeEngine) {
    let root = VirtualPath::root();
    engine.mkdir(&root, "docs").unwrap();
    let docs = VirtualPath::parse("/docs");

    let files: [(&VirtualPath, &str, Vec<u8>); 3] = [
        (&docs, "foo.md", b"# Foo\n\nbar baz\n".to_vec()),
        (&root, "bar.txt", b"bar\n".to_vec()),
        // > 4 KiB so it spans more than one encrypted block.
        (
            &docs,
            "big.bin",
            (0..10_000u32).map(|i| (i % 251) as u8).collect(),
        ),
    ];
    for (parent, name, data) in files {
        let (fh, _) = engine.create(parent, name).unwrap();
        engine.write(fh, 0, &data).unwrap();
        engine.release(fh).unwrap();
    }
    engine.flush_usage().unwrap();
}

async fn write_volume_config(dir: &Path, cfg: &VolumeConfig) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("volume.json"),
        serde_json::to_string_pretty(cfg).unwrap(),
    )
    .unwrap();
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

#[tokio::test]
#[ignore = "one-shot fixture generator; never re-run (see file header)"]
async fn generate_v0_2_7_fixtures() {
    let out = out_dir();
    assert!(
        !out.exists(),
        "{} already exists; these fixtures are frozen and must not be regenerated",
        out.display()
    );

    let tmp = tempfile::tempdir().unwrap();
    let stage = tmp.path();

    // --- datadir: a mixed pre-0.2.1 install -----------------------------
    // Realistic "old user" state: no .key_scheme_version marker yet, some
    // modules still on KeyScheme::V0 (raw master key), others already
    // rewritten under V1 by a newer binary, one External blob, and RSS
    // stored as plaintext (its default).
    let datadir = stage.join("datadir");
    let security = SecurityManager::new_with_root(datadir.clone());
    let master = security
        .initialize_vault(Zeroizing::new(VAULT_PASSWORD.into()))
        .await
        .unwrap();

    write_v0_record(
        &datadir,
        "todo",
        "items",
        &master,
        &serde_json::to_value(tasks()).unwrap(),
    );
    write_v0_record(
        &datadir,
        "secrets",
        "entries",
        &master,
        &serde_json::to_value(secrets()).unwrap(),
    );

    let session = Arc::new(VaultSession::new());
    session.unlock(
        security
            .unlock_vault(Zeroizing::new(VAULT_PASSWORD.into()))
            .await
            .unwrap(),
    );
    let storage = StorageManager::new_with_root(session.clone(), datadir.clone())
        .await
        .unwrap();
    storage
        .save("bookmark", "bookmarks_encrypted", &bookmarks(), true)
        .await
        .unwrap();
    storage.save("rss", "feeds", &feeds(), false).await.unwrap();
    storage
        .save("rss", "items", &feed_items(), false)
        .await
        .unwrap();
    for m in ["todo", "secrets", "bookmark", "rss"] {
        storage.close_db(m).await;
    }
    drop(storage);
    let marker = datadir.join("vault").join(".key_scheme_version");
    if marker.exists() {
        std::fs::remove_file(&marker).unwrap();
    }

    // --- volumes/default-foo: Default mode, key derived from the app vault --
    let default_dir = stage.join("volumes").join("default-foo");
    let default_cfg = VolumeConfig {
        id: "default-foo".into(),
        display_name: "Default Foo".into(),
        size_limit_bytes: 64 * 1024 * 1024,
        password_mode: PasswordMode::Default,
        created_at: TS,
    };
    write_volume_config(&default_dir, &default_cfg).await;
    let vm = derive_default_volume_master_key(&master, &default_cfg.id);
    let engine = VolumeEngine::open_volume(
        default_dir.join("data"),
        derive_volume_keys(&vm),
        default_dir.join("usage.json"),
        default_cfg.size_limit_bytes,
    )
    .unwrap();
    write_volume_tree(&engine);
    drop(engine);

    // --- volumes/custom-bar: Custom mode, its own vault/meta.json ----------
    let custom_dir = stage.join("volumes").join("custom-bar");
    let custom_cfg = VolumeConfig {
        id: "custom-bar".into(),
        display_name: "Custom Bar".into(),
        size_limit_bytes: 64 * 1024 * 1024,
        password_mode: PasswordMode::Custom,
        created_at: TS,
    };
    write_volume_config(&custom_dir, &custom_cfg).await;
    let custom_master = SecurityManager::new_with_root(custom_dir.clone())
        .initialize_vault(Zeroizing::new(CUSTOM_VOLUME_PASSWORD.into()))
        .await
        .unwrap();
    let engine = VolumeEngine::open_volume(
        custom_dir.join("data"),
        derive_volume_keys(&custom_master),
        custom_dir.join("usage.json"),
        custom_cfg.size_limit_bytes,
    )
    .unwrap();
    write_volume_tree(&engine);
    drop(engine);

    // --- msxp: secrets encrypted export v1 -----------------------------------
    let msxp = moku_secrets::engine::export_encrypted(&secrets(), EXPORT_PASSWORD)
        .await
        .unwrap();
    std::fs::create_dir_all(stage.join("msxp")).unwrap();
    std::fs::write(stage.join("msxp").join("export-v1.msxp"), msxp).unwrap();

    // --- blobs: a real SecurityManager::encrypt output under the K1 key ----
    let k1 = SecurityManager::derive_key("correct horse", &[0x01; 16])
        .await
        .unwrap();
    let legacy = SecurityManager::encrypt(b"foo-legacy-plaintext", &k1).unwrap();
    std::fs::create_dir_all(stage.join("blobs")).unwrap();
    std::fs::write(stage.join("blobs").join("legacy-gcm.bin"), legacy).unwrap();

    copy_dir(stage, &out);
    println!("fixtures written to {}", out.display());
}
