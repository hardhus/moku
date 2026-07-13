use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tempfile::tempdir;
use tokio::fs;

use moku_core::StorageManager;
use moku_core::security::VaultSession;

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
struct TestData {
    id: u32,
    payload: String,
}

fn plain_session() -> Arc<VaultSession> {
    Arc::new(VaultSession::new())
}

#[tokio::test]
async fn test_storage_concurrency_stress() {
    let temp = tempdir().unwrap();
    let manager = Arc::new(
        StorageManager::new_with_root(plain_session(), temp.path().to_path_buf())
            .await
            .unwrap(),
    );

    let mut tasks = vec![];
    for i in 0..20 {
        let m = Arc::clone(&manager);
        tasks.push(tokio::spawn(async move {
            let data = TestData {
                id: i,
                payload: format!("Data {}", i),
            };
            m.save("stress_mod", &format!("key_{}", i), &data, false)
                .await
        }));
    }
    for task in tasks {
        task.await.unwrap().expect("Concurrent save failed!");
    }

    for i in 0..20 {
        let loaded: TestData = manager
            .load("stress_mod", &format!("key_{}", i))
            .await
            .unwrap();
        assert_eq!(loaded.id, i);
    }
}

#[tokio::test]
async fn test_storage_corruption_handling() {
    let temp = tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let manager = StorageManager::new_with_root(plain_session(), root.clone())
        .await
        .unwrap();

    let large_data = "X".repeat(60 * 1024);
    manager
        .save("corrupt_mod", "target_key", &large_data, false)
        .await
        .unwrap();

    let blob_path = root.join("vault/corrupt_mod/blobs/corrupt_mod_target_key.blob");
    fs::remove_file(blob_path).await.unwrap();

    let result: Result<String, _> = manager.load("corrupt_mod", "target_key").await;
    assert!(result.is_err(), "Should return error for missing blob file");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("External blob file missing")
    );
}

#[tokio::test]
async fn test_db_isolation() {
    let temp = tempdir().unwrap();
    let manager = StorageManager::new_with_root(plain_session(), temp.path().to_path_buf())
        .await
        .unwrap();

    manager
        .save("mod_a", "common_key", &"Data A".to_string(), false)
        .await
        .unwrap();
    manager
        .save("mod_b", "common_key", &"Data B".to_string(), false)
        .await
        .unwrap();

    let res_a: String = manager.load("mod_a", "common_key").await.unwrap();
    let res_b: String = manager.load("mod_b", "common_key").await.unwrap();

    assert_ne!(res_a, res_b);
    assert_eq!(res_a, "Data A");
}
