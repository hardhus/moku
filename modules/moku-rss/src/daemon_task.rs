use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

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

/// Platform-specific OS notification.
/// Logs a warning if the notification cannot be delivered.
fn send_notification(title: &str, body: &str) {
    use notify_rust::Notification;
    if let Err(e) = Notification::new()
        .app_id("Microsoft.Windows.Explorer")
        .summary(title)
        .body(body)
        .timeout(notify_rust::Timeout::Milliseconds(7000))
        .show()
    {
        tracing::warn!("OS notification failed (this is the diagnostic step — see Bölüm 5): {e:?}");
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
            send_notification(
                &format!("[RSS] {}", item.feed_title),
                &item.title,
            );
        }

        let count = new_items.len();
        if count > 0 {
            tracing::info!("{} new RSS items fetched and notification attempted", count);
        } else {
            tracing::debug!("RSS tick: no new items");
        }

        Ok(count)
    }
}
