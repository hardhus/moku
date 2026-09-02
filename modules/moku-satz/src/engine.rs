use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use satz_core::{Index, walk_vault};

/// `[modules.notes]` config, read via `MokuConfig::resolve_module_config`
/// (the same dynamic-TOML-table mechanism every other module uses).
#[derive(serde::Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct NotesConfig {
    pub vault_path: String,
}

/// Resolves the vault root to walk: an explicit CLI path argument wins,
/// otherwise `[modules.notes] vault_path` from config.
pub fn resolve_vault_root(config: &NotesConfig, path_override: Option<&str>) -> Result<PathBuf> {
    let raw = path_override
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| (!config.vault_path.is_empty()).then(|| config.vault_path.clone()))
        .ok_or_else(|| anyhow!("no notes vault configured — set [modules.notes] vault_path in config.toml, or pass a path"))?;
    Ok(PathBuf::from(raw))
}

/// Walks and indexes the vault at `vault_root`.
pub fn build_index(vault_root: &Path) -> Result<Index> {
    let docs = walk_vault(vault_root).with_context(|| format!("failed to walk vault at {}", vault_root.display()))?;
    Ok(Index::build(docs))
}

pub fn today_string() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Ensures today's daily note exists under `<vault_root>/daily/<date>.md`,
/// creating it from `satz_core::generate_document_template` if missing.
/// Returns (path, was_newly_created). Shared by the CLI `daily` command
/// and the TUI's `[d]` key — v1 uses this fixed convention rather than
/// satz's full `DailyNoteConfig` (scope cut, see plan Bölüm B).
pub fn ensure_daily_note(vault_root: &Path) -> Result<(PathBuf, bool)> {
    let today = today_string();
    let path = vault_root.join("daily").join(format!("{today}.md"));
    if path.exists() {
        return Ok((path, false));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let content = satz_core::generate_document_template(&today, Some(&today));
    std::fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok((path, true))
}
