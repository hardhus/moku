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
                println!("Testing Windows Toast notification via notify-rust...");
                use notify_rust::Notification;

                let aumid = "Microsoft.Windows.Explorer";
                println!("Sending test notification using app_id = {}", aumid);
                match Notification::new()
                    .app_id(aumid)
                    .summary("Moku Test Notification")
                    .body("If you see this, notifications are working successfully!")
                    .timeout(notify_rust::Timeout::Milliseconds(5000))
                    .show()
                {
                    Ok(_) => println!("✅ Notification sent successfully!"),
                    Err(e) => println!("❌ Notification failed: {:?}", e),
                }
            }
            Some(other) => bail!("Unknown subcommand: {other} (add | remove | list | test-notify)"),
        }
        Ok(())
    }
}
