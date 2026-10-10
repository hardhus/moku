use std::path::{Path, PathBuf};

use moku_vault::VaultSession;

use crate::backup::create_backup;
use crate::detect::detect_with;
use crate::format::write_format;
use crate::hop::pending_in;
use crate::{
    CURRENT_DATA_FORMAT, DataFormat, DataState, HOPS, Hop, HopReport, MigrateError, MigrationCtx,
    Progress,
};

/// What `upgrade` did.
#[derive(Debug, Clone, Default)]
pub struct UpgradeReport {
    pub from: Option<DataFormat>,
    pub to: Option<DataFormat>,
    /// The backup taken before the first hop, if there was data to back up.
    pub backup: Option<PathBuf>,
    pub hops: Vec<(&'static str, HopReport)>,
}

impl UpgradeReport {
    /// One line for a toast or the CLI.
    pub fn summary(&self) -> String {
        let (Some(from), Some(to)) = (self.from, self.to) else {
            return "Data format is already up to date".to_string();
        };
        let records: usize = self.hops.iter().map(|(_, r)| r.migrated).sum();
        let mut line =
            format!("Data format upgraded {from} → {to} ({records} record(s) converted)");
        if let Some(backup) = &self.backup {
            line.push_str(&format!("; backup: {}", backup.display()));
        }
        line
    }
}

/// Upgrades `data_dir` to `CURRENT_DATA_FORMAT`.
///
/// 1. Re-detects the format (a `Newer` or unreadable marker is an error).
/// 2. Refuses with `Locked` if a pending hop needs the root key and the
///    session is locked.
/// 3. Backs up `vault/` (audit KN-4.2).
/// 4. Runs each pending hop and writes `FORMAT` right after it succeeds,
///    so an interrupted upgrade resumes at the first unfinished hop.
///
/// Synchronous (sled and file IO): async callers run it on
/// `spawn_blocking`. The caller makes sure no other handle holds the
/// module DBs (the TUI closes its own; the daemon is stopped first).
/// `written_by` is the moku-bin version, recorded in `FORMAT`.
pub fn upgrade(
    data_dir: &Path,
    session: &VaultSession,
    written_by: &str,
    progress: &dyn Fn(Progress),
) -> Result<UpgradeReport, MigrateError> {
    upgrade_with(
        data_dir,
        session,
        written_by,
        progress,
        HOPS,
        CURRENT_DATA_FORMAT,
    )
}

pub(crate) fn upgrade_with(
    data_dir: &Path,
    session: &VaultSession,
    written_by: &str,
    progress: &dyn Fn(Progress),
    hops: &'static [Hop],
    current: DataFormat,
) -> Result<UpgradeReport, MigrateError> {
    let from = match detect_with(data_dir, current)? {
        DataState::Fresh | DataState::Current { .. } => return Ok(UpgradeReport::default()),
        DataState::Newer { found } => {
            return Err(MigrateError::NewerFormat {
                found,
                supported: current,
            });
        }
        DataState::Behind { from, .. } => from,
    };
    let pending = pending_in(hops, from);
    if pending.last().map(|h| h.to) != Some(current) {
        return Err(MigrateError::NoPath { from });
    }
    if pending.iter().any(|h| h.needs_unlock) && !session.is_unlocked() {
        return Err(MigrateError::Locked);
    }

    // A DB another process (or another handle in this one) still holds
    // can't be copied consistently or converted: fail before touching
    // anything, with the error that says what to close.
    ensure_stores_not_in_use(data_dir)?;
    let backup = create_backup(data_dir, from)?;
    let mut report = UpgradeReport {
        from: Some(from),
        to: Some(current),
        backup,
        hops: Vec::new(),
    };
    for hop in pending {
        let ctx = MigrationCtx {
            data_dir,
            session,
            hop: hop.description,
            progress,
        };
        let hop_report = (hop.run)(&ctx).map_err(|e| MigrateError::HopFailed {
            hop: hop.description,
            source: Box::new(e),
        })?;
        write_format(data_dir, hop.to, written_by)?;
        report.hops.push((hop.description, hop_report));
    }
    Ok(report)
}

/// Fails with `MigrateError::InUse` if any sled-era module DB under
/// `vault/` is held by another process (or another handle in this one).
///
/// Probes the same exclusive lock sled takes on `db/db` (sled 0.34,
/// `config.rs::try_lock`: fs2's `try_lock_exclusive`, i.e. `LockFileEx` /
/// `flock`, which std's `File::try_lock` also uses) instead of opening the
/// DB: opening it would rewrite sled's files before the backup copies them.
fn ensure_stores_not_in_use(data_dir: &Path) -> Result<(), MigrateError> {
    let vault = data_dir.join("vault");
    let entries = match std::fs::read_dir(&vault) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(MigrateError::io("list the vault directory", &vault)(e)),
    };
    for entry in entries {
        let entry = entry.map_err(MigrateError::io("list the vault directory", &vault))?;
        let db_file = entry.path().join("db").join("db");
        if !db_file.is_file() {
            continue;
        }
        let module = entry.file_name().to_string_lossy().into_owned();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&db_file)
            .map_err(MigrateError::io("open a module database", &db_file))?;
        match file.try_lock() {
            // Dropping `file` releases the probe lock.
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(MigrateError::InUse {
                    module,
                    source: "its database is locked by another handle".into(),
                });
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(MigrateError::io("check a module database's lock", &db_file)(e));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::detect_format;
    use crate::format::read_format;

    static RUNS: AtomicUsize = AtomicUsize::new(0);
    static FAIL_SECOND: AtomicUsize = AtomicUsize::new(0);

    fn ok_hop(ctx: &MigrationCtx<'_>) -> Result<HopReport, MigrateError> {
        RUNS.fetch_add(1, Ordering::SeqCst);
        ctx.report("m", 1);
        Ok(HopReport {
            migrated: 1,
            ..Default::default()
        })
    }

    fn flaky_hop(ctx: &MigrationCtx<'_>) -> Result<HopReport, MigrateError> {
        if FAIL_SECOND.load(Ordering::SeqCst) == 1 {
            return Err(MigrateError::Locked);
        }
        ok_hop(ctx)
    }

    static FAKE: &[Hop] = &[
        Hop {
            from: DataFormat(0),
            to: DataFormat(1),
            needs_unlock: false,
            description: "first",
            run: ok_hop,
        },
        Hop {
            from: DataFormat(1),
            to: DataFormat(2),
            needs_unlock: false,
            description: "second",
            run: flaky_hop,
        },
    ];

    static NEEDS_KEY: &[Hop] = &[Hop {
        from: DataFormat(0),
        to: DataFormat(1),
        needs_unlock: true,
        description: "keyed",
        run: ok_hop,
    }];

    fn legacy_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vault")).unwrap();
        dir
    }

