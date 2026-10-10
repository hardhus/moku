use std::io;
use std::path::Path;

use crate::format::{read_format, write_format};
use crate::{CURRENT_DATA_FORMAT, DataFormat, MigrateError};

/// The sled-era store directory, and the key-scheme marker moku-bin
/// 0.2.1–0.2.11 wrote inside it (read here only, to infer the format of
/// a data directory that predates `FORMAT`).
const VAULT_DIR: &str = "vault";
const LEGACY_KEY_SCHEME_MARKER: &str = ".key_scheme_version";

/// What a data directory is, format-wise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataState {
    /// Nothing stored yet (no `vault/`, no `FORMAT`).
    Fresh,
    /// Already on the current format. `stamped == false`: no `FORMAT` yet,
    /// the format was inferred (`stamp_if_needed` writes it).
    Current { stamped: bool },
    /// Older than this build: `upgrade` runs the pending hops.
    Behind { from: DataFormat, stamped: bool },
    /// Written by a newer moku; must not be opened (audit KN-4.5).
    Newer { found: DataFormat },
}

impl DataState {
    /// The format the directory is in, if it has any data.
    pub fn format(&self) -> Option<DataFormat> {
        match *self {
            DataState::Fresh => None,
            DataState::Current { .. } => Some(CURRENT_DATA_FORMAT),
            DataState::Behind { from, .. } => Some(from),
            DataState::Newer { found } => Some(found),
        }
    }
}

/// Detects the data directory's format without unlocking anything.
///
/// - `FORMAT` present: it decides. Unreadable/unparseable is an error
///   (fail closed, never guessed).
/// - No `FORMAT`, no `vault/`: a fresh install.
/// - No `FORMAT` but `vault/`: a pre-P4 directory. Its format is the old
///   `vault/.key_scheme_version` (0 or 1). Missing, empty or garbage reads
///   as 0: that marker was written non-atomically before P4, so a torn
///   write is possible, and re-running the (idempotent) hop is the safe
///   direction. A value no moku ever wrote (> 1) is an error.
pub fn detect_format(data_dir: &Path) -> Result<DataState, MigrateError> {
    detect_with(data_dir, CURRENT_DATA_FORMAT)
}

pub(crate) fn detect_with(data_dir: &Path, current: DataFormat) -> Result<DataState, MigrateError> {
    let (found, stamped) = match read_format(data_dir)? {
        Some(file) => (file.format(), true),
        None => {
            let vault = data_dir.join(VAULT_DIR);
            if !vault.exists() {
                return Ok(DataState::Fresh);
            }
            (infer_legacy_format(&vault)?, false)
        }
    };
    Ok(if found == current {
        DataState::Current { stamped }
    } else if found < current {
        DataState::Behind {
            from: found,
            stamped,
        }
    } else {
        DataState::Newer { found }
    })
}

fn infer_legacy_format(vault: &Path) -> Result<DataFormat, MigrateError> {
    let path = vault.join(LEGACY_KEY_SCHEME_MARKER);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(DataFormat(0)),
        Err(e) => {
            return Err(MigrateError::FormatUnreadable {
                path,
                source: Box::new(e),
            });
        }
    };
    match text.trim().parse::<u32>() {
        Ok(v @ (0 | 1)) => Ok(DataFormat(v)),
        Ok(v) => Err(MigrateError::FormatUnreadable {
            path,
            source: format!("key-scheme version {v} was never written by any moku").into(),
        }),
        Err(_) => Ok(DataFormat(0)),
    }
}

/// Writes `FORMAT` for a fresh directory or one already on the current
/// format that has none yet. Not a migration: no data changes. A
/// directory that is behind is left alone (the upgrade writes `FORMAT`).
pub fn stamp_if_needed(
    data_dir: &Path,
    state: DataState,
    written_by: &str,
) -> Result<(), MigrateError> {
    match state {
        DataState::Fresh | DataState::Current { stamped: false } => {
            write_format(data_dir, CURRENT_DATA_FORMAT, written_by)
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FORMAT_FILE;

    fn legacy_dir(marker: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vault")).unwrap();
        if let Some(m) = marker {
            std::fs::write(dir.path().join("vault/.key_scheme_version"), m).unwrap();
        }
        dir
    }

    #[test]
    fn test_empty_dir_is_fresh() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_format(dir.path()).unwrap(), DataState::Fresh);
    }

    #[test]
    fn test_legacy_marker_is_inferred() {
        let behind = DataState::Behind {
            from: DataFormat(0),
            stamped: false,
        };
        assert_eq!(detect_format(legacy_dir(None).path()).unwrap(), behind);
        assert_eq!(detect_format(legacy_dir(Some("0")).path()).unwrap(), behind);
        assert_eq!(detect_format(legacy_dir(Some("")).path()).unwrap(), behind);
        assert_eq!(
            detect_format(legacy_dir(Some("garbage")).path()).unwrap(),
            behind
        );
        assert_eq!(
            detect_format(legacy_dir(Some("1")).path()).unwrap(),
            DataState::Current { stamped: false }
        );
        assert!(matches!(
            detect_format(legacy_dir(Some("5")).path()),
            Err(MigrateError::FormatUnreadable { .. })
        ));
    }

    #[test]
    fn test_format_file_wins_over_the_legacy_marker() {
        let dir = legacy_dir(Some("0"));
        write_format(dir.path(), DataFormat(1), "t").unwrap();
        assert_eq!(
            detect_format(dir.path()).unwrap(),
            DataState::Current { stamped: true }
        );
    }

    #[test]
    fn test_newer_format_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FORMAT_FILE), "data_format = 99\n").unwrap();
        assert_eq!(
            detect_format(dir.path()).unwrap(),
            DataState::Newer {
                found: DataFormat(99)
            }
        );
    }

    #[test]
    fn test_stamp_writes_only_when_current_or_fresh() {
        let fresh = tempfile::tempdir().unwrap();
        stamp_if_needed(fresh.path(), DataState::Fresh, "t").unwrap();
        assert_eq!(
            detect_format(fresh.path()).unwrap(),
            DataState::Current { stamped: true }
        );

        let current = legacy_dir(Some("1"));
        let state = detect_format(current.path()).unwrap();
        stamp_if_needed(current.path(), state, "t").unwrap();
        assert!(current.path().join(FORMAT_FILE).exists());

        let behind = legacy_dir(Some("0"));
        let state = detect_format(behind.path()).unwrap();
        stamp_if_needed(behind.path(), state, "t").unwrap();
        assert!(!behind.path().join(FORMAT_FILE).exists());
    }
}
