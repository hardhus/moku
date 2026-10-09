use std::io;
use std::path::PathBuf;

use moku_crypto::CryptoError;
use moku_sys::dirs::DirsError;

/// Why a storage operation failed. Each variant is a case a caller can
/// act on differently: `Locked` (unlock first), `InUseByOtherProcess`
/// (close the other moku), `Tampered`/`Corrupt`/`SchemaMismatch`/
/// `MissingBlob` (data is stored but can't be read; never treat it as
/// empty), versus plain I/O or backend failures.
///
/// "Nothing stored yet" is not an error: `StorageManager::load_opt`
/// returns `Ok(None)` for it. `NotFound` only comes from `load`, which
/// requires the record to exist.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the vault is locked")]
    Locked,
    #[error("no record '{key}' in module '{module}'")]
    NotFound { module: String, key: String },
    #[error("module '{module}' storage is open in another moku process")]
    InUseByOtherProcess {
        module: String,
        #[source]
        source: sled::Error,
    },
    #[error(
        "record '{key}' in module '{module}' failed authentication (wrong key, or it was modified on disk)"
    )]
    Tampered { module: String, key: String },
    #[error("record '{key}' in module '{module}' is corrupt")]
    Corrupt {
        module: String,
        key: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("record '{key}' in module '{module}' doesn't match the expected data shape")]
    SchemaMismatch {
        module: String,
        key: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("external blob for record '{key}' in module '{module}' is missing")]
    MissingBlob {
        module: String,
        key: String,
        #[source]
        source: io::Error,
    },
    #[error("migrating record '{key}' in module '{module}' failed: {message}")]
    Transform {
        module: String,
        key: String,
        message: String,
    },
    #[error("failed to {what} at {}", path.display())]
    Io {
        what: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to encode a record")]
    Encode(#[source] serde_json::Error),
    #[error("storage backend error")]
    Backend(#[from] sled::Error),
    #[error("a background storage task failed")]
    Task(#[from] tokio::task::JoinError),
    #[error("couldn't locate the moku data directory")]
    DataDir(#[from] DirsError),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

impl StoreError {
    pub(crate) fn io(what: &'static str, path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> Self {
        let path = path.into();
        move |source| StoreError::Io { what, path, source }
    }

    /// sled reports "another process holds this DB's lock" only as an
    /// `Io(ErrorKind::Other)` whose message starts with "could not acquire
    /// lock" (sled 0.34, `config.rs::try_lock`). That is the one case
    /// mapped to `InUseByOtherProcess`; every other open error stays
    /// `Backend`.
    pub(crate) fn from_open(module: &str, e: sled::Error) -> Self {
        let held = matches!(&e, sled::Error::Io(io) if io.to_string().starts_with("could not acquire lock"));
        if held {
            StoreError::InUseByOtherProcess {
                module: module.to_string(),
                source: e,
            }
        } else {
            StoreError::Backend(e)
        }
    }
}
