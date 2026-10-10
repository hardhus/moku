use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::format::format_path;
use crate::{DataFormat, FORMAT_FILE, MigrateError};

/// Where pre-upgrade backups go, under the data directory. Backups are
/// never deleted automatically (audit KN-4.2); `moku data backups` lists
/// them.
pub const BACKUPS_DIR: &str = "backups";

const PARTIAL_PREFIX: &str = ".partial-";

/// A finished backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupInfo {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    /// Seconds since the Unix epoch, from the directory name.
    pub created_unix: u64,
    /// The format the backed-up data was in.
    pub from: DataFormat,
}

/// Copies `vault/` (and `FORMAT`, if present) to
/// `backups/data-v<from>-<unix>/`. The copy is made under a `.partial-`
/// name, fsynced, then renamed, so a half-made backup never looks like a
/// finished one; leftovers of an interrupted backup are removed first.
/// Only `vault/` is copied: it is all a data hop changes (logs, daemon
/// state and the volume index are not data-format files). Returns `None`
/// when there is no `vault/` to back up.
pub fn create_backup(data_dir: &Path, from: DataFormat) -> Result<Option<PathBuf>, MigrateError> {
    let vault = data_dir.join("vault");
    if !vault.exists() {
        return Ok(None);
    }
    let root = data_dir.join(BACKUPS_DIR);
    fs::create_dir_all(&root).map_err(MigrateError::io("create the backups directory", &root))?;
    remove_partials(&root)?;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut name = format!("data-v{}-{stamp}", from.0);
    let mut n = 2;
    while root.join(&name).exists() {
        name = format!("data-v{}-{stamp}-{n}", from.0);
        n += 1;
    }

    let partial = root.join(format!("{PARTIAL_PREFIX}{name}"));
    copy_dir(&vault, &partial.join("vault"))?;
    let format = format_path(data_dir);
    if format.exists() {
        copy_file(&format, &partial.join(FORMAT_FILE))?;
    }
    let done = root.join(&name);
    fs::rename(&partial, &done).map_err(MigrateError::io("finish the backup", &done))?;
    Ok(Some(done))
}

/// Finished backups, oldest first. Partial (interrupted) ones are skipped.
pub fn list_backups(data_dir: &Path) -> Result<Vec<BackupInfo>, MigrateError> {
    let root = data_dir.join(BACKUPS_DIR);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(MigrateError::io("list the backups directory", &root)(e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(MigrateError::io("list the backups directory", &root))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((from, created_unix)) = parse_name(&name) else {
            continue;
        };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        out.push(BackupInfo {
            bytes: dir_size(&path)?,
            name,
            path,
            created_unix,
            from,
        });
    }
    out.sort_by(|a, b| (a.created_unix, &a.name).cmp(&(b.created_unix, &b.name)));
    Ok(out)
}

/// `data-v<from>-<unix>[-<n>]` → `(from, unix)`.
fn parse_name(name: &str) -> Option<(DataFormat, u64)> {
    let rest = name.strip_prefix("data-v")?;
    let mut parts = rest.split('-');
    let from = parts.next()?.parse().ok()?;
    let unix = parts.next()?.parse().ok()?;
    Some((DataFormat(from), unix))
}

fn remove_partials(root: &Path) -> Result<(), MigrateError> {
    let entries =
        fs::read_dir(root).map_err(MigrateError::io("list the backups directory", root))?;
    for entry in entries {
        let entry = entry.map_err(MigrateError::io("list the backups directory", root))?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(PARTIAL_PREFIX)
        {
            let path = entry.path();
            fs::remove_dir_all(&path)
                .map_err(MigrateError::io("remove an interrupted backup", &path))?;
        }
    }
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<(), MigrateError> {
    fs::create_dir_all(dst).map_err(MigrateError::io("create a backup directory", dst))?;
    let entries =
        fs::read_dir(src).map_err(MigrateError::io("read a directory to back up", src))?;
    for entry in entries {
        let entry = entry.map_err(MigrateError::io("read a directory to back up", src))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(MigrateError::io("read a directory to back up", &from))?;
        if kind.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            copy_file(&from, &to)?;
        }
    }
    Ok(())
}

fn copy_file(from: &Path, to: &Path) -> Result<(), MigrateError> {
    fs::copy(from, to).map_err(MigrateError::io("copy a file into the backup", from))?;
    // Windows only flushes a handle opened for writing.
    fs::OpenOptions::new()
        .write(true)
        .open(to)
        .and_then(|f| f.sync_all())
        .map_err(MigrateError::io("flush a backup file", to))
}

fn dir_size(dir: &Path) -> Result<u64, MigrateError> {
    let mut total = 0;
    let entries = fs::read_dir(dir).map_err(MigrateError::io("read a backup", dir))?;
    for entry in entries {
        let entry = entry.map_err(MigrateError::io("read a backup", dir))?;
        let meta = entry
            .metadata()
            .map_err(MigrateError::io("read a backup", entry.path()))?;
        total += if meta.is_dir() {
            dir_size(&entry.path())?
        } else {
            meta.len()
        };
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_dir_with_vault() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("vault/todo/db")).unwrap();
        fs::write(dir.path().join("vault/meta.json"), b"{}").unwrap();
        fs::write(dir.path().join("vault/todo/db/conf"), b"12345").unwrap();
        fs::create_dir_all(dir.path().join("logs")).unwrap();
        fs::write(dir.path().join("logs/moku.log"), b"not backed up").unwrap();
        dir
    }

    #[test]
    fn test_backup_copies_only_the_vault() {
        let dir = data_dir_with_vault();
        let path = create_backup(dir.path(), DataFormat(0)).unwrap().unwrap();
        assert_eq!(fs::read(path.join("vault/todo/db/conf")).unwrap(), b"12345");
        assert_eq!(fs::read(path.join("vault/meta.json")).unwrap(), b"{}");
        assert!(!path.join("logs").exists());

        let list = list_backups(dir.path()).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].from, DataFormat(0));
        assert_eq!(list[0].bytes, 7);
    }

    #[test]
    fn test_partial_backups_are_not_listed_and_get_cleaned_up() {
        let dir = data_dir_with_vault();
        let partial = dir.path().join("backups/.partial-data-v0-1");
        fs::create_dir_all(&partial).unwrap();
        assert!(list_backups(dir.path()).unwrap().is_empty());

        create_backup(dir.path(), DataFormat(0)).unwrap();
        assert!(!partial.exists());
        assert_eq!(list_backups(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn test_two_backups_in_the_same_second_get_distinct_names() {
        let dir = data_dir_with_vault();
        let a = create_backup(dir.path(), DataFormat(0)).unwrap().unwrap();
        let b = create_backup(dir.path(), DataFormat(0)).unwrap().unwrap();
        assert_ne!(a, b);
        assert_eq!(list_backups(dir.path()).unwrap().len(), 2);
    }

    #[test]
    fn test_no_vault_means_no_backup() {
        let dir = tempfile::tempdir().unwrap();
        assert!(create_backup(dir.path(), DataFormat(0)).unwrap().is_none());
    }
}
