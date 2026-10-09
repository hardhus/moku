use super::*;
use moku_vault::{SecurityManager, VaultSession};
use tempfile::tempdir;

fn locked_session() -> Arc<VaultSession> {
    Arc::new(VaultSession::new())
}

async fn unlocked_session() -> Arc<VaultSession> {
    let session = VaultSession::new();
    let key = SecurityManager::derive_key("test_pass", &[7u8; 16])
        .await
        .unwrap();
    session.unlock(key);
    Arc::new(session)
}

#[tokio::test]
async fn test_has_encrypted_records_ignores_plaintext_and_finds_encrypted() {
    let temp = tempdir().unwrap();
    let session = unlocked_session().await;
    let manager = StorageManager::new_with_root(Arc::clone(&session), temp.path().to_path_buf())
        .await
        .unwrap();
    assert!(!manager.has_encrypted_records().await.unwrap());
    manager.save("rss", "feeds", &vec!["x".to_string()], false).await.unwrap();
    assert!(!manager.has_encrypted_records().await.unwrap());
    manager.save("todo", "items", &vec!["y".to_string()], true).await.unwrap();
    assert!(manager.has_encrypted_records().await.unwrap());
}

#[tokio::test]
async fn test_load_opt_missing_key_is_none() {
    let temp = tempdir().unwrap();
    let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
        .await
        .unwrap();
    let loaded: Option<Vec<String>> = manager.load_opt("mod1", "items").await.unwrap();
    assert!(loaded.is_none());
}

#[tokio::test]
async fn test_load_opt_returns_stored_value() {
    let temp = tempdir().unwrap();
    let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
        .await
        .unwrap();
    manager
        .save("mod1", "items", &vec!["a".to_string()], false)
        .await
        .unwrap();
    let loaded: Option<Vec<String>> = manager.load_opt("mod1", "items").await.unwrap();
    assert_eq!(loaded, Some(vec!["a".to_string()]));
}

#[tokio::test]
async fn test_load_opt_corrupt_envelope_is_an_error_not_none() {
    let temp = tempdir().unwrap();
    let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
        .await
        .unwrap();
    let db = manager.get_or_open_db("mod1").await.unwrap();
    db.insert("items", b"not an envelope".to_vec()).unwrap();
    db.flush().unwrap();
    let loaded: Result<Option<Vec<String>>> = manager.load_opt("mod1", "items").await;
    assert!(matches!(loaded, Err(StoreError::Corrupt { .. })), "got {loaded:?}");
}

#[tokio::test]
async fn test_load_opt_locked_vault_is_an_error_not_none() {
    let temp = tempdir().unwrap();
    let session = unlocked_session().await;
    let manager =
        StorageManager::new_with_root(Arc::clone(&session), temp.path().to_path_buf())
            .await
            .unwrap();
    manager
        .save("mod1", "items", &vec!["secret".to_string()], true)
        .await
        .unwrap();
    session.lock();
    let loaded: Result<Option<Vec<String>>> = manager.load_opt("mod1", "items").await;
    assert!(matches!(loaded, Err(StoreError::Locked)), "got {loaded:?}");
}

#[tokio::test]
async fn test_load_opt_shape_mismatch_is_an_error_not_none() {
    let temp = tempdir().unwrap();
    let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
        .await
        .unwrap();
    manager.save("mod1", "items", &42u32, false).await.unwrap();
    let loaded: Result<Option<Vec<String>>> = manager.load_opt("mod1", "items").await;
    assert!(matches!(loaded, Err(StoreError::SchemaMismatch { .. })), "got {loaded:?}");
}

#[tokio::test]
async fn test_load_opt_under_a_different_key_is_tampered_not_none() {
    let temp = tempdir().unwrap();
    let session = unlocked_session().await;
    let manager =
        StorageManager::new_with_root(Arc::clone(&session), temp.path().to_path_buf())
            .await
            .unwrap();
    manager
        .save("mod1", "items", &vec!["secret".to_string()], true)
        .await
        .unwrap();
    // Same data dir, a different vault key (as after a wrong-key unlock).
    session.lock();
    session.unlock(SecurityManager::derive_key("other", &[8u8; 16]).await.unwrap());
    let loaded: Result<Option<Vec<String>>> = manager.load_opt("mod1", "items").await;
    assert!(matches!(loaded, Err(StoreError::Tampered { .. })), "got {loaded:?}");
}

