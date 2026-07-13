use anyhow::Result;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{self, layer::SubscriberExt, util::SubscriberInitExt};

pub fn init() -> Result<WorkerGuard> {
    let data_dir = moku_core::dirs::get_data_dir()?;
    let log_dir = data_dir.join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let file_appender = tracing_appender::rolling::daily(&log_dir, "moku-daemon.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false);

    let _ = tracing_subscriber::registry().with(file_layer).try_init();
    tracing::info!("--- Moku Daemon Started ---");
    Ok(guard)
}
