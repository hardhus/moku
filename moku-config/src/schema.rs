use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{ConfigIssue, ThemeColors};

/// The config.toml format version this binary reads and writes. A file
/// without the field is version 1 (every config written before it existed
/// has this format). Format changes add a hop to the P4 migration chain
/// (a separate counter from the data-directory version, same mechanism).
pub const CURRENT_CONFIG_VERSION: u32 = 1;

/// Main configuration structure holding all settings.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct MokuConfig {
    pub config_version: u32,
    pub general: GeneralSettings,
    pub keys: KeyBindings,
    pub storage: StorageSettings,
    pub themes: HashMap<String, ThemeColors>,
    /// Module settings are stored as a dynamic TOML table.
    pub modules: HashMap<String, toml::Value>,
    /// Lua plugins enabled by the user.
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
}

/// Each entry in the `[[plugins]]` TOML array.
/// `script` is a relative path to `dirs::get_plugins_dir()`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PluginEntry {
    pub id: String,
    pub title: String,
    pub script: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct GeneralSettings {
    pub theme: String,
    pub input_cursor_style: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct KeyBindings {
    pub quit: String,
    pub menu: String,
    pub select: String,
    pub up: String,
    pub down: String,
    /// Global hotkey that locks the vault immediately, from anywhere in
    /// the unlocked TUI. Intercepted centrally in moku-bin/src/app_loop.rs
    /// before normal event dispatch.
    pub lock_vault: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct StorageSettings {
    pub default_encrypt: bool,
    pub auto_lock_timeout: u64,
}

impl MokuConfig {
    /// Reads `[modules.<module_id>]` as `T`. A missing table is `T`'s
    /// defaults (missing fields are filled by `#[serde(default)]`); a table
    /// that exists but doesn't match `T` is an error naming the offending
    /// field, never a silent fallback. Callers that can't fail (rendering,
    /// a module's normal reads) use `ConfigHandle::module_config`, which
    /// falls back to the defaults *and* reports the issue to the user.
    pub fn resolve_module_config<T>(&self, module_id: &str) -> Result<T, ConfigIssue>
    where
        T: serde::de::DeserializeOwned + Default,
    {
        match self.modules.get(module_id) {
            None => Ok(T::default()),
            Some(value) => deserialize_at(value.clone(), &format!("modules.{module_id}")),
        }
    }

    /// `[modules.<module_id>.keys]` as a key-name -> binding map, or `None`
    /// if the module has no such table.
    pub fn get_module_keys(
        &self,
        module_id: &str,
    ) -> Result<Option<HashMap<String, String>>, ConfigIssue> {
        match self.modules.get(module_id).and_then(|v| v.get("keys")) {
            None => Ok(None),
            Some(keys) => deserialize_at(keys.clone(), &format!("modules.{module_id}.keys")).map(Some),
        }
    }
}

/// Deserializes `value` as `T`, reporting the exact failing field as a
/// dotted path under `base` (e.g. `modules.pomodoro.profiles[0].plan`).
pub(crate) fn deserialize_at<T: serde::de::DeserializeOwned>(
    value: toml::Value,
    base: &str,
) -> Result<T, ConfigIssue> {
    serde_path_to_error::deserialize(value).map_err(|e| {
        let inner = e.path().to_string();
        let path = if inner.is_empty() || inner == "." {
            base.to_string()
        } else {
            format!("{base}.{inner}")
        };
        ConfigIssue {
            path,
            message: e.into_inner().to_string(),
        }
    })
}

// --- Default Implementations (Pastel, Hacker, Light, System) ---
impl Default for MokuConfig {
    fn default() -> Self {
        let mut themes = HashMap::new();
        themes.insert("system".to_string(), ThemeColors::default());

        // Keep these theme definitions in code as a safety net.
        // base_bg is intentionally left at ThemeColors::default()'s "Reset"
        // — no built-in theme forces an opaque background over a
        // transparent terminal; the "hacker" look comes entirely from
        // base_fg/success. Users who want an opaque theme can still set
        // base_bg explicitly in their own [themes.custom] table.
        themes.insert(
            "hacker".to_string(),
            ThemeColors {
                base_fg: "Green".to_string(),
                success: "Green".to_string(),
                ..ThemeColors::default()
            },
        );

        Self {
            config_version: CURRENT_CONFIG_VERSION,
            general: GeneralSettings::default(),
            keys: KeyBindings::default(),
            storage: StorageSettings::default(),
            modules: HashMap::new(),
            themes,
            plugins: Vec::new(),
        }
    }
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            theme: "system".to_string(),
            input_cursor_style: "Block".to_string(),
        }
    }
}

impl Default for KeyBindings {
    fn default() -> Self {
        Self {
            quit: "q".to_string(),
            menu: "esc".to_string(),
            select: "enter".to_string(),
            up: "k".to_string(),
            down: "j".to_string(),
            lock_vault: "ctrl-l".to_string(),
        }
    }
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            default_encrypt: true,
            auto_lock_timeout: 300,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize, PartialEq, Debug)]
    #[serde(default)]
    struct TodoConfig {
        show_completed: bool,
        max_items: usize,
    }

    impl Default for TodoConfig {
        fn default() -> Self {
            Self {
                show_completed: false,
                max_items: 10,
            }
        }
    }

    #[test]
    fn test_resolve_module_config_full() {
        let mut config = MokuConfig::default();
        let todo_toml = toml::toml! {
            show_completed = true
            max_items = 50
        };

        // toml! macro returns a table, we can wrap it directly in Value::Table
        config
            .modules
            .insert("todo".to_string(), toml::Value::Table(todo_toml));

        let todo_conf: TodoConfig = config.resolve_module_config("todo").unwrap();
        assert_eq!(todo_conf.show_completed, true);
        assert_eq!(todo_conf.max_items, 50);
    }

    #[test]
    fn test_resolve_module_config_type_mismatch() {
        let mut config = MokuConfig::default();
        let todo_toml = toml::toml! {
            max_items = "not_a_number"
        };

        config
            .modules
            .insert("todo".to_string(), toml::Value::Table(todo_toml));
        let issue = config
            .resolve_module_config::<TodoConfig>("todo")
            .unwrap_err();

        // A wrong-typed field is an error naming the field, never a
        // silent fallback to the defaults.
        assert_eq!(issue.path, "modules.todo.max_items");
    }
}
