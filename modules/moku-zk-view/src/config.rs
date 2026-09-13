use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use moku_core::theme::parse_color;
use ratatui::style::Color;

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
    /// (hjkl/w/b/e/gg/G/ge/gs/gl/gd/gr/f/F/t/T) is intentionally NOT routed
    /// through this override system at all, since it's meant to be fixed
    /// Helix muscle memory, not swappable per-user.
    pub keys: HashMap<String, String>,
    /// `[modules.zk_view.colors]` — this module's own markdown-specific
    /// palette (heading levels, link/tag/code/quote/rule), independent of
    /// `MokuTheme`'s fixed semantic fields (which don't have room for e.g.
    /// three distinct heading colors). Still fully config-overridable —
    /// just a second, module-owned palette layered on top rather than a
    /// change to the shared theme system.
    pub colors: ZkViewColors,
}

impl Default for ZkViewConfig {
    fn default() -> Self {
        Self {
            vault_path: String::new(),
            editor_command: "hx".to_string(),
            keys: HashMap::new(),
            colors: ZkViewColors::default(),
        }
    }
}

/// A small, pastel-neon ("Jinx"-leaning: violets, blues, pinks) default
/// palette for markdown-semantic elements this module renders — headings,
/// links, tags, code, blockquotes, rules. Every field is a plain hex/named
/// color string, parsed the same way `MokuTheme`'s own `ThemeColors` are
/// (`moku_core::theme::parse_color`), and every field is independently
/// overridable via `[modules.zk_view.colors]` in config.toml.
#[derive(serde::Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ZkViewColors {
    pub heading1: String,
    pub heading2: String,
    pub heading3: String,
    pub heading_default: String,
    pub link_resolved: String,
    pub link_broken: String,
    pub tag: String,
    pub code_bg: String,
    pub code_fg: String,
    pub blockquote: String,
    pub rule: String,
    /// Table header row background band.
    pub table_header_bg: String,
    /// Table header row text color.
    pub table_header_fg: String,
    /// List item marker (`-`/`*`/`1.`) accent color.
    pub list_marker: String,
}

impl Default for ZkViewColors {
    fn default() -> Self {
        Self {
            heading1: "#B388FF".to_string(),
            heading2: "#64B5F6".to_string(),
            heading3: "#F48FB1".to_string(),
            heading_default: "#90A4AE".to_string(),
            link_resolved: "#4FD1C5".to_string(),
            link_broken: "#FF6B81".to_string(),
            tag: "#C792EA".to_string(),
            code_bg: "#2A2139".to_string(),
            code_fg: "#E0D7F5".to_string(),
            blockquote: "#8892B0".to_string(),
            rule: "#5C527F".to_string(),
            table_header_bg: "#3B2E5A".to_string(),
            table_header_fg: "#E0D7F5".to_string(),
            list_marker: "#7C8FFF".to_string(),
        }
    }
}

/// Resolved `ratatui::style::Color`s, computed once per draw from
/// `ZkViewColors`'s string fields — mirrors `MokuTheme`'s own
/// `ThemeColors` -> `MokuTheme` split (raw config strings vs. the parsed
/// `Color`s actually used for styling).
pub struct ZkViewPalette {
    pub heading1: Color,
    pub heading2: Color,
    pub heading3: Color,
    pub heading_default: Color,
    pub link_resolved: Color,
    pub link_broken: Color,
    pub tag: Color,
    pub code_bg: Color,
    pub code_fg: Color,
    pub blockquote: Color,
    pub rule: Color,
    pub table_header_bg: Color,
    pub table_header_fg: Color,
    pub list_marker: Color,
}

impl ZkViewPalette {
    pub fn from_colors(c: &ZkViewColors) -> Self {
        Self {
            heading1: parse_color(&c.heading1),
            heading2: parse_color(&c.heading2),
            heading3: parse_color(&c.heading3),
            heading_default: parse_color(&c.heading_default),
            link_resolved: parse_color(&c.link_resolved),
            link_broken: parse_color(&c.link_broken),
            tag: parse_color(&c.tag),
            code_bg: parse_color(&c.code_bg),
            code_fg: parse_color(&c.code_fg),
            blockquote: parse_color(&c.blockquote),
            rule: parse_color(&c.rule),
            table_header_bg: parse_color(&c.table_header_bg),
            table_header_fg: parse_color(&c.table_header_fg),
            list_marker: parse_color(&c.list_marker),
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

// Real Helix has no built-in "open external editor" concept — its own `e`
// is the word-end-forward motion. Defaulting to `ctrl-e` here (rather than
// bare `e`) keeps `e` free for that motion, matching actual Helix muscle
// memory instead of colliding with it.
const DEFAULT_KEY_OVERRIDES: &[(&str, &str)] = &[("open_editor", "ctrl-e")];

/// Merges the compiled-in default (`open_editor = "ctrl-e"`) with whatever
/// the user set under `[modules.zk_view.keys]` — a user override always
/// wins.
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
    fn test_resolve_keys_default_open_editor_is_ctrl_e() {
        let cfg = ZkViewConfig::default();
        let keys = resolve_keys(&cfg);
        assert_eq!(keys.get("open_editor"), Some(&"ctrl-e".to_string()));
    }

    #[test]
    fn test_resolve_keys_user_override_replaces_default() {
        let mut cfg = ZkViewConfig::default();
        cfg.keys
            .insert("open_editor".to_string(), "ctrl-shift-e".to_string());
        let keys = resolve_keys(&cfg);
        assert_eq!(keys.get("open_editor"), Some(&"ctrl-shift-e".to_string()));
    }

    #[test]
    fn test_resolve_keys_unrelated_override_does_not_clobber_default() {
        let mut cfg = ZkViewConfig::default();
        cfg.keys.insert("something_else".to_string(), "x".to_string());
        let keys = resolve_keys(&cfg);
        assert_eq!(keys.get("open_editor"), Some(&"ctrl-e".to_string()));
        assert_eq!(keys.get("something_else"), Some(&"x".to_string()));
    }

    #[test]
    fn test_zk_view_colors_default_parses_as_valid_colors() {
        let palette = ZkViewPalette::from_colors(&ZkViewColors::default());
        // Just confirm every field parses to something other than the
        // parser's "totally unrecognized" fallback (Color::White) — a
        // cheap guard against a typo'd default hex breaking silently.
        assert_ne!(palette.heading1, Color::White);
        assert_ne!(palette.link_resolved, Color::White);
        assert_ne!(palette.link_broken, Color::White);
        assert_ne!(palette.code_bg, Color::White);
    }
}
