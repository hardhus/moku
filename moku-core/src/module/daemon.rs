use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::context::DaemonContext;
use crate::module::ModuleMeta;

/// Trait representing a background daemon task running periodically.
#[async_trait]
pub trait DaemonTask: ModuleMeta {
    fn interval(&self) -> Duration;
    async fn tick(&mut self, ctx: &DaemonContext) -> Result<()>;
}
