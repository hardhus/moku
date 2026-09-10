//! The pomodoro daemon's control channel — a singleton (not per-item, like
//! `moku-volume-daemon`'s per-volume channel), reached at a fixed
//! well-known path: a named pipe on Windows, a unix socket under the data
//! dir on Unix. Framed JSON request/response (see `protocol.rs`) instead
//! of the volume daemon's 1-byte stop signal, since `Start`'s `Plan` and a
//! `QueryStatus` response are real structured payloads.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::protocol::{PomodoroRequest, PomodoroResponse, read_framed, write_framed};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Transport for T {}

/// One accepted (server-side) or connected (client-side) end of the
/// control channel. Wraps the platform-specific stream type behind a
/// trait object so the rest of the crate never needs `#[cfg(windows)]`/
/// `#[cfg(unix)]` branches of its own.
pub struct Connection(Box<dyn Transport>);

impl Connection {
    /// Client side: send one request, wait for the matching response.
    pub async fn request(&mut self, req: &PomodoroRequest) -> Result<PomodoroResponse> {
        write_framed(&mut self.0, req).await?;
        read_framed(&mut self.0).await
    }

    /// Server side: read the next request off an accepted connection.
    pub async fn recv_request(&mut self) -> Result<PomodoroRequest> {
        read_framed(&mut self.0).await
    }

    /// Server side: answer the request just read via `recv_request`.
    pub async fn send_response(&mut self, resp: &PomodoroResponse) -> Result<()> {
        write_framed(&mut self.0, resp).await
    }
}

#[cfg(windows)]
fn pipe_name() -> &'static str {
    r"\\.\pipe\moku-pomodoro"
}

#[cfg(unix)]
fn socket_path() -> Result<std::path::PathBuf> {
    Ok(moku_core::dirs::get_data_dir()?.join("pomodoro-control.sock"))
}

/// Listens for incoming control-channel connections. Only one `Listener`
/// can ever successfully `bind()` at a time — a second daemon trying to
/// start while one is already running fails here, which is how `start`'s
/// "connect first, spawn only if nothing answers" flow stays race-free
/// against a well-behaved second daemon (a genuinely concurrent double
/// spawn is still possible in principle, same as any other PID-file-based
/// daemon in this repo; not specifically hardened against here).
pub struct Listener {
    #[cfg(windows)]
    first_instance: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
}

impl Listener {
    #[cfg(windows)]
    pub async fn bind() -> Result<Listener> {
        use tokio::net::windows::named_pipe::ServerOptions;
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(pipe_name())
            .context("failed to create control pipe (daemon already running?)")?;
        Ok(Listener {
            first_instance: Some(server),
        })
    }

    #[cfg(windows)]
    pub async fn accept(&mut self) -> Result<Connection> {
        use tokio::net::windows::named_pipe::ServerOptions;
        let server = match self.first_instance.take() {
            Some(s) => s,
            None => ServerOptions::new()
                .create(pipe_name())
                .context("failed to create a new control pipe instance")?,
        };
        server
            .connect()
            .await
            .context("failed accepting a control pipe connection")?;
        Ok(Connection(Box::new(server)))
    }

    #[cfg(unix)]
    pub async fn bind() -> Result<Listener> {
        use tokio::net::{UnixListener, UnixStream};
        let path = socket_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("failed to create data directory")?;
        }
        if UnixStream::connect(&path).await.is_ok() {
            anyhow::bail!("control socket already in use (daemon already running?)");
        }
        let _ = std::fs::remove_file(&path); // stale socket from an unclean previous exit
        let inner = UnixListener::bind(&path).context("failed to bind control socket")?;
        Ok(Listener { inner })
    }

    #[cfg(unix)]
    pub async fn accept(&mut self) -> Result<Connection> {
        let (stream, _) = self
            .inner
            .accept()
            .await
            .context("failed accepting a control socket connection")?;
        Ok(Connection(Box::new(stream)))
    }
}

#[cfg(unix)]
impl Drop for Listener {
    fn drop(&mut self) {
        if let Ok(path) = socket_path() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Client side: connects to an already-running daemon, with a bounded
/// timeout — `Err` (including "nothing is listening") is the expected,
/// common outcome when no daemon is running yet, not an exceptional case.
#[cfg(windows)]
pub async fn connect() -> Result<Connection> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let client = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::task::spawn_blocking(|| ClientOptions::new().open(pipe_name())),
    )
    .await
    .context("timed out connecting to the pomodoro daemon")?
    .context("connect task panicked")?
    .context("failed to connect to the pomodoro daemon (not running?)")?;
    Ok(Connection(Box::new(client)))
}

#[cfg(unix)]
pub async fn connect() -> Result<Connection> {
    use tokio::net::UnixStream;
    let path = socket_path()?;
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&path))
        .await
        .context("timed out connecting to the pomodoro daemon")?
        .context("failed to connect to the pomodoro daemon (not running?)")?;
    Ok(Connection(Box::new(stream)))
}

/// Convenience used by every client (TUI, CLI, the `start` spawn-retry
/// loop): connect, send one request, wait for the response — all under a
/// single bounded timeout.
pub async fn send_request(req: &PomodoroRequest) -> Result<PomodoroResponse> {
    let mut conn = connect().await?;
    tokio::time::timeout(REQUEST_TIMEOUT, conn.request(req))
        .await
        .context("timed out waiting for the pomodoro daemon's response")?
}
