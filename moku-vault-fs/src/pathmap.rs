use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use rand::RngCore;
use rand::rngs::OsRng;
use secrecy::SecretBox;

use crate::keys::NameKey;
use crate::names::NameCipher;
use crate::types::{VResult, VaultFsError, VirtualPath};

const DIR_IV_FILE: &str = ".moku_dir_iv";

/// Resolves virtual paths to backing filesystem paths, encrypting each
/// path segment along the way, and manages per-directory IV files
/// (gocryptfs-style dir-IV scheme — plan §1).
pub struct PathMapper {
    data_root: PathBuf,
    dir_iv_cache: Mutex<HashMap<PathBuf, [u8; 16]>>,
}

impl PathMapper {
    pub fn new(data_root: PathBuf) -> Self {
        Self { data_root, dir_iv_cache: Mutex::new(HashMap::new()) }
    }

    /// Ensures the root directory and its `.moku_dir_iv` exist.
    pub fn ensure_root(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_root)?;
        let root = self.data_root.clone();
        self.dir_iv_for_backing(&root)?;
        Ok(())
    }

    fn dir_iv_for_backing(&self, backing_dir: &PathBuf) -> Result<[u8; 16]> {
        if let Some(iv) = self.dir_iv_cache.lock().unwrap().get(backing_dir) {
            return Ok(*iv);
        }
        let iv_path = backing_dir.join(DIR_IV_FILE);
        let iv = if iv_path.exists() {
            let bytes = std::fs::read(&iv_path).context("failed to read directory IV")?;
            let arr: [u8; 16] =
                bytes.as_slice().try_into().map_err(|_| anyhow!("corrupt .moku_dir_iv (wrong length)"))?;
            arr
        } else {
            let mut arr = [0u8; 16];
            OsRng.fill_bytes(&mut arr);
            std::fs::write(&iv_path, arr).context("failed to write directory IV")?;
            arr
        };
        self.dir_iv_cache.lock().unwrap().insert(backing_dir.clone(), iv);
        Ok(iv)
    }

    /// Resolves a virtual path down to its backing filesystem path,
    /// encrypting each path segment. Every ancestor directory must already
    /// exist on disk; the final segment is allowed not to exist yet (the
    /// caller may be about to create it).
    pub fn resolve(&self, name_key: &SecretBox<NameKey>, path: &VirtualPath) -> VResult<PathBuf> {
        let cipher = NameCipher::new(name_key);
        let mut backing = self.data_root.clone();
        let comps = path.components();
        for (i, comp) in comps.iter().enumerate() {
            let dir_iv = self.dir_iv_for_backing(&backing).map_err(VaultFsError::from)?;
            let encrypted = cipher.encrypt_name(&dir_iv, comp).map_err(|_| VaultFsError::NameTooLong)?;
            backing.push(encrypted);
            let is_last = i == comps.len() - 1;
            if !is_last && !backing.is_dir() {
                return Err(VaultFsError::NotFound);
            }
        }
        Ok(backing)
    }

    /// Reads a directory's plaintext entries by decrypting every backing
    /// entry name against that directory's own IV.
    pub fn read_dir_plain(
        &self,
        name_key: &SecretBox<NameKey>,
        backing_dir: &PathBuf,
    ) -> Result<Vec<(String, PathBuf, bool)>> {
        let dir_iv = self.dir_iv_for_backing(backing_dir)?;
        let cipher = NameCipher::new(name_key);
        let mut out = Vec::new();
        for entry in std::fs::read_dir(backing_dir)? {
            let entry = entry?;
            let raw_name = entry.file_name();
            let raw_name = raw_name.to_string_lossy();
            if raw_name == DIR_IV_FILE {
                continue;
            }
            let plain = cipher.decrypt_name(&dir_iv, &raw_name)?;
            let is_dir = entry.file_type()?.is_dir();
            out.push((plain, entry.path(), is_dir));
        }
        Ok(out)
    }

    /// Seeds a freshly-created directory's `.moku_dir_iv`.
    pub fn init_dir_iv(&self, backing_dir: &PathBuf) -> Result<()> {
        self.dir_iv_for_backing(backing_dir)?;
        Ok(())
    }

    /// Drops a cached dir IV — used after rmdir/rename so a stale cache
    /// entry can't resurrect a deleted directory's IV for a later path
    /// reused at that backing location.
    pub fn forget_dir(&self, backing_dir: &PathBuf) {
        self.dir_iv_cache.lock().unwrap().remove(backing_dir);
    }

    pub fn data_root(&self) -> &PathBuf {
        &self.data_root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::NameKey;
    use tempfile::tempdir;

    fn key() -> SecretBox<NameKey> {
        SecretBox::new(Box::new(NameKey([4u8; 64])))
    }

    #[test]
    fn test_resolve_root() {
        let dir = tempdir().unwrap();
        let mapper = PathMapper::new(dir.path().to_path_buf());
        mapper.ensure_root().unwrap();
        let resolved = mapper.resolve(&key(), &VirtualPath::root());
        // root resolves to itself when queried as an empty-component path
        assert!(resolved.is_ok());
        assert_eq!(resolved.unwrap(), dir.path());
    }

    #[test]
    fn test_resolve_missing_ancestor_is_not_found() {
        let dir = tempdir().unwrap();
        let mapper = PathMapper::new(dir.path().to_path_buf());
        mapper.ensure_root().unwrap();
        let result = mapper.resolve(&key(), &VirtualPath::parse("/missing-dir/file.md"));
        assert!(matches!(result, Err(VaultFsError::NotFound)));
    }

    #[test]
    fn test_resolve_same_path_is_stable() {
        let dir = tempdir().unwrap();
        let mapper = PathMapper::new(dir.path().to_path_buf());
        mapper.ensure_root().unwrap();
        let k = key();
        std::fs::create_dir(mapper.resolve(&k, &VirtualPath::parse("/notes")).unwrap()).unwrap();
        mapper.init_dir_iv(&mapper.resolve(&k, &VirtualPath::parse("/notes")).unwrap()).unwrap();

        let a = mapper.resolve(&k, &VirtualPath::parse("/notes/a.md")).unwrap();
        let b = mapper.resolve(&k, &VirtualPath::parse("/notes/a.md")).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn test_read_dir_plain_roundtrips_names() {
        let dir = tempdir().unwrap();
        let mapper = PathMapper::new(dir.path().to_path_buf());
        mapper.ensure_root().unwrap();
        let k = key();

        let backing_a = mapper.resolve(&k, &VirtualPath::parse("/a.md")).unwrap();
        std::fs::write(&backing_a, b"content").unwrap();

        let entries = mapper.read_dir_plain(&k, &dir.path().to_path_buf()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "a.md");
        assert!(!entries[0].2);
    }
}
