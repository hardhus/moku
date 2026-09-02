use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use moku_core::{DaemonContext, DaemonTask, ModuleId, ModuleMeta};
use moku_notify::{NotificationAction, NotificationRequest};

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

    async fn tick(&mut self, ctx: &DaemonContext) -> Result<usize> {
        let new_items = RssEngine::fetch_all(&ctx.storage).await?;

        for item in &new_items {
            moku_notify::send(NotificationRequest {
                title: format!("[RSS] {}", item.feed_title),
                body: item.title.clone(),
                action: Some(NotificationAction::OpenUrl(item.link.clone())),
            });
        }

        let count = new_items.len();
        if count > 0 {
            tracing::info!("{} yeni RSS öğesi bulundu ve bildirim gönderildi", count);
        } else {
            tracing::debug!("RSS tick: yeni öğe yok");
        }

        Ok(count)
    }
}