    fn marker(dir: &Path) -> u32 {
        read_format(dir).unwrap().unwrap().data_format
    }

    /// The fake hops share statics, so the whole sequence lives in one test.
    #[test]
    fn test_runner_order_marker_and_resume() {
        let session = VaultSession::new();
        let seen = std::cell::RefCell::new(Vec::new());
        let progress = |p: Progress| seen.borrow_mut().push(p.hop);

        // The second hop fails: the marker stays at its `from`.
        let dir = legacy_dir();
        FAIL_SECOND.store(1, Ordering::SeqCst);
        let err =
            upgrade_with(dir.path(), &session, "t", &progress, FAKE, DataFormat(2)).unwrap_err();
        assert!(
            matches!(err, MigrateError::HopFailed { hop: "second", .. }),
            "{err}"
        );
        assert_eq!(marker(dir.path()), 1);
        assert_eq!(RUNS.load(Ordering::SeqCst), 1);

        // Retrying resumes at the failed hop only.
        FAIL_SECOND.store(0, Ordering::SeqCst);
        let report =
            upgrade_with(dir.path(), &session, "t", &progress, FAKE, DataFormat(2)).unwrap();
        assert_eq!(marker(dir.path()), 2);
        assert_eq!(RUNS.load(Ordering::SeqCst), 2);
        assert_eq!(report.from, Some(DataFormat(1)));
        assert_eq!(report.hops.len(), 1);
        assert_eq!(*seen.borrow(), ["first", "second"]);
        assert_eq!(crate::list_backups(dir.path()).unwrap().len(), 2);

        // Nothing left to do.
        let report =
            upgrade_with(dir.path(), &session, "t", &progress, FAKE, DataFormat(2)).unwrap();
        assert!(report.hops.is_empty());
        assert_eq!(RUNS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_newer_data_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("FORMAT"), "data_format = 7\n").unwrap();
        let err = upgrade(dir.path(), &VaultSession::new(), "t", &|_| {}).unwrap_err();
        assert!(matches!(err, MigrateError::NewerFormat { .. }));
        assert_eq!(marker(dir.path()), 7);
    }

    #[test]
    fn test_unreadable_format_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("FORMAT"), "garbage = [").unwrap();
        assert!(matches!(
            upgrade(dir.path(), &VaultSession::new(), "t", &|_| {}),
            Err(MigrateError::FormatUnreadable { .. })
        ));
    }

    #[test]
    fn test_locked_session_is_refused_before_any_backup() {
        let dir = legacy_dir();
        let err = upgrade_with(
            dir.path(),
            &VaultSession::new(),
            "t",
            &|_| {},
            NEEDS_KEY,
            DataFormat(1),
        )
        .unwrap_err();
        assert!(matches!(err, MigrateError::Locked));
        assert!(!dir.path().join("backups").exists());
        assert!(matches!(
            detect_format(dir.path()).unwrap(),
            DataState::Behind { .. }
        ));
    }

    #[test]
    fn test_fresh_dir_has_nothing_to_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let report = upgrade(dir.path(), &VaultSession::new(), "t", &|_| {}).unwrap();
        assert!(report.hops.is_empty());
        assert_eq!(report.summary(), "Data format is already up to date");
    }
}
