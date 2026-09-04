use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use moku_core::SecurityManager;
use serde::{Deserialize, Serialize};

pub const VOLUME_FILE: &str = "volume.json";
pub const USAGE_FILE: &str = "usage.json";
pub const DATA_DIR: &str = "data";
const INDEX_FILE: &str = "index.json";

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

/// Maps a volume id to its actual directory, wherever it lives. Volumes
/// created with an explicit `--path` (or the new CWD default — see
/// `create_volume`) are registered in the index at creation time; volumes
/// that predate this (or otherwise have no index entry) fall back to the
/// fixed `volumes_root()` location, which is where they've always lived.
pub fn volume_dir(id: &str) -> Result<PathBuf> {
    if let Some(path) = load_index().get(id) {
        return Ok(path.clone());
    }
    Ok(volumes_root()?.join(id))
}

fn index_path() -> Result<PathBuf> {
    Ok(volumes_root()?.join(INDEX_FILE))
}

/// Small, blocking read — the index is a tiny JSON map, and keeping
/// `volume_dir` (a widely-used, synchronous function) synchronous avoids
/// cascading an async signature change through every caller for what's a
/// negligible amount of I/O. Missing/corrupt index → empty map, so a
/// volume just falls back to the fixed-root lookup instead of erroring.
fn load_index() -> HashMap<String, PathBuf> {
    let Ok(path) = index_path() else {
        return HashMap::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

async fn save_index(index: &HashMap<String, PathBuf>) -> Result<()> {
    let root = volumes_root()?;
    tokio::fs::create_dir_all(&root).await?;
    let json = serde_json::to_string_pretty(index)?;
    tokio::fs::write(root.join(INDEX_FILE), json).await?;
    Ok(())
}

fn slugify(name: &str) -> String {
    let mapped: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let mut slug = mapped.trim_matches('-').to_string();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    if slug.is_empty() {
        "volume".to_string()
    } else {
        slug
    }
}

/// Picks an id that's free both at the target `base` directory (no
/// collision with an existing folder there) and globally in the index
/// (since the index is one flat `id -> path` map, two volumes created in
/// different directories must still never share an id) and in the fixed
/// `volumes_root()` (covers ids taken by pre-index volumes).
fn unique_id(name: &str, base: &Path) -> Result<String> {
    let stem = slugify(name);
    let index = load_index();
    let root = volumes_root()?;
    let mut candidate = stem.clone();
    let mut n = 2;
    loop {
        let taken = base.join(&candidate).exists()
            || index.contains_key(&candidate)
            || root.join(&candidate).exists();
        if !taken {
            return Ok(candidate);
        }
        candidate = format!("{stem}-{n}");
        n += 1;
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
///
/// `base_dir` is where the volume's own directory (`<base_dir>/<id>/`)
/// gets created — `None` defaults to the current working directory (so a
/// plain `vault create NAME` puts it wherever the user's shell happens to
/// be, not a fixed app-managed folder); `Some(path)` creates it there
/// instead. Either way the volume is registered in the index so it can
/// still be found by name/id regardless of where it physically lives.
pub async fn create_volume(
    display_name: &str,
    size_limit_bytes: u64,
    password_mode: PasswordMode,
    password: String,
    base_dir: Option<PathBuf>,
) -> Result<VolumeConfig> {
    let base = match base_dir {
        Some(p) => p,
        None => std::env::current_dir().context("failed to resolve the current directory")?,
    };
    tokio::fs::create_dir_all(&base)
        .await
        .with_context(|| format!("failed to create directory {}", base.display()))?;
    let base = tokio::fs::canonicalize(&base).await.unwrap_or(base);

    let id = unique_id(display_name, &base)?;
    let dir = base.join(&id);
    tokio::fs::create_dir_all(&dir).await?;

    let security = SecurityManager::new_with_root(dir.clone());
    security
        .initialize_vault(password)
        .await
        .context("failed to initialize volume vault")?;

    moku_vault_fs::pathmap::PathMapper::new(dir.join(DATA_DIR)).ensure_root()?;

    let config = VolumeConfig {
        id: id.clone(),
        display_name: display_name.to_string(),
        size_limit_bytes,
        password_mode,
        created_at: now(),
    };
    save_config(&dir, &config).await?;

    moku_vault_fs::quota::Quota::load(dir.join(USAGE_FILE), size_limit_bytes).flush()?;

    let mut index = load_index();
    index.insert(id.clone(), dir.clone());
    save_index(&index).await?;

    Ok(config)
}

pub async fn list_volumes() -> Result<Vec<VolumeConfig>> {
    let mut seen_ids = std::collections::HashSet::new();
    let mut out = Vec::new();

    // Index-registered volumes first (created anywhere via `create_volume`,
    // including the new CWD default and `--path`).
    for (id, dir) in load_index() {
        if let Ok(cfg) = load_config(&dir).await {
            seen_ids.insert(id);
            out.push(cfg);
        }
    }

    // Fixed-root scan — covers volumes created before this index existed
    // (no migration needed, they're just found here as always), and
    // self-heals anything that ended up under volumes_root() without an
    // index entry.
    let root = volumes_root()?;
    if root.exists() {
        let mut entries = tokio::fs::read_dir(&root).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_dir()
                && let Ok(cfg) = load_config(&entry.path()).await
                && seen_ids.insert(cfg.id.clone())
            {
                out.push(cfg);
            }
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

    // `unique_id` also checks `volumes_root()` (this machine's real,
    // shared vault data dir) and the index there — untestable in
    // isolation without a way to override that fixed location (a
    // pre-existing gap in this crate's testability, not introduced here).
    // These tests only exercise the *local* collision check against a
    // throwaway temp directory, using names unique enough that a
    // collision against anything real on the test machine is effectively
    // impossible.

    #[test]
    fn test_unique_id_returns_plain_slug_when_nothing_collides() {
        let dir = tempfile::tempdir().unwrap();
        let id = unique_id("claude-plan-test-9f3e1c", dir.path()).unwrap();
        assert_eq!(id, "claude-plan-test-9f3e1c");
    }

    #[test]
    fn test_unique_id_avoids_local_directory_collision() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("claude-plan-test-7a21bd")).unwrap();
        let id = unique_id("claude-plan-test-7a21bd", dir.path()).unwrap();
        assert_ne!(id, "claude-plan-test-7a21bd");
        assert!(id.starts_with("claude-plan-test-7a21bd-"));
    }
}
