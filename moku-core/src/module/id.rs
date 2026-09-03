use std::fmt::Display;

use serde::{Deserialize, Serialize};

/// Unique identifier for a module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModuleId(&'static str);

impl ModuleId {
    pub const LAUNCHER: Self = Self("launcher");
    pub const DASHBOARD: Self = Self("dashboard");
    pub const TODO: Self = Self("todo");
    pub const BOOKMARK: Self = Self("bookmark");
    pub const SETTINGS: Self = Self("settings");
    pub const LOCK_SCREEN: Self = Self("lock_screen");
    pub const RSS: Self = Self("rss");
    pub const DAEMON: Self = Self("daemon");
    pub const CONTEXT: Self = Self("context");
    pub const COMMIT: Self = Self("commit");
    pub const VAULT: Self = Self("vault");
    pub const NOTES: Self = Self("notes");
    pub const SECRETS: Self = Self("secrets");

    pub const fn new(id: &'static str) -> Self {
        Self(id)
    }

    pub fn as_str(&self) -> &'static str {
        self.0
    }

    pub fn title(&self) -> &'static str {
        match *self {
            Self::LAUNCHER => "Moku Launcher",
            Self::DASHBOARD => "Dashboard",
            Self::TODO => "Todo List",
            Self::BOOKMARK => "Bookmark",
            Self::SETTINGS => "Settings",
            Self::LOCK_SCREEN => "Vault Security",
            Self::RSS => "RSS Feed Reader",
            Self::DAEMON => "Daemon Status",
            Self::VAULT => "Encrypted Vaults",
            Self::NOTES => "Notes",
            Self::SECRETS => "Secrets",
            _ => self.0,
        }
    }

    /// Returns list of visible TUI modules in the Launcher.
    pub fn all_visible() -> Vec<ModuleId> {
        vec![
            Self::DASHBOARD,
            Self::TODO,
            Self::BOOKMARK,
            Self::SETTINGS,
            Self::RSS,
            Self::DAEMON,
            Self::VAULT,
            Self::NOTES,
            Self::SECRETS,
        ]
    }
}

impl Display for ModuleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&'static str> for ModuleId {
    fn from(s: &'static str) -> Self {
        Self::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_id_logic() {
        assert_eq!(ModuleId::TODO.as_str(), "todo");
        assert_eq!(ModuleId::TODO.title(), "Todo List");
        assert!(ModuleId::all_visible().contains(&ModuleId::TODO));
        assert!(ModuleId::all_visible().contains(&ModuleId::RSS));
        assert!(ModuleId::all_visible().contains(&ModuleId::DAEMON));
    }

    #[test]
    fn test_cli_only_module_ids_not_in_launcher() {
        // context/commit are CLI-only and deliberately hidden from the
        // TUI launcher menu.
        assert_eq!(ModuleId::CONTEXT.as_str(), "context");
        assert_eq!(ModuleId::COMMIT.as_str(), "commit");
        assert!(!ModuleId::all_visible().contains(&ModuleId::CONTEXT));
        assert!(!ModuleId::all_visible().contains(&ModuleId::COMMIT));
    }
}
