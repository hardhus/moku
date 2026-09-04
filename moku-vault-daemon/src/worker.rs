use std::io::{BufRead, BufReader, Read, Write};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use moku_core::SafeKey;
use moku_vault_fs::{VolumeEngine, derive_volume_keys};
use secrecy::SecretBox;

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
/// this is just how long the *caller* waits for an initial answer). Kept
/// short deliberately: a real mount or a bad-password failure both
/// resolve in well under a second in practice, and the caller (CLI or
/// TUI) should feel like it returns control immediately rather than
/// hanging around "just in case" — `TimedOut` isn't treated as an error,
/// just "still going, check status".
const MOUNT_READY_TIMEOUT: Duration = Duration::from_millis(1500);

/// Reads the mount secret from stdin (piped by the parent process — see
/// `spawn_mount_process`/`spawn_mount_process_with_key` below) rather than
/// a CLI arg or env var, so it never appears in `ps`/Task Manager or
/// `/proc/<pid>/environ` (plan §5). The first byte tags the payload:
/// `0x00` followed by a UTF-8 password (typed by a human, CLI or TUI
/// prompt), or `0x01` followed by 32 raw key bytes (the app vault's
/// already-unlocked master key, handed straight through by the TUI's
/// no-reprompt fast path for Default-mode volumes — see
/// `registry::resolve_volume_master_key`).
fn read_mount_secret_from_stdin() -> Result<registry::MountSecret> {
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .context("failed to read mount secret from stdin")?;
    match buf.split_first() {
        Some((0x01, rest)) if rest.len() == 32 => {
            let mut key = [0u8; 32];
            key.copy_from_slice(rest);
            Ok(registry::MountSecret::Key(SecretBox::new(Box::new(
                SafeKey(key),
            ))))
        }
        Some((0x00, rest)) => {
            let password =
                String::from_utf8(rest.to_vec()).context("password payload was not valid UTF-8")?;
            Ok(registry::MountSecret::Password(
                password.trim_end_matches(['\n', '\r']).to_string(),
            ))
        }
        _ => bail!("malformed mount secret on stdin"),
    }
}

/// Runs the mount worker for one volume: unlocks its independent vault,
/// opens the engine, mounts it at `mountpoint`, and blocks until the
/// process is asked to stop — via Ctrl+C, the control channel
/// (`crate::control`), or (as a last resort) a hard kill.
pub async fn run(volume_id: &str, mountpoint: &str) -> Result<()> {
    let secret = read_mount_secret_from_stdin()?;
    let volume_dir = registry::volume_dir(volume_id)?;
    let cfg = registry::load_config(&volume_dir).await?;

    let master_key = registry::resolve_volume_master_key(&volume_dir, &cfg, secret).await?;
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

/// One line/event read off the spawned worker's stdout, stderr, or its
/// eventual exit — produced by the raw, unmanaged `std::thread`s in
/// `spawn_mount_process_inner` and consumed by the async `tokio::select!`-
/// free receive loop in `run_and_wait_for_outcome`.
enum WorkerEvent {
    Stdout(String),
    Stderr(String),
    Exited(std::io::Result<std::process::ExitStatus>),
}

/// Spawns the `vault mount-worker` child process for `volume_id`, writes
/// `stdin_payload` to its stdin, and returns its pid plus a channel that
/// yields its stdout/stderr lines and eventual exit status.
///
/// The stdout/stderr reads happen on plain, never-joined `std::thread`s —
/// deliberately NOT via `tokio::process`/`spawn_blocking`. On Windows,
/// tokio has no true async read for anonymous pipes, so every read there
/// is dispatched to tokio's own *blocking thread pool*, and `Runtime::drop`
/// (which every `#[tokio::main]` fn runs on return) waits for every
/// blocking-pool thread to finish before the process can exit. Since the
/// worker is deliberately left running after a timeout and never closes
/// its end of the pipe, a blocking-pool thread stuck reading it would
/// never return — which is exactly what made `vault mount` hang the
/// terminal after printing "starting in the background" until Ctrl+C. A
/// bare `std::thread` has no such owner: nothing here ever joins it, so
/// the OS just discards it at process exit, hang-free.
fn spawn_mount_process_inner(
    volume_id: &str,
    mountpoint: &str,
    stdin_payload: Vec<u8>,
) -> Result<(u32, tokio::sync::mpsc::UnboundedReceiver<WorkerEvent>)> {
    let exe = std::env::current_exe()
        .map_err(|e| anyhow!("failed to resolve current executable: {e}"))?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("vault")
        .arg("mount-worker")
        .arg(volume_id)
        .arg("--mountpoint")
        .arg(mountpoint);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP — the latter takes
        // the worker out of the parent's console process group, so a
        // Ctrl+C in the parent's terminal (e.g. the user giving up on what
        // used to be a hang) can no longer also kill the worker it was
        // meant to leave running in the background.
        cmd.creation_flags(0x08000000 | 0x00000200);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("failed to spawn mount worker: {e}"))?;
    let pid = child.id();

    {
        let mut stdin = child.stdin.take().expect("stdin was requested as piped");
        stdin
            .write_all(&stdin_payload)
            .map_err(|e| anyhow!("failed to send secret to mount worker: {e}"))?;
        // `stdin` drops here, closing the pipe so the worker's blocking
        // `read_to_end` on its end sees EOF and returns.
    }

    let stdout = child.stdout.take().expect("stdout was requested as piped");
    let stderr = child.stderr.take().expect("stderr was requested as piped");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WorkerEvent>();

    for (reader, wrap) in [
        (
            Box::new(stdout) as Box<dyn Read + Send>,
            WorkerEvent::Stdout as fn(String) -> WorkerEvent,
        ),
        (
            Box::new(stderr) as Box<dyn Read + Send>,
            WorkerEvent::Stderr as fn(String) -> WorkerEvent,
        ),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut lines = BufReader::new(reader).lines();
            while let Some(Ok(line)) = lines.next() {
                if tx.send(wrap(line)).is_err() {
                    break;
                }
            }
        });
    }
    std::thread::spawn(move || {
        let status = child.wait();
        let _ = tx.send(WorkerEvent::Exited(status));
    });

    Ok((pid, rx))
}

