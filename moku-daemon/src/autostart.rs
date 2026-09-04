use anyhow::Result;
use auto_launch::AutoLaunchBuilder;

fn build_launcher(exe_path: &std::path::Path, args: &[&str]) -> Result<auto_launch::AutoLaunch> {
    AutoLaunchBuilder::new()
        .set_app_name("moku-daemon")
        .set_app_path(&exe_path.to_string_lossy())
        .set_args(args)
        .build()
        .map_err(|e| anyhow::anyhow!("AutoLaunch error: {e}"))
}

/// Enable or disable autostart.
/// `exe_path` should be the path to the current executable.
/// `args` are the CLI arguments to execute.
///
/// No user-facing output here — callers own how (or whether) to report
/// success, since this is shared between the CLI (which prints to stdout)
/// and the TUI (which must never write outside its own alternate screen).
pub fn set_autostart(enable: bool, exe_path: &std::path::Path, args: &[&str]) -> Result<()> {
    let launcher = build_launcher(exe_path, args)?;
    if enable {
        launcher.enable().map_err(|e| anyhow::anyhow!("Failed to enable autostart: {e}"))?;
    } else {
        launcher.disable().map_err(|e| anyhow::anyhow!("Failed to disable autostart: {e}"))?;
    }
    Ok(())
}

/// Whether moku is currently registered as a system autostart entry.
/// `args` only needs to match what `set_autostart` was called with for the
/// registered command line to be meaningful to read back — `is_enabled`
/// itself only checks the registry key's presence, not its content.
pub fn is_autostart_enabled(exe_path: &std::path::Path, args: &[&str]) -> bool {
    build_launcher(exe_path, args).and_then(|l| l.is_enabled().map_err(|e| anyhow::anyhow!(e))).unwrap_or(false)
}
