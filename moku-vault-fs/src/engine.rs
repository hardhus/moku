use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use rand::RngCore;
use rand::rngs::OsRng;
use secrecy::SecretBox;

use crate::content;
use crate::keys::{ContentKey, NameKey, VolumeKeys};
use crate::names::MAX_VIRTUAL_NAME_LEN;
use crate::pathmap::PathMapper;
use crate::quota::Quota;
use crate::types::{Attr, DirEntry, FileKind, VaultFsError, VResult, VirtualPath};

struct OpenFile {
    backing: PathBuf,
    file_id: [u8; 16],
}

/// The platform-agnostic encrypted-volume engine (plan §2). Every method
/// is synchronous and self-contained — FUSE's and WinFsp's callback traits
/// are both synchronous, so the OS mount shims in `moku-vault-mount` can
/// call straight into this with no runtime bridging.
pub struct VolumeEngine {
    pathmap: PathMapper,
    content_key: SecretBox<ContentKey>,
    name_key: SecretBox<NameKey>,
    quota: Quota,
    open_files: Mutex<HashMap<u64, OpenFile>>,
    next_fh: AtomicU64,
}

impl VolumeEngine {
    pub fn open_volume(
        data_root: PathBuf,
        keys: VolumeKeys,
        usage_path: PathBuf,
        size_limit_bytes: u64,
    ) -> anyhow::Result<Self> {
        let pathmap = PathMapper::new(data_root);
        pathmap.ensure_root()?;
        let quota = Quota::load(usage_path, size_limit_bytes);
        Ok(Self {
            pathmap,
            content_key: keys.content,
            name_key: keys.name,
            quota,
            open_files: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
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

    pub fn flush_usage(&self) -> anyhow::Result<()> {
        self.quota.flush()
    }

    pub fn reconcile_usage(&self) -> anyhow::Result<()> {
        self.quota.reconcile_from_scan(self.pathmap.data_root())
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
        let backing = self.pathmap.resolve(&self.name_key, path)?;
        self.stat_backing(&backing)
    }

    pub fn read_dir(&self, path: &VirtualPath) -> VResult<Vec<DirEntry>> {
        let backing = if path.is_root() {
            self.pathmap.data_root().clone()
        } else {
            self.pathmap.resolve(&self.name_key, path)?
        };
        if !backing.is_dir() {
            return Err(VaultFsError::NotADirectory);
        }
        let entries = self.pathmap.read_dir_plain(&self.name_key, &backing).map_err(VaultFsError::from)?;
        Ok(entries
            .into_iter()
            .map(|(name, _p, is_dir)| DirEntry {
                name,
                kind: if is_dir { FileKind::Directory } else { FileKind::File },
            })
            .collect())
    }

    pub fn mkdir(&self, parent: &VirtualPath, name: &str) -> VResult<Attr> {
        if name.len() > MAX_VIRTUAL_NAME_LEN {
            return Err(VaultFsError::NameTooLong);
        }
        let child = parent.join(name);
        let backing = self.pathmap.resolve(&self.name_key, &child)?;
        if backing.exists() {
            return Err(VaultFsError::AlreadyExists);
        }
        fs::create_dir(&backing)?;
        self.pathmap.init_dir_iv(&backing).map_err(VaultFsError::from)?;
        self.stat_backing(&backing)
    }

    pub fn rmdir(&self, parent: &VirtualPath, name: &str) -> VResult<()> {
        let child = parent.join(name);
        let backing = self.pathmap.resolve(&self.name_key, &child)?;
        if !backing.is_dir() {
            return Err(VaultFsError::NotADirectory);
        }
        let has_children = fs::read_dir(&backing)?.any(|e| match e {
            Ok(e) => e.file_name() != std::ffi::OsStr::new(".moku_dir_iv"),
            Err(_) => true,
        });
        if has_children {
            return Err(VaultFsError::NotEmpty);
        }
        fs::remove_dir_all(&backing)?;
        self.pathmap.forget_dir(&backing);
        Ok(())
    }

    pub fn create(&self, parent: &VirtualPath, name: &str) -> VResult<(u64, Attr)> {
        if name.len() > MAX_VIRTUAL_NAME_LEN {
            return Err(VaultFsError::NameTooLong);
        }
        let child = parent.join(name);
        let backing = self.pathmap.resolve(&self.name_key, &child)?;
        if backing.exists() {
            return Err(VaultFsError::AlreadyExists);
        }
        if !self.quota.try_grow(content::HEADER_SIZE) {
            return Err(VaultFsError::QuotaExceeded);
        }
        let mut file_id = [0u8; 16];
        OsRng.fill_bytes(&mut file_id);
        if let Err(e) = content::create_empty_file(&backing, &file_id) {
            self.quota.shrink(content::HEADER_SIZE);
            return Err(VaultFsError::from(e));
        }

        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        self.open_files.lock().unwrap().insert(fh, OpenFile { backing: backing.clone(), file_id });

        let attr = self.stat_backing(&backing)?;
        Ok((fh, attr))
    }

    pub fn open(&self, path: &VirtualPath) -> VResult<u64> {
        let backing = self.pathmap.resolve(&self.name_key, path)?;
        if !backing.is_file() {
            return Err(VaultFsError::NotFound);
        }
        let mut file = File::open(&backing)?;
        let file_id = content::read_file_id(&mut file).map_err(VaultFsError::from)?;
        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        self.open_files.lock().unwrap().insert(fh, OpenFile { backing, file_id });
        Ok(fh)
    }

    fn lookup_open(&self, fh: u64) -> VResult<(PathBuf, [u8; 16])> {
        let files = self.open_files.lock().unwrap();
        let f = files.get(&fh).ok_or(VaultFsError::BadFileHandle)?;
        Ok((f.backing.clone(), f.file_id))
    }

    pub fn read(&self, fh: u64, offset: u64, buf: &mut [u8]) -> VResult<usize> {
        let (backing, file_id) = self.lookup_open(fh)?;
        let mut file = OpenOptions::new().read(true).open(&backing)?;
        content::read_range(&mut file, &self.content_key, &file_id, offset, buf).map_err(VaultFsError::from)
    }

    pub fn write(&self, fh: u64, offset: u64, data: &[u8]) -> VResult<usize> {
        let (backing, file_id) = self.lookup_open(fh)?;
        let mut file = OpenOptions::new().read(true).write(true).open(&backing)?;
        let physical_before = file.metadata()?.len();
        let projected_growth = (offset + data.len() as u64).saturating_sub(physical_before);
        if !self.quota.try_grow(projected_growth) {
            return Err(VaultFsError::QuotaExceeded);
        }
        let (written, before, after) =
            content::write_range(&mut file, &self.content_key, &file_id, offset, data).map_err(VaultFsError::from)?;
        // try_grow reserved a conservative upper bound; true physical
        // growth from block alignment is usually smaller, so reconcile.
        self.reconcile_reservation(projected_growth, after.saturating_sub(before));
        Ok(written)
    }

    pub fn setattr_size(&self, path: &VirtualPath, size: u64) -> VResult<Attr> {
        let backing = self.pathmap.resolve(&self.name_key, path)?;
        let mut file = OpenOptions::new().read(true).write(true).open(&backing)?;
        let file_id = content::read_file_id(&mut file).map_err(VaultFsError::from)?;
        let physical_before = file.metadata()?.len();
        let projected_growth = size.saturating_sub(physical_before);
        if projected_growth > 0 && !self.quota.try_grow(projected_growth) {
            return Err(VaultFsError::QuotaExceeded);
        }
        let (before, after) = content::set_len(&mut file, &self.content_key, &file_id, size).map_err(VaultFsError::from)?;
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
            let _ = self.quota.try_grow(actual - reserved);
        }
    }

    pub fn release(&self, fh: u64) -> VResult<()> {
        self.open_files.lock().unwrap().remove(&fh);
        Ok(())
    }

    pub fn unlink(&self, parent: &VirtualPath, name: &str) -> VResult<()> {
        let child = parent.join(name);
        let backing = self.pathmap.resolve(&self.name_key, &child)?;
        let meta = fs::metadata(&backing)?;
        if meta.is_dir() {
            return Err(VaultFsError::IsADirectory);
        }
        let size = meta.len();
        fs::remove_file(&backing)?;
        self.quota.shrink(size);
        Ok(())
    }

    pub fn rename(
        &self,
        old_parent: &VirtualPath,
        old_name: &str,
        new_parent: &VirtualPath,
        new_name: &str,
    ) -> VResult<()> {
        if new_name.len() > MAX_VIRTUAL_NAME_LEN {
            return Err(VaultFsError::NameTooLong);
        }
        let old_child = old_parent.join(old_name);
        let new_child = new_parent.join(new_name);
        let old_backing = self.pathmap.resolve(&self.name_key, &old_child)?;
        let new_backing = self.pathmap.resolve(&self.name_key, &new_child)?;
        if !old_backing.exists() {
            return Err(VaultFsError::NotFound);
        }
        if new_backing.exists() {
            return Err(VaultFsError::AlreadyExists);
        }
        fs::rename(&old_backing, &new_backing)?;
        self.pathmap.forget_dir(&old_backing);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::VolumeKeys;
    use secrecy::SecretBox;
    use tempfile::tempdir;

    fn engine(dir: &Path, limit: u64) -> VolumeEngine {
        let keys = VolumeKeys {
            content: SecretBox::new(Box::new(ContentKey([1u8; 32]))),
            name: SecretBox::new(Box::new(NameKey([2u8; 64]))),
        };
        VolumeEngine::open_volume(dir.join("data"), keys, dir.join("usage.json"), limit).unwrap()
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
        assert!(matches!(eng.create(&VirtualPath::root(), "x.md"), Err(VaultFsError::AlreadyExists)));
    }

    #[test]
    fn test_open_missing_file_fails() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 1_000_000);
        assert!(matches!(eng.open(&VirtualPath::parse("/missing.md")), Err(VaultFsError::NotFound)));
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
        assert!(matches!(eng.rmdir(&VirtualPath::root(), "d"), Err(VaultFsError::NotEmpty)));
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
        eng.rename(&VirtualPath::root(), "old.md", &VirtualPath::parse("/dest"), "new.md").unwrap();

        assert!(eng.getattr(&VirtualPath::parse("/old.md")).is_err());
        let attr = eng.getattr(&VirtualPath::parse("/dest/new.md")).unwrap();
        assert_eq!(attr.size, 4);
    }

    #[test]
    fn test_quota_rejects_over_limit_write() {
        let dir = tempdir().unwrap();
        let eng = engine(dir.path(), 100);
        let (fh, _) = eng.create(&VirtualPath::root(), "f").unwrap();
        assert!(matches!(eng.write(fh, 0, &vec![0u8; 1000]), Err(VaultFsError::QuotaExceeded)));
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
}
