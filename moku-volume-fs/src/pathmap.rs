use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use moku_crypto::random::fill_random;

use crate::names::NameCipher;
use crate::types::{VResult, VResult as Result, VolumeFsError, VirtualPath};

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

    /// Public accessor for a directory's IV, reused as a stable identity
    /// value for directories (WinFsp `index_number`) — directories have no
    /// on-disk `file_id` header of their own, so this already-cached,
    /// already-random-per-directory value fills that role for free.
    pub fn dir_iv(&self, backing_dir: &Path) -> Result<[u8; 16]> {
        self.dir_iv_for_backing(&backing_dir.to_path_buf())
    }

    fn dir_iv_for_backing(&self, backing_dir: &PathBuf) -> Result<[u8; 16]> {
        if let Some(iv) = self.dir_iv_cache.lock().unwrap().get(backing_dir) {
            return Ok(*iv);
        }
        let iv_path = backing_dir.join(DIR_IV_FILE);
        let iv = match std::fs::read(&iv_path) {
            Ok(bytes) => parse_dir_iv(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A missing IV may only be created for a directory that has
                // no encrypted entries yet (a fresh volume root, or a crash
                // between `mkdir` and writing its IV). If entries exist,
                // their names were encrypted under the lost IV: minting a
                // new one would make every one of them permanently
                // unreadable, so that is an error the user must see (restore
                // `.moku_dir_iv` from a backup), never a silent reset.
                if has_encrypted_entries(backing_dir)? {
                    return Err(VolumeFsError::MissingDirIv {
                        dir: backing_dir.clone(),
                    });
                }
                create_dir_iv_no_replace(backing_dir)?
            }
            Err(e) => return Err(VolumeFsError::io_at("read a directory IV")(e)),
        };
        self.dir_iv_cache.lock().unwrap().insert(backing_dir.clone(), iv);
        Ok(iv)
    }

    /// Resolves a virtual path down to its backing filesystem path,
    /// encrypting each path segment. Every ancestor directory must already
    /// exist on disk; the final segment is allowed not to exist yet (the
    /// caller may be about to create it).
    pub fn resolve(&self, cipher: &NameCipher, path: &VirtualPath) -> VResult<PathBuf> {
        let mut backing = self.data_root.clone();
        let comps = path.components();
        for (i, comp) in comps.iter().enumerate() {
            let dir_iv = self.dir_iv_for_backing(&backing)?;
            let encrypted = cipher.encrypt_name(&dir_iv, comp)?;
            backing.push(encrypted);
            let is_last = i == comps.len() - 1;
            if !is_last && !backing.is_dir() {
                return Err(VolumeFsError::NotFound);
            }
        }
        Ok(backing)
    }

    /// Reads a directory's plaintext entries by decrypting every backing
    /// entry name against that directory's own IV.
    pub fn read_dir_plain(
        &self,
        cipher: &NameCipher,
        backing_dir: &PathBuf,
    ) -> Result<Vec<(String, PathBuf, bool)>> {
        let dir_iv = self.dir_iv_for_backing(backing_dir)?;
        let mut out = Vec::new();
        for entry in std::fs::read_dir(backing_dir)? {
            let entry = entry?;
            let raw_name = entry.file_name();
            let raw_name = raw_name.to_string_lossy();
            // The IV file itself, plus any temp file left by an interrupted
            // `create_dir_iv_no_replace`, are bookkeeping, not entries.
            if raw_name.starts_with(DIR_IV_FILE) {
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

fn parse_dir_iv(bytes: &[u8]) -> Result<[u8; 16]> {
    bytes
        .try_into()
        .map_err(|_| VolumeFsError::Corrupt(format!("{DIR_IV_FILE} has the wrong length")))
}

/// Whether `dir` holds anything besides IV bookkeeping files.
fn has_encrypted_entries(dir: &Path) -> Result<bool> {
    for entry in std::fs::read_dir(dir).map_err(VolumeFsError::io_at("list a directory"))? {
        let name = entry?.file_name();
        if !name.to_string_lossy().starts_with(DIR_IV_FILE) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Writes a fresh random IV for `dir` without ever replacing an existing
/// one: the bytes go to a temp file first, which is then hard-linked into
/// place. The link fails if the target already exists, so two racing
/// creators (threads or processes) can never leave a directory with two
/// different IVs: the loser reads the winner's. A torn write can't occur
/// either, since the IV file only ever appears complete. On filesystems
/// without hard links (FAT/exFAT) it falls back to `create_new`, which
/// still never replaces an existing IV.
fn create_dir_iv_no_replace(dir: &Path) -> Result<[u8; 16]> {
    use std::io::Write;

    let mut iv = [0u8; 16];
    fill_random(&mut iv);
    let iv_path = dir.join(DIR_IV_FILE);
    let mut suffix = [0u8; 8];
    fill_random(&mut suffix);
    let tmp_name = format!(
        "{DIR_IV_FILE}.tmp-{}",
        suffix.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    let tmp_path = dir.join(tmp_name);

    let write_tmp = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
        f.write_all(&iv)?;
        f.sync_all()
    };
    let linked = write_tmp().and_then(|()| std::fs::hard_link(&tmp_path, &iv_path));
    // Best-effort: the temp file is ignored by listings either way.
    let _ = std::fs::remove_file(&tmp_path);
    match linked {
        Ok(()) => Ok(iv),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => parse_dir_iv(
            &std::fs::read(&iv_path).map_err(VolumeFsError::io_at("read a directory IV"))?,
        ),
        Err(_) => {
            // No hard-link support: create_new directly (still never replaces).
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&iv_path)
            {
                Ok(mut f) => {
                    f.write_all(&iv)
                        .map_err(VolumeFsError::io_at("write a directory IV"))?;
                    f.sync_all()
                        .map_err(VolumeFsError::io_at("write a directory IV"))?;
                    Ok(iv)
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => parse_dir_iv(
                    &std::fs::read(&iv_path).map_err(VolumeFsError::io_at("read a directory IV"))?,
                ),
                Err(e) => Err(VolumeFsError::io_at("write a directory IV")(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moku_crypto::Key;
    use tempfile::tempdir;

    fn key() -> NameCipher {
        NameCipher::new(Key::from_slice(&[4u8; 64]).unwrap())
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
        assert!(matches!(result, Err(VolumeFsError::NotFound)));
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

    #[test]
    fn test_missing_iv_in_populated_directory_is_an_error_not_a_reset() {
        let tmp = tempdir().unwrap();
        let mapper = PathMapper::new(tmp.path().join("data"));
        let k = key();
        mapper.ensure_root().unwrap();
        let root = mapper.data_root().clone();
        let root_iv = std::fs::read(root.join(DIR_IV_FILE)).unwrap();
        // An encrypted entry exists, then the IV file goes missing.
        let entry = mapper.resolve(&k, &VirtualPath::parse("/a.md")).unwrap();
        std::fs::write(&entry, b"x").unwrap();
        std::fs::remove_file(root.join(DIR_IV_FILE)).unwrap();

        let fresh = PathMapper::new(root.clone());
        assert!(matches!(
            fresh.read_dir_plain(&k, &root),
            Err(VolumeFsError::MissingDirIv { .. })
        ));
        assert!(
            !root.join(DIR_IV_FILE).exists(),
            "a new IV must never be minted for a populated directory"
        );
        // Restoring the original IV brings the names back.
        std::fs::write(root.join(DIR_IV_FILE), &root_iv).unwrap();
        let names: Vec<String> = fresh
            .read_dir_plain(&k, &root)
            .unwrap()
            .into_iter()
            .map(|(n, _, _)| n)
            .collect();
        assert_eq!(names, vec!["a.md".to_string()]);
    }

    #[test]
    fn test_missing_iv_in_empty_directory_is_created() {
        let tmp = tempdir().unwrap();
        let mapper = PathMapper::new(tmp.path().join("data"));
        mapper.ensure_root().unwrap();
        let dir = mapper.data_root().join("empty");
        std::fs::create_dir(&dir).unwrap();
        mapper.init_dir_iv(&dir).unwrap();
        assert_eq!(std::fs::read(dir.join(DIR_IV_FILE)).unwrap().len(), 16);
    }

    #[test]
    fn test_create_dir_iv_never_replaces_an_existing_iv() {
        let tmp = tempfile::tempdir().unwrap();
        let first = create_dir_iv_no_replace(tmp.path()).unwrap();
        let second = create_dir_iv_no_replace(tmp.path()).unwrap();
        assert_eq!(first, second, "a second creator must read the first IV");
        assert_eq!(std::fs::read(tmp.path().join(DIR_IV_FILE)).unwrap(), first);
        let leftovers = std::fs::read_dir(tmp.path()).unwrap().count();
        assert_eq!(leftovers, 1, "temp files are cleaned up");
    }

    #[test]
    fn test_concurrent_creators_agree_on_one_iv() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let d = dir.clone();
                std::thread::spawn(move || create_dir_iv_no_replace(&d).unwrap())
            })
            .collect();
        let ivs: Vec<[u8; 16]> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(ivs.windows(2).all(|w| w[0] == w[1]));
    }
}
