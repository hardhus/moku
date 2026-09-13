use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Result, anyhow};

/// `[modules.zk_view]` config, read via `MokuConfig::resolve_module_config`
/// (the same dynamic-TOML-table mechanism every other module uses).
/// Deliberately independent of `modules/moku-satz`'s own `[modules.notes]
/// vault_path` for now — this module always browses its own configured
/// directory as a whole, no shared state between the two.
#[derive(serde::Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ZkViewConfig {
    pub vault_path: String,
    pub editor_command: String,
    /// `[modules.zk_view.keys]` — only `open_editor` has a compiled-in
    /// default (see `resolve_keys`); every other action this module binds
    /// (hjkl/w/b/gg/G/gd) is intentionally NOT routed through this
    /// override system at all, since it's meant to be fixed Helix muscle
    /// memory, not swappable per-user.
    pub keys: HashMap<String, String>,
}

impl Default for ZkViewConfig {
    fn default() -> Self {
        Self {
            vault_path: String::new(),
            editor_command: "hx".to_string(),
            keys: HashMap::new(),
        }
    }
}

pub fn resolve_vault_root(config: &ZkViewConfig) -> Result<PathBuf> {
    if config.vault_path.is_empty() {
        return Err(anyhow!(
            "no notes vault configured — set [modules.zk_view] vault_path in config.toml"
        ));
    }
    Ok(PathBuf::from(&config.vault_path))
}

const DEFAULT_KEY_OVERRIDES: &[(&str, &str)] = &[("open_editor", "e")];

/// Merges the compiled-in default (`open_editor = "e"`) with whatever the
/// user set under `[modules.zk_view.keys]` — a user override always wins.
pub fn resolve_keys(cfg: &ZkViewConfig) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = DEFAULT_KEY_OVERRIDES
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    map.extend(cfg.keys.clone());
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_vault_root_errors_when_unset() {
        let cfg = ZkViewConfig::default();
        assert!(resolve_vault_root(&cfg).is_err());
    }

    #[test]
    fn test_resolve_vault_root_uses_configured_path() {
        let cfg = ZkViewConfig {
            vault_path: "D:/notes".to_string(),
            ..ZkViewConfig::default()
        };
        assert_eq!(resolve_vault_root(&cfg).unwrap(), PathBuf::from("D:/notes"));
    }

    #[test]
    fn test_resolve_keys_default_open_editor_is_e() {
        let cfg = ZkViewConfig::default();
        let keys = resolve_keys(&cfg);
        assert_eq!(keys.get("open_editor"), Some(&"e".to_string()));
    }

    #[test]
    fn test_resolve_keys_user_override_replaces_default() {
        let mut cfg = ZkViewConfig::default();
        cfg.keys.insert("open_editor".to_string(), "ctrl-e".to_string());
        let keys = resolve_keys(&cfg);
        assert_eq!(keys.get("open_editor"), Some(&"ctrl-e".to_string()));
    }

    #[test]
    fn test_resolve_keys_unrelated_override_does_not_clobber_default() {
        let mut cfg = ZkViewConfig::default();
        cfg.keys.insert("something_else".to_string(), "x".to_string());
        let keys = resolve_keys(&cfg);
        assert_eq!(keys.get("open_editor"), Some(&"e".to_string()));
        assert_eq!(keys.get("something_else"), Some(&"x".to_string()));
    }
}
