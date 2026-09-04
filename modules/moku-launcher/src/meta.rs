use moku_core::ModuleId;

/// A single icon + description per `ModuleId::all_visible()` member, kept
/// local to the launcher (not promoted to `moku-core::ModuleId`) since this
/// is the only consumer today — presentation concerns don't belong on the
/// shared module-identity type until a second module actually needs them.
pub fn icon_for(id: ModuleId) -> &'static str {
    match id {
        ModuleId::DASHBOARD => "📊",
        ModuleId::TODO => "✅",
        ModuleId::BOOKMARK => "🔖",
        ModuleId::SETTINGS => "⚙️",
        ModuleId::RSS => "📰",
        ModuleId::DAEMON => "🛰️",
        ModuleId::VAULT => "🔐",
        ModuleId::NOTES => "📝",
        ModuleId::SECRETS => "🔑",
        ModuleId::HTTP => "🌐",
        _ => "🧩", // Lua plugins / unknown future entries
    }
}

pub fn description_for(id: ModuleId) -> &'static str {
    match id {
        ModuleId::DASHBOARD => "At-a-glance overview of your tasks, notes, and recent activity",
        ModuleId::TODO => "Track tasks and to-do items",
        ModuleId::BOOKMARK => "Save and organize web bookmarks, encrypted at rest",
        ModuleId::SETTINGS => "Configure themes, keybindings, and module preferences",
        ModuleId::RSS => "Follow and read RSS and Atom feeds",
        ModuleId::DAEMON => "Monitor and control the background daemon process",
        ModuleId::VAULT => "Manage encrypted vaults for sensitive files",
        ModuleId::NOTES => "Write and organize personal notes",
        ModuleId::SECRETS => "Store passwords, TOTP codes, and other secrets securely",
        ModuleId::HTTP => "Send and inspect HTTP API requests",
        _ => "Custom plugin module",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_every_visible_module_has_a_non_fallback_icon_and_description() {
        for id in ModuleId::all_visible() {
            assert_ne!(icon_for(id), "🧩", "missing icon entry for {}", id.as_str());
            assert_ne!(
                description_for(id),
                "Custom plugin module",
                "missing description entry for {}",
                id.as_str()
            );
        }
    }
}
