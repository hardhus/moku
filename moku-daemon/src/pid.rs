use anyhow::Result;

pub fn write() -> Result<()> {
    let data_dir = moku_core::dirs::get_data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    std::fs::write(
        data_dir.join("moku_daemon.pid"),
        std::process::id().to_string(),
    )?;
    Ok(())
}

pub fn remove() {
    if let Ok(data_dir) = moku_core::dirs::get_data_dir() {
        let _ = std::fs::remove_file(data_dir.join("moku_daemon.pid"));
    }
}

pub fn read() -> Option<u32> {
    let data_dir = moku_core::dirs::get_data_dir().ok()?;
    let s = std::fs::read_to_string(data_dir.join("moku_daemon.pid")).ok()?;
    s.trim().parse().ok()
}
