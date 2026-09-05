use std::sync::Arc;

use tempfile::tempdir;
use zeroize::Zeroizing;

use moku_core::StorageManager;
use moku_core::security::{SecurityManager, VaultSession};

#[tokio::test]
async fn test_end_to_end_vault_lifecycle() {
    let temp = tempdir().unwrap();
    let root = temp.path().to_path_buf();

    let password = Zeroizing::new("super_secret_password".to_string());
    let secret_content = "This data can only be read with the correct password.".to_string();

    let sm_init = SecurityManager::new_with_root(root.clone());
    assert!(!sm_init.is_vault_initialized());

    let master_key_box = sm_init
        .initialize_vault(password.clone())
        .await
        .expect("Failed to initialize vault");
    assert!(sm_init.is_vault_initialized());

    let session = Arc::new(VaultSession::new());
    session.unlock(master_key_box);

    let storage = StorageManager::new_with_root(Arc::clone(&session), root.clone())
        .await
        .unwrap();
    storage
        .save("secure_mod", "secret_key", &secret_content, true)
        .await
        .expect("Encrypted write failed");

    drop(storage);
    drop(session);
    drop(sm_init);

    let sm_reload = SecurityManager::new_with_root(root.clone());
    assert!(sm_reload.is_vault_initialized());

    let recovered_key_box = sm_reload
        .unlock_vault(password)
        .await
        .expect("Failed to unlock vault");

    let session_reload = Arc::new(VaultSession::new());
    session_reload.unlock(recovered_key_box);

    let storage_reload = StorageManager::new_with_root(session_reload, root.clone())
        .await
        .unwrap();
    let loaded_content: String = storage_reload
        .load("secure_mod", "secret_key")
        .await
        .expect("Failed to read data");

    assert_eq!(
        secret_content, loaded_content,
        "Vault lifecycle corrupted data or key mismatch!"
    );
}

#[tokio::test]
async fn test_rekeying_simulation_proval() {
    let temp = tempdir().unwrap();
    let root = temp.path().to_path_buf();

    let old_pass = Zeroizing::new("old_123".to_string());
    let new_pass = Zeroizing::new("new_456".to_string());
    let sensitive_data = vec![1, 2, 3, 4, 5];

    let sm = SecurityManager::new_with_root(root.clone());
    let old_key_box = sm.initialize_vault(old_pass).await.unwrap();

    let old_session = Arc::new(VaultSession::new());
    old_session.unlock(old_key_box);

    let storage = StorageManager::new_with_root(Arc::clone(&old_session), root.clone())
        .await
        .unwrap();
    storage
        .save("wallet", "seed", &sensitive_data, true)
        .await
        .unwrap();

    let meta_path = root.join("vault/meta.json");
    tokio::fs::remove_file(meta_path).await.unwrap();

    let new_key_box = sm.initialize_vault(new_pass).await.unwrap();
    let new_session = Arc::new(VaultSession::new());
    new_session.unlock(new_key_box);

    let storage_new = StorageManager::new_with_root(new_session, root.clone())
        .await
        .unwrap();
    let result: Result<Vec<i32>, _> = storage_new.load("wallet", "seed").await;

    assert!(
        result.is_err(),
        "Reading encrypted data with a different key should return an error"
    );
}
