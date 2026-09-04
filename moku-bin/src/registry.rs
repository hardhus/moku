use moku_daemon::DaemonStatusModule;
use moku_core::{CliRegistry, ModuleId, MokuConfig, TuiModule, TuiRegistry};

pub fn build_tui_registry(config: &MokuConfig) -> TuiRegistry {
    let mut r = TuiRegistry::new();

    #[cfg(feature = "lua-plugins")]
    let loaded_plugins = load_lua_plugins(config);
    #[cfg(not(feature = "lua-plugins"))]
    let loaded_plugins: Vec<Box<dyn TuiModule>> = Vec::new();

    let plugin_ids: Vec<ModuleId> = loaded_plugins.iter().map(|m| m.id()).collect();

    r.insert(Box::new(moku_launcher::LauncherModule::new(plugin_ids, config)));
    r.insert(Box::new(moku_lock_screen::LockScreenModule::new()));
    r.insert(Box::new(moku_todo::TodoModule::new()));
    r.insert(Box::new(moku_settings::SettingsModule::new(config)));
    r.insert(Box::new(moku_dashboard::DashboardModule::new()));
    r.insert(Box::new(moku_bookmark::BookmarkModule::new()));
    r.insert(Box::new(moku_rss::RssTuiModule::new()));
    r.insert(Box::new(DaemonStatusModule::new()));
    r.insert(Box::new(moku_vault_daemon::VaultManagerModule::new()));
    r.insert(Box::new(moku_satz::NotesModule::new()));
    r.insert(Box::new(moku_secrets::SecretsModule::new()));
    r.insert(Box::new(moku_http::HttpModule::new()));

    for module in loaded_plugins {
        r.insert(module);
    }

    r
}

#[cfg(feature = "lua-plugins")]
fn load_lua_plugins(config: &MokuConfig) -> Vec<Box<dyn TuiModule>> {
    let mut loaded = Vec::new();

    if let Ok(plugins_dir) = moku_core::dirs::get_plugins_dir() {
        for entry in &config.plugins {
            let id = ModuleId::new(Box::leak(entry.id.clone().into_boxed_str()));
            let title: &'static str = Box::leak(entry.title.clone().into_boxed_str());
            let script_path = plugins_dir.join(&entry.script);

            match moku_lua::LuaModule::load(id, title, &script_path) {
                Ok(module) => loaded.push(Box::new(module) as Box<dyn TuiModule>),
                Err(e) => {
                    tracing::warn!("Failed to load plugin '{}': {e}", entry.id);
                }
            }
        }
    }

    #[cfg(debug_assertions)]
    {
        if let Ok(cwd) = std::env::current_dir() {
            let examples_dir = cwd.join("plugins").join("examples");
            if examples_dir.is_dir() {
                if let Ok(entries) = std::fs::read_dir(examples_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().map_or(false, |ext| ext == "lua") {
                            let file_stem = path.file_stem().unwrap().to_string_lossy().into_owned();
                            if config.plugins.iter().any(|p| p.id == file_stem) {
                                continue;
                            }
                            
                            let id = ModuleId::new(Box::leak(format!("example_{}", file_stem).into_boxed_str()));
                            let title: &'static str = Box::leak(format!("Example {}", file_stem.to_uppercase()).into_boxed_str());

                            match moku_lua::LuaModule::load(id, title, &path) {
                                Ok(module) => loaded.push(Box::new(module) as Box<dyn TuiModule>),
                                Err(e) => {
                                    tracing::warn!("Failed to load dev example plugin '{:?}': {e}", path);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    loaded
}

pub fn build_cli_registry() -> CliRegistry {
    let mut r = CliRegistry::new();
    r.insert(Box::new(moku_context::ContextModule::new()));
    r.insert(Box::new(moku_commit::CommitModule::new()));
    r.insert(Box::new(moku_rss::RssCliModule::new()));
    r.insert(Box::new(moku_satz::NotesCliModule::new()));
    r.insert(Box::new(moku_secrets::SecretsCliModule::new()));
    r.insert(Box::new(moku_http::HttpCliModule::new()));
    r
}
