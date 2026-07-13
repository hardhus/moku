use moku_core::TuiRegistry;

pub fn build_tui_registry(config: &moku_core::MokuConfig) -> TuiRegistry {
    let mut r = TuiRegistry::new();
    r.insert(Box::new(moku_launcher::LauncherModule::new()));
    r.insert(Box::new(moku_lock_screen::LockScreenModule::new()));
    r.insert(Box::new(moku_todo::TodoModule::new()));
    r.insert(Box::new(moku_settings::SettingsModule::new(config)));
    r.insert(Box::new(moku_dashboard::DashboardModule::new()));
    r.insert(Box::new(moku_bookmark::BookmarkModule::new()));
    r
}

pub fn build_cli_registry() -> moku_core::CliRegistry {
    let mut r = moku_core::CliRegistry::new();
    r.insert(Box::new(moku_context::ContextModule::new()));
    r.insert(Box::new(moku_commit::CommitModule::new()));
    r
}
