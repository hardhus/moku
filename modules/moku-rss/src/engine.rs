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
    /// A short, HTML-stripped preview of the article body, if the feed
    /// provided one. Absent on items fetched before this field existed —
    /// `#[serde(default)]` keeps already-persisted items loading cleanly.
    #[serde(default)]
    pub summary: Option<String>,
}

const STORAGE_NS: &str = "rss";
const FEEDS_KEY: &str = "feeds";
const ITEMS_KEY: &str = "items";
const MAX_ITEMS: usize = 200;

pub struct RssEngine;

impl RssEngine {
    pub async fn load_feeds(storage: &moku_core::StorageManager) -> Vec<FeedSubscription> {
        storage
            .load(STORAGE_NS, FEEDS_KEY)
            .await
            .unwrap_or_default()
    }

    pub async fn save_feeds(
        storage: &moku_core::StorageManager,
        config: &moku_core::MokuConfig,
        feeds: &[FeedSubscription],
    ) -> Result<()> {
        let encrypt = moku_core::resolve_encryption(config, STORAGE_NS, false);
        storage
            .save(STORAGE_NS, FEEDS_KEY, &feeds.to_vec(), encrypt)
            .await
    }

    pub async fn load_items(storage: &moku_core::StorageManager) -> Vec<FeedItem> {
        storage
            .load(STORAGE_NS, ITEMS_KEY)
            .await
            .unwrap_or_default()
    }

    async fn save_items(
        storage: &moku_core::StorageManager,
        config: &moku_core::MokuConfig,
        items: &[FeedItem],
    ) -> Result<()> {
        let encrypt = moku_core::resolve_encryption(config, STORAGE_NS, false);
        storage
            .save(STORAGE_NS, ITEMS_KEY, &items.to_vec(), encrypt)
            .await
    }

    /// Fetches all subscribed feeds, appends new items to the persistent list,
    /// and returns items for which notifications should be sent (previously unseen and from favorite feeds).
    pub async fn fetch_all(
        storage: &moku_core::StorageManager,
        config: &moku_core::MokuConfig,
    ) -> Result<Vec<FeedItem>> {
        let mut feeds = Self::load_feeds(storage).await;
        let mut items = Self::load_items(storage).await;
        let known_ids: std::collections::HashSet<String> =
            items.iter().map(|i| i.id.clone()).collect();

        let mut newly_found_all = Vec::new();
        let mut newly_found_favorite = Vec::new();
        let mut titles_changed = false;

        for feed in feeds.iter_mut() {
            let fetched = match fetch_one(&feed.url).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("RSS fetch error ({}): {e}", feed.url);
                    continue;
                }
            };

            if maybe_adopt_fetched_title(feed, &fetched.title) {
                titles_changed = true;
            }

            let (all, favorite) = merge_feed_entries(&mut items, &known_ids, feed, fetched);
            newly_found_all.extend(all);
            newly_found_favorite.extend(favorite);
        }

        if titles_changed {
            Self::save_feeds(storage, config, &feeds).await?;
        }

        let save_result = if !newly_found_all.is_empty() {
            sort_and_truncate(&mut items);
            Self::save_items(storage, config, &items).await
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

/// Fills in `feed.title` from the feed's own parsed title the first time a
/// fetch succeeds — but only if the subscription doesn't already have one.
/// A user's own title (set via the edit modal) or an already-adopted one is
/// never overwritten. Returns whether it changed anything, so callers can
/// batch a single save instead of one per feed. Pure/no I/O — unit-testable
/// without a mock HTTP server, same reasoning as `merge_feed_entries`.
fn maybe_adopt_fetched_title(feed: &mut FeedSubscription, fetched_title: &Option<String>) -> bool {
    if feed.title.is_none()
        && let Some(t) = fetched_title.as_ref().filter(|t| !t.trim().is_empty())
    {
        feed.title = Some(t.clone());
        return true;
    }
    false
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
    summary: Option<String>,
}

/// Merges one feed's freshly-fetched entries into `items` in place
/// (skipping ones already in `known_ids`), returning (every newly-added
/// item, only the ones from a favorited feed). Pure/no I/O — kept
/// separate from fetch_one()'s network call specifically so this part is
/// unit-testable without a mock HTTP server.
fn merge_feed_entries(
    items: &mut Vec<FeedItem>,
    known_ids: &std::collections::HashSet<String>,
    feed: &FeedSubscription,
    fetched: FetchedFeed,
) -> (Vec<FeedItem>, Vec<FeedItem>) {
    let feed_title = feed
        .title
        .clone()
        .unwrap_or_else(|| fetched.title.clone().unwrap_or_default());

    let mut all = Vec::new();
    let mut favorite = Vec::new();
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
            summary: entry.summary,
        };
        if feed.favorite {
            favorite.push(item.clone());
        }
        all.push(item.clone());
        items.push(item);
    }
    (all, favorite)
}

