use std::io::Write;

use anyhow::{Result, anyhow, bail};

use moku_vault_daemon::{PasswordMode, control, pid, registry, size, status, worker};

use crate::cli::VaultCommands;

pub async fn handle(sub: &VaultCommands) -> Result<()> {
    match sub {
        VaultCommands::Create { name, size: size_str, custom_password } => {
            let size_bytes = size::parse_size(size_str)?;
            let mode = if *custom_password { PasswordMode::Custom } else { PasswordMode::Default };
            let password = prompt_new_password(mode)?;

            let cfg = registry::create_volume(name, size_bytes, mode, password).await?;
            println!(
                "✅ Created encrypted volume '{}' (id: {}, size limit: {})",
                cfg.display_name,
                cfg.id,
                size::format_size(cfg.size_limit_bytes)
            );
            Ok(())
        }
        VaultCommands::List => {
            let volumes = registry::list_volumes().await?;
            if volumes.is_empty() {
                println!("No encrypted volumes yet. To create one: moku vault create <name> --size 10GB");
                return Ok(());
            }
            for v in &volumes {
                let used = registry::usage_bytes(&v.id).unwrap_or(0);
                let mounted = status::is_mounted(&v.id);
                println!(
                    "- {} ({})  {} / {}  [{}]",
                    v.display_name,
                    v.id,
                    size::format_size(used),
                    size::format_size(v.size_limit_bytes),
                    if mounted { "mounted" } else { "not mounted" }
                );
            }
            Ok(())
        }
        VaultCommands::Status { name } => {
            let cfg = registry::find_volume(name).await?;
            let used = registry::usage_bytes(&cfg.id).unwrap_or(0);
            let mounted = status::is_mounted(&cfg.id);
            println!("Volume:        {} (id: {})", cfg.display_name, cfg.id);
            println!("Status:        {}", if mounted { "🟢 mounted" } else { "⚫ not mounted" });
            println!("Usage:         {} / {}", size::format_size(used), size::format_size(cfg.size_limit_bytes));
            println!("Password mode: {}", if cfg.password_mode == PasswordMode::Default { "default (moku vault password)" } else { "custom" });
            Ok(())
        }
        VaultCommands::Resize { name, size: size_str } => {
            let size_bytes = size::parse_size(size_str)?;
            let cfg = registry::resize_volume(name, size_bytes).await?;
            println!(
                "✅ '{}' resized to {} (takes effect on its next mount)",
                cfg.display_name,
                size::format_size(cfg.size_limit_bytes)
            );
            Ok(())
        }
        VaultCommands::Mount { name, mountpoint } => mount(name, mountpoint).await,
        VaultCommands::Unmount { name } => unmount(name).await,
        VaultCommands::MountWorker { name, mountpoint } => worker::run(name, mountpoint).await,
    }
}

async fn mount(name: &str, mountpoint: &str) -> Result<()> {
    let cfg = registry::find_volume(name).await?;
    if status::is_mounted(&cfg.id) {
        println!("'{}' is already mounted.", cfg.display_name);
        return Ok(());
    }

    let prompt_label = match cfg.password_mode {
        PasswordMode::Default => "Moku vault password: ",
        PasswordMode::Custom => "Volume password: ",
    };
    let password = rpassword::prompt_password(prompt_label).map_err(|e| anyhow!("Failed to read password: {e}"))?;

    let exe = std::env::current_exe().map_err(|e| anyhow!("failed to resolve current executable: {e}"))?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("vault").arg("mount-worker").arg(&cfg.id).arg("--mountpoint").arg(mountpoint);
    cmd.stdin(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().map_err(|e| anyhow!("failed to spawn mount worker: {e}"))?;

    // Password crosses the process boundary only via the child's stdin
    // pipe — never as a CLI arg (visible in Task Manager/`ps`) or env var
    // (visible in /proc/<pid>/environ). Dropping the handle closes the
    // pipe (EOF), telling the worker the whole password has been sent.
    {
        let mut stdin = child.stdin.take().expect("stdin was requested as piped");
        stdin.write_all(password.as_bytes()).map_err(|e| anyhow!("failed to send password to mount worker: {e}"))?;
    }

    println!("✅ Mounting '{}' at {} (worker PID: {})...", cfg.display_name, mountpoint, child.id());
    Ok(())
}

async fn unmount(name: &str) -> Result<()> {
    let cfg = registry::find_volume(name).await?;
    let Some(worker_pid) = pid::read(&cfg.id) else {
        println!("'{}' is not mounted.", cfg.display_name);
        return Ok(());
    };
    if !status::pid_is_alive(worker_pid) {
        println!("'{}' has a stale mount record; cleaning up.", cfg.display_name);
        pid::remove(&cfg.id);
        return Ok(());
    }

    // Preferred path: ask the worker to stop over its control channel, so
    // it reaches mount_and_wait's own clean unmount + usage flush, then
    // wait for it to actually exit. Only fall back to a hard kill (which
    // skips both of those and risks a stuck mount point on some FUSE/
    // WinFsp versions) if the graceful request can't be delivered or the
    // worker doesn't exit within a few seconds.
    if control::send_stop(&cfg.id).await.is_ok() {
        for _ in 0..25 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if !status::pid_is_alive(worker_pid) {
                pid::remove(&cfg.id);
                println!("✅ Unmounted '{}'.", cfg.display_name);
                return Ok(());
            }
        }
    }

    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill").args(["/PID", &worker_pid.to_string(), "/F"]).output();
    #[cfg(not(windows))]
    let _ = std::process::Command::new("kill").arg(worker_pid.to_string()).output();

    std::thread::sleep(std::time::Duration::from_millis(400));
    pid::remove(&cfg.id);
    println!("✅ Unmounted '{}' (forced).", cfg.display_name);
    Ok(())
}

fn prompt_new_password(mode: PasswordMode) -> Result<String> {
    match mode {
        PasswordMode::Default => rpassword::prompt_password("Moku vault password (this volume's default password too): ")
            .map_err(|e| anyhow!("Failed to read password: {e}")),
        PasswordMode::Custom => {
            let p1 = rpassword::prompt_password("New volume password: ").map_err(|e| anyhow!("Failed to read password: {e}"))?;
            let p2 = rpassword::prompt_password("Confirm volume password: ").map_err(|e| anyhow!("Failed to read password: {e}"))?;
            if p1 != p2 {
                bail!("passwords did not match");
            }
            Ok(p1)
        }
    }
}
