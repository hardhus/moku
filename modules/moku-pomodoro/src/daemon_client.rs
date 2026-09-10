//! Thin typed wrapper over `moku_pomodoro_daemon`'s control-channel client
//! helpers — kept as its own file/module (rather than calling
//! `moku_pomodoro_daemon::worker::*` directly from `tui_module.rs`/
//! `cli_module.rs`) so both the TUI and CLI share one place to change if
//! the daemon crate's API shifts.

use anyhow::Result;
use moku_pomodoro_daemon::{Plan, StatusSnapshot};

pub async fn start(plan: Plan, notify: bool) -> Result<()> {
    moku_pomodoro_daemon::worker::ensure_running_and_start(plan, notify).await
}

pub async fn stop() -> Result<()> {
    moku_pomodoro_daemon::worker::stop().await
}

pub async fn pause() -> Result<()> {
    moku_pomodoro_daemon::worker::pause().await
}

pub async fn resume() -> Result<()> {
    moku_pomodoro_daemon::worker::resume().await
}

pub async fn reset() -> Result<()> {
    moku_pomodoro_daemon::worker::reset().await
}

pub async fn skip() -> Result<()> {
    moku_pomodoro_daemon::worker::skip().await
}

/// `Ok(None)` means the daemon is unreachable — treated as "not running"
/// by every caller, not propagated as a hard error, since that's the
/// overwhelmingly common case (nothing started yet).
pub async fn query_status() -> Result<Option<StatusSnapshot>> {
    moku_pomodoro_daemon::worker::query_status().await
}
