use anyhow::{Result, anyhow, bail};

use moku_core::SecurityManager;
use moku_vault_daemon::registry::VolumeSecret;
use moku_vault_daemon::worker::{MountOutcome, StopOutcome};
use moku_vault_daemon::{PasswordMode, registry, size, status, worker};

use crate::cli::VaultCommands;

pub async fn handle(sub: &VaultCommands) -> Result<()> {
    match sub {
        VaultCommands::Create {
            name,
            size: size_str,
            custom_password,
            path,
        } => {
            let size_bytes = size::parse_size(size_str)?;
            let base_dir = path.as_deref().map(std::path::PathBuf::from);

            let secret = if *custom_password {
                let p1 = rpassword::prompt_password("New volume password: ")
                    .map_err(|e| anyhow!("Failed to read password: {e}"))?;
                let p2 = rpassword::prompt_password("Confirm volume password: ")
                    .map_err(|e| anyhow!("Failed to read password: {e}"))?;
                if p1 != p2 {
                    bail!("passwords did not match");
                }
                VolumeSecret::Password(p1)
            } else {
                // Default mode: the volume gets no password of its own —
                // its key is derived from moku's real app vault, so this
                // must actually be verified against it (not just typed
                // twice and assumed to match, which is what silently
                // failed before this).
                let app_security = SecurityManager::new().map_err(|e| anyhow!("{e}"))?;
                if !app_security.is_vault_initialized() {
                    bail!(
                        "moku's vault isn't set up yet — initialize it first from the Vault Security screen in the TUI, or create this volume with --custom-password instead."
                    );
                }
                let password = rpassword::prompt_password("Moku vault password: ")
                    .map_err(|e| anyhow!("Failed to read password: {e}"))?;
                let key = app_security
                    .unlock_vault(password)
                    .await
                    .map_err(|e| anyhow!("failed to unlock moku vault: {e}"))?;
                VolumeSecret::FromAppVault(key)
            };

            let cfg = registry::create_volume(name, size_bytes, secret, base_dir).await?;
            let dir = registry::volume_dir(&cfg.id)?;
            println!(
                "✅ Created encrypted volume '{}' (id: {}, size limit: {}) at {}",
                cfg.display_name,
                cfg.id,
                size::format_size(cfg.size_limit_bytes),
                dir.display()
            );
            Ok(())
        }
        VaultCommands::List => {
            let volumes = registry::list_volumes().await?;
            if volumes.is_empty() {
                println!(
                    "No encrypted volumes yet. To create one: moku vault create <name> --size 10GB"
                );
                return Ok(());
            }
            for v in &volumes {
                let used = registry::usage_bytes(&v.id).unwrap_or(0);
                let mounted = status::is_mounted(&v.id);
                let dir = registry::volume_dir(&v.id)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                println!(
                    "- {} ({})  {} / {}  [{}]\n    {}",
                    v.display_name,
                    v.id,
                    size::format_size(used),
                    size::format_size(v.size_limit_bytes),
                    if mounted { "mounted" } else { "not mounted" },
                    dir
                );
            }
            Ok(())
        }
        VaultCommands::Status { name } => {
            let cfg = registry::find_volume(name).await?;
            let used = registry::usage_bytes(&cfg.id).unwrap_or(0);
            let mounted = status::is_mounted(&cfg.id);
            println!("Volume:        {} (id: {})", cfg.display_name, cfg.id);
            println!(
                "Location:      {}",
                registry::volume_dir(&cfg.id)?.display()
            );
            println!(
                "Status:        {}",
                if mounted {
                    "🟢 mounted"
                } else {
                    "⚫ not mounted"
                }
            );
            println!(
                "Usage:         {} / {}",
                size::format_size(used),
                size::format_size(cfg.size_limit_bytes)
            );
            println!(
                "Password mode: {}",
                if cfg.password_mode == PasswordMode::Default {
                    "default (moku vault password)"
                } else {
                    "custom"
                }
            );
            Ok(())
        }
        VaultCommands::Resize {
            name,
            size: size_str,
        } => {
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
        VaultCommands::Import { path } => {
            let cfg = registry::import_volume(std::path::Path::new(path)).await?;
            println!("✅ Imported '{}' (id: {}).", cfg.display_name, cfg.id);
            Ok(())
        }
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
    let password = rpassword::prompt_password(prompt_label)
        .map_err(|e| anyhow!("Failed to read password: {e}"))?;

    match worker::spawn_mount_process(&cfg.id, mountpoint, &password).await? {
        MountOutcome::Ready { pid } => {
            println!(
                "✅ Mounted '{}' at {} (worker PID: {}).",
                cfg.display_name, mountpoint, pid
            );
        }
        MountOutcome::Failed { message } => bail!("Mount failed: {message}"),
        MountOutcome::TimedOut { pid } => {
            println!(
                "⏳ '{}' is starting in the background (worker PID: {}) — check `moku vault status {}` to confirm.",
                cfg.display_name, pid, name
            );
        }
    }
    Ok(())
}

async fn unmount(name: &str) -> Result<()> {
    let cfg = registry::find_volume(name).await?;
    match worker::stop_mount_process(&cfg.id).await? {
        StopOutcome::NotMounted => println!("'{}' is not mounted.", cfg.display_name),
        StopOutcome::StaleCleanedUp => println!(
            "'{}' had a stale mount record; cleaned up.",
            cfg.display_name
        ),
        StopOutcome::Graceful => println!("✅ Unmounted '{}'.", cfg.display_name),
        StopOutcome::Forced => println!("✅ Unmounted '{}' (forced).", cfg.display_name),
    }
    Ok(())
}
