use anyhow::Result;
use auto_launch::AutoLaunchBuilder;

/// Enable or disable autostart.
/// `exe_path` should be the path to the current executable.
/// `args` are the CLI arguments to execute.
pub fn set_autostart(enable: bool, exe_path: &std::path::Path, args: &[&str]) -> Result<()> {
    let launcher = AutoLaunchBuilder::new()
        .set_app_name("moku-daemon")
        .set_app_path(&exe_path.to_string_lossy())
        .set_args(args)
        .build()
        .map_err(|e| anyhow::anyhow!("AutoLaunch error: {e}"))?;

    if enable {
        launcher
            .enable()
            .map_err(|e| anyhow::anyhow!("Failed to enable autostart: {e}"))?;
        println!("✅ Moku added to system autostart.");
    } else {
        launcher
            .disable()
            .map_err(|e| anyhow::anyhow!("Failed to disable autostart: {e}"))?;
        println!("🧹 Moku removed from system autostart.");
    }
    Ok(())
}
