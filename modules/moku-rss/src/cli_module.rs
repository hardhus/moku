use anyhow::{Result, bail};
use async_trait::async_trait;

use moku_core::{CliContext, CliModule, ModuleId, ModuleMeta};

use crate::engine::{FeedSubscription, RssEngine};

pub struct RssCliModule;

impl RssCliModule {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RssCliModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for RssCliModule {
    fn id(&self) -> ModuleId {
        ModuleId::RSS
    }
    fn title(&self) -> &'static str {
        ModuleId::RSS.title()
    }
}

#[async_trait]
impl CliModule for RssCliModule {
    async fn run(&self, args: &[String], ctx: &CliContext) -> Result<()> {
        let Some(storage) = &ctx.storage else {
            bail!("RSS commands require storage access (internal error: CliContext.storage is empty).");
        };

        match args.first().map(String::as_str) {
            Some("add") => {
                let Some(url) = args.get(1) else {
                    bail!("Usage: moku rss add <url>");
                };
                let mut feeds = RssEngine::load_feeds(storage).await;
                if feeds.iter().any(|f| &f.url == url) {
                    println!("This feed is already added: {url}");
                    return Ok(());
                }
                feeds.push(FeedSubscription { url: url.clone(), title: None, favorite: false });
                RssEngine::save_feeds(storage, &feeds).await?;
                println!("✅ Added: {url}");
            }
            Some("remove") => {
                let Some(url) = args.get(1) else {
                    bail!("Usage: moku rss remove <url>");
                };
                let mut feeds = RssEngine::load_feeds(storage).await;
                let before = feeds.len();
                feeds.retain(|f| &f.url != url);
                RssEngine::save_feeds(storage, &feeds).await?;
                if feeds.len() < before {
                    println!("🧹 Removed: {url}");
                } else {
                    println!("Feed not found: {url}");
                }
            }
            Some("list") | None => {
                let feeds = RssEngine::load_feeds(storage).await;
                if feeds.is_empty() {
                    println!("No subscribed feeds yet. To add: moku rss add <url>");
                } else {
                    for f in &feeds {
                        println!("- {}", f.url);
                    }
                }
            }
            Some("test-notify") => {
                println!("Moku markalı test bildirimi gönderiliyor...");
                moku_notify::send(moku_notify::NotificationRequest {
                    title: "Moku Test Bildirimi".to_string(),
                    body: "Bunu görüyorsan bildirimler doğru markayla çalışıyor!".to_string(),
                    action: Some(moku_notify::NotificationAction::OpenUrl(
                        "https://github.com".to_string(),
                    )),
                });
                println!("✅ Gönderildi (hata varsa log dosyasında görünür).");
            }
            Some(other) => bail!("Unknown subcommand: {other} (add | remove | list | test-notify)"),
        }
        Ok(())
    }
}
