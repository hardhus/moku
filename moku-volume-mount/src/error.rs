/// Why mounting a volume failed, at the OS-driver level. Each variant is a
/// different fix for the user: install WinFsp/FUSE, pick another mount
/// point, or (host/dispatcher) a driver problem to report.
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("the filesystem driver isn't available ({0}); is WinFsp/FUSE installed?")]
    BackendMissing(String),
    #[error("couldn't mount at '{mountpoint}': {detail}")]
    MountPointUnavailable { mountpoint: String, detail: String },
    #[error("failed to create the filesystem host: {0}")]
    Host(String),
    #[error("failed to start the filesystem dispatcher: {0}")]
    Dispatcher(String),
    #[error("failed to unmount")]
    Unmount(#[source] std::io::Error),
}
