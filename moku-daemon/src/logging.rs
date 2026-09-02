use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Delete log files older than `keep_days` days from the given directory.
/// Only touches files whose names start with "moku-daemon" to avoid
/// accidentally removing unrelated files.
fn cleanup_old_logs(log_dir: &Path, keep_days: u64) {
    let cutoff = match SystemTime::now().checked_sub(Duration::from_secs(keep_days * 24 * 3600)) {
        Some(t) => t,
        None => return,
    };

    let Ok(entries) = std::fs::read_dir(log_dir) else { return; };

    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("moku-daemon") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue; };
        let Ok(modified) = meta.modified() else { continue; };
        if modified < cutoff {
            match std::fs::remove_file(entry.path()) {
                Ok(_) => tracing::info!("Removed old log: {:?}", entry.path()),
                Err(e) => tracing::warn!("Failed to remove old log {:?}: {e}", entry.path()),
            }
        }
    }
}

pub fn init() -> Result<WorkerGuard> {
    let data_dir = moku_core::dirs::get_data_dir()?;
    let log_dir = data_dir.join("logs");
    std::fs::create_dir_all(&log_dir)?;

    // Clean up log files older than 7 days before opening new appender
    cleanup_old_logs(&log_dir, 7);

    let file_appender = tracing_appender::rolling::daily(&log_dir, "moku-daemon.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    // Default: INFO for moku crates, WARN for noisy third-party crates (sled, etc.)
    // Override at runtime with RUST_LOG env var, e.g. RUST_LOG=debug
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,sled=warn,rustls=warn,reqwest=warn,hyper=warn")
    });

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false);

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .try_init();

    tracing::info!("--- Moku Daemon Started ---");
    Ok(guard)
}

