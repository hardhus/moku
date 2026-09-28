//! Cross-process volume lock.
//!
//! A volume's backing directory must only ever be opened by one
//! `VolumeEngine` at a time. Each engine keeps its own in-memory caches
//! (directory IVs, file ids) and quota counter, and tail writes are
//! read-modify-write, so two engines on the same directory (two mount
//! workers for one volume, or a mount plus a future backup reader) can
//! corrupt data and overwrite each other's `usage.json`. `VolumeLock` is an
//! OS-level advisory lock on `<volume_dir>/volume.lock`. The OS releases
//! it automatically when the owning process exits, even after a crash,
//! so a stale lock file never blocks a later mount.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use crate::types::{VResult, VolumeFsError};

/// File name of the lock inside a volume's directory (next to
/// `volume.json`, not inside `data/`).
pub const LOCK_FILE: &str = "volume.lock";

/// How a volume is held. Only `Exclusive` exists today: a mount worker is
/// a writer and must be alone. A shared, read-only mode (e.g. for a
/// future backup reader) would slot in here without changing callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    Exclusive,
}

/// Held for as long as a `VolumeEngine` has the volume open; dropping it
/// (or the process exiting) releases the OS lock.
#[derive(Debug)]
pub struct VolumeLock {
    _file: File,
    mode: LockMode,
    path: PathBuf,
}

impl VolumeLock {
    /// Takes the exclusive lock on `volume_dir`, failing fast with
    /// `VolumeFsError::Locked` (never blocking) if another process or
    /// another engine in this process already holds it.
    pub fn acquire(volume_dir: &Path) -> VResult<Self> {
        let path = volume_dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        file.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => VolumeFsError::Locked,
            TryLockError::Error(io) => VolumeFsError::from(io),
        })?;
        Ok(Self {
            _file: file,
            mode: LockMode::Exclusive,
            path,
        })
    }

    /// Whether some engine currently holds `volume_dir`, checked by trying
    /// the lock and releasing it immediately. Lets the CLI and TUI say
    /// "already mounted" before asking for a password (and before Argon2
    /// runs). It is only a hint: the mount worker still takes the real
    /// lock itself.
    pub fn is_held(volume_dir: &Path) -> VResult<bool> {
        match Self::acquire(volume_dir) {
            Ok(_released_on_drop) => Ok(false),
            Err(VolumeFsError::Locked) => Ok(true),
            Err(e) => Err(e),
        }
    }

    pub fn mode(&self) -> LockMode {
        self.mode
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_second_acquire_is_rejected_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let first = VolumeLock::acquire(dir.path()).unwrap();
        assert_eq!(first.mode(), LockMode::Exclusive);
        assert!(matches!(
            VolumeLock::acquire(dir.path()),
            Err(VolumeFsError::Locked)
        ));
        assert!(VolumeLock::is_held(dir.path()).unwrap());
        drop(first);
        assert!(!VolumeLock::is_held(dir.path()).unwrap());
        VolumeLock::acquire(dir.path()).unwrap();
    }

    #[test]
    fn test_is_held_does_not_keep_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!VolumeLock::is_held(dir.path()).unwrap());
        // `is_held` must have released it again.
        VolumeLock::acquire(dir.path()).unwrap();
    }

    #[test]
    fn test_acquire_fails_for_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(VolumeLock::acquire(&dir.path().join("missing")).is_err());
    }
}
