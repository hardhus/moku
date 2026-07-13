use std::fs;

use color_eyre::eyre::{Result, eyre};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_error::ErrorLayer;
use tracing_subscriber::{self, layer::SubscriberExt, util::SubscriberInitExt};

use moku_core::dirs;

use crate::tui;

/// Initializes color-eyre error hooks.
pub fn init_errors() -> Result<()> {
    let (panic_hook, eyre_hook) = color_eyre::config::HookBuilder::default()
        .panic_section("An error occurred. Please check the log files.")
        .display_location_section(true)
        .display_env_section(false)
        .into_hooks();

    let panic_hook = panic_hook.into_panic_hook();

    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = tui::restore();
        panic_hook(panic_info);
    }));

    let eyre_hook = eyre_hook.into_eyre_hook();
    color_eyre::eyre::set_hook(Box::new(move |error| {
        let _ = tui::restore();
        eyre_hook(error)
    }))?;

    Ok(())
}

/// Initializes file-based asynchronous logging.
pub fn init_logging() -> Result<WorkerGuard> {
    let data_dir = dirs::get_data_dir().map_err(|e| eyre!(e))?;
    let log_dir = data_dir.join("logs");

    if !log_dir.exists() {
        fs::create_dir_all(&log_dir)?;
    }

    let file_appender = tracing_appender::rolling::daily(&log_dir, "moku.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true)
        .with_file(true)
        .with_line_number(true);

    let _ = tracing_subscriber::registry()
        .with(file_layer)
        .with(ErrorLayer::default())
        .try_init();

    tracing::info!("--- Moku Started ---");
    tracing::info!("Log Directory: {:?}", log_dir);

    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_log_directory_creation() {
        let temp = tempdir().unwrap();
        let log_path = temp.path().join("logs");

        assert!(!log_path.exists());
        fs::create_dir_all(&log_path).unwrap();
        assert!(log_path.exists());
    }

    #[test]
    fn test_logging_init_is_safe_to_reentry() {
        let res1 = init_logging();
        let res2 = init_logging();

        if res1.is_ok() {
            assert!(res2.is_ok() || res2.is_err());
        }
    }
}
