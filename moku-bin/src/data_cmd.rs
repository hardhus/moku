//! `moku data`: the data-format status, the one-shot upgrade and the
//! pre-upgrade backups (audit P4). Unrelated to `moku config migrate`,
//! which only toggles a module between encrypted and plaintext storage.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use moku_migrate::{BACKUPS_DIR, CURRENT_DATA_FORMAT, DataState, FORMAT_FILE};
use moku_vault::{SecurityManager, VaultSession};

use crate::cli::DataCommands;
use crate::data_upgrade::MOKU_VERSION;
use crate::volume_cmd::prompt_line;

pub async fn handle(sub: &DataCommands, data_dir: &Path) -> Result<()> {
    match sub {
        DataCommands::Status => status(data_dir).await,
        DataCommands::Upgrade => upgrade(data_dir).await,
        DataCommands::Backups => backups(data_dir),
    }
}

async fn status(data_dir: &Path) -> Result<()> {
    println!("Data directory: {}", data_dir.display());
    let state = match moku_migrate::detect_format(data_dir) {
        Ok(state) => state,
        Err(e) => {
            println!("Format: unreadable ({:#})", anyhow::Error::from(e));
            return backups(data_dir);
        }
    };
    let inferred = matches!(
        state,
        DataState::Current { stamped: false } | DataState::Behind { stamped: false, .. }
    );
    match state {
        DataState::Fresh => println!("Format: none yet (nothing stored)"),
        DataState::Current { .. } => println!("Format: {CURRENT_DATA_FORMAT} (current)"),
        DataState::Behind { from, .. } => {
            println!(
                "Format: {from} (this moku uses {CURRENT_DATA_FORMAT}; run `moku data upgrade`)"
            );
            println!("Pending steps:");
            for hop in moku_migrate::pending_hops(from) {
                let unlock = if hop.needs_unlock {
                    " (needs the vault password)"
                } else {
                    ""
                };
                println!("  - {}{unlock}", hop.description);
            }
        }
        DataState::Newer { found } => println!(
            "Format: {found}, newer than this moku ({CURRENT_DATA_FORMAT}); use a newer moku"
        ),
    }
    if inferred {
        println!("({FORMAT_FILE} not written yet; inferred from vault/.key_scheme_version)");
    }
    volumes().await;
    backups(data_dir)
}

/// Volumes carry their own format, upgraded when each is next mounted.
async fn volumes() {
    let Ok(list) = moku_volume_daemon::registry::list_volumes().await else {
        return;
    };
    if list.is_empty() {
        return;
    }
    println!(
        "Volumes (format upgraded on their next mount; this moku uses v{}):",
        moku_migrate::volume::CURRENT_VOLUME_FORMAT
    );
    for v in list {
        let format = moku_volume_daemon::registry::volume_dir(&v.id)
            .map_err(|e| format!("{e:#}"))
            .and_then(|dir| {
                moku_migrate::volume::volume_format(&dir).map_err(|e| format!("{:#}", anyhow::Error::from(e)))
            });
        match format {
            Ok(f) => println!("  {} ({}): v{f}", v.display_name, v.id),
            Err(e) => println!("  {} ({}): unreadable ({e})", v.display_name, v.id),
        }
    }
}

fn backups(data_dir: &Path) -> Result<()> {
    let list = moku_migrate::list_backups(data_dir)?;
    if list.is_empty() {
        println!("Backups: none");
        return Ok(());
    }
    println!(
        "Backups ({}, never deleted automatically):",
        data_dir.join(BACKUPS_DIR).display()
    );
    for b in list {
        println!(
            "  {}  (format {}, {:.1} MiB)",
            b.name,
            b.from,
            b.bytes as f64 / (1024.0 * 1024.0)
        );
    }
    Ok(())
}

async fn upgrade(data_dir: &Path) -> Result<()> {
    let from = match moku_migrate::detect_format(data_dir)? {
        DataState::Fresh | DataState::Current { .. } => {
            println!("The data format is already up to date ({CURRENT_DATA_FORMAT}).");
            return Ok(());
        }
        DataState::Newer { found } => {
            return Err(moku_migrate::MigrateError::NewerFormat {
                found,
                supported: CURRENT_DATA_FORMAT,
            }
            .into());
        }
        DataState::Behind { from, .. } => from,
    };
    let hops = moku_migrate::pending_hops(from);
    println!("Data format upgrade: {from} -> {CURRENT_DATA_FORMAT}");
    for hop in hops {
        println!("  - {}", hop.description);
    }

    if moku_daemon::status::is_running() {
        let answer = prompt_line(
            "The background daemon is running and holds the data open. Stop it? [y/N]: ",
        )?;
        if !is_yes(&answer) {
            bail!("the daemon must be stopped first (`moku daemon stop`)");
        }
        moku_daemon::status::stop_daemon_and_wait(Duration::from_secs(5))?;
        println!("Daemon stopped.");
    }

    let answer = prompt_line(&format!(
        "Upgrade now? A backup of the data goes to {} first. [y/N]: ",
        data_dir.join(BACKUPS_DIR).display()
    ))?;
    if !is_yes(&answer) {
        println!("Cancelled.");
        return Ok(());
    }

    let session = Arc::new(VaultSession::new());
    if hops.iter().any(|h| h.needs_unlock) {
        let security = SecurityManager::new_with_root(data_dir.to_path_buf());
        moku_vault::cli::ensure_unlocked(&session, &security, "Vault password: ").await?;
    }

    let dir = data_dir.to_path_buf();
    let report = tokio::task::spawn_blocking(move || {
        let last = std::cell::RefCell::new(String::new());
        moku_migrate::upgrade(&dir, &session, MOKU_VERSION, &|p| {
            // One line per module, not per record.
            let line = format!("  {}: {}", p.hop, p.module);
            if *last.borrow() != line {
                println!("{line}");
                *last.borrow_mut() = line;
            }
        })
    })
    .await
    .map_err(|e| anyhow!("the upgrade task failed: {e}"))??;
    println!("{}", report.summary());
    Ok(())
}

fn is_yes(answer: &str) -> bool {
    matches!(answer, "y" | "Y" | "yes" | "Yes" | "YES")
}
