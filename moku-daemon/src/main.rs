use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use auto_launch::AutoLaunchBuilder;
use clap::{Parser, Subcommand};
use sysinfo::{Pid, System};

use moku_core::{
    ConfigManager, DaemonContext, DaemonTask, StorageManager, VaultSession,
};

mod logging;

#[derive(Parser)]
#[command(name = "moku-daemon", about = "Moku background service")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run in the foreground (default). Stopped with Ctrl+C.
    Run,
    /// Get background service status
    Status,
    /// Enable system autostart for the daemon
    EnableAutostart,
    /// Disable system autostart for the daemon
    DisableAutostart,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::Run) {
        Commands::Run => run_worker().await,
        Commands::Status => print_status(),
        Commands::EnableAutostart => set_autostart(true),
        Commands::DisableAutostart => set_autostart(false),
    }
}

async fn run_worker() -> Result<()> {
    let _guard = logging::init()?;
    write_pid_file()?;

    let loaded_config = ConfigManager::load().await.unwrap_or_default();
    let config = Arc::new(ArcSwap::from_pointee(loaded_config));

    // Daemon runs unencrypted: vault always remains locked. Tasks like RSS
    // must therefore call storage.save/load with is_encryption_enabled=false
    // (perfectly fine for non-sensitive cached data).
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

    remove_pid_file();
    Ok(())
}

fn write_pid_file() -> Result<()> {
    let data_dir = moku_core::dirs::get_data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    std::fs::write(
        data_dir.join("moku_daemon.pid"),
        std::process::id().to_string(),
    )?;
    Ok(())
}

fn remove_pid_file() {
    if let Ok(data_dir) = moku_core::dirs::get_data_dir() {
        let _ = std::fs::remove_file(data_dir.join("moku_daemon.pid"));
    }
}

fn print_status() -> Result<()> {
    let data_dir = moku_core::dirs::get_data_dir()?;
    let pid_path = data_dir.join("moku_daemon.pid");

    let Ok(pid_str) = std::fs::read_to_string(&pid_path) else {
        println!("⚫ Moku Daemon is not running (no pid file).");
        return Ok(());
    };

    let pid: u32 = pid_str.trim().parse().context("Invalid pid file")?;
    let mut sys = System::new_all();
    sys.refresh_all();

    if sys.process(Pid::from_u32(pid)).is_some() {
        println!("🟢 Moku Daemon is running (PID: {pid}).");
    } else {
        println!("⚫ Moku Daemon is not running (stale pid file found, cleaning up).");
        let _ = std::fs::remove_file(pid_path);
    }
    Ok(())
}

fn set_autostart(enable: bool) -> Result<()> {
    let current_exe = std::env::current_exe()?;
    let launcher = AutoLaunchBuilder::new()
        .set_app_name("moku-daemon")
        .set_app_path(&current_exe.to_string_lossy())
        .set_args(&["run"])
        .build()
        .map_err(|e| anyhow::anyhow!("AutoLaunch error: {e}"))?;

    if enable {
        launcher
            .enable()
            .map_err(|e| anyhow::anyhow!("Failed to enable autostart: {e}"))?;
        println!("✅ Moku Daemon added to system autostart.");
    } else {
        launcher
            .disable()
            .map_err(|e| anyhow::anyhow!("Failed to disable autostart: {e}"))?;
        println!("🧹 Moku Daemon removed from system autostart.");
    }
    Ok(())
}
