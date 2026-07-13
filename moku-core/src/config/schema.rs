use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{MokuTheme, theme::ThemeColors};

/// Main configuration structure holding all settings.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct MokuConfig {
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
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct StorageSettings {
    pub default_encrypt: bool,
    pub auto_lock_timeout: u64,
}

impl MokuConfig {
    pub fn resolve_module_config<T>(&self, module_id: &str) -> T
    where
        T: serde::de::DeserializeOwned + Default,
    {
        self.modules
            .get(module_id)
            .cloned()
            .and_then(|value| value.try_into().ok())
            .unwrap_or_default()
    }

    pub fn get_module_keys(&self, module_id: &str) -> Option<HashMap<String, String>> {
        self.modules
            .get(module_id)
            .and_then(|v| v.get("keys"))
            .cloned()
            .and_then(|k| k.try_into().ok())
    }

    pub fn get_active_theme(&self) -> MokuTheme {
        let theme_name = &self.general.theme;
        if let Some(colors) = self.themes.get(theme_name) {
            MokuTheme::from_colors(colors)
        } else {
            MokuTheme::default()
        }
    }
}

// --- Default Implementations (Pastel, Hacker, Light, System) ---
impl Default for MokuConfig {
    fn default() -> Self {
        let mut themes = HashMap::new();
        themes.insert("system".to_string(), ThemeColors::default());

        // Keep these theme definitions in code as a safety net
        themes.insert(
            "hacker".to_string(),
            ThemeColors {
                base_fg: "Green".to_string(),
                base_bg: "Black".to_string(),
                ..ThemeColors::default()
            },
        );

        Self {
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

        let todo_conf: TodoConfig = config.resolve_module_config("todo");
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
        let todo_conf: TodoConfig = config.resolve_module_config("todo");

        // Should silently return Default on error
        assert_eq!(todo_conf.max_items, 10);
    }
}
