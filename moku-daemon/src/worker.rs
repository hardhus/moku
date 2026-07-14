use std::sync::Arc;

use anyhow::Result;
use arc_swap::ArcSwap;
use moku_core::{ConfigManager, DaemonContext, DaemonTask, StorageManager, VaultSession};

/// Run the background service daemon.
pub async fn run_worker() -> Result<()> {
    let _guard = crate::logging::init()?;
    crate::pid::write()?;

    let loaded_config = ConfigManager::load().await.unwrap_or_default();
    let config = Arc::new(ArcSwap::from_pointee(loaded_config));

    // Daemon runs unencrypted: vault always remains locked. Tasks like RSS
    // must therefore call storage.save/load with is_encryption_enabled=false.
    let session = Arc::new(VaultSession::new());
    let storage = Arc::new(StorageManager::new(session).await?);
    let ctx = Arc::new(DaemonContext { config, storage });

    let tasks: Vec<Box<dyn DaemonTask>> = vec![Box::new(moku_rss::RssDaemonTask::new())];

    tracing::info!("Moku Daemon started, loading {} tasks", tasks.len());

    let mut handles = Vec::new();
    for mut task in tasks {
        let ctx = Arc::clone(&ctx);
        handles.push(tokio::spawn(async move {
            let mut timer = tokio::time::interval(task.interval());
            loop {
                timer.tick().await;
                if let Err(e) = task.tick(&ctx).await {
                    tracing::warn!("{}: {e}", task.id());
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
