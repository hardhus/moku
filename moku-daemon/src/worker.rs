use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use arc_swap::ArcSwap;
use moku_core::{ConfigManager, DaemonContext, DaemonTask, StorageManager, VaultSession};

use crate::task_status::{TaskStatus, write_statuses};

/// Applies one task's tick outcome to its `TaskStatus` entry in `statuses`,
/// inserting a new entry if this is the task's first recorded tick.
/// Pure/synchronous so it's directly unit-testable without tokio or I/O.
fn record_tick_result(
    statuses: &mut Vec<TaskStatus>,
    task_id: &str,
    result: &Result<usize>,
    now_secs: Option<u64>,
) {
    match statuses.iter_mut().find(|s| s.id == task_id) {
        Some(entry) => {
            entry.last_run_secs = now_secs;
            match result {
                Ok(count) => {
                    entry.last_item_count = *count;
                    entry.last_error = None;
                }
                Err(e) => {
                    entry.last_error = Some(e.to_string());
                }
            }
        }
        None => {
            statuses.push(TaskStatus {
                id: task_id.to_string(),
                last_run_secs: now_secs,
                last_item_count: result.as_ref().ok().copied().unwrap_or(0),
                last_error: result.as_ref().err().map(|e| e.to_string()),
            });
        }
    }
}

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

                // sled only allows one process to hold a module's DB open
                // at a time; release it here so the TUI/CLI can open the
                // same DB between ticks (see DaemonTask::storage_module_ids).
                for mid in task.storage_module_ids() {
                    ctx.storage.close_db(mid).await;
                }

                let mut lock = statuses.lock().await;
                record_tick_result(&mut lock, &task_id, &result, now_secs);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_first_tick_success_inserts_entry() {
        let mut statuses = Vec::new();
        record_tick_result(&mut statuses, "rss", &Ok(5), Some(1000));

        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].id, "rss");
        assert_eq!(statuses[0].last_run_secs, Some(1000));
        assert_eq!(statuses[0].last_item_count, 5);
        assert_eq!(statuses[0].last_error, None);
    }

    #[test]
    fn test_record_first_tick_error_inserts_entry_with_error() {
        let mut statuses = Vec::new();
        let result: Result<usize> = Err(anyhow::anyhow!("fetch failed"));
        record_tick_result(&mut statuses, "rss", &result, Some(1000));

        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].last_item_count, 0);
        assert_eq!(statuses[0].last_error.as_deref(), Some("fetch failed"));
    }

    #[test]
    fn test_record_subsequent_success_updates_existing_entry_and_clears_error() {
        let mut statuses = vec![TaskStatus {
            id: "rss".to_string(),
            last_run_secs: Some(1000),
            last_item_count: 0,
            last_error: Some("previous failure".to_string()),
        }];

        record_tick_result(&mut statuses, "rss", &Ok(3), Some(2000));

        assert_eq!(statuses.len(), 1, "must update in place, not duplicate");
        assert_eq!(statuses[0].last_run_secs, Some(2000));
        assert_eq!(statuses[0].last_item_count, 3);
        assert_eq!(statuses[0].last_error, None);
    }

    #[test]
    fn test_record_subsequent_error_keeps_last_item_count_and_sets_error() {
        let mut statuses = vec![TaskStatus {
            id: "rss".to_string(),
            last_run_secs: Some(1000),
            last_item_count: 7,
            last_error: None,
        }];
        let result: Result<usize> = Err(anyhow::anyhow!("timeout"));

        record_tick_result(&mut statuses, "rss", &result, Some(2000));

        assert_eq!(statuses[0].last_item_count, 7, "count is only touched on Ok");
        assert_eq!(statuses[0].last_error.as_deref(), Some("timeout"));
        assert_eq!(statuses[0].last_run_secs, Some(2000));
    }

    #[test]
    fn test_record_only_touches_matching_task_id() {
        let mut statuses = vec![TaskStatus {
            id: "other".to_string(),
            last_run_secs: Some(1),
            last_item_count: 9,
            last_error: None,
        }];

        record_tick_result(&mut statuses, "rss", &Ok(1), Some(2000));

        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0].id, "other");
        assert_eq!(statuses[0].last_item_count, 9, "untouched");
        assert_eq!(statuses[1].id, "rss");
    }
}
