use std::io;
use std::path::PathBuf;

use moku_sys::util::AtomicWriteError;

use crate::DataFormat;

/// One record a hop could not convert. Collected per hop so the user sees
/// every failure at once instead of only the first.
#[derive(Debug, Clone)]
pub struct RecordFailure {
    pub module: String,
    pub key: String,
    pub reason: String,
}

/// Why detecting or upgrading the data format failed.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error("the vault is locked; unlock it to upgrade the data format")]
    Locked,
    #[error(
        "module '{module}' storage is open in another moku process (close other moku windows and stop the daemon)"
    )]
    InUse {
        module: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("can't read the data format marker {}", path.display())]
    FormatUnreadable {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error(
        "this data directory is in data format {found}, newer than this moku supports ({supported}); use a newer moku"
    )]
    NewerFormat {
        found: DataFormat,
        supported: DataFormat,
    },
    #[error("no migration path from data format {from}")]
    NoPath { from: DataFormat },
    #[error("data format step \"{hop}\" failed")]
    HopFailed {
        hop: &'static str,
        #[source]
        source: Box<MigrateError>,
    },
    #[error("{} record(s) could not be upgraded: {}", failures.len(), summarize(failures))]
    Records { failures: Vec<RecordFailure> },
    #[error("failed to {what} at {}", path.display())]
    Io {
        what: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to write a data file")]
    Write(#[from] AtomicWriteError),
    #[error("storage backend error")]
    Sled(#[from] sled::Error),
}

impl MigrateError {
    pub(crate) fn io(
        what: &'static str,
        path: impl Into<PathBuf>,
    ) -> impl FnOnce(io::Error) -> Self {
        let path = path.into();
        move |source| MigrateError::Io { what, path, source }
    }

    /// sled reports a DB held by another process (or another handle in this
    /// one) only as an `Io` error starting with "could not acquire lock"
    /// (sled 0.34, `config.rs::try_lock`); same check as moku-store's.
    pub(crate) fn from_open(module: &str, e: sled::Error) -> Self {
        let held = matches!(&e, sled::Error::Io(io) if io.to_string().starts_with("could not acquire lock"));
        if held {
            MigrateError::InUse {
                module: module.to_string(),
                source: Box::new(e),
            }
        } else {
            MigrateError::Sled(e)
        }
    }
}

fn summarize(failures: &[RecordFailure]) -> String {
    const SHOWN: usize = 3;
    let mut parts: Vec<String> = failures
        .iter()
        .take(SHOWN)
        .map(|f| format!("{}/{}: {}", f.module, f.key, f.reason))
        .collect();
    if failures.len() > SHOWN {
        parts.push(format!("and {} more", failures.len() - SHOWN));
    }
    parts.join("; ")
}
