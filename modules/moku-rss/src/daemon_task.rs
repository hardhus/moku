use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use notify_rust::Notification;

use moku_core::{DaemonContext, DaemonTask, ModuleId, ModuleMeta};

use crate::engine::RssEngine;

pub struct RssDaemonTask;

impl RssDaemonTask {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RssDaemonTask {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for RssDaemonTask {
    fn id(&self) -> ModuleId {
        ModuleId::RSS
    }
    fn title(&self) -> &'static str {
        ModuleId::RSS.title()
    }
}

#[async_trait]
impl DaemonTask for RssDaemonTask {
    fn interval(&self) -> Duration {
        Duration::from_secs(15 * 60)
    }

    async fn tick(&mut self, ctx: &DaemonContext) -> Result<()> {
        let new_items = RssEngine::fetch_all(&ctx.storage).await?;

        for item in &new_items {
            let _ = Notification::new()
                .summary(&format!("📢 {}", item.feed_title))
                .body(&item.title)
                .timeout(7000)
                .show();
        }

        if !new_items.is_empty() {
            tracing::info!("{} new RSS items found", new_items.len());
        }
        Ok(())
    }
}
