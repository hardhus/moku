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

/// Whether this volume currently has a live mount worker process. Mount
/// itself lands in a later phase, so today this only ever reports `false`
/// (no pid file is ever written yet) — kept here now so `vault list`/
/// `status` display code doesn't need to change shape once mounting
/// exists.
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
