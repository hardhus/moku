use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use moku_core::SecurityManager;
use serde::{Deserialize, Serialize};

pub const VOLUME_FILE: &str = "volume.json";
pub const USAGE_FILE: &str = "usage.json";
pub const DATA_DIR: &str = "data";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum PasswordMode {
    /// Same password value as moku's own vault (but a distinct derived
    /// key, since this volume has its own independent salt) — plan §5.
    Default,
    /// A password set specifically for this volume, independent of
    /// moku's own vault password.
    Custom,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VolumeConfig {
    pub id: String,
    pub display_name: String,
    pub size_limit_bytes: u64,
    pub password_mode: PasswordMode,
    pub created_at: u64,
}

pub fn volumes_root() -> Result<PathBuf> {
    Ok(moku_core::dirs::get_data_dir()?.join("vaults"))
}

pub fn volume_dir(id: &str) -> Result<PathBuf> {
    Ok(volumes_root()?.join(id))
}

fn slugify(name: &str) -> String {
    let mapped: String =
        name.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let mut slug = mapped.trim_matches('-').to_string();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    if slug.is_empty() { "volume".to_string() } else { slug }
}

fn unique_id(name: &str) -> Result<String> {
    let base = slugify(name);
    let root = volumes_root()?;
    let mut candidate = base.clone();
    let mut n = 2;
    while root.join(&candidate).exists() {
        candidate = format!("{base}-{n}");
        n += 1;
    }
    Ok(candidate)
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

async fn save_config(dir: &Path, config: &VolumeConfig) -> Result<()> {
    let json = serde_json::to_string_pretty(config)?;
    tokio::fs::write(dir.join(VOLUME_FILE), json).await?;
    Ok(())
}

pub async fn load_config(dir: &Path) -> Result<VolumeConfig> {
    let content = tokio::fs::read_to_string(dir.join(VOLUME_FILE))
        .await
        .with_context(|| format!("no volume found at {}", dir.display()))?;
    Ok(serde_json::from_str(&content)?)
}

/// Creates a new volume: its own independent `SecurityManager` vault (own
/// salt/meta.json, so it never shares key material with moku's main
/// vault even in Default password mode — plan §0/§5), an empty backing
/// data root, and the `volume.json` record. Does not mount anything.
pub async fn create_volume(
    display_name: &str,
    size_limit_bytes: u64,
    password_mode: PasswordMode,
    password: String,
) -> Result<VolumeConfig> {
    let root = volumes_root()?;
    tokio::fs::create_dir_all(&root).await?;

    let id = unique_id(display_name)?;
    let dir = root.join(&id);
    tokio::fs::create_dir_all(&dir).await?;

    let security = SecurityManager::new_with_root(dir.clone());
    security.initialize_vault(password).await.context("failed to initialize volume vault")?;

    moku_vault_fs::pathmap::PathMapper::new(dir.join(DATA_DIR)).ensure_root()?;

    let config = VolumeConfig { id: id.clone(), display_name: display_name.to_string(), size_limit_bytes, password_mode, created_at: now() };
    save_config(&dir, &config).await?;

    moku_vault_fs::quota::Quota::load(dir.join(USAGE_FILE), size_limit_bytes).flush()?;

    Ok(config)
}

pub async fn list_volumes() -> Result<Vec<VolumeConfig>> {
    let root = volumes_root()?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut entries = tokio::fs::read_dir(&root).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir()
            && let Ok(cfg) = load_config(&entry.path()).await
        {
            out.push(cfg);
        }
    }
    out.sort_by_key(|v| v.display_name.to_ascii_lowercase());
    Ok(out)
}

/// Finds a volume by id or (case-insensitive) display name.
pub async fn find_volume(name_or_id: &str) -> Result<VolumeConfig> {
    list_volumes()
        .await?
        .into_iter()
        .find(|v| v.id == name_or_id || v.display_name.eq_ignore_ascii_case(name_or_id))
        .ok_or_else(|| anyhow!("no such volume: '{name_or_id}'"))
}

pub async fn resize_volume(name_or_id: &str, new_size_bytes: u64) -> Result<VolumeConfig> {
    let mut cfg = find_volume(name_or_id).await?;
    cfg.size_limit_bytes = new_size_bytes;
    save_config(&volume_dir(&cfg.id)?, &cfg).await?;
    Ok(cfg)
}

/// Reads a volume's cached physical-bytes usage counter directly, without
/// needing its vault unlocked (the counter lives in a small plaintext
/// `usage.json`, not inside the encrypted volume itself).
pub fn usage_bytes(id: &str) -> Result<u64> {
    let usage = moku_vault_fs::quota::Quota::load(volume_dir(id)?.join(USAGE_FILE), 0);
    Ok(usage.used_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slugify_basic() {
        assert_eq!(slugify("My Notes Vault!"), "my-notes-vault");
    }

    #[test]
    fn test_slugify_collapses_repeated_separators() {
        assert_eq!(slugify("a   b"), "a-b");
    }

    #[test]
    fn test_slugify_empty_falls_back() {
        assert_eq!(slugify("!!!"), "volume");
    }
}
