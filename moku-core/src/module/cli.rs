use anyhow::Result;
use async_trait::async_trait;

use crate::context::CliContext;
use crate::module::ModuleMeta;

/// Trait representing a CLI module running without starting the TUI.
/// Implementations handle command line arguments and run task-specific actions.
#[async_trait]
pub trait CliModule: ModuleMeta {
    async fn run(&self, args: &[String], ctx: &CliContext) -> Result<()>;
}
