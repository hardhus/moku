use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default)]
struct UsageFile {
    used_bytes: u64,
}

/// Soft, physical-bytes-based quota tracker (plan §4). There is no fixed
/// backing image to enforce a hard OS-level limit against — every write
/// past the limit is rejected explicitly by the engine before it touches
/// disk.
pub struct Quota {
    usage_path: PathBuf,
    used: AtomicU64,
    limit: AtomicU64,
}

impl Quota {
    pub fn load(usage_path: PathBuf, limit_bytes: u64) -> Self {
        let used = std::fs::read_to_string(&usage_path)
            .ok()
            .and_then(|s| serde_json::from_str::<UsageFile>(&s).ok())
            .map(|u| u.used_bytes)
            .unwrap_or(0);
        Self {
            usage_path,
            used: AtomicU64::new(used),
            limit: AtomicU64::new(limit_bytes),
        }
    }

    pub fn used_bytes(&self) -> u64 {
        self.used.load(Ordering::SeqCst)
    }

    pub fn limit_bytes(&self) -> u64 {
        self.limit.load(Ordering::SeqCst)
    }

    pub fn set_limit(&self, new_limit: u64) {
        self.limit.store(new_limit, Ordering::SeqCst);
    }

    /// Reserves `delta` additional physical bytes, failing without
    /// mutating state if that would exceed the size limit.
    pub fn try_grow(&self, delta: u64) -> bool {
        if delta == 0 {
            return true;
        }
        loop {
            let current = self.used.load(Ordering::SeqCst);
            let projected = current + delta;
            if projected > self.limit.load(Ordering::SeqCst) {
                return false;
            }
            if self
                .used
                .compare_exchange(current, projected, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return true;
            }
        }
    }

    pub fn shrink(&self, delta: u64) {
        if delta == 0 {
            return;
        }
        loop {
            let current = self.used.load(Ordering::SeqCst);
            let next = current.saturating_sub(delta);
            if self
                .used
                .compare_exchange(current, next, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return;
            }
        }
    }

    pub fn flush(&self) -> Result<()> {
        let data = UsageFile { used_bytes: self.used_bytes() };
        if let Some(parent) = self.usage_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.usage_path, serde_json::to_string(&data)?)?;
        Ok(())
    }

    /// Recomputes `used_bytes` from an actual on-disk scan — the
    /// self-healing path after an unclean shutdown, where the cached
    /// counter may be stale (plan §4).
    pub fn reconcile_from_scan(&self, data_root: &Path) -> Result<()> {
        let total = dir_physical_size(data_root)?;
        self.used.store(total, Ordering::SeqCst);
        Ok(())
    }
}

fn dir_physical_size(dir: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            total += dir_physical_size(&entry.path())?;
        } else if ty.is_file() {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_try_grow_within_limit_succeeds() {
        let dir = tempdir().unwrap();
        let q = Quota::load(dir.path().join("usage.json"), 100);
        assert!(q.try_grow(50));
        assert_eq!(q.used_bytes(), 50);
    }

    #[test]
    fn test_try_grow_over_limit_fails_and_does_not_mutate() {
        let dir = tempdir().unwrap();
        let q = Quota::load(dir.path().join("usage.json"), 100);
        assert!(q.try_grow(90));
        assert!(!q.try_grow(20));
        assert_eq!(q.used_bytes(), 90, "failed reservation must not change usage");
    }

    #[test]
    fn test_shrink_never_underflows() {
        let dir = tempdir().unwrap();
        let q = Quota::load(dir.path().join("usage.json"), 100);
        q.shrink(50);
        assert_eq!(q.used_bytes(), 0);
    }

    #[test]
    fn test_flush_and_reload_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let q = Quota::load(path.clone(), 1000);
        q.try_grow(123);
        q.flush().unwrap();

        let q2 = Quota::load(path, 1000);
        assert_eq!(q2.used_bytes(), 123);
    }

    #[test]
    fn test_reconcile_from_scan_matches_real_files() {
        let dir = tempdir().unwrap();
        let data_root = dir.path().join("data");
        std::fs::create_dir_all(data_root.join("sub")).unwrap();
        std::fs::write(data_root.join("a"), vec![0u8; 10]).unwrap();
        std::fs::write(data_root.join("sub").join("b"), vec![0u8; 20]).unwrap();

        let q = Quota::load(dir.path().join("usage.json"), 1000);
        q.reconcile_from_scan(&data_root).unwrap();
        assert_eq!(q.used_bytes(), 30);
    }
}
