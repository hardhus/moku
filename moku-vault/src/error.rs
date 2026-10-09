use std::io;
use std::path::PathBuf;

use moku_crypto::CryptoError;
use moku_sys::dirs::DirsError;
use moku_sys::util::AtomicWriteError;

/// Why opening, creating or prompting for the vault failed. The variants
/// are the distinctions a caller acts on differently: above all
/// `WrongPassword` (ask again) vs. everything else (a disk, permission or
/// corruption problem that retyping the password won't fix).
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("the moku vault hasn't been set up yet: open `moku` (the TUI) once and create it there")]
    NotInitialized,
    #[error("a vault already exists at {}; refusing to overwrite it", .0.display())]
    AlreadyInitialized(PathBuf),
    #[error("incorrect password")]
    WrongPassword,
    #[error("vault metadata at {} is corrupt", path.display())]
    MetadataCorrupt {
        path: PathBuf,
        #[source]
        source: Option<serde_json::Error>,
    },
    #[error("couldn't access vault metadata at {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("couldn't locate the moku data directory")]
    DataDir(#[from] DirsError),
    #[error("couldn't save vault metadata")]
    Write(#[from] AtomicWriteError),
    #[error("couldn't read the password from the terminal")]
    Prompt(#[source] io::Error),
    #[error("password cannot be empty")]
    EmptyPassword,
    #[error("passwords did not match")]
    PasswordMismatch,
    #[error("the key derivation task failed")]
    Task(#[source] tokio::task::JoinError),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}
