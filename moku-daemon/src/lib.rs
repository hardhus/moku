pub mod logging;
pub mod worker;
pub mod pid;
pub mod status;
pub mod autostart;
pub mod tui_module;

/// Convenience re-export for callers registering the TUI module.
pub use tui_module::DaemonStatusModule;
