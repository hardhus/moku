use std::io::Read;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use moku_core::SecurityManager;
use moku_vault_fs::{VolumeEngine, derive_volume_keys};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::registry;
use crate::status;

/// Printed by `run` (from inside `mount_and_wait`'s `on_mounted` callback)
/// on the worker's stdout once WinFsp confirms the mount is actually live
/// — `spawn_mount_process` (in the *parent* process) waits for this exact
/// line before declaring success, instead of the previous fire-and-forget
/// "spawned, therefore succeeded" assumption.
const MOUNT_READY_SENTINEL: &str = "MOKU_MOUNT_READY";

/// How long `spawn_mount_process` waits for the worker to either report
/// `MOUNT_READY_SENTINEL` or fail before giving up and reporting
/// `MountOutcome::TimedOut` (the worker itself keeps running either way —
/// this is just how long the *caller* waits for an initial answer).
const MOUNT_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Reads the mount password from stdin (piped by the parent process — see
/// `moku-bin/src/vault_cmd.rs`'s `Mount` handler) rather than a CLI arg or
/// env var, so the secret never appears in `ps`/Task Manager or
/// `/proc/<pid>/environ` (plan §5).
fn read_password_from_stdin() -> Result<String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("failed to read password from stdin")?;
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
    let master_key = security
        .unlock_vault(password)
        .await
        .map_err(|e| anyhow!("failed to unlock volume '{volume_id}': {e}"))?;
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
        engine
            .reconcile_usage()
            .context("failed to reconcile usage after an unclean previous session")?;
        engine
            .flush_usage()
            .context("failed to persist reconciled usage")?;
    }

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
    // The pid file is only written once WinFsp confirms the mount is
    // actually live (`on_mounted`, called from inside `mount_and_wait`
    // right after `host.start()` succeeds) — previously it was written
    // *before* the mount was even attempted, so `status::is_mounted()`
    // could report true for a mount that then failed. `MOKU_MOUNT_READY`
    // on stdout is the signal `spawn_mount_process` (in the parent
    // process) waits for to know the mount genuinely succeeded, instead of
    // declaring success the instant the child process merely spawns.
    let volume_id_owned = volume_id.to_string();
    let on_mounted = move || {
        if let Err(e) = crate::pid::write(&volume_id_owned, std::process::id()) {
            eprintln!("failed to write pid file: {e}");
        }
        println!("{MOUNT_READY_SENTINEL}");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    };

    // `mount_and_wait` blocks synchronously for the whole mount lifetime.
    // This process exists solely to run one mount, so running it via
    // spawn_blocking just keeps the (otherwise idle) tokio worker threads
    // free rather than changing behavior.
    let result: Result<()> = tokio::task::spawn_blocking(move || {
        moku_vault_mount::mount_and_wait(engine, &mountpoint, stop_rx, on_mounted)
    })
    .await
    .context("mount worker task panicked")?;

    crate::pid::remove(volume_id);
    result
}

/// What `spawn_mount_process` learned about the worker it just spawned.
#[derive(Debug, Clone)]
pub enum MountOutcome {
    /// WinFsp confirmed the mount is live (`MOUNT_READY_SENTINEL` seen).
    Ready { pid: u32 },
    /// The worker exited before reporting ready — `message` is its last
    /// stderr line (the real `anyhow::Error` text from `mount_and_wait`),
    /// or a generic fallback if it produced no output at all.
    Failed { message: String },
    /// Neither a ready signal nor an exit happened within
    /// `MOUNT_READY_TIMEOUT` — the worker (pid) is still running; it may
    /// yet succeed (e.g. unusually slow disk/key-derivation), so it's left
    /// running rather than killed.
    TimedOut { pid: u32 },
}

/// Spawns the `vault mount-worker` child process for `volume_id`, pipes
/// `password` over its stdin, and waits for it to either report
/// `MOUNT_READY_SENTINEL` on stdout or fail — rather than declaring
/// success the instant the process merely spawns, which previously let a
/// worker that failed silently (detached, `CREATE_NO_WINDOW`, output never
/// read) get reported as a success. Shared by the CLI (`moku vault mount`)
/// and the TUI (`VaultManagerModule`) so this logic lives in one place.
pub async fn spawn_mount_process(
    volume_id: &str,
    mountpoint: &str,
    password: &str,
) -> Result<MountOutcome> {
    let exe = std::env::current_exe()
        .map_err(|e| anyhow!("failed to resolve current executable: {e}"))?;
    let mut cmd = tokio::process::Command::new(&exe);
    cmd.arg("vault")
        .arg("mount-worker")
        .arg(volume_id)
        .arg("--mountpoint")
        .arg(mountpoint);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("failed to spawn mount worker: {e}"))?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow!("mount worker exited before it could report its pid"))?;

    {
        let mut stdin = child.stdin.take().expect("stdin was requested as piped");
        stdin
            .write_all(password.as_bytes())
            .await
            .map_err(|e| anyhow!("failed to send password to mount worker: {e}"))?;
        // `stdin` drops here, closing the pipe so the worker's blocking
        // `read_to_string` on its end sees EOF and returns.
    }

    let mut stdout_lines =
        BufReader::new(child.stdout.take().expect("stdout was requested as piped")).lines();
    let mut stderr_lines =
        BufReader::new(child.stderr.take().expect("stderr was requested as piped")).lines();
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut last_stderr_line: Option<String> = None;

    let wait_for_outcome = async {
        loop {
            tokio::select! {
                line = stdout_lines.next_line(), if !stdout_done => {
                    match line {
                        Ok(Some(l)) if l.trim() == MOUNT_READY_SENTINEL => return MountOutcome::Ready { pid },
                        Ok(Some(_)) => {}
                        _ => stdout_done = true,
                    }
                }
                line = stderr_lines.next_line(), if !stderr_done => {
                    match line {
                        Ok(Some(l)) => last_stderr_line = Some(l),
                        _ => stderr_done = true,
                    }
                }
                status = child.wait() => {
                    let message = last_stderr_line.clone().unwrap_or_else(|| match status {
                        Ok(s) => format!("mount worker exited ({s}) with no error output"),
                        Err(e) => format!("failed to wait for mount worker: {e}"),
                    });
                    return MountOutcome::Failed { message };
                }
            }
        }
    };

    match tokio::time::timeout(MOUNT_READY_TIMEOUT, wait_for_outcome).await {
        Ok(outcome) => Ok(outcome),
        Err(_) => Ok(MountOutcome::TimedOut { pid }),
    }
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
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &worker_pid.to_string(), "/F"])
        .output();
    #[cfg(not(windows))]
    let _ = std::process::Command::new("kill")
        .arg(worker_pid.to_string())
        .output();

    std::thread::sleep(std::time::Duration::from_millis(400));
    crate::pid::remove(volume_id);
    Ok(StopOutcome::Forced)
}
