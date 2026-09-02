use anyhow::Result;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Refreshes only the given PID's process entry (no full system scan).
pub(crate) fn refresh_single(pid: u32) -> System {
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

pub fn is_running() -> bool {
    match crate::pid::read() {
        None => false,
        Some(pid_val) => pid_is_alive(pid_val),
    }
}

pub fn print_status() -> Result<()> {
    match crate::pid::read() {
        None => {
            println!("⚫ Moku Daemon is not running (no pid file).");
        }
        Some(pid_val) => {
            if pid_is_alive(pid_val) {
                println!("🟢 Moku Daemon is running (PID: {pid_val}).");
            } else {
                println!("⚫ Moku Daemon is not running (stale pid file found, cleaning up).");
                crate::pid::remove();
            }
        }
    }
    Ok(())
}

pub fn stop_daemon() -> Result<()> {
    match crate::pid::read() {
        None => {
            println!("Daemon is not running (no PID file).");
        }
        Some(pid_val) => {
            let sys = refresh_single(pid_val);
            if let Some(process) = sys.process(Pid::from_u32(pid_val)) {
                process.kill();
                println!("Stopping daemon (PID: {pid_val})...");
                std::thread::sleep(std::time::Duration::from_millis(150));
                crate::pid::remove();
                println!("✅ Daemon stopped.");
            } else {
                println!("Daemon process (PID: {pid_val}) not found. Cleaning up stale PID file.");
                crate::pid::remove();
            }
        }
    }
    Ok(())
}
