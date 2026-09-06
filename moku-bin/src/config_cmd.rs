use std::sync::Arc;

use anyhow::{Result, anyhow};
use arc_swap::ArcSwap;

use moku_core::{ConfigManager, MokuConfig, SecurityManager, StorageManager, VaultSession};

use crate::cli::ConfigCommands;

/// (module id, ModuleMeta::encrypt_by_default()) for every module whose
/// storage encryption is actually resolved via moku_core::resolve_encryption
/// today (see Faz 5) — kept in sync with the ModuleMeta::encrypt_by_default
/// overrides in moku-todo, moku-bookmark, moku-rss and moku-secrets, and
/// with modules/moku-settings/src/tabs/storage.rs's identical list for the
/// TUI equivalent. `secrets` is included so `config migrate` can also
/// re-key it to the current per-module HKDF storage key scheme.
///
/// `pub(crate)` so `app_loop.rs`'s startup key-scheme-version check can
/// reuse this same module id list instead of introducing a third
/// hand-kept copy alongside this one and moku-settings's.
pub(crate) const ENCRYPTABLE_MODULES: &[(&str, bool)] =
    &[("todo", true), ("bookmark", true), ("rss", false), ("secrets", true)];

pub async fn handle(
    sub: &ConfigCommands,
    config: &Arc<ArcSwap<MokuConfig>>,
    session: &Arc<VaultSession>,
    security: &Arc<SecurityManager>,
    storage: &Arc<StorageManager>,
) -> Result<()> {
    match sub {
        ConfigCommands::ShowEncrypt => {
            let cfg = config.load();
            for (module, module_default) in ENCRYPTABLE_MODULES {
                let encrypted = moku_core::resolve_encryption(&cfg, module, *module_default);
                println!("{module}: {}", if encrypted { "encrypted" } else { "plaintext" });
            }
            Ok(())
        }
        ConfigCommands::SetEncrypt { module, value } => {
            let (_, module_default) = lookup_module(module)?;

            let mut new_config = (**config.load()).clone();
            let table = new_config
                .modules
                .entry(module.clone())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            if let toml::Value::Table(t) = table {
                t.insert("encrypt".to_string(), toml::Value::Boolean(*value));
            }
            ConfigManager::save(&new_config).await?;
            config.store(Arc::new(new_config.clone()));
            println!(
                "✅ [modules.{module}].encrypt = {value} written to config.toml"
            );

            ensure_unlocked_if_needed(session, security, *value).await?;

            run_migration(storage, module, moku_core::resolve_encryption(&new_config, module, module_default)).await
        }
        ConfigCommands::Migrate { module: Some(module) } => {
            let (_, module_default) = lookup_module(module)?;
            let cfg = config.load();
            let target = moku_core::resolve_encryption(&cfg, module, module_default);
            ensure_unlocked_if_needed(session, security, target).await?;
            run_migration(storage, module, target).await
        }
        ConfigCommands::Migrate { module: None } => {
            let cfg = config.load();
            for (module, module_default) in ENCRYPTABLE_MODULES {
                let target = moku_core::resolve_encryption(&cfg, module, *module_default);
                ensure_unlocked_if_needed(session, security, target).await?;
                run_migration(storage, module, target).await?;
            }
            Ok(())
        }
    }
}

fn lookup_module(module: &str) -> Result<(&'static str, bool)> {
    ENCRYPTABLE_MODULES
        .iter()
        .find(|(id, _)| *id == module)
        .copied()
        .ok_or_else(|| {
            let known: Vec<&str> = ENCRYPTABLE_MODULES.iter().map(|(id, _)| *id).collect();
            anyhow!("Unknown module '{module}'. Known modules: {}", known.join(", "))
        })
}

/// Prompts for the vault password (masked, no echo) if `target_encrypted`
/// and the vault isn't already unlocked. A locked vault is fine when
/// migrating to plaintext IF none of the existing records are actually
/// encrypted — but we can't know that without reading them, so this only
/// pre-emptively prompts for the encrypt direction; a decrypt migration
/// that hits already-encrypted records without an unlocked vault will
/// simply report per-key errors instead (see StorageManager::migrate_module_encryption).
async fn ensure_unlocked_if_needed(
    session: &Arc<VaultSession>,
    security: &Arc<SecurityManager>,
    target_encrypted: bool,
) -> Result<()> {
    if !target_encrypted || session.is_unlocked() {
        return Ok(());
    }

    let password = zeroize::Zeroizing::new(
        rpassword::prompt_password("Vault password: ")
            .map_err(|e| anyhow!("Failed to read password: {e}"))?,
    );

    let result = if security.is_vault_initialized() {
        security.unlock_vault(password).await
    } else {
        security.initialize_vault(password).await
    };

    match result {
        Ok(key) => {
            session.unlock(key);
            Ok(())
        }
        Err(e) => Err(anyhow!("Vault unlock failed: {e}")),
    }
}

async fn run_migration(storage: &Arc<StorageManager>, module: &str, target_encrypted: bool) -> Result<()> {
    let report = storage.migrate_module_encryption(module, target_encrypted).await?;

    println!(
        "{module}: {} migrated, {} already {}, {} error(s)",
        report.migrated,
        report.skipped,
        if target_encrypted { "encrypted" } else { "plaintext" },
        report.errors.len()
    );
    for (key, err) in &report.errors {
        println!("  - {key}: {err}");
    }
    if !report.errors.is_empty() {
        return Err(anyhow!("{} record(s) in '{module}' failed to migrate", report.errors.len()));
    }
    Ok(())
}
