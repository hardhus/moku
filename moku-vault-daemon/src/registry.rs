use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use hkdf::Hkdf;
use moku_core::{SafeKey, SecurityManager};
use secrecy::{ExposeSecret, SecretBox};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

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

/// The secret used to *create* a volume — mirrors `PasswordMode` but
/// carries the actual material instead of just labeling it.
pub enum VolumeSecret {
    /// Custom mode — a password unique to this volume, independent of
    /// moku's own vault. `create_volume` sets up its own `vault/meta.json`
    /// for it, exactly as before.
    Password(String),
    /// Default mode — moku's own, already-verified app-vault master key.
    /// The volume gets no `vault/meta.json` of its own; its data keys are
    /// derived on demand from this via `derive_default_volume_master_key`
    /// (see `resolve_volume_master_key`), so the same real "moku vault
    /// password" genuinely unlocks it later — not just a copy typed twice.
    FromAppVault(SecretBox<SafeKey>),
}

/// The secret used to *mount* an existing volume.
pub enum MountSecret {
    /// A typed password — for Custom-mode volumes, or old-scheme
    /// Default-mode volumes (their own independent `vault/meta.json`
    /// predates this scheme and still needs its own password), this
    /// unlocks the volume's own vault directly; for new-scheme Default-mode
    /// volumes it's verified against moku's *real* app vault instead.
    Password(String),
    /// The app vault's master key, already unlocked and held in memory
    /// (e.g. the TUI's `ctx.session`) — lets a new-scheme Default-mode
    /// volume mount with zero re-prompting.
    ///
    /// Trusted as-is, with no canary check of its own (a new-scheme
    /// volume has no `vault/meta.json` to check one against) — callers
    /// must only ever pass a key that was already authenticated elsewhere
    /// (e.g. `ctx.session`'s key is only ever populated by a real
    /// `SecurityManager::unlock_vault` call). A mismatched key here
    /// "succeeds" at mount time and only surfaces as garbled data (or an
    /// AES-GCM authentication failure) when actually reading content that
    /// was encrypted under a different key — never pass an unverified key
    /// through this variant.
    Key(SecretBox<SafeKey>),
}

const DEFAULT_VOLUME_INFO_PREFIX: &[u8] = b"moku-vault-daemon/default-volume/v1/";

