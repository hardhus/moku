use moku_vault::VaultError;
use moku_volume_fs::VolumeFsError;
use moku_volume_mount::MountError;

/// Why opening or mounting a volume failed. The first variants are the
/// ones a caller reacts to differently (ask for the password again, ask
/// for the volume's own password, say it's already mounted, point at the
/// driver); the rest carry the underlying typed error.
#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    /// The password didn't open the vault it was checked against. The
    /// message names which one ("wrong password for volume 'x'" vs.
    /// "wrong moku vault password").
    #[error("{0}")]
    WrongPassword(String),
    #[error(
        "'{0}' needs its own password to mount; it can't be unlocked from an already-open moku vault"
    )]
    NeedsOwnPassword(String),
    #[error("volume '{0}' is already mounted (or in use by another moku process)")]
    AlreadyMounted(String),
    #[error(transparent)]
    Vault(#[from] VaultError),
    #[error(transparent)]
    Fs(#[from] VolumeFsError),
    #[error(transparent)]
    Mount(#[from] MountError),
    /// Registry/config/process plumbing (volume.json, the index, stdin,
    /// ...): nothing a caller branches on, so it stays an `anyhow` chain.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl VolumeError {
    pub fn code(&self) -> MountErrorCode {
        match self {
            VolumeError::WrongPassword(_) => MountErrorCode::WrongPassword,
            VolumeError::NeedsOwnPassword(_) => MountErrorCode::NeedsOwnPassword,
            VolumeError::AlreadyMounted(_) => MountErrorCode::AlreadyMounted,
            VolumeError::Mount(MountError::BackendMissing(_)) => MountErrorCode::BackendMissing,
            VolumeError::Mount(MountError::MountPointUnavailable { .. }) => {
                MountErrorCode::MountPointUnavailable
            }
            VolumeError::Fs(
                VolumeFsError::Corrupt(_)
                | VolumeFsError::MissingDirIv { .. }
                | VolumeFsError::Integrity(_),
            )
            | VolumeError::Vault(VaultError::MetadataCorrupt { .. }) => MountErrorCode::Corrupt,
            _ => MountErrorCode::Other,
        }
    }
}

/// The error class a mount worker reports to its parent process on the
/// `MOKU_MOUNT_ERR <code>\t<message>` stdout line (see `worker`), so the
/// parent can react to it (e.g. re-ask for a mistyped password) instead of
/// string-matching stderr. The codes are a wire format between two
/// processes that may come from different moku versions mid-upgrade:
/// existing codes never change meaning, and an unrecognized one parses as
/// `Unknown`. P8/P9 move this set into the agent's IPC protocol unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountErrorCode {
    WrongPassword,
    NeedsOwnPassword,
    AlreadyMounted,
    BackendMissing,
    MountPointUnavailable,
    Corrupt,
    Other,
    /// No code was reported (a worker that crashed or predates the
    /// protocol), or one this binary doesn't know.
    Unknown,
}

impl MountErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            MountErrorCode::WrongPassword => "wrong_password",
            MountErrorCode::NeedsOwnPassword => "needs_own_password",
            MountErrorCode::AlreadyMounted => "already_mounted",
            MountErrorCode::BackendMissing => "backend_missing",
            MountErrorCode::MountPointUnavailable => "mount_point_unavailable",
            MountErrorCode::Corrupt => "corrupt",
            MountErrorCode::Other => "other",
            MountErrorCode::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "wrong_password" => MountErrorCode::WrongPassword,
            "needs_own_password" => MountErrorCode::NeedsOwnPassword,
            "already_mounted" => MountErrorCode::AlreadyMounted,
            "backend_missing" => MountErrorCode::BackendMissing,
            "mount_point_unavailable" => MountErrorCode::MountPointUnavailable,
            "corrupt" => MountErrorCode::Corrupt,
            "other" => MountErrorCode::Other,
            _ => MountErrorCode::Unknown,
        }
    }
}
