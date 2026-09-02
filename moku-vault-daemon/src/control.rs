//! Graceful-unmount control channel (plan Faz 5). The worker process opens
//! this right after it starts, so `moku vault unmount` can ask it to stop
//! cleanly (which lets `mount_and_wait` reach its `engine.flush_usage()`
//! and unmount steps) instead of always hard-killing the process, which
//! skips both.
//!
//! One instance per volume, named by volume id. Windows uses a named
//! pipe; Unix a domain socket under the volume's own directory. Both use
//! `tokio::net` — already a workspace dependency via the "full" feature,
//! no new crate needed.

use std::sync::mpsc::Sender;

use anyhow::{Context, Result};

/// Listens for a single stop signal on this volume's control channel and
/// forwards it to `stop_tx` — the same channel the worker's Ctrl+C
/// listener already feeds, so `mount_and_wait` doesn't need to know which
/// source asked it to stop.
#[cfg(windows)]
pub async fn listen_for_stop(volume_id: &str, stop_tx: Sender<()>) -> Result<()> {
    use tokio::io::AsyncReadExt;
    use tokio::net::windows::named_pipe::ServerOptions;

    let pipe_name = pipe_name(volume_id);
    let server = ServerOptions::new().first_pipe_instance(true).create(&pipe_name).context("failed to create control pipe")?;
    server.connect().await.context("failed waiting for control pipe connection")?;

    let mut buf = [0u8; 1];
    let mut server = server;
    let _ = server.read(&mut buf).await;
    let _ = stop_tx.send(());
    Ok(())
}

#[cfg(windows)]
pub async fn send_stop(volume_id: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::windows::named_pipe::ClientOptions;

    let pipe_name = pipe_name(volume_id);
    let mut client = ClientOptions::new().open(&pipe_name).context("failed to connect to control pipe (worker not running?)")?;
    client.write_all(&[1u8]).await.context("failed to send stop signal")?;
    Ok(())
}

#[cfg(windows)]
fn pipe_name(volume_id: &str) -> String {
    format!(r"\\.\pipe\moku-vault-{volume_id}")
}

#[cfg(unix)]
pub async fn listen_for_stop(volume_id: &str, stop_tx: Sender<()>) -> Result<()> {
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    let path = socket_path(volume_id)?;
    let _ = std::fs::remove_file(&path); // stale socket from a previous unclean exit
    let listener = UnixListener::bind(&path).context("failed to bind control socket")?;
    let (mut stream, _) = listener.accept().await.context("failed accepting control socket connection")?;

    let mut buf = [0u8; 1];
    let _ = stream.read(&mut buf).await;
    let _ = stop_tx.send(());
    let _ = std::fs::remove_file(&path);
    Ok(())
}

#[cfg(unix)]
pub async fn send_stop(volume_id: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    let path = socket_path(volume_id)?;
    let mut stream = UnixStream::connect(&path).await.context("failed to connect to control socket (worker not running?)")?;
    stream.write_all(&[1u8]).await.context("failed to send stop signal")?;
    Ok(())
}

#[cfg(unix)]
fn socket_path(volume_id: &str) -> Result<std::path::PathBuf> {
    Ok(crate::registry::volume_dir(volume_id)?.join("control.sock"))
}