/// Derives a per-volume master key from moku's real app-vault master key,
/// domain-separated by volume id via HKDF-Expand (same technique as
/// `moku_vault_fs::derive_volume_keys`) so that two Default-mode volumes
/// sharing the same app master key never end up with the same encryption
/// key — sharing one would be a real confidentiality bug, not just a
/// cosmetic one.
pub fn derive_default_volume_master_key(
    app_master: &SecretBox<SafeKey>,
    volume_id: &str,
) -> SecretBox<SafeKey> {
    let hk = Hkdf::<Sha256>::new(None, &app_master.expose_secret().0);
    let mut info = Vec::with_capacity(DEFAULT_VOLUME_INFO_PREFIX.len() + volume_id.len());
    info.extend_from_slice(DEFAULT_VOLUME_INFO_PREFIX);
    info.extend_from_slice(volume_id.as_bytes());
    let mut out = [0u8; 32];
    hk.expand(&info, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    SecretBox::new(Box::new(SafeKey(out)))
}

/// Whether this volume has its own independent `vault/meta.json` (the old
/// per-volume-password scheme, still used by Custom mode and by any
/// Default-mode volume created before this scheme existed) rather than
/// deriving its key from moku's app vault. Exposed so callers outside this
/// module (the TUI, deciding whether a no-prompt mount is available) don't
/// have to duplicate the check `resolve_volume_master_key` uses internally.
pub fn has_own_vault(volume_dir: &Path) -> bool {
    SecurityManager::new_with_root(volume_dir.to_path_buf()).is_vault_initialized()
}

/// Resolves the real key needed to open a volume's data, transparently
/// handling both the old per-volume-independent-vault scheme and the new
/// app-vault-derived scheme — the single place `worker::run` (and this
/// module's own tests) go through, so mounting logic never has to
/// special-case password modes anywhere else.
pub async fn resolve_volume_master_key(
    volume_dir: &Path,
    cfg: &VolumeConfig,
    secret: MountSecret,
) -> Result<SecretBox<SafeKey>> {
    if cfg.password_mode == PasswordMode::Custom || has_own_vault(volume_dir) {
        let MountSecret::Password(password) = secret else {
            bail!(
                "'{}' needs its own password to mount — it can't be unlocked from an already-open moku vault",
                cfg.display_name
            );
        };
        let security = SecurityManager::new_with_root(volume_dir.to_path_buf());
        return security
            .unlock_vault(password)
            .await
            .map_err(|e| anyhow!("failed to unlock volume '{}': {e}", cfg.id));
    }

    // New-scheme Default mode: the volume has no vault of its own — its
    // key comes from moku's real app vault, either freshly verified here
    // (CLI) or already unlocked and handed straight in (TUI fast path).
    let app_key = match secret {
        MountSecret::Key(key) => key,
        MountSecret::Password(password) => SecurityManager::new()?
            .unlock_vault(password)
            .await
            .map_err(|e| anyhow!("wrong moku vault password: {e}"))?,
    };
    Ok(derive_default_volume_master_key(&app_key, &cfg.id))
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

/// Serializes read-modify-write access to the index file within this
/// process — without it, two registrations happening back to back (e.g.
/// two volumes created in quick succession from the TUI, or just two
/// concurrent tests) can race: both read the same starting content, and
/// whichever writes second silently clobbers the other's insert. Doesn't
/// protect against a *second moku process* writing at the same instant
/// (no cross-process file lock here), but that's an extremely narrow case
/// this app doesn't guard against for its other data files either.
static INDEX_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

fn save_index(index: &HashMap<String, PathBuf>) -> Result<()> {
    let root = volumes_root()?;
    std::fs::create_dir_all(&root)?;
    let json = serde_json::to_string_pretty(index)?;
    std::fs::write(root.join(INDEX_FILE), json)?;
    Ok(())
}

/// Reads, mutates, and writes back the index as one locked step — see
/// `INDEX_LOCK`. Synchronous end to end (no `.await` inside the critical
/// section) specifically so the lock never needs to be held across an
/// await point.
fn update_index(mutate: impl FnOnce(&mut HashMap<String, PathBuf>)) -> Result<()> {
    let _guard = INDEX_LOCK.lock().unwrap();
    let mut index = load_index();
    mutate(&mut index);
    save_index(&index)
}

/// Loads the index and forgets any entry whose directory is *completely
/// gone* — e.g. deleted by hand with `rm -rf` instead of `vault delete`,
/// which previously left a permanent ghost: `unique_id` saw the old id as
/// still taken forever, so recreating a volume under the same name kept
/// accumulating `-2`, `-3`, ... suffixes instead of ever reusing the
/// original. Deliberately checks `Path::exists()` rather than trying to
/// load `volume.json` — a directory that exists but is temporarily
/// unreadable (a not-currently-mounted removable/network drive, say)
/// should never be pruned just because it can't be read *right now*; only
/// outright absence is treated as "really deleted". Used by both
/// `list_volumes` (so listings don't show ghosts) and `unique_id` (so a
/// name freed up by deleting its old directory is immediately reusable).
fn pruned_index() -> HashMap<String, PathBuf> {
    let index = load_index();
    let mut alive = HashMap::new();
    let mut stale = Vec::new();
    for (id, dir) in index {
        if dir.exists() {
            alive.insert(id, dir);
        } else {
            stale.push(id);
        }
    }
    if !stale.is_empty() {
        let _ = update_index(|index| {
            for id in &stale {
                index.remove(id);
            }
        });
    }
    alive
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
    let index = pruned_index();
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

/// Creates a new volume: for `VolumeSecret::Password` (Custom mode), its
/// own independent `SecurityManager` vault (own salt/meta.json); for
/// `VolumeSecret::FromAppVault` (Default mode), no vault of its own at all
/// — its keys are derived on demand from moku's real app-vault master key
/// (see `derive_default_volume_master_key`/`resolve_volume_master_key`),
/// so "the same password as moku's own vault" is a genuine fact, not two
/// independently-typed strings that happen to match. Either way this also
/// sets up an empty backing data root and the `volume.json` record. Does
/// not mount anything.
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
    secret: VolumeSecret,
    base_dir: Option<PathBuf>,
) -> Result<VolumeConfig> {
    let password_mode = match &secret {
        VolumeSecret::Password(_) => PasswordMode::Custom,
        VolumeSecret::FromAppVault(_) => PasswordMode::Default,
    };

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

    if let VolumeSecret::Password(password) = secret {
        let security = SecurityManager::new_with_root(dir.clone());
        security
            .initialize_vault(password)
            .await
            .context("failed to initialize volume vault")?;
    }
    // VolumeSecret::FromAppVault needs nothing persisted here — its key is
    // re-derived from the app vault on every mount instead.

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

    update_index(|index| {
        index.insert(id.clone(), dir.clone());
    })?;

    Ok(config)
}

/// Registers an existing volume directory (one with its own `volume.json`
/// already in it — e.g. moved by hand, copied from another machine, or
/// created before the index existed) so it becomes manageable by name/id
/// like any other, without touching its contents.
pub async fn import_volume(path: &Path) -> Result<VolumeConfig> {
    let path = tokio::fs::canonicalize(path)
        .await
        .with_context(|| format!("no such directory: {}", path.display()))?;
    let cfg = load_config(&path)
        .await
        .with_context(|| format!("no volume.json found at {}", path.display()))?;

    // The embedded id is fixed (unlike create_volume's freshly-slugified
    // one), so a real collision with something already registered
    // elsewhere can't be auto-resolved by renaming — refuse instead.
    let existing = volume_dir(&cfg.id)?;
    if existing.exists() && existing != path {
        bail!(
            "a volume with id '{}' is already registered at {}",
            cfg.id,
            existing.display()
        );
    }

    update_index(|index| {
        index.insert(cfg.id.clone(), path);
    })?;

    Ok(cfg)
}

pub async fn list_volumes() -> Result<Vec<VolumeConfig>> {
    let mut seen_ids = std::collections::HashSet::new();
    let mut out = Vec::new();

    // Index-registered volumes first (created anywhere via `create_volume`,
    // including the new CWD default and `--path`). `pruned_index` already
    // dropped entries whose directory is completely gone; a directory
    // that exists but fails to load (corrupt/unreadable) is still just
    // skipped here, same as before.
    for (id, dir) in pruned_index() {
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

/// Permanently deletes a volume: its whole backing directory (all its
/// data, gone) and its index entry. Does not check whether it's currently
/// mounted — deleting a live-mounted volume's backing files out from
/// under WinFsp is unsafe, so callers (the CLI/TUI delete commands) are
/// responsible for unmounting first.
pub async fn delete_volume(id: &str) -> Result<()> {
    let dir = volume_dir(id)?;
    tokio::fs::remove_dir_all(&dir)
        .await
        .with_context(|| format!("failed to delete {}", dir.display()))?;
    update_index(|index| {
        index.remove(id);
    })?;
    Ok(())
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

    #[test]
    fn test_pruned_index_removes_entries_whose_directory_is_completely_gone() {
        let id = "claude-prune-test-gone-1a2b";
        let dir = tempfile::tempdir().unwrap();
        let vol_dir = dir.path().join("vol");
        std::fs::create_dir_all(&vol_dir).unwrap();
        update_index(|index| {
            index.insert(id.to_string(), vol_dir.clone());
        })
        .unwrap();

        std::fs::remove_dir_all(&vol_dir).unwrap(); // simulate `rm -rf` by hand

        let pruned = pruned_index();
        assert!(
            !pruned.contains_key(id),
            "a completely deleted directory's index entry must be forgotten"
        );
    }

    #[test]
    fn test_pruned_index_keeps_entries_whose_directory_still_exists() {
        let id = "claude-prune-test-alive-3c4d";
        let dir = tempfile::tempdir().unwrap();
        let vol_dir = dir.path().join("vol");
        std::fs::create_dir_all(&vol_dir).unwrap();
        update_index(|index| {
            index.insert(id.to_string(), vol_dir.clone());
        })
        .unwrap();

        let pruned = pruned_index();
        assert!(
            pruned.contains_key(id),
            "a still-existing directory must not be pruned"
        );

        remove_index_entry(id);
    }

    #[tokio::test]
    async fn test_unique_id_reuses_a_name_after_its_directory_was_deleted_by_hand() {
        let base = tempfile::tempdir().unwrap();
        let name = "claude-reuse-test-5e6f";

        let cfg = create_volume(
            name,
            1024,
            VolumeSecret::Password("pw".to_string()),
            Some(base.path().to_path_buf()),
        )
        .await
        .unwrap();
        assert_eq!(cfg.id, name);

        // The user's exact scenario: delete the directory by hand (not via
        // `vault delete`), leaving the index entry orphaned.
        std::fs::remove_dir_all(base.path().join(&cfg.id)).unwrap();

        let cfg2 = create_volume(
            name,
            1024,
            VolumeSecret::Password("pw".to_string()),
            Some(base.path().to_path_buf()),
        )
        .await
        .unwrap();
        assert_eq!(
            cfg2.id, name,
            "a name freed up by deleting its directory by hand must be reusable immediately, not suffixed with -2"
        );

        let _ = std::fs::remove_dir_all(base.path().join(&cfg2.id));
        remove_index_entry(&cfg2.id);
    }

    #[tokio::test]
    async fn test_delete_volume_removes_directory_and_index_entry() {
        let base = tempfile::tempdir().unwrap();
        let cfg = create_volume(
            "claude-delete-test-7g8h",
            1024,
            VolumeSecret::Password("pw".to_string()),
            Some(base.path().to_path_buf()),
        )
        .await
        .unwrap();
        let dir = volume_dir(&cfg.id).unwrap();
        assert!(dir.exists());

        delete_volume(&cfg.id).await.unwrap();

        assert!(!dir.exists(), "the volume's directory must be gone");
        assert!(
            !load_index().contains_key(&cfg.id),
            "the volume's index entry must be gone"
        );
    }

    // `import_volume` writes into the real, shared `volumes_root()/index.json`
    // (same testability gap as `unique_id` above — no way to override that
    // fixed location) — each test below cleans up its own index entry
    // afterward so no residue is left in the developer's real environment,
    // and uses a name unique enough that a collision with anything real is
    // effectively impossible.

    fn write_fake_volume_json(dir: &Path, id: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let cfg = VolumeConfig {
            id: id.to_string(),
            display_name: id.to_string(),
            size_limit_bytes: 1024,
            password_mode: PasswordMode::Custom,
            created_at: 0,
        };
        std::fs::write(
            dir.join(VOLUME_FILE),
            serde_json::to_string_pretty(&cfg).unwrap(),
        )
        .unwrap();
    }

    fn remove_index_entry(id: &str) {
        update_index(|index| {
            index.remove(id);
        })
        .unwrap();
    }

    #[tokio::test]
    async fn test_import_volume_registers_it_in_the_index() {
        let id = "claude-import-test-4d8f21";
        let dir = tempfile::tempdir().unwrap();
        let vol_dir = dir.path().join("myvol");
        write_fake_volume_json(&vol_dir, id);

        let cfg = import_volume(&vol_dir).await.unwrap();
        assert_eq!(cfg.id, id);

        let found = volume_dir(id).unwrap();
        assert_eq!(found, tokio::fs::canonicalize(&vol_dir).await.unwrap());

        remove_index_entry(id);
    }

    #[tokio::test]
    async fn test_import_volume_errors_without_a_volume_json() {
        let dir = tempfile::tempdir().unwrap();
        assert!(import_volume(dir.path()).await.is_err());
    }

    #[tokio::test]
    async fn test_import_volume_rejects_id_collision_with_another_real_directory() {
        let id = "claude-import-test-collision-9c3e";
        let dir = tempfile::tempdir().unwrap();
        let vol_dir_a = dir.path().join("a");
        let vol_dir_b = dir.path().join("b");
        write_fake_volume_json(&vol_dir_a, id);
        write_fake_volume_json(&vol_dir_b, id);

        import_volume(&vol_dir_a).await.unwrap();
        let result = import_volume(&vol_dir_b).await;
        assert!(
            result.is_err(),
            "importing a second directory with the same id must fail"
        );

        remove_index_entry(id);
    }

    fn fake_key(byte: u8) -> SecretBox<SafeKey> {
        SecretBox::new(Box::new(SafeKey([byte; 32])))
    }

    fn fake_config(id: &str, mode: PasswordMode) -> VolumeConfig {
        VolumeConfig {
            id: id.to_string(),
            display_name: id.to_string(),
            size_limit_bytes: 1024,
            password_mode: mode,
            created_at: 0,
        }
    }

    #[test]
    fn test_derive_default_volume_master_key_deterministic_and_distinct_by_volume_id() {
        let app_key = fake_key(9);
        let k1 = derive_default_volume_master_key(&app_key, "vol-a");
        let k2 = derive_default_volume_master_key(&app_key, "vol-a");
        let k3 = derive_default_volume_master_key(&app_key, "vol-b");
        assert_eq!(k1.expose_secret().0, k2.expose_secret().0);
        assert_ne!(
            k1.expose_secret().0,
            k3.expose_secret().0,
            "two Default-mode volumes must never derive the same real key"
        );
    }

    #[test]
    fn test_derive_default_volume_master_key_differs_by_app_master() {
        let k1 = derive_default_volume_master_key(&fake_key(1), "vol-a");
        let k2 = derive_default_volume_master_key(&fake_key(2), "vol-a");
        assert_ne!(k1.expose_secret().0, k2.expose_secret().0);
    }

    #[tokio::test]
    async fn test_resolve_volume_master_key_old_scheme_uses_volume_own_password() {
        let dir = tempfile::tempdir().unwrap();
        let security = SecurityManager::new_with_root(dir.path().to_path_buf());
        security
            .initialize_vault("volume-own-pw".to_string())
            .await
            .unwrap();
        let cfg = fake_config("old-scheme-vol", PasswordMode::Default);

        assert!(
            resolve_volume_master_key(dir.path(), &cfg, MountSecret::Password("wrong".to_string()))
                .await
                .is_err()
        );
        assert!(
            resolve_volume_master_key(
                dir.path(),
                &cfg,
                MountSecret::Password("volume-own-pw".to_string())
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn test_resolve_volume_master_key_new_scheme_default_derives_from_supplied_key() {
        let dir = tempfile::tempdir().unwrap(); // no vault/meta.json inside — new scheme
        let cfg = fake_config("new-scheme-vol", PasswordMode::Default);
        let app_key = fake_key(7);

        let resolved = resolve_volume_master_key(
            dir.path(),
            &cfg,
            MountSecret::Key(SecretBox::new(Box::new(SafeKey(app_key.expose_secret().0)))),
        )
        .await
        .unwrap();

        let expected = derive_default_volume_master_key(&app_key, &cfg.id);
        assert_eq!(resolved.expose_secret().0, expected.expose_secret().0);
    }

    #[tokio::test]
    async fn test_resolve_volume_master_key_custom_mode_rejects_a_bare_key() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = fake_config("custom-vol", PasswordMode::Custom);

        let result =
            resolve_volume_master_key(dir.path(), &cfg, MountSecret::Key(fake_key(3))).await;
        assert!(
            result.is_err(),
            "Custom mode must always require its own password, never a bare key"
        );
    }

    #[tokio::test]
    async fn test_create_volume_default_mode_creates_no_own_vault_meta() {
        // Explicit `base_dir` rather than `std::env::set_current_dir` — the
        // latter is process-global and unsafe to mutate from a test when
        // `cargo test` runs tests in parallel within the same binary.
        let dir = tempfile::tempdir().unwrap();

        let cfg = create_volume(
            "claude-default-mode-test",
            1024,
            VolumeSecret::FromAppVault(fake_key(4)),
            Some(dir.path().to_path_buf()),
        )
        .await
        .unwrap();
        assert_eq!(cfg.password_mode, PasswordMode::Default);

        let vol_dir = dir.path().join(&cfg.id);
        assert!(
            !has_own_vault(&vol_dir),
            "Default mode must not create its own vault/meta.json"
        );

        remove_index_entry(&cfg.id);
    }
}
