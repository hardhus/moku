use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::context::DaemonContext;
use crate::module::ModuleMeta;

/// Trait representing a background daemon task running periodically.
#[async_trait]
pub trait DaemonTask: ModuleMeta {
    fn interval(&self) -> Duration;
    async fn tick(&mut self, ctx: &DaemonContext) -> Result<usize>;

    /// Storage module_ids this task's `tick()` touches. The worker closes
    /// each one's cached sled handle after every tick so other processes
    /// (e.g. the TUI/CLI) can open the same DB — sled only allows one
    /// process to hold a given DB open at a time. Defaults to `[self.id()]`,
    /// which covers a task that only ever touches its own module's storage;
    /// override if a task reads/writes a different or additional module_id.
    fn storage_module_ids(&self) -> Vec<&str> {
        vec![self.id().as_str()]
    }
}
