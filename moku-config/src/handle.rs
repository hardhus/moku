use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use arc_swap::{ArcSwap, Guard};
use serde::Serialize;
use serde::de::DeserializeOwned;
use toml_edit::DocumentMut;

use crate::edit;
use crate::schema::{CURRENT_CONFIG_VERSION, deserialize_at};
use crate::{ConfigError, ConfigIssue, MokuConfig};

const HEADER: &str = "# -----------------------------------------------------------\n\
                      #  MOKU - CONFIGURATION FILE\n\
                      # -----------------------------------------------------------\n\n";

/// The single owner of config.toml for a moku process: lock-free reads of
/// the current config for everyone, and the only way to change it.
///
/// - **Reads** (`load`, `snapshot`, `module_config`) never fail.
/// - **Writes** (`update`, `update_module`) are saved immediately and
///   atomically (`atomic_write`), and change only the keys that actually
///   changed, so the user's comments, ordering and unknown keys survive.
///   Writers are serialized, so two changes can't lose each other.
/// - **A broken file is never overwritten.** If config.toml can't be read
///   or parsed, or comes from a newer moku, the handle runs on defaults in
///   read-only mode (every write returns `ConfigError::ReadOnly`) and keeps
///   a `config.toml.broken-<unix time>` copy. If one section or module
///   table has a wrong-typed value, that part falls back to its defaults,
///   is reported as a `ConfigIssue`, and stays write-locked
///   (`ConfigError::SectionInvalid`) so a default never replaces what the
///   user wrote.
pub struct ConfigHandle {
    current: ArcSwap<MokuConfig>,
    state: Mutex<State>,
    issues: Mutex<Issues>,
}

struct State {
    /// `None` for a detached (in-memory) handle.
    path: Option<PathBuf>,
    doc: DocumentMut,
    read_only: Option<String>,
    /// Top-level sections whose file contents didn't parse, keyed by name.
    invalid_sections: HashMap<String, ConfigIssue>,
}

#[derive(Default)]
struct Issues {
    pending: Vec<ConfigIssue>,
    /// Paths already reported this session (a broken table is read on
    /// every frame; it should be reported once, not once per read).
    seen: HashSet<String>,
}

impl ConfigHandle {
    /// A handle that isn't backed by a file: writes update memory only.
    /// For tests and for a process that can't locate its config directory.
    pub fn detached(config: MokuConfig) -> Self {
        // Only an in-memory mirror for `update`'s edits (nothing is ever
        // written from a detached handle). Serializing/parsing a
        // `MokuConfig` can't realistically fail; an empty document would
        // just start from no keys.
        let doc = toml::to_string_pretty(&config)
            .ok()
            .and_then(|s| s.parse::<DocumentMut>().ok())
            .unwrap_or_default();
        Self::from_parts(config, State {
            path: None,
            doc,
            read_only: None,
            invalid_sections: HashMap::new(),
        }, Vec::new())
    }

    /// Opens the user's config.toml (creating it with the defaults if it
    /// doesn't exist yet). Never fails: every problem becomes a
    /// `ConfigIssue` (see `take_issues`) and, where the file can't be
    /// trusted, read-only mode.
    pub fn open_default() -> Self {
        match moku_sys::dirs::get_config_dir() {
            Ok(dir) => Self::open(dir.join("config.toml")),
            Err(e) => {
                let reason = format!(
                    "couldn't locate the config directory ({}); using the defaults",
                    moku_sys::util::error_chain(&e)
                );
                let handle = Self::detached(MokuConfig::default());
                handle.state().read_only = Some(reason.clone());
                handle.report(ConfigIssue {
                    path: "config.toml".into(),
                    message: reason,
                });
                handle
            }
        }
    }

