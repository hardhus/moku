use anyhow::{Result, anyhow, bail};

use moku_vault_daemon::{PasswordMode, registry, size, status};

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
    }
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
