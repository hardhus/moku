//! Singleton pid tracking, mirroring `moku-daemon/src/pid.rs` — one well-
//! known file, since (unlike `moku-volume-daemon`) only one pomodoro
//! daemon ever runs at a time.

use anyhow::Result;

const PID_FILE: &str = "moku_pomodoro.pid";

pub fn write() -> Result<()> {
    let data_dir = moku_core::dirs::get_data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    std::fs::write(data_dir.join(PID_FILE), std::process::id().to_string())?;
    Ok(())
}

pub fn remove() {
    if let Ok(data_dir) = moku_core::dirs::get_data_dir() {
        let _ = std::fs::remove_file(data_dir.join(PID_FILE));
    }
}

pub fn read() -> Option<u32> {
    let data_dir = moku_core::dirs::get_data_dir().ok()?;
    let s = std::fs::read_to_string(data_dir.join(PID_FILE)).ok()?;
    s.trim().parse().ok()
}
