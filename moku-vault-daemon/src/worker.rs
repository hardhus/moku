use std::io::Read;

use anyhow::{Context, Result, anyhow};
use moku_core::SecurityManager;
use moku_vault_fs::{VolumeEngine, derive_volume_keys};

use crate::registry;

/// Reads the mount password from stdin (piped by the parent process — see
/// `moku-bin/src/vault_cmd.rs`'s `Mount` handler) rather than a CLI arg or
/// env var, so the secret never appears in `ps`/Task Manager or
/// `/proc/<pid>/environ` (plan §5).
fn read_password_from_stdin() -> Result<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).context("failed to read password from stdin")?;
    Ok(buf.trim_end_matches(['\n', '\r']).to_string())
}

/// Runs the mount worker for one volume: unlocks its independent vault,
/// opens the engine, mounts it at `mountpoint`, and blocks until the
/// process is asked to stop. There is no graceful control-channel
/// handshake yet (plan §3, a later phase) — today that's Ctrl+C in the
/// foreground case, or a hard kill from `moku vault unmount`.
pub async fn run(volume_id: &str, mountpoint: &str) -> Result<()> {
    let password = read_password_from_stdin()?;
    let volume_dir = registry::volume_dir(volume_id)?;
    let cfg = registry::load_config(&volume_dir).await?;

    let security = SecurityManager::new_with_root(volume_dir.clone());
    let master_key =
        security.unlock_vault(password).await.map_err(|e| anyhow!("failed to unlock volume '{volume_id}': {e}"))?;
    let keys = derive_volume_keys(&master_key);

    let engine = VolumeEngine::open_volume(
        volume_dir.join(registry::DATA_DIR),
        keys,
        volume_dir.join(registry::USAGE_FILE),
        cfg.size_limit_bytes,
    )?;

    crate::pid::write(volume_id, std::process::id())?;

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = stop_tx.send(());
    });

    let mountpoint = mountpoint.to_string();
    // `mount_and_wait` blocks synchronously for the whole mount lifetime.
    // This process exists solely to run one mount, so running it via
    // spawn_blocking just keeps the (otherwise idle) tokio worker threads
    // free rather than changing behavior.
    let result: Result<()> =
        tokio::task::spawn_blocking(move || moku_vault_mount::mount_and_wait(engine, &mountpoint, stop_rx))
            .await
            .context("mount worker task panicked")?;

    crate::pid::remove(volume_id);
    result
}
