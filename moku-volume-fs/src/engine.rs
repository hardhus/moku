use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use moku_crypto::BlockAead;

use crate::content;
use crate::keys::VolumeKeys;
use crate::names::NameCipher;
use crate::lock::VolumeLock;
use crate::names::MAX_VIRTUAL_NAME_LEN;
use crate::pathmap::PathMapper;
use crate::quota::Quota;
use crate::types::{Attr, DirEntry, FileKind, VolumeFsError, VResult, VirtualPath};

struct OpenFile {
    backing: PathBuf,
    file_id: [u8; 16],
}

/// The platform-agnostic encrypted-volume engine (plan §2). Every method
/// is synchronous and self-contained — FUSE's and WinFsp's callback traits
/// are both synchronous, so the OS mount shims in `moku-volume-mount` can
/// call straight into this with no runtime bridging.
pub struct VolumeEngine {
    pathmap: PathMapper,
    /// Content cipher, keyed once for the whole time the volume is open.
    blocks: BlockAead,
    names: NameCipher,
    quota: Quota,
    open_files: Mutex<HashMap<u64, OpenFile>>,
    next_fh: AtomicU64,
    file_id_cache: Mutex<HashMap<PathBuf, [u8; 16]>>,
    /// One reader/writer lock per file, keyed by its stable `file_id`
    /// (unchanged by rename): reads share, writes and resizes are
    /// exclusive, so two concurrent writes to one file can't interleave
    /// their read-modify-write of the same blocks.
    file_locks: Mutex<HashMap<[u8; 16], Arc<RwLock<()>>>>,
    /// Held for the engine's whole lifetime; see `VolumeLock`.
    _lock: VolumeLock,
}

impl VolumeEngine {
    /// Opens a volume's data. `lock` must already be held on the volume's
    /// directory (take it with `VolumeLock::acquire` BEFORE deriving keys,
    /// so a second mount fails fast instead of after the Argon2 wait).
    pub fn open_volume(
        data_root: PathBuf,
        keys: VolumeKeys,
        usage_path: PathBuf,
        size_limit_bytes: u64,
        lock: VolumeLock,
    ) -> VResult<Self> {
        let pathmap = PathMapper::new(data_root);
        pathmap.ensure_root()?;
        let quota = Quota::load(usage_path, size_limit_bytes);
        Ok(Self {
            pathmap,
            blocks: BlockAead::new(&keys.content),
            names: NameCipher::new(keys.name),
            quota,
            open_files: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            file_id_cache: Mutex::new(HashMap::new()),
            file_locks: Mutex::new(HashMap::new()),
            _lock: lock,
        })
    }

    pub fn usage_bytes(&self) -> u64 {
        self.quota.used_bytes()
    }

    pub fn size_limit_bytes(&self) -> u64 {
        self.quota.limit_bytes()
    }

    pub fn set_size_limit_bytes(&self, new_limit: u64) {
        self.quota.set_limit(new_limit)
    }

    pub fn flush_usage(&self) -> VResult<()> {
        self.quota.flush()
    }

    pub fn reconcile_usage(&self) -> VResult<()> {
        self.quota.reconcile_from_scan(self.pathmap.data_root())
    }

    /// Reads a file's on-disk `file_id` (mirrors `pathmap.rs`'s
    /// `dir_iv_for_backing` caching shape: lock -> check -> drop -> I/O ->
    /// lock -> insert, never holding the lock across I/O).
    fn file_id_for_backing(&self, backing: &Path) -> VResult<[u8; 16]> {
        if let Some(id) = self.file_id_cache.lock().unwrap().get(backing) {
            return Ok(*id);
        }
        let mut file = File::open(backing)?;
        let id = content::read_file_id(&mut file)?;
        self.file_id_cache.lock().unwrap().insert(backing.to_path_buf(), id);
        Ok(id)
    }

    fn cache_file_id(&self, backing: &Path, id: [u8; 16]) {
        self.file_id_cache.lock().unwrap().insert(backing.to_path_buf(), id);
    }

    /// A stable identity value for a path — a file's on-disk `file_id` for
    /// files, or the directory's own IV for directories/root. Used by the
    /// WinFsp shim as a stable `index_number` (plan: stable file ID).
    pub fn file_id(&self, path: &VirtualPath) -> VResult<[u8; 16]> {
        let backing = if path.is_root() {
            self.pathmap.data_root().clone()
        } else {
            self.pathmap.resolve(&self.names, path)?
        };
        if backing.is_dir() {
            self.pathmap.dir_iv(&backing)
        } else {
            self.file_id_for_backing(&backing)
        }
    }

