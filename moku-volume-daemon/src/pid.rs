//! Per-volume pid tracking — a generalization of `moku-daemon/src/pid.rs`
//! keyed by volume id, since more than one volume can be mounted (each in
//! its own OS process) at a time (plan §3).

use std::path::PathBuf;

use anyhow::Result;

use crate::registry::volume_dir;

const PID_FILE: &str = "mount.pid";

pub fn pid_file_path(volume_id: &str) -> Result<PathBuf> {
    Ok(volume_dir(volume_id)?.join(PID_FILE))
}

pub fn write(volume_id: &str, pid: u32) -> Result<()> {
    let path = pid_file_path(volume_id)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, pid.to_string())?;
    Ok(())
}

pub fn remove(volume_id: &str) {
    if let Ok(path) = pid_file_path(volume_id) {
        let _ = std::fs::remove_file(path);
    }
}

pub fn read(volume_id: &str) -> Option<u32> {
    let path = pid_file_path(volume_id).ok()?;
    let s = std::fs::read_to_string(path).ok()?;
    s.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pid_file_path_is_under_volume_dir() {
        let path = pid_file_path("some-volume").unwrap();
        assert!(path.ends_with("mount.pid"));
        assert!(path.to_string_lossy().contains("some-volume"));
    }
}
