use std::fmt::Display;

use serde::{Deserialize, Serialize};

/// Unique identifier for a module.
/// Uses static strings for zero-cost compile-time identification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModuleId(&'static str);

impl ModuleId {
    pub const LAUNCHER: Self = Self("launcher");
    pub const DASHBOARD: Self = Self("dashboard");
    pub const TODO: Self = Self("todo");
    pub const BOOKMARK: Self = Self("bookmark");
    pub const SETTINGS: Self = Self("settings");
    pub const LOCK_SCREEN: Self = Self("lock_screen");

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
            _ => self.0,
        }
    }

    /// Returns list of visible TUI modules in the Launcher.
    pub fn all_visible() -> Vec<ModuleId> {
        vec![Self::DASHBOARD, Self::TODO, Self::BOOKMARK, Self::SETTINGS]
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
    }
}
