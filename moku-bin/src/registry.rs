use moku_core::{CliRegistry, ModuleId, MokuConfig, TuiModule, TuiRegistry};

pub fn build_tui_registry(config: &MokuConfig) -> TuiRegistry {
    let mut r = TuiRegistry::new();

    #[cfg(feature = "lua-plugins")]
    let loaded_plugins = load_lua_plugins(config);
    #[cfg(not(feature = "lua-plugins"))]
    let loaded_plugins: Vec<Box<dyn TuiModule>> = Vec::new();

    let plugin_ids: Vec<ModuleId> = loaded_plugins.iter().map(|m| m.id()).collect();

    r.insert(Box::new(moku_launcher::LauncherModule::new(plugin_ids)));
    r.insert(Box::new(moku_lock_screen::LockScreenModule::new()));
    r.insert(Box::new(moku_todo::TodoModule::new()));
    r.insert(Box::new(moku_settings::SettingsModule::new(config)));
    r.insert(Box::new(moku_dashboard::DashboardModule::new()));
    r.insert(Box::new(moku_bookmark::BookmarkModule::new()));
    r.insert(Box::new(moku_rss::RssTuiModule::new()));

    for module in loaded_plugins {
        r.insert(module);
    }

    r
}

#[cfg(feature = "lua-plugins")]
fn load_lua_plugins(config: &MokuConfig) -> Vec<Box<dyn TuiModule>> {
    let Ok(plugins_dir) = moku_core::dirs::get_plugins_dir() else {
        return Vec::new();
    };

    config
        .plugins
        .iter()
        .filter_map(|entry| {
            // Plugin id/title are read from config at runtime; since ModuleId
            // requires a 'static str, we extend the lifetime for the program duration
            // using Box::leak. This is a common Rust pattern done once at program start
            // and INTENDED to remain in memory (clap and similar libraries do this too)
            // — hot-reload is out of scope for v1.
            let id = ModuleId::new(Box::leak(entry.id.clone().into_boxed_str()));
            let title: &'static str = Box::leak(entry.title.clone().into_boxed_str());
            let script_path = plugins_dir.join(&entry.script);

            match moku_lua::LuaModule::load(id, title, &script_path) {
                Ok(module) => Some(Box::new(module) as Box<dyn TuiModule>),
                Err(e) => {
                    tracing::warn!("Failed to load plugin '{}': {e}", entry.id);
                    None
                }
            }
        })
        .collect()
}

pub fn build_cli_registry() -> CliRegistry {
    let mut r = CliRegistry::new();
    r.insert(Box::new(moku_context::ContextModule::new()));
    r.insert(Box::new(moku_commit::CommitModule::new()));
    r.insert(Box::new(moku_rss::RssCliModule::new()));
    r
}