/// Drives `spawn_mount_process_inner`'s event channel to a `MountOutcome`,
/// giving up after `MOUNT_READY_TIMEOUT` — shared by `spawn_mount_process`
/// (password) and `spawn_mount_process_with_key` (already-unlocked app-
/// vault key), which differ only in what they put in `stdin_payload`.
async fn run_and_wait_for_outcome(
    volume_id: &str,
    mountpoint: &str,
    stdin_payload: Vec<u8>,
) -> Result<MountOutcome> {
    let (pid, mut rx) = spawn_mount_process_inner(volume_id, mountpoint, stdin_payload)?;
    let mut last_stderr_line: Option<String> = None;

    let wait_for_outcome = async {
        loop {
            match rx.recv().await {
                Some(WorkerEvent::Stdout(l)) if l.trim() == MOUNT_READY_SENTINEL => {
                    return MountOutcome::Ready { pid };
                }
                Some(WorkerEvent::Stdout(_)) => {}
                Some(WorkerEvent::Stderr(l)) => last_stderr_line = Some(l),
                Some(WorkerEvent::Exited(status)) => {
                    let message = last_stderr_line.clone().unwrap_or_else(|| match status {
                        Ok(s) => format!("mount worker exited ({s}) with no error output"),
                        Err(e) => format!("failed to wait for mount worker: {e}"),
                    });
                    return MountOutcome::Failed { message };
                }
                None => {
                    return MountOutcome::Failed {
                        message: "mount worker's status channel closed unexpectedly".to_string(),
                    };
                }
            }
        }
    };

    match tokio::time::timeout(MOUNT_READY_TIMEOUT, wait_for_outcome).await {
        Ok(outcome) => Ok(outcome),
        Err(_) => Ok(MountOutcome::TimedOut { pid }),
    }
}

/// Spawns the `vault mount-worker` child process for `volume_id`, pipes
/// `password` over its stdin, and waits for it to either report
/// `MOUNT_READY_SENTINEL` on stdout or fail — rather than declaring
/// success the instant the process merely spawns, which previously let a
/// worker that failed silently (detached, `CREATE_NO_WINDOW`, output never
/// read) get reported as a success. Used for Custom-mode volumes, and for
/// Default-mode volumes mounted from the CLI (no persistent session to
/// reuse a key from — see `spawn_mount_process_with_key` for the TUI's
/// no-reprompt fast path).
pub async fn spawn_mount_process(
    volume_id: &str,
    mountpoint: &str,
    password: &str,
) -> Result<MountOutcome> {
    let mut payload = vec![0x00u8];
    payload.extend_from_slice(password.as_bytes());
    run_and_wait_for_outcome(volume_id, mountpoint, payload).await
}

/// Same as `spawn_mount_process`, but for a Default-mode volume whose real
/// key can be derived from an already-unlocked app-vault master key
/// without asking the user to type anything — the TUI's no-reprompt mount
/// path (`VaultManagerModule::start_mount_with_key`), used only when
/// `ctx.session.is_unlocked()` and the volume has no vault of its own
/// (`registry::has_own_vault` is false).
pub async fn spawn_mount_process_with_key(
    volume_id: &str,
    mountpoint: &str,
    key: &SecretBox<SafeKey>,
) -> Result<MountOutcome> {
    use secrecy::ExposeSecret;
    let mut payload = vec![0x01u8];
    payload.extend_from_slice(&key.expose_secret().0);
    run_and_wait_for_outcome(volume_id, mountpoint, payload).await
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
