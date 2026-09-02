use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FeedSubscription {
    pub url: String,
    pub title: Option<String>,
    #[serde(default)]
    pub favorite: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FeedItem {
    pub id: String,
    pub feed_title: String,
    pub title: String,
    pub link: String,
    pub published_at: u64,
}

const STORAGE_NS: &str = "rss";
const FEEDS_KEY: &str = "feeds";
const ITEMS_KEY: &str = "items";
const MAX_ITEMS: usize = 200;

pub struct RssEngine;

impl RssEngine {
    pub async fn load_feeds(storage: &moku_core::StorageManager) -> Vec<FeedSubscription> {
        storage.load(STORAGE_NS, FEEDS_KEY).await.unwrap_or_default()
    }

    pub async fn save_feeds(storage: &moku_core::StorageManager, feeds: &[FeedSubscription]) -> Result<()> {
        storage.save(STORAGE_NS, FEEDS_KEY, &feeds.to_vec(), false).await
    }

    pub async fn load_items(storage: &moku_core::StorageManager) -> Vec<FeedItem> {
        storage.load(STORAGE_NS, ITEMS_KEY).await.unwrap_or_default()
    }

    async fn save_items(storage: &moku_core::StorageManager, items: &[FeedItem]) -> Result<()> {
        storage.save(STORAGE_NS, ITEMS_KEY, &items.to_vec(), false).await
    }

    /// Fetches all subscribed feeds, appends new items to the persistent list,
    /// and returns items for which notifications should be sent (previously unseen and from favorite feeds).
    pub async fn fetch_all(storage: &moku_core::StorageManager) -> Result<Vec<FeedItem>> {
        let feeds = Self::load_feeds(storage).await;
        let mut items = Self::load_items(storage).await;
        let known_ids: std::collections::HashSet<String> =
            items.iter().map(|i| i.id.clone()).collect();

        let mut newly_found_all = Vec::new();
        let mut newly_found_favorite = Vec::new();

        for feed in &feeds {
            let fetched = match fetch_one(&feed.url).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("RSS fetch error ({}): {e}", feed.url);
                    continue;
                }
            };

            let feed_title = feed
                .title
                .clone()
                .unwrap_or_else(|| fetched.title.clone().unwrap_or_default());

            for entry in fetched.entries {
                if known_ids.contains(&entry.id) {
                    continue;
                }
                let item = FeedItem {
                    id: entry.id,
                    feed_title: feed_title.clone(),
                    title: entry.title,
                    link: entry.link,
                    published_at: entry.published_at,
                };
                if feed.favorite {
                    newly_found_favorite.push(item.clone());
                }
                newly_found_all.push(item.clone());
                items.push(item);
            }
        }

        let save_result = if !newly_found_all.is_empty() {
            items.sort_by(|a, b| b.published_at.cmp(&a.published_at));
            items.truncate(MAX_ITEMS);
            Self::save_items(storage, &items).await
        } else {
            Ok(())
        };

        // Releasing sled's process-level lock after a tick is the daemon
        // worker's job now (DaemonTask::storage_module_ids, released
        // generically in moku-daemon/src/worker.rs) — not done here, since
        // this function is also called from the TUI's manual refresh,
        // which should keep the DB cached for the rest of its session like
        // every other RSS storage call.
        save_result?;
        Ok(newly_found_favorite)
    }
}

struct FetchedFeed {
    title: Option<String>,
    entries: Vec<FetchedEntry>,
}

struct FetchedEntry {
    id: String,
    title: String,
    link: String,
    published_at: u64,
}

async fn fetch_one(url: &str) -> Result<FetchedFeed> {
    let bytes = reqwest::get(url)
        .await
        .context("HTTP request failed")?
        .bytes()
        .await
        .context("Failed to read response body")?;

    let parsed = feed_rs::parser::parse(&bytes[..]).context("Failed to parse feed")?;

    let entries = parsed
        .entries
        .into_iter()
        .take(20)
        .map(|e| {
            let title = e.title.map(|t| t.content).unwrap_or_else(|| "(untitled)".to_string());
            let link = e.links.first().map(|l| l.href.clone()).unwrap_or_default();
            let published_at = e
                .published
                .or(e.updated)
                .map(|dt| dt.timestamp() as u64)
                .unwrap_or(0);
            FetchedEntry { id: e.id, title, link, published_at }
        })
        .collect();

    Ok(FetchedFeed { title: parsed.title.map(|t| t.content), entries })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feed_item_id_dedup_shape() {
        let items = vec![
            FeedItem {
                id: "a".into(),
                feed_title: "Test".into(),
                title: "First".into(),
                link: "https://x.com/1".into(),
                published_at: 100,
            },
            FeedItem {
                id: "b".into(),
                feed_title: "Test".into(),
                title: "Second".into(),
                link: "https://x.com/2".into(),
                published_at: 200,
            },
        ];
        let known: std::collections::HashSet<String> = items.iter().map(|i| i.id.clone()).collect();
        assert!(known.contains("a"));
        assert!(!known.contains("c"));
    }
}
