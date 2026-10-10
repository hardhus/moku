use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::MigrateError;

/// The marker file at the data directory root, next to `vault/`.
pub const FORMAT_FILE: &str = "FORMAT";

/// One monotonic version number for the whole data directory. Every
/// on-disk format change (key scheme, `meta.json`, store backend, ...) is
/// one step of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DataFormat(pub u32);

impl fmt::Display for DataFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// The format this build writes and the target of every upgrade.
///
/// - `0`: sled store, records under the raw root key (`KeyScheme` V0).
/// - `1`: sled store, per-module HKDF subkeys (`KeyScheme` V1).
pub const CURRENT_DATA_FORMAT: DataFormat = DataFormat(1);

/// The `FORMAT` file. `data_format` is the only field anything decides on;
/// the rest is informational (for a human, or a support question).
/// Unknown fields are accepted, so a newer moku may add some.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FormatFile {
    pub data_format: u32,
    /// The moku-bin version that last wrote this file.
    #[serde(default)]
    pub written_by: String,
    #[serde(default)]
    pub components: Components,
}

/// What each part of the data directory looks like in a given format.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Components {
    #[serde(default)]
    pub store: String,
    #[serde(default)]
    pub key_scheme: u32,
    /// `vault/meta.json` version.
    #[serde(default)]
    pub meta: u32,
}

impl FormatFile {
    pub fn new(format: DataFormat, written_by: &str) -> Self {
        Self {
            data_format: format.0,
            written_by: written_by.to_string(),
            components: components_of(format),
        }
    }

    pub fn format(&self) -> DataFormat {
        DataFormat(self.data_format)
    }
}

/// The informational component breakdown of each data format. A new hop
/// adds its target format here.
pub(crate) fn components_of(format: DataFormat) -> Components {
    let (store, key_scheme, meta) = match format.0 {
        0 => ("sled", 0, 1),
        1 => ("sled", 1, 1),
        _ => ("unknown", 0, 0),
    };
    Components {
        store: store.to_string(),
        key_scheme,
        meta,
    }
}

pub(crate) fn format_path(data_dir: &Path) -> PathBuf {
    data_dir.join(FORMAT_FILE)
}

/// Reads `FORMAT`. A missing file is `Ok(None)`; anything else that keeps
/// it from being read and parsed is `FormatUnreadable` (never guessed).
pub fn read_format(data_dir: &Path) -> Result<Option<FormatFile>, MigrateError> {
    let path = format_path(data_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(MigrateError::FormatUnreadable {
                path,
                source: Box::new(e),
            });
        }
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|e| MigrateError::FormatUnreadable {
            path,
            source: Box::new(e),
        })
}

pub(crate) fn write_format(
    data_dir: &Path,
    format: DataFormat,
    written_by: &str,
) -> Result<(), MigrateError> {
    let path = format_path(data_dir);
    let body = toml::to_string(&FormatFile::new(format, written_by)).map_err(|e| MigrateError::Io {
        what: "encode the FORMAT marker",
        path: path.clone(),
        source: io::Error::other(e),
    })?;
    let text = format!(
        "# moku data directory format. Written by moku; do not edit.\n\
         # Only `data_format` is read; the other fields are informational.\n\n{body}"
    );
    moku_sys::util::atomic_write(&path, text.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        write_format(dir.path(), DataFormat(1), "0.2.12").unwrap();
        let read = read_format(dir.path()).unwrap().unwrap();
        assert_eq!(read, FormatFile::new(DataFormat(1), "0.2.12"));
        assert_eq!(read.components.store, "sled");
        assert_eq!(read.components.key_scheme, 1);
    }

    #[test]
    fn test_missing_format_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_format(dir.path()).unwrap().is_none());
    }

    #[test]
    fn test_garbage_format_file_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FORMAT_FILE), "not = [toml").unwrap();
        assert!(matches!(
            read_format(dir.path()),
            Err(MigrateError::FormatUnreadable { .. })
        ));
        std::fs::write(dir.path().join(FORMAT_FILE), "written_by = \"x\"\n").unwrap();
        assert!(
            matches!(
                read_format(dir.path()),
                Err(MigrateError::FormatUnreadable { .. })
            ),
            "a file without data_format is unreadable, not format 0"
        );
    }

    #[test]
    fn test_unknown_fields_are_accepted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FORMAT_FILE),
            "data_format = 1\nfuture_field = true\n[components]\nstore = \"sled\"\nnew_part = 3\n",
        )
        .unwrap();
        assert_eq!(
            read_format(dir.path()).unwrap().unwrap().format(),
            DataFormat(1)
        );
    }
}
