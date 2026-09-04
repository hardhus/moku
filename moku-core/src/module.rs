mod cli;
mod daemon;
mod id;
mod registry;
mod status;
mod tui;

pub use cli::CliModule;
pub use daemon::DaemonTask;
pub use id::ModuleId;
pub use registry::{CliRegistry, TuiRegistry};
pub use status::{ModuleStatus, StatusTone};
pub use tui::{AsAny, TuiModule};

/// Metadata required for all TUI, CLI, and Daemon modules.
pub trait ModuleMeta: Send + Sync {
    fn id(&self) -> ModuleId;
    fn title(&self) -> &'static str;

    /// Whether this module's storage is encrypted absent any config
    /// override (`[modules.<id>].encrypt` in config.toml always wins over
    /// this). Modules a human enters data into (todo, bookmark, ...)
    /// should stay at the default `true`. A module a `DaemonTask` writes
    /// to unattended should override to `false` — the daemon runs headless
    /// with the vault always locked (no password is stored anywhere for it
    /// to unlock itself with), so an encrypted-by-default module it owns
    /// would simply fail to save.
    fn encrypt_by_default(&self) -> bool {
        true
    }
}
