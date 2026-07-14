use anyhow::Result;
use sysinfo::{Pid, System};

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