#[tokio::test]
async fn test_db_held_by_another_handle_is_in_use_by_other_process() {
    let temp = tempdir().unwrap();
    let first = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
        .await
        .unwrap();
    first.save("mod1", "k", &1u32, false).await.unwrap();
    // A second manager on the same data dir can't open the DB the first
    // one still holds (sled's exclusive file lock, as across processes).
    let second = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
        .await
        .unwrap();
    let loaded: Result<Option<u32>> = second.load_opt("mod1", "k").await;
    assert!(
        matches!(loaded, Err(StoreError::InUseByOtherProcess { .. })),
        "got {loaded:?}"
    );
}

#[tokio::test]
async fn test_db_cache_mechanism() {
    let temp = tempdir().unwrap();
    let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
        .await
        .unwrap();

    let _db1 = manager
        .get_or_open_db("test_mod")
        .await
        .expect("First open failed");
    let _db2 = manager
        .get_or_open_db("test_mod")
        .await
        .expect("Second open failed");

    assert_eq!(
        manager.db_cache.read().await.len(),
        1,
        "Cache must contain exactly 1 DB instance"
    );
}

#[tokio::test]
async fn test_storage_envelope_logic() {
    let temp = tempdir().unwrap();
    let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
        .await
        .unwrap();

    let small_data = "Moku".to_string();
    manager
        .save("mod1", "key1", &small_data, false)
        .await
        .unwrap();
    let loaded: String = manager.load("mod1", "key1").await.unwrap();
    assert_eq!(small_data, loaded);

    let large_data = "A".repeat(EXTERNAL_STORAGE_THRESHOLD + 1024);
    manager
        .save("mod1", "large_key", &large_data, false)
        .await
        .unwrap();
    let loaded_large: String = manager.load("mod1", "large_key").await.unwrap();
    assert_eq!(large_data, loaded_large);
}

#[tokio::test]
async fn test_encryption_error_without_unlocked_session() {
    let temp = tempdir().unwrap();
    let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
        .await
        .unwrap();

    let data = "secret".to_string();
    let result = manager.save("mod1", "key1", &data, true).await;

    assert!(matches!(result, Err(StoreError::Locked)), "got {result:?}");
}

#[tokio::test]
async fn test_encrypted_roundtrip_with_unlocked_session() {
    let temp = tempdir().unwrap();
    let manager =
        StorageManager::new_with_root(unlocked_session().await, temp.path().to_path_buf())
            .await
            .unwrap();

    let data = "secret".to_string();
    manager.save("mod1", "key1", &data, true).await.unwrap();
    let loaded: String = manager.load("mod1", "key1").await.unwrap();
    assert_eq!(data, loaded);
}

/// REGRESSION TEST: Verifies vault can be unlocked mid-lifetime via `VaultSession`
/// without recreating the `StorageManager` or wiping the internal database cache.
#[tokio::test]
async fn test_session_unlock_mid_lifetime_without_recreating_manager() {
    let temp = tempdir().unwrap();
    let session = Arc::new(VaultSession::new());
    let manager =
        StorageManager::new_with_root(Arc::clone(&session), temp.path().to_path_buf())
            .await
            .unwrap();

    let data = "secret".to_string();
    assert!(manager.save("mod1", "key1", &data, true).await.is_err());

    let key = SecurityManager::derive_key("test_pass", &[3u8; 16])
        .await
        .unwrap();
    session.unlock(key);

    manager.save("mod1", "key1", &data, true).await.unwrap();
    let loaded: String = manager.load("mod1", "key1").await.unwrap();
    assert_eq!(data, loaded);
}

