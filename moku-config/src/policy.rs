use serde::Deserialize;

use crate::{ConfigIssue, MokuConfig};

#[derive(Deserialize, Default)]
#[serde(default)]
struct EncryptOverride {
    encrypt: Option<bool>,
}

/// Resolves whether `module_id`'s storage should be encrypted, checking
/// (in order): an explicit `[modules.<module_id>].encrypt` override in
/// config.toml, then `module_default` (the module's own
/// `ModuleMeta::encrypt_by_default()` — daemon-driven modules like RSS
/// report `false` here since the daemon never has the vault unlocked),
/// then (only for modules whose own default is `true`) the global
/// `storage.default_encrypt` setting.
///
/// This is the single source of truth for "is this module encrypted" —
/// both `StorageManager::save()` call sites and the TUI's vault-unlock
/// gate (`enter_module()` in moku-bin) should go through it, so a
/// module's effective encryption state can never disagree with itself.
///
/// An override that isn't a boolean (`encrypt = "true"`) is an error, not
/// "no override": falling back would silently store a module the user
/// asked to encrypt as plaintext. Save paths propagate it (the write is
/// refused until config.toml is fixed); gates that only decide whether
/// the vault must be unlocked treat it as encrypted.
pub fn resolve_encryption(
    config: &MokuConfig,
    module_id: &str,
    module_default: bool,
) -> Result<bool, ConfigIssue> {
    let override_value = config
        .resolve_module_config::<EncryptOverride>(module_id)?
        .encrypt;

    Ok(override_value.unwrap_or(if module_default {
        config.storage.default_encrypt
    } else {
        false
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_override_uses_module_default_true_and_global_default() {
        let mut config = MokuConfig::default();
        config.storage.default_encrypt = true;
        assert!(resolve_encryption(&config, "todo", true).unwrap());

        config.storage.default_encrypt = false;
        assert!(!resolve_encryption(&config, "todo", true).unwrap());
    }

    #[test]
    fn test_module_default_false_ignores_global_default() {
        let mut config = MokuConfig::default();
        config.storage.default_encrypt = true;
        // A daemon-driven module (module_default = false) stays
        // unencrypted regardless of the global default, unless overridden.
        assert!(!resolve_encryption(&config, "rss", false).unwrap());
    }

    #[test]
    fn test_explicit_override_wins_over_everything() {
        let mut config = MokuConfig::default();
        config.storage.default_encrypt = false;

        let mut table = toml::map::Map::new();
        table.insert("encrypt".to_string(), toml::Value::Boolean(true));
        config
            .modules
            .insert("rss".to_string(), toml::Value::Table(table));

        // module_default = false, global default = false, but the
        // explicit per-module override still forces true.
        assert!(resolve_encryption(&config, "rss", false).unwrap());
    }

    #[test]
    fn test_explicit_override_false_wins_over_module_default_true() {
        let mut config = MokuConfig::default();
        config.storage.default_encrypt = true;

        let mut table = toml::map::Map::new();
        table.insert("encrypt".to_string(), toml::Value::Boolean(false));
        config
            .modules
            .insert("todo".to_string(), toml::Value::Table(table));

        assert!(!resolve_encryption(&config, "todo", true).unwrap());
    }

    #[test]
    fn test_non_boolean_override_is_an_error_not_plaintext() {
        let mut config = MokuConfig::default();
        let mut table = toml::map::Map::new();
        table.insert("encrypt".to_string(), toml::Value::String("true".to_string()));
        config
            .modules
            .insert("rss".to_string(), toml::Value::Table(table));

        let issue = resolve_encryption(&config, "rss", false).unwrap_err();
        assert_eq!(issue.path, "modules.rss.encrypt");
    }
}
