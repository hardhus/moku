use anyhow::Result;
use sysinfo::{Pid, System};

pub fn is_running() -> bool {
    match crate::pid::read() {
        None => false,
        Some(pid_val) => {
            let mut sys = System::new_all();
            sys.refresh_all();
            sys.process(Pid::from_u32(pid_val)).is_some()
        }
    }
}

pub fn print_status() -> Result<()> {
    match crate::pid::read() {
        None => {
            println!("⚫ Moku Daemon is not running (no pid file).");
        }
        Some(pid_val) => {
            let mut sys = System::new_all();
            sys.refresh_all();
            if sys.process(Pid::from_u32(pid_val)).is_some() {
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
            let mut sys = System::new_all();
            sys.refresh_all();
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
