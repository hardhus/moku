mod cli;
mod daemon;
mod id;
mod registry;
mod tui;

pub use cli::CliModule;
pub use daemon::DaemonTask;
pub use id::ModuleId;
pub use registry::{CliRegistry, TuiRegistry};
pub use tui::TuiModule;

/// Metadata required for all TUI, CLI, and Daemon modules.
pub trait ModuleMeta: Send + Sync {
    fn id(&self) -> ModuleId;
    fn title(&self) -> &'static str;
}