#[tokio::test]
async fn test_concurrent_access() {
    let temp = tempdir().unwrap();
    let manager = Arc::new(
        StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
            .await
            .unwrap(),
    );

    let mut handles = vec![];
    for i in 0..10 {
        let m = Arc::clone(&manager);
        handles.push(tokio::spawn(async move {
            m.save("shared_mod", &format!("key_{}", i), &i, false).await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }

    for i in 0..10 {
        let val: i32 = manager
            .load("shared_mod", &format!("key_{}", i))
            .await
            .unwrap();
        assert_eq!(val, i);
    }
}

#[tokio::test]
async fn test_storage_atomic_integrity_simulation() {
    let temp = tempdir().unwrap();
    let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
        .await
        .unwrap();

    let initial_data = "Original Safe Data".to_string();
    manager
        .save("atomic_test", "key1", &initial_data, false)
        .await
        .unwrap();

    let corrupted_attempt = "New Corrupted Data".to_string();
    let result = manager
        .save("atomic_test", "key1", &corrupted_attempt, true)
        .await;
    assert!(result.is_err());

    let final_check: String = manager.load("atomic_test", "key1").await.unwrap();
    assert_eq!(final_check, initial_data);
}

#[tokio::test]
async fn test_migrate_plaintext_to_encrypted() {
    let temp = tempdir().unwrap();
    let manager =
        StorageManager::new_with_root(unlocked_session().await, temp.path().to_path_buf())
            .await
            .unwrap();

    manager
        .save("mod1", "a", &"one".to_string(), false)
        .await
        .unwrap();
    manager
        .save("mod1", "b", &"two".to_string(), false)
        .await
        .unwrap();

    let report = manager
        .migrate_module_encryption("mod1", true)
        .await
        .unwrap();
    assert_eq!(report.migrated, 2);
    assert_eq!(report.skipped, 0);
    assert!(report.errors.is_empty());

    // Data survives the round trip and reads back as still-encrypted.
    let a: String = manager.load("mod1", "a").await.unwrap();
    assert_eq!(a, "one");
}

#[tokio::test]
async fn test_migrate_requires_unlocked_vault_for_encrypted_target() {
    let temp = tempdir().unwrap();
    let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
        .await
        .unwrap();

    manager
        .save("mod1", "a", &"one".to_string(), false)
        .await
        .unwrap();

    let result = manager.migrate_module_encryption("mod1", true).await;
    assert!(matches!(result, Err(StoreError::Locked)), "got {result:?}");
}

#[tokio::test]
async fn test_migrate_is_idempotent() {
    let temp = tempdir().unwrap();
    let manager =
        StorageManager::new_with_root(unlocked_session().await, temp.path().to_path_buf())
            .await
            .unwrap();

    manager
        .save("mod1", "a", &"one".to_string(), true)
        .await
        .unwrap();

    // Already encrypted, target is encrypted: nothing to do.
    let report = manager
        .migrate_module_encryption("mod1", true)
        .await
        .unwrap();
    assert_eq!(report.migrated, 0);
    assert_eq!(report.skipped, 1);
}

#[tokio::test]
async fn test_migrate_encrypted_to_plaintext_readable_after_relock() {
    let temp = tempdir().unwrap();
    let session = Arc::new(VaultSession::new());
    let key = SecurityManager::derive_key("test_pass", &[9u8; 16])
        .await
        .unwrap();
    session.unlock(key);
    let manager =
        StorageManager::new_with_root(Arc::clone(&session), temp.path().to_path_buf())
            .await
            .unwrap();

    manager
        .save("mod1", "a", &"one".to_string(), true)
        .await
        .unwrap();

    let report = manager
        .migrate_module_encryption("mod1", false)
        .await
        .unwrap();
    assert_eq!(report.migrated, 1);

    // Once migrated to plaintext, the record must be readable even
    // with the vault locked again — proving it's genuinely plaintext
    // on disk now, not still encrypted.
    session.lock();
    let a: String = manager.load("mod1", "a").await.unwrap();
    assert_eq!(a, "one");
}

/// Simulates a record written before per-module HKDF subkeys existed
/// (directly encrypted under the raw master key, `key_scheme: V0`, no
/// `save()` call involved) — `load()` must still transparently read
/// it correctly, with zero action required, forever.
#[tokio::test]
async fn test_v0_key_scheme_record_still_loads() {
    let temp = tempdir().unwrap();
    let session = unlocked_session().await;
    let manager =
        StorageManager::new_with_root(Arc::clone(&session), temp.path().to_path_buf())
            .await
            .unwrap();

    let master = session.current().unwrap();
    let raw_bytes = serde_json::to_vec(&"legacy value".to_string()).unwrap();
    let ciphertext = moku_crypto::encrypt(&raw_bytes, &master).unwrap();
    let envelope = StorageEnvelope {
        schema_version: 1,
        status: EncryptionStatus::Encrypted,
        storage_type: StorageType::Embedded,
        payload: ciphertext,
        hash: None,
        key_scheme: KeyScheme::V0,
    };
    let envelope_bytes = serde_json::to_vec(&envelope).unwrap();

    let db = manager.get_or_open_db("mod1").await.unwrap();
    db.insert("a", envelope_bytes).unwrap();
    db.flush().unwrap();

    let loaded: String = manager.load("mod1", "a").await.unwrap();
    assert_eq!(loaded, "legacy value");
}

/// Every new encrypted write must land on `CURRENT_KEY_SCHEME`, not an
/// older scheme.
#[tokio::test]
async fn test_save_always_writes_current_key_scheme() {
    let temp = tempdir().unwrap();
    let manager =
        StorageManager::new_with_root(unlocked_session().await, temp.path().to_path_buf())
            .await
            .unwrap();

    manager
        .save("mod1", "a", &"one".to_string(), true)
        .await
        .unwrap();

    let db = manager.get_or_open_db("mod1").await.unwrap();
    let raw = db.get("a").unwrap().unwrap();
    let envelope: StorageEnvelope = serde_json::from_slice(&raw).unwrap();
    assert_eq!(envelope.key_scheme, CURRENT_KEY_SCHEME);
}

/// Real domain separation: the per-module subkey differs from the raw
/// master key, and ciphertext encrypted under one cannot be decrypted
/// with the other (AEAD auth failure, not silent garbage).
#[tokio::test]
async fn test_per_module_key_differs_from_master_and_cannot_cross_decrypt() {
    let master = SecretBox::new(Box::new(SafeKey([5u8; 32])));
    let derived = derive_module_storage_key(&master, "mod1");
    assert_ne!(master.expose_secret().0, derived.expose_secret().0);

    let plaintext = b"hello".to_vec();
    let ciphertext = moku_crypto::encrypt(&plaintext, &master).unwrap();
    assert!(moku_crypto::decrypt(&ciphertext, &derived).is_err());
}

/// `migrate_module_encryption` is purely about the encrypted/plaintext
/// toggle now — it must NOT silently upgrade an already-`Encrypted`
/// record's key scheme as a side effect (that responsibility moved
/// entirely to `migrate_key_scheme_to_latest`, tested below).
#[tokio::test]
async fn test_migrate_module_encryption_no_longer_touches_key_scheme() {
    let temp = tempdir().unwrap();
    let session = unlocked_session().await;
    let manager =
        StorageManager::new_with_root(Arc::clone(&session), temp.path().to_path_buf())
            .await
            .unwrap();

    let master = session.current().unwrap();
    let raw_bytes = serde_json::to_vec(&"legacy value".to_string()).unwrap();
    let ciphertext = moku_crypto::encrypt(&raw_bytes, &master).unwrap();
    let envelope = StorageEnvelope {
        schema_version: 1,
        status: EncryptionStatus::Encrypted,
        storage_type: StorageType::Embedded,
        payload: ciphertext,
        hash: None,
        key_scheme: KeyScheme::V0,
    };
    let envelope_bytes = serde_json::to_vec(&envelope).unwrap();
    let db = manager.get_or_open_db("mod1").await.unwrap();
    db.insert("a", envelope_bytes).unwrap();
    db.flush().unwrap();

    let report = manager
        .migrate_module_encryption("mod1", true)
        .await
        .unwrap();
    assert_eq!(
        report.migrated, 0,
        "EncryptionStatus already matched target, nothing for this pass to do"
    );
    assert_eq!(report.skipped, 1);

    let db = manager.get_or_open_db("mod1").await.unwrap();
    let raw = db.get("a").unwrap().unwrap();
    let envelope: StorageEnvelope = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        envelope.key_scheme,
        KeyScheme::V0,
        "key scheme must be untouched by the encrypt/plaintext toggle"
    );
}

/// The actual key-scheme upgrade mechanism: `migrate_key_scheme_to_
/// latest` upgrades a `V0` `Encrypted` record straight to
/// `CURRENT_KEY_SCHEME`, preserves the data, and is idempotent.
#[tokio::test]
async fn test_migrate_key_scheme_to_latest_upgrades_v0_to_current() {
    let temp = tempdir().unwrap();
    let session = unlocked_session().await;
    let manager =
        StorageManager::new_with_root(Arc::clone(&session), temp.path().to_path_buf())
            .await
            .unwrap();

    let master = session.current().unwrap();
    let raw_bytes = serde_json::to_vec(&"legacy value".to_string()).unwrap();
    let ciphertext = moku_crypto::encrypt(&raw_bytes, &master).unwrap();
    let envelope = StorageEnvelope {
        schema_version: 1,
        status: EncryptionStatus::Encrypted,
        storage_type: StorageType::Embedded,
        payload: ciphertext,
        hash: None,
        key_scheme: KeyScheme::V0,
    };
    let envelope_bytes = serde_json::to_vec(&envelope).unwrap();
    let db = manager.get_or_open_db("mod1").await.unwrap();
    db.insert("a", envelope_bytes).unwrap();
    db.flush().unwrap();

    let report = manager.migrate_key_scheme_to_latest("mod1").await.unwrap();
    assert_eq!(report.migrated, 1);
    assert_eq!(report.skipped, 0);

    let db = manager.get_or_open_db("mod1").await.unwrap();
    let raw = db.get("a").unwrap().unwrap();
    let envelope: StorageEnvelope = serde_json::from_slice(&raw).unwrap();
    assert_eq!(envelope.key_scheme, CURRENT_KEY_SCHEME);

    let loaded: String = manager.load("mod1", "a").await.unwrap();
    assert_eq!(loaded, "legacy value");

    // Idempotent: running it again on already-upgraded data is a no-op.
    let report2 = manager.migrate_key_scheme_to_latest("mod1").await.unwrap();
    assert_eq!(report2.migrated, 0);
    assert_eq!(report2.skipped, 1);
}

#[tokio::test]
async fn test_migrate_key_scheme_to_latest_requires_unlocked_vault() {
    let temp = tempdir().unwrap();
    let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
        .await
        .unwrap();

    let result = manager.migrate_key_scheme_to_latest("mod1").await;
    assert!(matches!(result, Err(StoreError::Locked)), "got {result:?}");
}

#[tokio::test]
async fn test_migrate_key_scheme_to_latest_skips_plaintext_records() {
    let temp = tempdir().unwrap();
    let manager =
        StorageManager::new_with_root(unlocked_session().await, temp.path().to_path_buf())
            .await
            .unwrap();

    manager
        .save("mod1", "a", &"one".to_string(), false)
        .await
        .unwrap();

    let report = manager.migrate_key_scheme_to_latest("mod1").await.unwrap();
    assert_eq!(report.migrated, 0);
    assert_eq!(report.skipped, 1);
}

/// A brand-new data directory has no pre-existing records, so it
/// starts life already marked on the latest key scheme.
#[tokio::test]
async fn test_fresh_vault_root_marks_current_key_scheme() {
    let temp = tempdir().unwrap();
    let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
        .await
        .unwrap();

    assert_eq!(
        manager.data_dir_key_scheme_version().await,
        CURRENT_KEY_SCHEME.version()
    );
}

/// A `vault_root` that already existed before this marker file was
/// introduced has no marker to read — that must read as version `0`,
/// exactly `KeyScheme::default()`'s meaning.
#[tokio::test]
async fn test_missing_marker_reads_as_version_zero() {
    let temp = tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("vault")).unwrap();

    let manager = StorageManager::new_with_root(locked_session(), temp.path().to_path_buf())
        .await
        .unwrap();

    assert_eq!(manager.data_dir_key_scheme_version().await, 0);
}

/// `migrate_all_key_schemes` only advances the data directory's
/// marker once every listed module's migration finishes cleanly.
#[tokio::test]
async fn test_migrate_all_key_schemes_advances_marker_on_full_success() {
    let temp = tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("vault")).unwrap();
    let session = unlocked_session().await;
    let manager =
        StorageManager::new_with_root(Arc::clone(&session), temp.path().to_path_buf())
            .await
            .unwrap();
    assert_eq!(manager.data_dir_key_scheme_version().await, 0);

    let master = session.current().unwrap();
    let raw_bytes = serde_json::to_vec(&"legacy value".to_string()).unwrap();
    let ciphertext = moku_crypto::encrypt(&raw_bytes, &master).unwrap();
    let envelope = StorageEnvelope {
        schema_version: 1,
        status: EncryptionStatus::Encrypted,
        storage_type: StorageType::Embedded,
        payload: ciphertext,
        hash: None,
        key_scheme: KeyScheme::V0,
    };
    let envelope_bytes = serde_json::to_vec(&envelope).unwrap();
    let db = manager.get_or_open_db("mod1").await.unwrap();
    db.insert("a", envelope_bytes).unwrap();
    db.flush().unwrap();

    let reports = manager.migrate_all_key_schemes(&["mod1"]).await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].0, "mod1");
    assert_eq!(reports[0].1.migrated, 1);
    assert_eq!(
        manager.data_dir_key_scheme_version().await,
        CURRENT_KEY_SCHEME.version()
    );
}
