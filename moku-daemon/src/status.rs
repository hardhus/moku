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

#[cfg(test)]
fn spawn_short_lived_process() -> std::process::Child {
    // A process that stays alive for a few seconds without needing a
    // console/TTY (unlike Windows' `timeout`, which errors on redirected
    // stdin in a test runner) or an external binary that may not exist.
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", "ping", "127.0.0.1", "-n", "6"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("failed to spawn test process")
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("failed to spawn test process")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pid_is_alive_true_for_running_process() {
        let mut child = spawn_short_lived_process();
        let pid = child.id();
        assert!(pid_is_alive(pid), "freshly spawned process should be alive");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn test_pid_is_alive_false_after_kill() {
        let mut child = spawn_short_lived_process();
        let pid = child.id();
        child.kill().expect("failed to kill test process");
        child.wait().expect("failed to wait for test process");
        // Give the OS a brief moment to reap the process table entry.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!pid_is_alive(pid), "killed process should not be alive");
    }

    #[test]
    fn test_pid_is_alive_false_for_implausible_pid() {
        assert!(!pid_is_alive(u32::MAX));
    }
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
