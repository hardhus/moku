use anyhow::{Result, bail};
use async_trait::async_trait;
use clap::{Parser, Subcommand};

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

#[derive(Parser, Debug)]
#[command(name = "rss", no_binary_name = true)]
struct RssArgs {
    #[command(subcommand)]
    cmd: Option<RssCmd>,
}

#[derive(Subcommand, Debug)]
enum RssCmd {
    /// Subscribe to a new feed URL.
    Add { url: String },
    /// Unsubscribe from a feed URL.
    #[command(alias = "rm")]
    Remove { url: String },
    /// List subscribed feeds (default when no subcommand is given).
    #[command(alias = "ls")]
    List,
    /// Send a branded test notification.
    TestNotify,
}

#[async_trait]
impl CliModule for RssCliModule {
    async fn run(&self, args: &[String], ctx: &CliContext) -> Result<()> {
        let Some(storage) = &ctx.storage else {
            bail!("RSS commands require storage access (internal error: CliContext.storage is empty).");
        };

        let parsed = match RssArgs::try_parse_from(args) {
            Ok(p) => p,
            Err(e) if e.exit_code() == 0 => {
                // --help / --version: print clap's own formatted output and succeed.
                print!("{e}");
                return Ok(());
            }
            Err(e) => {
                // Real parse error: fold into the same error-reporting path
                // as everything else in the app (clap's message already
                // includes usage info) instead of printing twice.
                bail!("{e}");
            }
        };

        match parsed.cmd.unwrap_or(RssCmd::List) {
            RssCmd::Add { url } => {
                let mut feeds = RssEngine::load_feeds(storage).await;
                if feeds.iter().any(|f| f.url == url) {
                    println!("This feed is already added: {url}");
                    return Ok(());
                }
                feeds.push(FeedSubscription { url: url.clone(), title: None, favorite: false });
                RssEngine::save_feeds(storage, &feeds).await?;
                println!("✅ Added: {url}");
            }
            RssCmd::Remove { url } => {
                let mut feeds = RssEngine::load_feeds(storage).await;
                let before = feeds.len();
                feeds.retain(|f| f.url != url);
                RssEngine::save_feeds(storage, &feeds).await?;
                if feeds.len() < before {
                    println!("🧹 Removed: {url}");
                } else {
                    println!("Feed not found: {url}");
                }
            }
            RssCmd::List => {
                let feeds = RssEngine::load_feeds(storage).await;
                if feeds.is_empty() {
                    println!("No subscribed feeds yet. To add: moku rss add <url>");
                } else {
                    for f in &feeds {
                        println!("- {}", f.url);
                    }
                }
            }
            RssCmd::TestNotify => {
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
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> std::result::Result<RssArgs, clap::Error> {
        RssArgs::try_parse_from(args)
    }

    #[test]
    fn test_no_args_defaults_to_list() {
        let parsed = parse(&[]).unwrap();
        assert!(matches!(parsed.cmd, None));
    }

    #[test]
    fn test_add_parses_url() {
        let parsed = parse(&["add", "https://example.com/feed.xml"]).unwrap();
        match parsed.cmd {
            Some(RssCmd::Add { url }) => assert_eq!(url, "https://example.com/feed.xml"),
            _ => panic!("expected Add"),
        }
    }

    #[test]
    fn test_remove_alias_rm() {
        let parsed = parse(&["rm", "https://example.com/feed.xml"]).unwrap();
        assert!(matches!(parsed.cmd, Some(RssCmd::Remove { .. })));
    }

    #[test]
    fn test_list_alias_ls() {
        let parsed = parse(&["ls"]).unwrap();
        assert!(matches!(parsed.cmd, Some(RssCmd::List)));
    }

    #[test]
    fn test_test_notify_parses() {
        let parsed = parse(&["test-notify"]).unwrap();
        assert!(matches!(parsed.cmd, Some(RssCmd::TestNotify)));
    }

    #[test]
    fn test_unknown_subcommand_is_error() {
        assert!(parse(&["bogus"]).is_err());
    }

    #[test]
    fn test_add_missing_url_is_error() {
        assert!(parse(&["add"]).is_err());
    }

    #[test]
    fn test_help_exits_zero() {
        let err = parse(&["--help"]).unwrap_err();
        assert_eq!(err.exit_code(), 0);
    }
}