/// Newest-first sort, capped at MAX_ITEMS.
fn sort_and_truncate(items: &mut Vec<FeedItem>) {
    items.sort_by(|a, b| b.published_at.cmp(&a.published_at));
    items.truncate(MAX_ITEMS);
}

/// Strips `<...>` HTML tags (common in RSS/Atom summaries), collapses runs
/// of whitespace into single spaces, and truncates to `max_len` chars with
/// an ellipsis. No HTML-parsing dependency needed — a preview pane doesn't
/// need real markup handling, just readable plain text.
fn clean_summary(raw: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_tag = false;
    for c in raw.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if in_tag => {}
            c if c.is_whitespace() => {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            c => out.push(c),
        }
    }
    let trimmed = out.trim();
    if trimmed.chars().count() > max_len {
        let truncated: String = trimmed.chars().take(max_len).collect();
        format!("{}…", truncated.trim_end())
    } else {
        trimmed.to_string()
    }
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
            let title = e
                .title
                .map(|t| t.content)
                .unwrap_or_else(|| "(untitled)".to_string());
            let link = e.links.first().map(|l| l.href.clone()).unwrap_or_default();
            let published_at = e
                .published
                .or(e.updated)
                .map(|dt| dt.timestamp() as u64)
                .unwrap_or(0);
            let summary = e
                .summary
                .map(|t| t.content)
                .or_else(|| e.content.and_then(|c| c.body))
                .map(|raw| clean_summary(&raw, 500))
                .filter(|s| !s.is_empty());
            FetchedEntry {
                id: e.id,
                title,
                link,
                published_at,
                summary,
            }
        })
        .collect();

    Ok(FetchedFeed {
        title: parsed.title.map(|t| t.content),
        entries,
    })
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
                summary: None,
            },
            FeedItem {
                id: "b".into(),
                feed_title: "Test".into(),
                title: "Second".into(),
                link: "https://x.com/2".into(),
                published_at: 200,
                summary: None,
            },
        ];
        let known: std::collections::HashSet<String> = items.iter().map(|i| i.id.clone()).collect();
        assert!(known.contains("a"));
        assert!(!known.contains("c"));
    }

    fn entry(id: &str, title: &str, published_at: u64) -> FetchedEntry {
        FetchedEntry {
            id: id.into(),
            title: title.into(),
            link: format!("https://x.com/{id}"),
            published_at,
            summary: None,
        }
    }

    fn feed(url: &str, favorite: bool) -> FeedSubscription {
        FeedSubscription {
            url: url.into(),
            title: None,
            favorite,
        }
    }

    #[test]
    fn test_merge_feed_entries_adds_new_and_skips_known() {
        let mut items = vec![FeedItem {
            id: "a".into(),
            feed_title: "Blog".into(),
            title: "Old".into(),
            link: "https://x.com/a".into(),
            published_at: 1,
            summary: None,
        }];
        let known: std::collections::HashSet<String> = items.iter().map(|i| i.id.clone()).collect();
        let fetched = FetchedFeed {
            title: Some("Blog".to_string()),
            entries: vec![entry("a", "Old (refetched)", 1), entry("b", "New", 2)],
        };

        let (all, favorite) = merge_feed_entries(
            &mut items,
            &known,
            &feed("https://x.com/feed", false),
            fetched,
        );

        assert_eq!(all.len(), 1, "already-known id 'a' must be skipped");
        assert_eq!(all[0].id, "b");
        assert!(
            favorite.is_empty(),
            "non-favorited feed produces no favorite items"
        );
        assert_eq!(items.len(), 2, "new item pushed into the running list");
    }

    #[test]
    fn test_merge_feed_entries_favorite_feed_populates_favorite_list() {
        let mut items = Vec::new();
        let known = std::collections::HashSet::new();
        let fetched = FetchedFeed {
            title: Some("Blog".to_string()),
            entries: vec![entry("a", "New", 1)],
        };

        let (all, favorite) = merge_feed_entries(
            &mut items,
            &known,
            &feed("https://x.com/feed", true),
            fetched,
        );

        assert_eq!(all.len(), 1);
        assert_eq!(favorite.len(), 1);
        assert_eq!(favorite[0].id, "a");
    }

    #[test]
    fn test_merge_feed_entries_uses_subscription_title_override() {
        let mut items = Vec::new();
        let known = std::collections::HashSet::new();
        let fetched = FetchedFeed {
            title: Some("Feed's Own Title".to_string()),
            entries: vec![entry("a", "New", 1)],
        };
        let mut f = feed("https://x.com/feed", false);
        f.title = Some("User's Custom Title".to_string());

        let (all, _) = merge_feed_entries(&mut items, &known, &f, fetched);

        assert_eq!(all[0].feed_title, "User's Custom Title");
    }

    #[test]
    fn test_sort_and_truncate_orders_newest_first() {
        let mut items = vec![
            FeedItem {
                id: "a".into(),
                feed_title: "T".into(),
                title: "T".into(),
                link: "".into(),
                published_at: 100,
                summary: None,
            },
            FeedItem {
                id: "b".into(),
                feed_title: "T".into(),
                title: "T".into(),
                link: "".into(),
                published_at: 300,
                summary: None,
            },
            FeedItem {
                id: "c".into(),
                feed_title: "T".into(),
                title: "T".into(),
                link: "".into(),
                published_at: 200,
                summary: None,
            },
        ];

        sort_and_truncate(&mut items);

        assert_eq!(
            items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "c", "a"]
        );
    }

    #[test]
    fn test_sort_and_truncate_caps_at_max_items() {
        let mut items: Vec<FeedItem> = (0..MAX_ITEMS + 50)
            .map(|i| FeedItem {
                id: i.to_string(),
                feed_title: "T".into(),
                title: "T".into(),
                link: "".into(),
                published_at: i as u64,
                summary: None,
            })
            .collect();

        sort_and_truncate(&mut items);

        assert_eq!(items.len(), MAX_ITEMS);
        // Newest-first, so the surviving items are the highest published_at values.
        assert_eq!(items[0].published_at as usize, MAX_ITEMS + 49);
    }

    #[test]
    fn test_maybe_adopt_fetched_title_fills_empty_title() {
        let mut f = feed("https://x.com/feed", false);
        let changed = maybe_adopt_fetched_title(&mut f, &Some("Real Channel Name".to_string()));
        assert!(changed);
        assert_eq!(f.title.as_deref(), Some("Real Channel Name"));
    }

    #[test]
    fn test_maybe_adopt_fetched_title_never_overwrites_existing_title() {
        let mut f = feed("https://x.com/feed", false);
        f.title = Some("User's Custom Title".to_string());
        let changed = maybe_adopt_fetched_title(&mut f, &Some("Feed's Own Title".to_string()));
        assert!(!changed);
        assert_eq!(f.title.as_deref(), Some("User's Custom Title"));
    }

    #[test]
    fn test_maybe_adopt_fetched_title_does_nothing_when_fetched_title_missing_or_blank() {
        let mut f = feed("https://x.com/feed", false);
        assert!(!maybe_adopt_fetched_title(&mut f, &None));
        assert!(f.title.is_none());
        assert!(!maybe_adopt_fetched_title(&mut f, &Some("   ".to_string())));
        assert!(f.title.is_none());
    }

    #[test]
    fn test_clean_summary_strips_html_tags() {
        assert_eq!(
            clean_summary("<p>Hello <b>world</b>!</p>", 500),
            "Hello world!"
        );
    }

    #[test]
    fn test_clean_summary_collapses_whitespace() {
        assert_eq!(clean_summary("a\n\n  b   c", 500), "a b c");
    }

    #[test]
    fn test_clean_summary_truncates_with_ellipsis() {
        let raw = "a".repeat(600);
        let cleaned = clean_summary(&raw, 500);
        assert_eq!(cleaned.chars().count(), 501); // 500 chars + '…'
        assert!(cleaned.ends_with('…'));
    }

    #[test]
    fn test_clean_summary_short_text_unchanged() {
        assert_eq!(clean_summary("Short text.", 500), "Short text.");
    }
}
