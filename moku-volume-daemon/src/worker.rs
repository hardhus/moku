use std::io::{BufRead, BufReader, Read, Write};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use moku_crypto::SafeKey;
use moku_volume_fs::{VolumeEngine, derive_volume_keys};
use secrecy::SecretBox;
use zeroize::Zeroizing;

use crate::registry;
use crate::status;
use crate::{MountErrorCode, VolumeError};

/// Printed by `run` (from inside `mount_and_wait`'s `on_mounted` callback)
/// on the worker's stdout once WinFsp confirms the mount is actually live
/// — `spawn_mount_process` (in the *parent* process) waits for this exact
/// line before declaring success, instead of the previous fire-and-forget
/// "spawned, therefore succeeded" assumption.
const MOUNT_READY_SENTINEL: &str = "MOKU_MOUNT_READY";

/// Printed by `run` on the worker's stdout when the mount fails, as
/// `MOKU_MOUNT_ERR <code>\t<message>` (code: `MountErrorCode::as_str`,
/// message: the full cause chain on one line). The parent turns it into
/// `MountOutcome::Failed { code, message }`, so it can react to the class
/// of failure (re-ask for a wrong password) instead of guessing from the
/// last stderr line.
const MOUNT_ERROR_PREFIX: &str = "MOKU_MOUNT_ERR";

fn mount_error_line(code: MountErrorCode, message: &str) -> String {
    let one_line = message.replace(['\n', '\r', '\t'], " ");
    format!("{MOUNT_ERROR_PREFIX} {}\t{one_line}", code.as_str())
}

/// Parses a `MOKU_MOUNT_ERR` line; `None` for any other line.
fn parse_mount_error_line(line: &str) -> Option<(MountErrorCode, String)> {
    let rest = line.trim_end().strip_prefix(MOUNT_ERROR_PREFIX)?.strip_prefix(' ')?;
    let (code, message) = rest.split_once('\t').unwrap_or((rest, ""));
    Some((MountErrorCode::parse(code), message.to_string()))
}

/// Reads the mount secret from stdin (piped by the parent process, see
/// `spawn_mount_process`/`spawn_mount_process_with_volume_key` below)
/// rather than a CLI arg or env var, so it never appears in `ps`/Task
/// Manager or `/proc/<pid>/environ` (plan §5). The first byte tags the
/// payload:
/// - `0x00` + a UTF-8 password (typed by a human, CLI or TUI prompt);
/// - `0x02` + 32 raw bytes: the volume's own derived master key (the
///   TUI's no-reprompt fast path for new-scheme Default-mode volumes);
/// - `0x01` + 32 raw bytes: the app vault's master key. Legacy: still
///   accepted from an older binary, never sent any more; removed in P9.
///   The worker only needs one volume's key, never the key that also
///   opens every module's data.
fn read_mount_secret_from_stdin() -> Result<registry::MountSecret> {
    let mut buf: Zeroizing<Vec<u8>> = Zeroizing::new(Vec::new());
    std::io::stdin()
        .read_to_end(&mut buf)
        .context("failed to read mount secret from stdin")?;
    parse_mount_secret(&buf)
}

