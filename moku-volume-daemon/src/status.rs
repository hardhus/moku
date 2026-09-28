//! Targeted process-liveness check — a small, deliberate duplicate of
//! `moku-daemon/src/status.rs`'s `refresh_single`/`pid_is_alive` rather
//! than a shared dependency between the two independent background-
//! process features (plan §3).

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

fn refresh_single(pid: u32) -> System {
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[Pid::from_u32(pid)]), true, ProcessRefreshKind::nothing());
    sys
}

pub fn pid_is_alive(pid: u32) -> bool {
    refresh_single(pid).process(Pid::from_u32(pid)).is_some()
}

/// Whether this volume currently has a live mount worker process: its
/// `mount.pid` (written by the worker once the mount is live) names a
/// process that is still running.
/// Whether some moku process currently holds this volume's cross-process
/// `VolumeLock` (a mount worker, or anything else with the volume open).
/// Checked before asking for a password so a second mount attempt fails
/// fast; only a hint, since the worker takes the real lock itself.
pub fn is_in_use(volume_id: &str) -> bool {
    crate::registry::volume_dir(volume_id)
        .ok()
        .and_then(|dir| moku_volume_fs::VolumeLock::is_held(&dir).ok())
        .unwrap_or(false)
}

pub fn is_mounted(volume_id: &str) -> bool {
    match crate::pid::read(volume_id) {
        None => false,
        Some(pid) => pid_is_alive(pid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pid_is_alive_false_for_implausible_pid() {
        assert!(!pid_is_alive(u32::MAX));
    }

    #[test]
    fn test_is_mounted_false_when_no_pid_file() {
        assert!(!is_mounted("definitely-not-a-real-volume-id"));
    }
}
