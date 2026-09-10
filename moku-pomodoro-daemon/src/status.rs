//! Targeted process-liveness check — a small, deliberate duplicate of
//! `moku-daemon/src/status.rs`/`moku-volume-daemon/src/status.rs`'s
//! `pid_is_alive`, same rationale as those two: independent background-
//! process features, not worth a shared dependency for one function.

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

fn refresh_single(pid: u32) -> System {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys
}

pub fn pid_is_alive(pid: u32) -> bool {
    refresh_single(pid).process(Pid::from_u32(pid)).is_some()
}

/// Whether a pomodoro daemon process is currently alive, based on the
/// pid file. Doesn't distinguish "idle, waiting for Start" from "actively
/// running a phase" — that distinction needs a live `QueryStatus` round
/// trip (see `control::send_request`), this is just process liveness.
pub fn is_running() -> bool {
    match crate::pid::read() {
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
}
