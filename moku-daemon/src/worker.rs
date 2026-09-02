use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use arc_swap::ArcSwap;
use moku_core::{ConfigManager, DaemonContext, DaemonTask, StorageManager, VaultSession};

use crate::task_status::{TaskStatus, write_statuses};

/// Run the background service daemon.
pub async fn run_worker() -> Result<()> {
    let _guard = crate::logging::init()?;
    if crate::status::is_running() {
        anyhow::bail!("Daemon is already running.");
    }
    crate::pid::write()?;

    // Windows'ta AUMID/ikon kaydını daemon başlarken bir kez dene — hata
    // olsa bile devam eder, sadece log'a düşer. send() zaten ilk
    // bildirimde aynı işi tembel yapardı, bu sadece erken görünürlük için.
    moku_notify::ensure_registered();

    let data_dir = moku_core::dirs::get_data_dir()?;
    let loaded_config = ConfigManager::load().await.unwrap_or_default();
    let config = Arc::new(ArcSwap::from_pointee(loaded_config));

    // Daemon runs unencrypted: vault always remains locked. Tasks like RSS
    // must therefore call storage.save/load with is_encryption_enabled=false.
    let session = Arc::new(VaultSession::new());
    let storage = Arc::new(StorageManager::new(session).await?);
    let ctx = Arc::new(DaemonContext { config, storage });

    let statuses: Arc<tokio::sync::Mutex<Vec<TaskStatus>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));

    let tasks: Vec<Box<dyn DaemonTask>> = vec![Box::new(moku_rss::RssDaemonTask::new())];

    tracing::info!("Moku Daemon started, loading {} tasks", tasks.len());

    let mut handles = Vec::new();
    for mut task in tasks {
        let ctx = Arc::clone(&ctx);
        let statuses = Arc::clone(&statuses);
        let data_dir = data_dir.clone();
        let task_id = task.id().to_string();

        handles.push(tokio::spawn(async move {
            let mut timer = tokio::time::interval(task.interval());
            loop {
                timer.tick().await;

                let now_secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .ok();

                let result = task.tick(&ctx).await;

                let mut lock = statuses.lock().await;
                let entry = lock.iter_mut().find(|s| s.id == task_id);
                if let Some(entry) = entry {
                    entry.last_run_secs = now_secs;
                    match &result {
                        Ok(count) => {
                            entry.last_item_count = *count;
                            entry.last_error = None;
                        }
                        Err(e) => {
                            entry.last_error = Some(e.to_string());
                        }
                    }
                } else {
                    lock.push(TaskStatus {
                        id: task_id.clone(),
                        last_run_secs: now_secs,
                        last_item_count: result.as_ref().ok().cloned().unwrap_or(0),
                        last_error: result.as_ref().err().map(|e| e.to_string()),
                    });
                }
                let _ = write_statuses(&data_dir, &lock);
                drop(lock);

                if let Err(e) = result {
                    tracing::warn!("{}: {e}", task_id);
                }
            }
        }));
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Shutdown signal received");
        }
        _ = futures::future::join_all(handles) => {}
    }

    crate::pid::remove();
    Ok(())
}