/// Decodes the tagged stdin payload; see `read_mount_secret_from_stdin`.
fn parse_mount_secret(buf: &[u8]) -> Result<registry::MountSecret> {
    match buf.split_first() {
        Some((0x01, rest)) if rest.len() == 32 => {
            let mut key = [0u8; 32];
            key.copy_from_slice(rest);
            Ok(registry::MountSecret::Key(SecretBox::new(Box::new(
                SafeKey(key),
            ))))
        }
        Some((0x02, rest)) if rest.len() == 32 => {
            let mut key = [0u8; 32];
            key.copy_from_slice(rest);
            Ok(registry::MountSecret::VolumeKey(SecretBox::new(Box::new(SafeKey(key)))))
        }
        Some((0x00, rest)) => {
            let password: Zeroizing<String> = Zeroizing::new(
                String::from_utf8(rest.to_vec()).context("password payload was not valid UTF-8")?,
            );
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
///
/// On failure it also reports the error's class to the parent on stdout
/// (`MOKU_MOUNT_ERR`, see `mount_error_line`) before returning it.
pub async fn run(volume_id: &str, mountpoint: &str) -> Result<()> {
    match run_inner(volume_id, mountpoint).await {
        Ok(()) => Ok(()),
        Err(e) => {
            println!(
                "{}",
                mount_error_line(e.code(), &moku_sys::util::error_chain(&e))
            );
            // Best-effort: if stdout is gone, so is the parent waiting on it.
            let _ = std::io::Write::flush(&mut std::io::stdout());
            Err(e.into())
        }
    }
}

async fn run_inner(volume_id: &str, mountpoint: &str) -> std::result::Result<(), VolumeError> {
    let secret = read_mount_secret_from_stdin()?;
    let volume_dir = registry::volume_dir(volume_id)?;
    let cfg = registry::load_config(&volume_dir).await?;

    // Take the cross-process volume lock BEFORE resolving the key: a second
    // mount of the same volume fails right here, instead of after the
    // multi-second Argon2 password derivation.
    let lock = moku_volume_fs::VolumeLock::acquire(&volume_dir).map_err(|e| match e {
        moku_volume_fs::VolumeFsError::Locked => {
            VolumeError::AlreadyMounted(cfg.display_name.clone())
        }
        other => VolumeError::Fs(other),
    })?;

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

    let engine = std::sync::Arc::new(VolumeEngine::open_volume(
        volume_dir.join(registry::DATA_DIR),
        keys,
        volume_dir.join(registry::USAGE_FILE),
        cfg.size_limit_bytes,
        lock,
    )?);

    if previous_session_unclean {
        engine.reconcile_usage()?;
        engine.flush_usage()?;
    }

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    {
        let tx = stop_tx.clone();
        tokio::spawn(async move {
            // Err only if the handler can't be installed: then Ctrl+C just
            // isn't a stop source (the control channel still is). Either
            // way, a closed receiver means we're already stopping.
            let _ = tokio::signal::ctrl_c().await;
            let _ = tx.send(());
        });
    }
    {
        let tx = stop_tx.clone();
        let id = volume_id.to_string();
        tokio::spawn(async move {
            // Without the control channel `moku volume unmount` can't stop
            // this worker gracefully (it falls back to a hard kill, which
            // skips the clean unmount), so a failure here must be visible.
            // stderr is captured by the parent and the worker log.
            if let Err(e) = crate::control::listen_for_stop(&id, tx).await {
                eprintln!("volume control channel unavailable: {e:#}");
            }
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
        // Best-effort: if stdout is gone, so is the parent waiting on it.
        let _ = std::io::Write::flush(&mut std::io::stdout());
    };

    // `mount_and_wait` blocks synchronously for the whole mount lifetime.
    // This process exists solely to run one mount, so running it via
    // spawn_blocking just keeps the (otherwise idle) tokio worker threads
    // free rather than changing behavior.
    let mount_engine = std::sync::Arc::clone(&engine);
    let result = tokio::task::spawn_blocking(move || {
        moku_volume_mount::mount_and_wait(mount_engine, &mountpoint, stop_rx, on_mounted)
    })
    .await
    .context("mount worker task panicked")?;

    // Persist the in-memory usage counter however the mount ended (clean
    // stop, mount failure, dispatcher error). A failure here is reported,
    // not swallowed: a stale usage.json means wrong quota numbers until the
    // next unclean-session reconcile.
    if let Err(e) = engine.flush_usage() {
        eprintln!(
            "failed to persist volume usage: {}",
            moku_sys::util::error_chain(&e)
        );
    }

    crate::pid::remove(volume_id);
    Ok(result?)
}

/// What `spawn_mount_process` learned about the worker it just spawned.
/// Always one or the other — the caller waits as long as it takes for a
/// real answer (see `run_and_wait_for_outcome`) rather than giving up
/// early on an arbitrary clock and leaving the user to guess. A real mount
/// or a bad-password failure both eventually resolve; a genuinely stuck
/// worker just means the wait takes longer; it does not hang the *caller*
/// either way (see `spawn_mount_process_inner`'s doc comment).
#[derive(Debug, Clone)]
pub enum MountOutcome {
    /// WinFsp confirmed the mount is live (`MOUNT_READY_SENTINEL` seen).
    Ready { pid: u32 },
    /// The worker exited before reporting ready. `code` and `message`
    /// come from its `MOKU_MOUNT_ERR` line; a worker that died without one
    /// (crash, older binary) is `MountErrorCode::Unknown` with its last
    /// stderr line, or a generic fallback if it printed nothing.
    Failed {
        code: MountErrorCode,
        message: String,
    },
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

/// Spawns the `volume mount-worker` child process for `volume_id`, writes
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
/// never return — which is exactly what made `volume mount` hang the
/// terminal after printing "starting in the background" until Ctrl+C. A
/// bare `std::thread` has no such owner: nothing here ever joins it, so
/// the OS just discards it at process exit, hang-free.
fn spawn_mount_process_inner(
    volume_id: &str,
    mountpoint: &str,
    stdin_payload: Zeroizing<Vec<u8>>,
) -> Result<(u32, tokio::sync::mpsc::UnboundedReceiver<WorkerEvent>)> {
    let exe = std::env::current_exe()
        .map_err(|e| anyhow!("failed to resolve current executable: {e}"))?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("volume")
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

    // All three were requested as `Stdio::piped()` above, so they are
    // present. If that ever breaks, fail cleanly rather than panic, and
    // never leave a half-started worker behind.
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        // Best-effort cleanup of a worker we can't drive; the error below
        // is what the caller needs to see.
        let _ = child.kill();
        bail!("mount worker started without its stdio pipes");
    };
    if let Err(e) = stdin.write_all(&stdin_payload) {
        // Best-effort: don't leak a worker that never got its secret.
        let _ = child.kill();
        bail!("failed to send secret to mount worker: {e}");
    }
    // Closing the pipe lets the worker's blocking `read_to_end` see EOF.
    drop(stdin);
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
        // Receiver dropped = nobody is waiting for the outcome any more.
        let _ = tx.send(WorkerEvent::Exited(status));
    });

    Ok((pid, rx))
}

/// Drives `spawn_mount_process_inner`'s event channel to a definitive
/// `MountOutcome` — shared by `spawn_mount_process` (password) and
/// `spawn_mount_process_with_key` (already-unlocked app-vault key), which
/// differ only in what they put in `stdin_payload`.
///
/// Waits as long as it takes rather than giving up on a clock: a CLI mount
/// always pays a real Argon2id cost (deliberately expensive — see
/// `moku-crypto/src/password.rs`) verifying the password against the
/// real app vault, which can easily take longer than a short fixed
/// timeout would allow, while the TUI's already-unlocked fast path
/// resolves almost instantly with no Argon2 at all. An earlier, shorter
/// version of this wait gave up after 1.5s and reported an ambiguous
/// "still starting" status — which the CLI path would hit *routinely*
/// (not as a rare edge case), leaving the user unable to tell whether
/// their password was even right, since the real answer arrived after the
/// caller had already given up and exited. Safe to simply await
/// indefinitely: `spawn_mount_process_inner`'s raw, unmanaged threads
/// never block process shutdown regardless of how long this waits.
async fn run_and_wait_for_outcome(
    volume_id: &str,
    mountpoint: &str,
    stdin_payload: Zeroizing<Vec<u8>>,
) -> Result<MountOutcome> {
    let (pid, mut rx) = spawn_mount_process_inner(volume_id, mountpoint, stdin_payload)?;
    let mut last_stderr_line: Option<String> = None;
    let mut reported: Option<(MountErrorCode, String)> = None;

    loop {
        match rx.recv().await {
            Some(WorkerEvent::Stdout(l)) if l.trim() == MOUNT_READY_SENTINEL => {
                return Ok(MountOutcome::Ready { pid });
            }
            Some(WorkerEvent::Stdout(l)) => {
                if let Some(err) = parse_mount_error_line(&l) {
                    reported = Some(err);
                }
            }
            Some(WorkerEvent::Stderr(l)) => last_stderr_line = Some(l),
            Some(WorkerEvent::Exited(status)) => {
                if let Some((code, message)) = reported {
                    return Ok(MountOutcome::Failed { code, message });
                }
                let message = last_stderr_line.clone().unwrap_or_else(|| match status {
                    Ok(s) => format!("mount worker exited ({s}) with no error output"),
                    Err(e) => format!("failed to wait for mount worker: {e}"),
                });
                return Ok(MountOutcome::Failed {
                    code: MountErrorCode::Unknown,
                    message,
                });
            }
            None => {
                return Ok(MountOutcome::Failed {
                    code: MountErrorCode::Unknown,
                    message: "mount worker's status channel closed unexpectedly".to_string(),
                });
            }
        }
    }
}

/// Spawns the `volume mount-worker` child process for `volume_id`, pipes
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
    let mut payload: Zeroizing<Vec<u8>> = Zeroizing::new(vec![0x00u8]);
    payload.extend_from_slice(password.as_bytes());
    run_and_wait_for_outcome(volume_id, mountpoint, payload).await
}

/// Spawns a mount worker for a new-scheme Default-mode volume, handing it
/// only that volume's derived master key (stdin tag `0x02`, see
/// `read_mount_secret_from_stdin`). The worker never receives the app
/// vault's master key, which would also decrypt every module's data; the
/// caller derives the volume key itself (`registry::
/// derive_default_volume_master_key`) and drops the app key right after.
///
/// The worker still ACCEPTS the older `0x01` (app master key) tag, for a
/// worker spawned by a pre-P1 binary during an upgrade; nothing sends it
/// any more, and it goes away with P9's agent-side volume supervisor.
pub async fn spawn_mount_process_with_volume_key(
    volume_id: &str,
    mountpoint: &str,
    volume_key: &SecretBox<SafeKey>,
) -> Result<MountOutcome> {
    use secrecy::ExposeSecret;
    let mut payload: Zeroizing<Vec<u8>> = Zeroizing::new(vec![0x02u8]);
    payload.extend_from_slice(&volume_key.expose_secret().0);
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

    // Graceful stop didn't work: force it. The kill command's own result
    // is only diagnostic; whether the worker is really gone is checked
    // right after, and that's what decides the outcome.
    #[cfg(windows)]
    let kill = std::process::Command::new("taskkill")
        .args(["/PID", &worker_pid.to_string(), "/F"])
        .output();
    #[cfg(not(windows))]
    let kill = std::process::Command::new("kill")
        .arg(worker_pid.to_string())
        .output();

    // Async sleep: this runs on the tokio runtime (TUI/CLI unmount).
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    if status::pid_is_alive(worker_pid) {
        let detail = match kill {
            Ok(out) => String::from_utf8_lossy(&out.stderr).trim().to_string(),
            Err(e) => e.to_string(),
        };
        bail!("could not stop the mount worker (pid {worker_pid}): {detail}");
    }
    crate::pid::remove(volume_id);
    Ok(StopOutcome::Forced)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mount_error_line_round_trips_code_and_message() {
        let line = mount_error_line(MountErrorCode::WrongPassword, "wrong moku vault password");
        assert_eq!(line, "MOKU_MOUNT_ERR wrong_password\twrong moku vault password");
        assert_eq!(
            parse_mount_error_line(&line),
            Some((MountErrorCode::WrongPassword, "wrong moku vault password".to_string()))
        );
    }

    #[test]
    fn test_mount_error_line_keeps_a_multiline_message_on_one_line() {
        let line = mount_error_line(MountErrorCode::Other, "a\nb\tc");
        assert_eq!(line.lines().count(), 1);
        assert_eq!(parse_mount_error_line(&line).unwrap().1, "a b c");
    }

    #[test]
    fn test_parse_mount_error_line_ignores_other_lines_and_unknown_codes() {
        assert_eq!(parse_mount_error_line("MOKU_MOUNT_READY"), None);
        assert_eq!(parse_mount_error_line("some log output"), None);
        assert_eq!(
            parse_mount_error_line("MOKU_MOUNT_ERR from_the_future\tx"),
            Some((MountErrorCode::Unknown, "x".to_string()))
        );
    }

    #[test]
    fn test_parse_mount_secret_tags() {
        use secrecy::ExposeSecret;
        let mut vk = vec![0x02u8];
        vk.extend_from_slice(&[9u8; 32]);
        match parse_mount_secret(&vk).unwrap() {
            registry::MountSecret::VolumeKey(k) => assert_eq!(k.expose_secret().0, [9u8; 32]),
            _ => panic!("0x02 must decode to a derived volume key"),
        }
        let mut app = vec![0x01u8];
        app.extend_from_slice(&[7u8; 32]);
        assert!(matches!(parse_mount_secret(&app).unwrap(), registry::MountSecret::Key(_)));
        match parse_mount_secret(b"\x00hunter2\n").unwrap() {
            registry::MountSecret::Password(p) => assert_eq!(p, "hunter2"),
            _ => panic!("0x00 must decode to a password"),
        }
        assert!(parse_mount_secret(&[0x02, 1, 2, 3]).is_err(), "a short key is malformed");
        assert!(parse_mount_secret(&[0x7f]).is_err());
    }
}
