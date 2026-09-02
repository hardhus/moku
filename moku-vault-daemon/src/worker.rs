use std::io::{Read, Write};

use anyhow::{Context, Result, anyhow};
use moku_core::SecurityManager;
use moku_vault_fs::{VolumeEngine, derive_volume_keys};

use crate::registry;
use crate::status;

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
/// process is asked to stop — via Ctrl+C, the control channel
/// (`crate::control`), or (as a last resort) a hard kill.
pub async fn run(volume_id: &str, mountpoint: &str) -> Result<()> {
    let password = read_password_from_stdin()?;
    let volume_dir = registry::volume_dir(volume_id)?;
    let cfg = registry::load_config(&volume_dir).await?;

    let security = SecurityManager::new_with_root(volume_dir.clone());
    let master_key =
        security.unlock_vault(password).await.map_err(|e| anyhow!("failed to unlock volume '{volume_id}': {e}"))?;
    let keys = derive_volume_keys(&master_key);

    // A pid file present but not alive means the previous mount ended
    // uncleanly (crash, hard kill) — mount_and_wait's final
    // engine.flush_usage() never ran, so the cached usage counter may be
    // stale. Detected before we overwrite the pid file with our own.
    let previous_session_unclean = match crate::pid::read(volume_id) {
        Some(stale_pid) => !status::pid_is_alive(stale_pid),
        None => false,
    };

    let engine = VolumeEngine::open_volume(
        volume_dir.join(registry::DATA_DIR),
        keys,
        volume_dir.join(registry::USAGE_FILE),
        cfg.size_limit_bytes,
    )?;

    if previous_session_unclean {
        engine.reconcile_usage().context("failed to reconcile usage after an unclean previous session")?;
        engine.flush_usage().context("failed to persist reconciled usage")?;
    }

    crate::pid::write(volume_id, std::process::id())?;

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    {
        let tx = stop_tx.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = tx.send(());
        });
    }
    {
        let tx = stop_tx.clone();
        let id = volume_id.to_string();
        tokio::spawn(async move {
            let _ = crate::control::listen_for_stop(&id, tx).await;
        });
    }

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

/// Spawns the `vault mount-worker` child process for `volume_id` and pipes
/// `password` over its stdin. Returns the child's PID. Shared by the CLI
/// (`moku vault mount`) and the TUI (`VaultManagerModule`) so the actual
/// spawn logic — CREATE_NO_WINDOW, piped stdin as the *only* way the
/// password crosses the process boundary — lives in one place.
pub fn spawn_mount_process(volume_id: &str, mountpoint: &str, password: &str) -> Result<u32> {
    let exe = std::env::current_exe().map_err(|e| anyhow!("failed to resolve current executable: {e}"))?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("vault").arg("mount-worker").arg(volume_id).arg("--mountpoint").arg(mountpoint);
    cmd.stdin(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().map_err(|e| anyhow!("failed to spawn mount worker: {e}"))?;

    {
        let mut stdin = child.stdin.take().expect("stdin was requested as piped");
        stdin.write_all(password.as_bytes()).map_err(|e| anyhow!("failed to send password to mount worker: {e}"))?;
    }

    Ok(child.id())
}

/// What `stop_mount_process` actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// No pid file — the volume wasn't mounted.
    NotMounted,
    /// A pid file existed but the process wasn't alive; cleaned up.
    StaleCleanedUp,
    /// The worker stopped via the graceful control-channel handshake.
    Graceful,
    /// The worker didn't respond in time (or the channel couldn't be
    /// reached) and was force-killed.
    Forced,
}

/// Stops a running mount worker: tries the graceful control-channel
/// handshake first (which lets `mount_and_wait` reach its clean unmount +
/// usage flush), polling for actual exit, and only falls back to a hard
/// kill if that can't be delivered or the worker doesn't exit in time.
/// Shared by the CLI and TUI unmount paths.
pub async fn stop_mount_process(volume_id: &str) -> Result<StopOutcome> {
    let Some(worker_pid) = crate::pid::read(volume_id) else {
        return Ok(StopOutcome::NotMounted);
    };
    if !status::pid_is_alive(worker_pid) {
        crate::pid::remove(volume_id);
        return Ok(StopOutcome::StaleCleanedUp);
    }

    if crate::control::send_stop(volume_id).await.is_ok() {
        for _ in 0..25 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if !status::pid_is_alive(worker_pid) {
                crate::pid::remove(volume_id);
                return Ok(StopOutcome::Graceful);
            }
        }
    }

    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill").args(["/PID", &worker_pid.to_string(), "/F"]).output();
    #[cfg(not(windows))]
    let _ = std::process::Command::new("kill").arg(worker_pid.to_string()).output();

    std::thread::sleep(std::time::Duration::from_millis(400));
    crate::pid::remove(volume_id);
    Ok(StopOutcome::Forced)
}