    /// Opens config.toml at `path`; see `open_default`.
    pub fn open(path: PathBuf) -> Self {
        let mut issues = Vec::new();
        let read_only = |reason: String, issues: Vec<ConfigIssue>| {
            let handle = Self::from_parts(MokuConfig::default(), State {
                path: Some(path.clone()),
                doc: DocumentMut::new(),
                read_only: Some(reason.clone()),
                invalid_sections: HashMap::new(),
            }, issues);
            handle.report(ConfigIssue {
                path: "config.toml".into(),
                message: reason,
            });
            handle
        };

        if !path.exists()
            && let Err(e) = write_default_file(&path)
        {
            // Not read-only: writes will be retried (and report their
            // own failure) when the user changes a setting.
            issues.push(ConfigIssue {
                path: "config.toml".into(),
                message: format!(
                    "couldn't create {}: {}; using the defaults",
                    path.display(),
                    moku_sys::util::error_chain(e.as_ref())
                ),
            });
            return Self::from_parts(MokuConfig::default(), State {
                path: Some(path.clone()),
                doc: default_document(),
                read_only: None,
                invalid_sections: HashMap::new(),
            }, issues);
        }

        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                return read_only(
                    format!(
                        "couldn't read {} ({e}); using the defaults, and changes won't be saved this session",
                        path.display()
                    ),
                    issues,
                );
            }
        };

        let parsed = text
            .parse::<DocumentMut>()
            .map_err(|e| e.to_string())
            .and_then(|doc| {
                toml::from_str::<toml::Table>(&text)
                    .map(|table| (doc, table))
                    .map_err(|e| e.to_string())
            });
        let (doc, table) = match parsed {
            Ok(v) => v,
            Err(parse_error) => {
                let copy = keep_broken_copy(&path);
                let parse_error = parse_error.trim().replace('\n', " ");
                return read_only(
                    format!(
                        "not valid TOML ({parse_error}); using the defaults, and changes won't be saved this session{copy}"
                    ),
                    issues,
                );
            }
        };

        let (config, invalid_sections, version_issue) = config_from_table(&table);
        issues.extend(invalid_sections.values().cloned());
        let handle = Self::from_parts(config, State {
            path: Some(path),
            doc,
            read_only: None,
            invalid_sections,
        }, issues);
        if let Some(reason) = version_issue {
            handle.state().read_only = Some(reason.clone());
            handle.report(ConfigIssue {
                path: "config_version".into(),
                message: reason,
            });
        }
        handle
    }

    fn from_parts(config: MokuConfig, state: State, issues: Vec<ConfigIssue>) -> Self {
        let handle = Self {
            current: ArcSwap::from_pointee(config),
            state: Mutex::new(state),
            issues: Mutex::new(Issues::default()),
        };
        for issue in issues {
            handle.report(issue);
        }
        handle
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock can't leave `State` half-written
        // (it is only replaced field by field after a successful write),
        // so a poisoned lock is still safe to use.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn issues(&self) -> MutexGuard<'_, Issues> {
        self.issues.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The current config. Cheap and lock-free; call it inline rather than
    /// holding the guard.
    pub fn load(&self) -> Guard<Arc<MokuConfig>> {
        self.current.load()
    }

    /// An owned reference to the current config.
    pub fn snapshot(&self) -> Arc<MokuConfig> {
        self.current.load_full()
    }

    /// Why writes are refused this session, if they are.
    pub fn read_only_reason(&self) -> Option<String> {
        self.state().read_only.clone()
    }

    /// The file this handle saves to (`None` when detached).
    pub fn path(&self) -> Option<PathBuf> {
        self.state().path.clone()
    }

    /// Applies `mutate` to the config and saves the result. Only the keys
    /// that changed are written; on any error, neither the file nor the
    /// in-memory config changes.
    pub fn update(&self, mutate: impl FnOnce(&mut MokuConfig)) -> Result<(), ConfigError> {
        let mut state = self.state();
        if let Some(reason) = &state.read_only {
            return Err(ConfigError::ReadOnly {
                reason: reason.clone(),
            });
        }
        let before = self.current.load_full();
        let mut after = (*before).clone();
        mutate(&mut after);

        let changes = edit::diff(&to_table(&before)?, &to_table(&after)?);
        if changes.is_empty() {
            return Ok(());
        }
        for change in &changes {
            if let Some(issue) = change.path.first().and_then(|s| state.invalid_sections.get(s)) {
                return Err(ConfigError::SectionInvalid(issue.clone()));
            }
        }

        let mut doc = state.doc.clone();
        edit::apply(&mut doc, &changes);
        if let Some(path) = &state.path {
            moku_sys::util::atomic_write(path, doc.to_string().as_bytes())?;
        }
        state.doc = doc;
        self.current.store(Arc::new(after));
        Ok(())
    }

    /// Reads `[modules.<module_id>]` as `T`, changes it with `mutate`, and
    /// saves only the fields that changed. Keys in the table that `T`
    /// doesn't know about are kept. If the table currently doesn't match
    /// `T`, nothing is written (`ConfigError::SectionInvalid`): saving
    /// would replace the user's hand-written settings with defaults.
    pub fn update_module<T>(
        &self,
        module_id: &str,
        mutate: impl FnOnce(&mut T),
    ) -> Result<(), ConfigError>
    where
        T: Serialize + DeserializeOwned + Default + Clone,
    {
        let mut failure = None;
        self.update(|cfg| {
            let typed_before: T = match cfg.resolve_module_config(module_id) {
                Ok(t) => t,
                Err(issue) => {
                    failure = Some(ConfigError::SectionInvalid(issue));
                    return;
                }
            };
            let mut typed_after = typed_before.clone();
            mutate(&mut typed_after);
            let (before, after) = match (value_table(&typed_before), value_table(&typed_after)) {
                (Ok(b), Ok(a)) => (b, a),
                (Err(e), _) | (_, Err(e)) => {
                    failure = Some(ConfigError::Encode(e));
                    return;
                }
            };
            let changes = edit::diff(&before, &after);
            if changes.is_empty() {
                return;
            }
            let mut raw = match cfg.modules.get(module_id) {
                Some(toml::Value::Table(t)) => t.clone(),
                _ => toml::Table::new(),
            };
            edit::apply_to_table(&mut raw, &changes);
            cfg.modules
                .insert(module_id.to_string(), toml::Value::Table(raw));
        })?;
        match failure {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// `[modules.<module_id>]` as `T`, for reading. If the table doesn't
    /// match `T`, returns `T`'s defaults and reports the problem (once per
    /// session) through `take_issues`, so it reaches the user as a warning
    /// instead of being swallowed.
    pub fn module_config<T>(&self, module_id: &str) -> T
    where
        T: DeserializeOwned + Default,
    {
        match self.load().resolve_module_config(module_id) {
            Ok(t) => t,
            Err(issue) => {
                self.report(issue);
                T::default()
            }
        }
    }

    /// `[modules.<module_id>.keys]` (key-binding overrides); on a type
    /// mismatch, none (reported to the user once, like `module_config`).
    pub fn module_keys(&self, module_id: &str) -> Option<HashMap<String, String>> {
        match self.load().get_module_keys(module_id) {
            Ok(keys) => keys,
            Err(issue) => {
                self.report(issue);
                None
            }
        }
    }

    /// Queues `issue` for the user, unless the same path was already
    /// reported this session.
    pub fn report(&self, issue: ConfigIssue) {
        let mut issues = self.issues();
        if issues.seen.insert(issue.path.clone()) {
            issues.pending.push(issue);
        }
    }

    /// Takes every issue not yet shown. The TUI turns them into warnings,
    /// the CLI prints them, the daemon logs them.
    pub fn take_issues(&self) -> Vec<ConfigIssue> {
        std::mem::take(&mut self.issues().pending)
    }
}

fn to_table(config: &MokuConfig) -> Result<toml::Table, ConfigError> {
    value_table(config).map_err(ConfigError::Encode)
}

fn value_table<T: Serialize>(value: &T) -> Result<toml::Table, toml::ser::Error> {
    match toml::Value::try_from(value)? {
        toml::Value::Table(t) => Ok(t),
        // Only structs/maps are ever stored as config sections.
        other => {
            let mut t = toml::Table::new();
            t.insert("value".to_string(), other);
            Ok(t)
        }
    }
}

/// The document a fresh default file would contain, used when that file
/// couldn't be created. Serializing the defaults and parsing our own
/// output can't realistically fail; if it did, the empty document still
/// loses nothing (later writes add only the keys they change, and absent
/// keys read as their defaults).
fn default_document() -> DocumentMut {
    format!(
        "{HEADER}{}",
        toml::to_string_pretty(&MokuConfig::default()).unwrap_or_default()
    )
    .parse()
    .unwrap_or_default()
}

fn write_default_file(path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = toml::to_string_pretty(&MokuConfig::default())?;
    moku_sys::util::atomic_write(path, format!("{HEADER}{body}").as_bytes())?;
    tracing::info!("New config file created at: {}", path.display());
    Ok(())
}

/// Copies an unparseable config.toml aside (the original is left in place
/// and never written to) and says where, for the user-facing message.
fn keep_broken_copy(path: &Path) -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut copy = path.as_os_str().to_owned();
    copy.push(format!(".broken-{stamp}"));
    let copy = PathBuf::from(copy);
    match std::fs::copy(path, &copy) {
        Ok(_) => format!(" (a copy was saved as {})", copy.display()),
        Err(e) => {
            tracing::warn!("couldn't copy the broken config to {}: {e}", copy.display());
            String::new()
        }
    }
}

/// Builds the config section by section (each top-level table of
/// config.toml is parsed on its own, so a mistake in one never throws away
/// the rest). A section that doesn't parse
/// keeps its defaults and is returned as invalid (write-locked). The third
/// value is set when the file's `config_version` is newer than this binary
/// understands (the whole file is then read-only).
fn config_from_table(
    table: &toml::Table,
) -> (MokuConfig, HashMap<String, ConfigIssue>, Option<String>) {
    let mut config = MokuConfig::default();
    let mut invalid = HashMap::new();
    let mut newer_version = None;

    fn take<T: DeserializeOwned>(
        table: &toml::Table,
        name: &str,
        invalid: &mut HashMap<String, ConfigIssue>,
        slot: &mut T,
    ) {
        if let Some(value) = table.get(name) {
            match deserialize_at::<T>(value.clone(), name) {
                Ok(v) => *slot = v,
                Err(issue) => {
                    invalid.insert(name.to_string(), issue);
                }
            }
        }
    }

    // A file without `config_version` predates the field: version 1.
    config.config_version = 1;
    take(table, "config_version", &mut invalid, &mut config.config_version);
    if config.config_version > CURRENT_CONFIG_VERSION {
        newer_version = Some(format!(
            "version {} was written by a newer moku (this one understands up to {}); using it read-only",
            config.config_version, CURRENT_CONFIG_VERSION
        ));
    }
    take(table, "general", &mut invalid, &mut config.general);
    take(table, "keys", &mut invalid, &mut config.keys);
    take(table, "storage", &mut invalid, &mut config.storage);
    take(table, "themes", &mut invalid, &mut config.themes);
    take(table, "plugins", &mut invalid, &mut config.plugins);
    take(table, "modules", &mut invalid, &mut config.modules);
    (config, invalid, newer_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, serde::Deserialize, Default, Clone, Debug, PartialEq)]
    #[serde(default)]
    struct Pomo {
        work_minutes: u32,
        profiles: Vec<String>,
    }

    fn write(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn test_missing_file_is_created_with_defaults_and_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let handle = ConfigHandle::open(path.clone());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# ----"));
        assert!(text.contains("config_version = 1"));
        assert!(handle.read_only_reason().is_none());
        assert!(handle.take_issues().is_empty());
    }

    #[test]
    fn test_unparseable_file_is_read_only_kept_and_copied() {
        let dir = tempfile::tempdir().unwrap();
        let broken = "[general\ntheme = \"mine\"\n";
        let path = write(dir.path(), broken);
        let handle = ConfigHandle::open(path.clone());

        assert!(handle.read_only_reason().is_some());
        let issues = handle.take_issues();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("broken-"), "{}", issues[0].message);
        assert!(matches!(
            handle.update(|c| c.storage.auto_lock_timeout = 1),
            Err(ConfigError::ReadOnly { .. })
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken, "never overwritten");
        let copies = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("config.toml.broken-")
            })
            .count();
        assert_eq!(copies, 1);
    }

    #[test]
    fn test_a_bad_section_is_reported_and_write_locked_but_the_rest_loads() {
        let dir = tempfile::tempdir().unwrap();
        let src = "[general]\ntheme = 5\n\n[storage]\nauto_lock_timeout = 120\n";
        let path = write(dir.path(), src);
        let handle = ConfigHandle::open(path.clone());

        let issues = handle.take_issues();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, "general.theme");
        assert_eq!(handle.load().general.theme, "system", "bad section falls back");
        assert_eq!(handle.load().storage.auto_lock_timeout, 120, "the rest still loads");

        assert!(matches!(
            handle.update(|c| c.general.theme = "hacker".into()),
            Err(ConfigError::SectionInvalid(_))
        ));
        handle.update(|c| c.storage.auto_lock_timeout = 300).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            src.replace("= 120", "= 300"),
            "the broken section stays exactly as written"
        );
    }

    #[test]
    fn test_update_preserves_comments_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let src = "# my config\n[general]\ntheme = \"system\" # keep me\nfuture_key = 1\n\n# lock fast\n[storage]\nauto_lock_timeout = 300\n";
        let path = write(dir.path(), src);
        let handle = ConfigHandle::open(path.clone());
        handle.update(|c| c.general.theme = "hacker".into()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            src.replace("\"system\"", "\"hacker\"")
        );
        assert_eq!(handle.load().general.theme, "hacker");
    }

    #[test]
    fn test_update_module_refuses_to_overwrite_an_invalid_table() {
        let dir = tempfile::tempdir().unwrap();
        let src = "[modules.pomodoro]\nwork_minutes = \"twenty-five\"\nprofiles = [\"deep\"]\n";
        let path = write(dir.path(), src);
        let handle = ConfigHandle::open(path.clone());

        let read: Pomo = handle.module_config("pomodoro");
        assert_eq!(read, Pomo::default());
        let issues = handle.take_issues();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, "modules.pomodoro.work_minutes");
        // Reported once per session, however often it is read.
        let _: Pomo = handle.module_config("pomodoro");
        assert!(handle.take_issues().is_empty());

        let result = handle.update_module::<Pomo>("pomodoro", |p| p.profiles.clear());
        assert!(matches!(result, Err(ConfigError::SectionInvalid(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), src);
    }

    #[test]
    fn test_update_module_writes_only_changed_fields_and_keeps_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let src = "[modules.pomodoro]\nwork_minutes = 25\nprofiles = [\"deep\"]\nhand_added = true # mine\n";
        let path = write(dir.path(), src);
        let handle = ConfigHandle::open(path.clone());
        handle
            .update_module::<Pomo>("pomodoro", |p| p.work_minutes = 50)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            src.replace("= 25", "= 50")
        );
    }

    #[test]
    fn test_update_module_creates_a_missing_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "[general]\ntheme = \"system\"\n");
        let handle = ConfigHandle::open(path.clone());
        handle
            .update_module::<Pomo>("pomodoro", |p| p.work_minutes = 30)
            .unwrap();
        let reopened = ConfigHandle::open(path);
        let p: Pomo = reopened.module_config("pomodoro");
        assert_eq!(p.work_minutes, 30);
        assert!(reopened.take_issues().is_empty());
    }

    #[test]
    fn test_newer_config_version_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "config_version = 99\n");
        let handle = ConfigHandle::open(path);
        assert!(handle.read_only_reason().unwrap().contains("newer moku"));
        assert!(handle.update(|c| c.storage.auto_lock_timeout = 1).is_err());
    }

    #[test]
    fn test_a_file_without_config_version_is_version_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "[general]\ntheme = \"system\"\n");
        let handle = ConfigHandle::open(path);
        assert_eq!(handle.load().config_version, 1);
        assert!(handle.read_only_reason().is_none());
    }

    #[test]
    fn test_detached_handle_updates_memory_only() {
        let handle = ConfigHandle::detached(MokuConfig::default());
        handle.update(|c| c.storage.auto_lock_timeout = 42).unwrap();
        assert_eq!(handle.load().storage.auto_lock_timeout, 42);
        assert!(handle.path().is_none());
    }
}