    fn stat_backing(&self, backing: &Path) -> VResult<Attr> {
        let meta = fs::metadata(backing)?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if meta.is_dir() {
            Ok(Attr { kind: FileKind::Directory, size: 0, created_at: modified, modified_at: modified })
        } else {
            let physical_len = meta.len();
            let (logical_size, _) = content::logical_layout(physical_len);
            Ok(Attr { kind: FileKind::File, size: logical_size, created_at: modified, modified_at: modified })
        }
    }

    pub fn getattr(&self, path: &VirtualPath) -> VResult<Attr> {
        if path.is_root() {
            return self.stat_backing(self.pathmap.data_root());
        }
        let backing = self.pathmap.resolve(&self.names, path)?;
        self.stat_backing(&backing)
    }

    pub fn read_dir(&self, path: &VirtualPath) -> VResult<Vec<DirEntry>> {
        let backing = if path.is_root() {
            self.pathmap.data_root().clone()
        } else {
            self.pathmap.resolve(&self.names, path)?
        };
        if !backing.is_dir() {
            return Err(VolumeFsError::NotADirectory);
        }
        let entries = self.pathmap.read_dir_plain(&self.names, &backing)?;
        let mut entries: Vec<DirEntry> = entries
            .into_iter()
            .map(|(name, _p, is_dir)| DirEntry {
                name,
                kind: if is_dir { FileKind::Directory } else { FileKind::File },
            })
            .collect();
        // Sorted, stable order — OS mount shims paginate directory
        // listings by comparing names against a marker (plan §2), which
        // only produces a well-defined, non-overlapping sequence of pages
        // if the underlying order is fixed across calls.
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    pub fn mkdir(&self, parent: &VirtualPath, name: &str) -> VResult<Attr> {
        if name.len() > MAX_VIRTUAL_NAME_LEN {
            return Err(VolumeFsError::NameTooLong);
        }
        let child = parent.join(name);
        let backing = self.pathmap.resolve(&self.names, &child)?;
        if backing.exists() {
            return Err(VolumeFsError::AlreadyExists);
        }
        fs::create_dir(&backing)?;
        self.pathmap.init_dir_iv(&backing)?;
        self.stat_backing(&backing)
    }

    pub fn rmdir(&self, parent: &VirtualPath, name: &str) -> VResult<()> {
        let child = parent.join(name);
        let backing = self.pathmap.resolve(&self.names, &child)?;
        if !backing.is_dir() {
            return Err(VolumeFsError::NotADirectory);
        }
        let has_children = fs::read_dir(&backing)?.any(|e| match e {
            Ok(e) => e.file_name() != std::ffi::OsStr::new(".moku_dir_iv"),
            Err(_) => true,
        });
        if has_children {
            return Err(VolumeFsError::NotEmpty);
        }
        fs::remove_dir_all(&backing)?;
        self.pathmap.forget_dir(&backing);
        Ok(())
    }

    pub fn create(&self, parent: &VirtualPath, name: &str) -> VResult<(u64, Attr)> {
        if name.len() > MAX_VIRTUAL_NAME_LEN {
            return Err(VolumeFsError::NameTooLong);
        }
        let child = parent.join(name);
        let backing = self.pathmap.resolve(&self.names, &child)?;
        if backing.exists() {
            return Err(VolumeFsError::AlreadyExists);
        }
        if !self.quota.try_grow(content::HEADER_SIZE) {
            return Err(VolumeFsError::QuotaExceeded);
        }
        let file_id = moku_crypto::random::random_id16();
        if let Err(e) = content::create_empty_file(&backing, &file_id) {
            self.quota.shrink(content::HEADER_SIZE);
            return Err(e);
        }

        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        self.cache_file_id(&backing, file_id);
        self.open_files.lock().unwrap().insert(fh, OpenFile { backing: backing.clone(), file_id });

        let attr = self.stat_backing(&backing)?;
        Ok((fh, attr))
    }

    pub fn open(&self, path: &VirtualPath) -> VResult<u64> {
        let backing = self.pathmap.resolve(&self.names, path)?;
        if !backing.is_file() {
            return Err(VolumeFsError::NotFound);
        }
        let mut file = File::open(&backing)?;
        let file_id = content::read_file_id(&mut file)?;
        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        self.open_files.lock().unwrap().insert(fh, OpenFile { backing, file_id });
        Ok(fh)
    }

    fn lookup_open(&self, fh: u64) -> VResult<(PathBuf, [u8; 16])> {
        let files = self.open_files.lock().unwrap();
        let f = files.get(&fh).ok_or(VolumeFsError::BadFileHandle)?;
        Ok((f.backing.clone(), f.file_id))
    }

    fn file_lock(&self, file_id: &[u8; 16]) -> Arc<RwLock<()>> {
        let mut locks = self.file_locks.lock().unwrap_or_else(|p| p.into_inner());
        Arc::clone(locks.entry(*file_id).or_default())
    }

    pub fn read(&self, fh: u64, offset: u64, buf: &mut [u8]) -> VResult<usize> {
        let (backing, file_id) = self.lookup_open(fh)?;
        let lock = self.file_lock(&file_id);
        let _shared = lock.read().unwrap_or_else(|p| p.into_inner());
        let mut file = OpenOptions::new().read(true).open(&backing)?;
        content::read_range(&mut file, &self.blocks, &file_id, offset, buf)
    }

    pub fn write(&self, fh: u64, offset: u64, data: &[u8]) -> VResult<usize> {
        let (backing, file_id) = self.lookup_open(fh)?;
        let lock = self.file_lock(&file_id);
        let _exclusive = lock.write().unwrap_or_else(|p| p.into_inner());
        let mut file = OpenOptions::new().read(true).write(true).open(&backing)?;
        let physical_before = file.metadata()?.len();
        let write_end = offset
            .checked_add(data.len() as u64)
            .ok_or(VolumeFsError::FileTooLarge)?;
        let projected_growth = write_end.saturating_sub(physical_before);
        if !self.quota.try_grow(projected_growth) {
            return Err(VolumeFsError::QuotaExceeded);
        }
        let (written, before, after) =
            match content::write_range(&mut file, &self.blocks, &file_id, offset, data) {
                Ok(v) => v,
                Err(e) => {
                    // Settle the reservation against what actually reached
                    // disk; a failed write must not leak quota forever.
                    let now = file.metadata().map(|m| m.len()).unwrap_or(physical_before);
                    self.reconcile_reservation(projected_growth, now.saturating_sub(physical_before));
                    return Err(e);
                }
            };
        // try_grow reserved a conservative upper bound; true physical
        // growth from block alignment is usually smaller, so reconcile.
        self.reconcile_reservation(projected_growth, after.saturating_sub(before));
        Ok(written)
    }

    pub fn setattr_size(&self, path: &VirtualPath, size: u64) -> VResult<Attr> {
        let backing = self.pathmap.resolve(&self.names, path)?;
        let mut file = OpenOptions::new().read(true).write(true).open(&backing)?;
        let file_id = content::read_file_id(&mut file)?;
        let lock = self.file_lock(&file_id);
        let _exclusive = lock.write().unwrap_or_else(|p| p.into_inner());
        let physical_before = file.metadata()?.len();
        let projected_growth = size.saturating_sub(physical_before);
        if projected_growth > 0 && !self.quota.try_grow(projected_growth) {
            return Err(VolumeFsError::QuotaExceeded);
        }
        let (before, after) = match content::set_len(&mut file, &self.blocks, &file_id, size) {
            Ok(v) => v,
            Err(e) => {
                let now = file.metadata().map(|m| m.len()).unwrap_or(physical_before);
                if now < physical_before {
                    self.quota.shrink(projected_growth + (physical_before - now));
                } else {
                    self.reconcile_reservation(projected_growth, now - physical_before);
                }
                return Err(e);
            }
        };
        if after < before {
            self.quota.shrink(before - after);
        } else {
            self.reconcile_reservation(projected_growth, after - before);
        }
        self.stat_backing(&backing)
    }

    /// `try_grow` above reserves an upper-bound estimate before the real
    /// write happens; once the actual physical delta is known, top up or
    /// give back the difference so `usage_bytes()` tracks real disk usage.
    fn reconcile_reservation(&self, reserved: u64, actual: u64) {
        if actual < reserved {
            self.quota.shrink(reserved - actual);
        } else if actual > reserved {
            let shortfall = actual - reserved;
            if !self.quota.try_grow(shortfall) {
                tracing::warn!(
                    shortfall,
                    "quota reservation shortfall: actual growth exceeded the upfront estimate \
                     and remaining quota could not cover the difference — tracked usage now \
                     undercounts real disk usage by this amount"
                );
            }
        }
    }

    pub fn release(&self, fh: u64) -> VResult<()> {
        self.open_files.lock().unwrap().remove(&fh);
        Ok(())
    }

    pub fn unlink(&self, parent: &VirtualPath, name: &str) -> VResult<()> {
        let child = parent.join(name);
        let backing = self.pathmap.resolve(&self.names, &child)?;
        let meta = fs::metadata(&backing)?;
        if meta.is_dir() {
            return Err(VolumeFsError::IsADirectory);
        }
        let size = meta.len();
        fs::remove_file(&backing)?;
        self.quota.shrink(size);
        // Prevents a stale file_id from surviving in the cache and being
        // handed out for a different file later created at this same path.
        self.file_id_cache.lock().unwrap().remove(&backing);
        Ok(())
    }

    pub fn rename(
        &self,
        old_parent: &VirtualPath,
        old_name: &str,
        new_parent: &VirtualPath,
        new_name: &str,
        replace_if_exists: bool,
    ) -> VResult<()> {
        if new_name.len() > MAX_VIRTUAL_NAME_LEN {
            return Err(VolumeFsError::NameTooLong);
        }
        let old_child = old_parent.join(old_name);
        let new_child = new_parent.join(new_name);
        let old_backing = self.pathmap.resolve(&self.names, &old_child)?;
        let new_backing = self.pathmap.resolve(&self.names, &new_child)?;
        if !old_backing.exists() {
            return Err(VolumeFsError::NotFound);
        }
        // Size of the file being clobbered, so its quota reservation can be
        // released once the replace actually happens -- computed up front
        // so a failed rename below never mutates quota state.
        let replaced_size = if new_backing.exists() {
            if !replace_if_exists {
                return Err(VolumeFsError::AlreadyExists);
            }
            let meta = fs::metadata(&new_backing)?;
            if meta.is_dir() {
                return Err(VolumeFsError::IsADirectory);
            }
            Some(meta.len())
        } else {
            None
        };
        // `fs::rename` already performs an atomic replace of an existing
        // destination file natively (Windows: MOVEFILE_REPLACE_EXISTING,
        // POSIX: default rename(2) semantics) -- no separate
        // unlink-then-rename dance needed. An editor's atomic save (write a
        // temp file, then rename it over the original -- Helix, vim, VS
        // Code) relies on exactly this replace-on-rename behavior.
        fs::rename(&old_backing, &new_backing)?;
        self.pathmap.forget_dir(&old_backing);
        // Single lock acquisition for the move so a concurrent file_id()
        // call on new_backing can never observe a half-updated cache.
        {
            let mut cache = self.file_id_cache.lock().unwrap();
            match cache.remove(&old_backing) {
                Some(id) => {
                    cache.insert(new_backing.clone(), id);
                }
                None => {
                    // No cached id for the source -- don't let a stale
                    // cached id for the (now-overwritten) destination
                    // survive; force a lazy re-read of the real header.
                    cache.remove(&new_backing);
                }
            }
        }
        if let Some(size) = replaced_size {
            self.quota.shrink(size);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::VolumeKeys;
    use moku_crypto::Key;
    use tempfile::tempdir;

    fn engine(dir: &Path, limit: u64) -> VolumeEngine {
        let keys = VolumeKeys {
            content: Key::from_slice(&[1u8; 32]).unwrap(),
            name: Key::from_slice(&[2u8; 64]).unwrap(),
        };
        let lock = VolumeLock::acquire(dir).unwrap();
        VolumeEngine::open_volume(dir.join("data"), keys, dir.join("usage.json"), limit, lock)
            .unwrap()
    }

    #[test]
    fn test_second_engine_on_same_volume_is_rejected_while_first_is_open() {
        let dir = tempdir().unwrap();
        let first = engine(dir.path(), 1_000_000);
        assert!(matches!(
            VolumeLock::acquire(dir.path()),
            Err(VolumeFsError::Locked)
        ));
        drop(first);
        // Released with the engine: the volume can be opened again.
        let _second = engine(dir.path(), 1_000_000);
    }

    #[test]
    fn test_create_write_read_roundtrip() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);

        let (fh, attr) = eng.create(&VirtualPath::root(), "hello.md").unwrap();
        assert_eq!(attr.kind, FileKind::File);
        eng.write(fh, 0, b"hello vault").unwrap();
        eng.release(fh).unwrap();

        let fh2 = eng.open(&VirtualPath::parse("/hello.md")).unwrap();
        let mut buf = [0u8; 11];
        let n = eng.read(fh2, 0, &mut buf).unwrap();
        assert_eq!(n, 11);
        assert_eq!(&buf, b"hello vault");
    }

    #[test]
    fn test_read_dir_is_sorted_by_name() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        for name in ["zeta.md", "alpha.md", "mu.md"] {
            eng.create(&VirtualPath::root(), name).unwrap();
        }
        let names: Vec<String> = eng.read_dir(&VirtualPath::root()).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["alpha.md", "mu.md", "zeta.md"]);
    }

    #[test]
    fn test_mkdir_and_nested_file() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);

        eng.mkdir(&VirtualPath::root(), "notes").unwrap();
        let (fh, _) = eng.create(&VirtualPath::parse("/notes"), "a.md").unwrap();
        eng.write(fh, 0, b"nested content").unwrap();
        eng.release(fh).unwrap();

        let entries = eng.read_dir(&VirtualPath::parse("/notes")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a.md");
    }

    #[test]
    fn test_create_duplicate_fails() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        eng.create(&VirtualPath::root(), "x.md").unwrap();
        assert!(matches!(eng.create(&VirtualPath::root(), "x.md"), Err(VolumeFsError::AlreadyExists)));
    }

    #[test]
    fn test_open_missing_file_fails() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        assert!(matches!(eng.open(&VirtualPath::parse("/missing.md")), Err(VolumeFsError::NotFound)));
    }

    #[test]
    fn test_unlink_frees_quota() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        let (fh, _) = eng.create(&VirtualPath::root(), "big.bin").unwrap();
        eng.write(fh, 0, &vec![0u8; 5000]).unwrap();
        eng.release(fh).unwrap();

        let used_before = eng.usage_bytes();
        assert!(used_before > 0);
        eng.unlink(&VirtualPath::root(), "big.bin").unwrap();
        assert!(eng.usage_bytes() < used_before);
    }

    #[test]
    fn test_rmdir_non_empty_fails() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        eng.mkdir(&VirtualPath::root(), "d").unwrap();
        eng.create(&VirtualPath::parse("/d"), "f.md").unwrap();
        assert!(matches!(eng.rmdir(&VirtualPath::root(), "d"), Err(VolumeFsError::NotEmpty)));
    }

    #[test]
    fn test_rmdir_empty_succeeds() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        eng.mkdir(&VirtualPath::root(), "d").unwrap();
        eng.rmdir(&VirtualPath::root(), "d").unwrap();
        assert!(eng.getattr(&VirtualPath::parse("/d")).is_err());
    }

    #[test]
    fn test_rename_moves_file() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        let (fh, _) = eng.create(&VirtualPath::root(), "old.md").unwrap();
        eng.write(fh, 0, b"data").unwrap();
        eng.release(fh).unwrap();

        eng.mkdir(&VirtualPath::root(), "dest").unwrap();
        eng.rename(&VirtualPath::root(), "old.md", &VirtualPath::parse("/dest"), "new.md", false).unwrap();

        assert!(eng.getattr(&VirtualPath::parse("/old.md")).is_err());
        let attr = eng.getattr(&VirtualPath::parse("/dest/new.md")).unwrap();
        assert_eq!(attr.size, 4);
    }

    #[test]
    fn test_rename_over_existing_without_replace_fails() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        let (fh, _) = eng.create(&VirtualPath::root(), "old.md").unwrap();
        eng.write(fh, 0, b"data").unwrap();
        eng.release(fh).unwrap();
        eng.create(&VirtualPath::root(), "new.md").unwrap();

        assert!(matches!(
            eng.rename(&VirtualPath::root(), "old.md", &VirtualPath::root(), "new.md", false),
            Err(VolumeFsError::AlreadyExists)
        ));
    }

    #[test]
    fn test_rename_over_existing_with_replace_succeeds() {
        // Mirrors an editor's atomic save: write a temp file, then rename
        // it over an already-existing destination file.
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        let (fh, _) = eng.create(&VirtualPath::root(), "new.md").unwrap();
        eng.write(fh, 0, b"old content").unwrap();
        eng.release(fh).unwrap();
        let used_before_temp = eng.usage_bytes();

        let (fh2, _) = eng.create(&VirtualPath::root(), ".new.md.tmp").unwrap();
        eng.write(fh2, 0, b"new content").unwrap();
        eng.release(fh2).unwrap();

        eng.rename(&VirtualPath::root(), ".new.md.tmp", &VirtualPath::root(), "new.md", true).unwrap();

        let mut buf = [0u8; 11];
        let fh3 = eng.open(&VirtualPath::parse("/new.md")).unwrap();
        let n = eng.read(fh3, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"new content");
        assert!(eng.getattr(&VirtualPath::parse("/.new.md.tmp")).is_err());
        // Quota should reflect only the surviving file's bytes, not both
        // the replaced destination and the renamed-in file.
        assert_eq!(eng.usage_bytes(), used_before_temp);
    }

    #[test]
    fn test_rename_over_existing_directory_fails() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        eng.create(&VirtualPath::root(), "old.md").unwrap();
        eng.mkdir(&VirtualPath::root(), "adir").unwrap();

        assert!(matches!(
            eng.rename(&VirtualPath::root(), "old.md", &VirtualPath::root(), "adir", true),
            Err(VolumeFsError::IsADirectory)
        ));
    }

    #[test]
    fn test_quota_rejects_over_limit_write() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 100);
        let (fh, _) = eng.create(&VirtualPath::root(), "f").unwrap();
        assert!(matches!(eng.write(fh, 0, &vec![0u8; 1000]), Err(VolumeFsError::QuotaExceeded)));
    }

    #[test]
    fn test_setattr_size_truncates_and_extends() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        let (fh, _) = eng.create(&VirtualPath::root(), "f").unwrap();
        eng.write(fh, 0, b"0123456789").unwrap();
        eng.release(fh).unwrap();

        let attr = eng.setattr_size(&VirtualPath::parse("/f"), 4).unwrap();
        assert_eq!(attr.size, 4);

        let attr = eng.setattr_size(&VirtualPath::parse("/f"), 8).unwrap();
        assert_eq!(attr.size, 8);
    }

    #[test]
    fn test_usage_persists_across_reopen() {
        let dir = tempdir().unwrap();
        {
            let eng = engine(dir.path(), 1_000_000);
            let (fh, _) = eng.create(&VirtualPath::root(), "f").unwrap();
            eng.write(fh, 0, &vec![0u8; 1000]).unwrap();
            eng.release(fh).unwrap();
            eng.flush_usage().unwrap();
        }
        let eng2 = engine(dir.path(), 1_000_000);
        assert!(eng2.usage_bytes() > 0);
    }

    #[test]
    fn test_backing_names_are_encrypted_on_disk() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        eng.create(&VirtualPath::root(), "secret-plan.md").unwrap();

        let backing_entries: Vec<_> = std::fs::read_dir(dir.path().join("data"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(!backing_entries.iter().any(|n| n.contains("secret-plan")));
    }

    #[test]
    fn test_file_id_stable_across_calls() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        eng.create(&VirtualPath::root(), "a.md").unwrap();

        let id1 = eng.file_id(&VirtualPath::parse("/a.md")).unwrap();
        let id2 = eng.file_id(&VirtualPath::parse("/a.md")).unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_file_id_survives_rename() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        eng.create(&VirtualPath::root(), "a.md").unwrap();
        let id_before = eng.file_id(&VirtualPath::parse("/a.md")).unwrap();

        eng.rename(&VirtualPath::root(), "a.md", &VirtualPath::root(), "b.md", false).unwrap();
        let id_after = eng.file_id(&VirtualPath::parse("/b.md")).unwrap();
        assert_eq!(id_before, id_after);
    }

    #[test]
    fn test_file_id_changes_after_unlink_and_recreate_same_name() {
        // Regression test for the "stale cache after path reuse" bug: a
        // deleted file's id must never leak into a later file created at
        // the same virtual path.
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        eng.create(&VirtualPath::root(), "a.md").unwrap();
        let id1 = eng.file_id(&VirtualPath::parse("/a.md")).unwrap();

        eng.unlink(&VirtualPath::root(), "a.md").unwrap();
        eng.create(&VirtualPath::root(), "a.md").unwrap();
        let id2 = eng.file_id(&VirtualPath::parse("/a.md")).unwrap();

        assert_ne!(id1, id2);
    }

    #[test]
    fn test_file_id_changes_after_rename_replace() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        eng.create(&VirtualPath::root(), "dest.md").unwrap();
        let id_dest = eng.file_id(&VirtualPath::parse("/dest.md")).unwrap();

        let (fh, _) = eng.create(&VirtualPath::root(), ".tmp").unwrap();
        eng.release(fh).unwrap();
        let id_tmp = eng.file_id(&VirtualPath::parse("/.tmp")).unwrap();
        assert_ne!(id_dest, id_tmp);

        eng.rename(&VirtualPath::root(), ".tmp", &VirtualPath::root(), "dest.md", true).unwrap();
        let id_after = eng.file_id(&VirtualPath::parse("/dest.md")).unwrap();
        // The surviving (renamed-in) file's id, not the clobbered one's.
        assert_eq!(id_after, id_tmp);
        assert_ne!(id_after, id_dest);
    }

    #[test]
    fn test_two_files_have_different_file_ids() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        eng.create(&VirtualPath::root(), "a.md").unwrap();
        eng.create(&VirtualPath::root(), "b.md").unwrap();

        let id_a = eng.file_id(&VirtualPath::parse("/a.md")).unwrap();
        let id_b = eng.file_id(&VirtualPath::parse("/b.md")).unwrap();
        assert_ne!(id_a, id_b);
    }

    #[test]
    fn test_directory_file_id_stable() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        eng.mkdir(&VirtualPath::root(), "notes").unwrap();

        let id1 = eng.file_id(&VirtualPath::parse("/notes")).unwrap();
        let id2 = eng.file_id(&VirtualPath::parse("/notes")).unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_root_file_id_stable() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);

        let id1 = eng.file_id(&VirtualPath::root()).unwrap();
        let id2 = eng.file_id(&VirtualPath::root()).unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_failed_write_does_not_leak_quota() {
        let dir = tempdir().unwrap();
        let e = engine(dir.path(), 10_000_000);
        let root = VirtualPath::root();
        let (fh, _) = e.create(&root, "f.bin").unwrap();
        e.write(fh, 0, &vec![3u8; 5000]).unwrap();
        let used_before = e.usage_bytes();

        // Corrupt the last on-disk block so the next tail write fails while
        // decrypting it (after the quota reservation was taken).
        let backing = e.pathmap.resolve(&e.names, &VirtualPath::parse("/f.bin")).unwrap();
        let mut bytes = std::fs::read(&backing).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&backing, &bytes).unwrap();

        assert!(matches!(
            e.write(fh, 5000, &vec![4u8; 100_000]),
            Err(VolumeFsError::Integrity(_))
        ));
        assert_eq!(e.usage_bytes(), used_before, "the reservation must be given back");
    }

    #[test]
    fn test_concurrent_writes_to_one_file_do_not_interleave() {
        let dir = tempdir().unwrap();
        let e = std::sync::Arc::new(engine(dir.path(), 100_000_000));
        let (fh, _) = e.create(&VirtualPath::root(), "c.bin").unwrap();
        e.write(fh, 0, &vec![0u8; 64 * 1024]).unwrap();
        let handles: Vec<_> = (0..4u8)
            .map(|t| {
                let e = std::sync::Arc::clone(&e);
                std::thread::spawn(move || {
                    for i in 0..16u64 {
                        let off = (t as u64) * 16 * 1024 + i * 1024;
                        e.write(fh, off, &vec![t + 1; 1024]).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let mut buf = vec![0u8; 64 * 1024];
        let mut done = 0;
        while done < buf.len() {
            done += e.read(fh, done as u64, &mut buf[done..]).unwrap();
        }
        for t in 0..4usize {
            assert!(buf[t * 16 * 1024..(t + 1) * 16 * 1024].iter().all(|&b| b == t as u8 + 1));
        }
    }
}
