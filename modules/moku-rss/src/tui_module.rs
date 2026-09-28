use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::Event;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    widgets::ListState,
};

use moku_core::{
    AppContext, ModuleId, ModuleMeta, ModuleStatus, MokuTheme, TuiModule, resolve_event,
};

use crate::engine::{FeedItem, FeedSubscription, RssEngine};

mod view_detail;
mod view_edit_feed;
mod view_split;

use view_edit_feed::EditField;

#[derive(PartialEq, Clone, Copy)]
pub enum Panel {
    Feeds,
    Items,
}

pub enum RssView {
    Split {
        active_panel: Panel,
        feed_state: ListState,
        item_state: ListState,
    },
    Detail {
        item: FeedItem,
    },
    EditFeed {
        url_input: String,
        name_input: String,
        focus: EditField,
        /// True while `name_input` is still just an auto-suggestion (empty,
        /// domain-derived, or fetched) that the user hasn't typed into
        /// themselves — only while this holds does a background title
        /// fetch get to overwrite it.
        name_is_suggested: bool,
        /// True while a background `RssEngine::peek_title` fetch for the
        /// current `url_input` is in flight (cosmetic — shows "fetching…").
        title_fetch_pending: bool,
        /// `None` = adding a new feed. `Some(i)` = editing `feeds[i]` (the
        /// real index into the `feeds` Vec, not the feed_state display
        /// index, which is offset by +1 for the "All Feeds" row).
        editing_index: Option<usize>,
    },
    /// Waiting for the user to confirm deleting `feeds[index]` — plain `d`
    /// enters this instead of deleting right away; `Shift+D`
    /// (`moku_core::is_delete_bypass`) still deletes immediately.
    ConfirmDeleteFeed {
        index: usize,
    },
}

pub struct RssTuiModule {
    feeds: Vec<FeedSubscription>,
    items: Vec<FeedItem>,
    view: RssView,
    refresh_result: RefreshResultSlot,
    is_refreshing: bool,
    /// Set by a background `RssEngine::peek_title` fetch spawned when the
    /// edit modal's focus leaves the URL field — `(url_it_was_fetched_for,
    /// title_or_none)`. Kept on the module (not the transient `RssView`)
    /// since the spawned task outlives any single view value.
    title_suggestion: TitleSuggestionSlot,
    /// `Some(reason)` when stored feeds/items couldn't be loaded. The module
    /// then stays read-only (banner; add/edit/delete/favorite/refresh
    /// refused) so the unreadable-but-intact data is never overwritten.
    load_error: Option<String>,
}

/// Slot a background `[r]`-refresh task reports into — shared between
/// `RssTuiModule` and `view_split`.
pub(super) type RefreshResultSlot = Arc<Mutex<Option<Result<Vec<FeedItem>, String>>>>;

/// Slot a background title-suggestion fetch reports `(url_it_was_fetched_for,
/// title_or_none)` into — shared between `RssTuiModule` and `view_edit_feed`.
pub(super) type TitleSuggestionSlot = Arc<Mutex<Option<(String, Option<String>)>>>;

impl RssTuiModule {
    pub fn new() -> Self {
        let mut feed_state = ListState::default();
        feed_state.select(Some(0));
        let mut item_state = ListState::default();
        item_state.select(Some(0));

        Self {
            feeds: Vec::new(),
            items: Vec::new(),
            view: RssView::Split {
                active_panel: Panel::Feeds,
                feed_state,
                item_state,
            },
            refresh_result: Arc::new(Mutex::new(None)),
            is_refreshing: false,
            title_suggestion: Arc::new(Mutex::new(None)),
            load_error: None,
        }
    }
}

/// Split-view keys that change stored feeds/items: refresh, add, edit,
/// favorite and delete (plain `d` and `Shift+D`).
fn is_mutating_key(event: &Event) -> bool {
    use crossterm::event::KeyCode;
    moku_core::is_delete_bypass(event)
        || matches!(
            event,
            Event::Key(key) if matches!(key.code, KeyCode::Char('r' | 'a' | 'e' | 'f' | 'd'))
        )
}

/// A URL's host with any leading `www.` stripped (same `reqwest::Url`
/// approach already used by `view_split::matches_feed`, not
/// moku-bookmark's separate string-based `extract_domain`). `None` if the
/// URL doesn't parse or has no host.
fn domain_of(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    Some(host.strip_prefix("www.").unwrap_or(host).to_string())
}

/// The text shown for a feed in the Feeds panel: its own title if it has
/// one (manually set, or auto-adopted from the feed's own parsed title —
/// see `RssEngine::fetch_all`/`maybe_adopt_fetched_title`), else the
/// feed's domain, else the raw URL as a last resort. Never the full URL
/// when a title or a parseable host is available — long URLs sharing a
/// common prefix (e.g. every YouTube channel feed) were indistinguishable
/// otherwise.
fn feed_label(feed: &FeedSubscription) -> String {
    if let Some(title) = feed.title.as_deref().filter(|t| !t.is_empty()) {
        return title.to_string();
    }
    domain_of(&feed.url).unwrap_or_else(|| feed.url.clone())
}

/// Removes `feeds[index]`, saves, and reports the outcome — shared by the
/// `Shift+D` bypass and the confirmation prompt's `y`/Enter so both paths
/// run identical logic (matches the original `d`-key handler exactly).
async fn delete_feed_at(
    feeds: &mut Vec<FeedSubscription>,
    feed_state: &mut ListState,
    index: usize,
    ctx: &mut AppContext,
) {
    if index >= feeds.len() {
        return;
    }
    let removed = feeds.remove(index);
    if let Err(e) = RssEngine::save_feeds(&ctx.storage, &ctx.config.load(), feeds).await {
        ctx.show_error(format!("Delete error: {}", e));
    } else {
        ctx.show_info(format!("Removed: {}", feed_label(&removed)));
        feed_state.select(Some(index));
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(r);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(popup_layout[1])[1]
}

impl Default for RssTuiModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for RssTuiModule {
    fn id(&self) -> ModuleId {
        ModuleId::RSS
    }
    fn title(&self) -> &'static str {
        ModuleId::RSS.title()
    }
    fn encrypt_by_default(&self) -> bool {
        // The daemon also writes RSS storage unattended (headless, vault
        // never unlocked) — RSS storage is never encrypted by default,
        // matching modules/moku-rss/src/engine.rs's own resolve_encryption
        // call. Kept in sync across all three RSS ModuleMeta impls
        // (TUI/CLI/daemon task) for consistency, even though only this
        // TUI one is actually consulted by the vault-unlock entry gate.
        false
    }
}

#[async_trait]
impl TuiModule for RssTuiModule {
    async fn init(&mut self, ctx: &mut AppContext) -> Result<()> {
        self.load_error = None;
        let loaded = async {
            Ok::<_, anyhow::Error>((
                RssEngine::load_feeds(&ctx.storage).await?,
                RssEngine::load_items(&ctx.storage).await?,
            ))
        }
        .await;
        match loaded {
            Ok((feeds, items)) => {
                self.feeds = feeds;
                self.items = items;
            }
            Err(e) => {
                self.feeds.clear();
                self.items.clear();
                let reason = format!("{e:#}");
                ctx.show_error(format!("Failed to load feeds: {reason}"));
                self.load_error = Some(reason);
            }
        }
        Ok(())
    }

    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        let command = resolve_event(event, &ctx.config.load().keys, None);
        let mut changed = false;

        // Check background refresh results in a separate block to avoid borrow conflict
        let got_result = {
            let mut slot = self.refresh_result.lock().unwrap();
            slot.take()
        };

        if let Some(res) = got_result {
            self.is_refreshing = false;
            match res {
                Ok(all_items) => {
                    self.items = all_items;
                    ctx.show_info("Feeds refreshed successfully.");
                }
                Err(e) => {
                    ctx.show_error(format!("Refresh error: {}", e));
                }
            }
            changed = true;
        }

        if self.load_error.is_some()
            && matches!(self.view, RssView::Split { .. })
            && is_mutating_key(event)
        {
            ctx.show_warning("RSS is read-only: stored feeds failed to load.");
            return Ok(true);
        }

        // Destructure self to allow disjoint mutable borrows of fields —
        // each view module below only takes exactly the fields it needs.
        let RssTuiModule {
            feeds,
            items,
            view,
            refresh_result,
            is_refreshing,
            title_suggestion,
            load_error: _,
        } = self;

        let view_changed = match view {
            RssView::Split { .. } => {
                view_split::handle_event(
                    view,
                    feeds,
                    items,
                    refresh_result,
                    is_refreshing,
                    event,
                    ctx,
                    command,
                )
                .await?
            }
            RssView::Detail { .. } => view_detail::handle_detail_event(view, event, ctx).await?,
            RssView::EditFeed { .. } => {
                view_edit_feed::handle_event(view, feeds, title_suggestion, event, ctx).await?
            }
            RssView::ConfirmDeleteFeed { .. } => {
                view_detail::handle_confirm_delete_event(view, feeds, event, ctx).await?
            }
        };

        Ok(changed || view_changed)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        // Scoped block to release mutex lock before drawing
        let got_result = {
            let mut slot = self.refresh_result.lock().unwrap();
            slot.take()
        };
        if let Some(res) = got_result {
            self.is_refreshing = false;
            if let Ok(all_items) = res {
                self.items = all_items;
                // No toast here — `draw()` has no `AppContext` access (see
                // `TuiModule::draw`'s signature), so a refresh completing
                // without the user pressing a key updates the list
                // visually but can only surface a toast the next time
                // `handle_event` runs its own copy of this check. This is
                // a pre-existing structural limit of `draw()`, not a
                // regression from moving off the old status-line field.
            }
        }

        let area = match &self.load_error {
            Some(reason) => {
                let split = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(area);
                moku_core::draw_load_error_banner(frame, split[0], theme, reason);
                split[1]
            }
            None => area,
        };

        // Destructure self to allow disjoint field accesses
        let RssTuiModule {
            feeds,
            items,
            view,
            refresh_result: _,
            is_refreshing,
            title_suggestion: _,
            load_error: _,
        } = self;

        match view {
            RssView::Split { .. } => {
                view_split::draw(view, feeds, items, *is_refreshing, frame, area, theme)
            }
            RssView::Detail { .. } => view_detail::draw_detail(view, frame, area, theme),
            RssView::EditFeed { .. } => view_edit_feed::draw(view, frame, area, theme),
            RssView::ConfirmDeleteFeed { .. } => {
                view_detail::draw_confirm_delete(view, feeds, frame, area, theme)
            }
        }
    }

    async fn dashboard_summary(&self, ctx: &AppContext) -> Option<ModuleStatus> {
        let (Ok(feeds), Ok(items)) = (
            RssEngine::load_feeds(&ctx.storage).await,
            RssEngine::load_items(&ctx.storage).await,
        ) else {
            return Some(ModuleStatus::error("Failed to load feeds"));
        };
        Some(ModuleStatus::normal(format!(
            "{} feeds, {} articles",
            feeds.len(),
            items.len()
        )))
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn sub(url: &str, title: Option<&str>, favorite: bool) -> FeedSubscription {
        FeedSubscription {
            url: url.to_string(),
            title: title.map(str::to_string),
            favorite,
        }
    }

    #[test]
    fn test_feed_label_prefers_title() {
        let f = sub("https://example.com/feed", Some("My Blog"), false);
        assert_eq!(feed_label(&f), "My Blog");
    }

    #[test]
    fn test_feed_label_falls_back_to_domain_without_www() {
        let f = sub(
            "https://www.youtube.com/feeds/videos.xml?channel_id=abc123",
            None,
            false,
        );
        assert_eq!(feed_label(&f), "youtube.com");
    }

    #[test]
    fn test_feed_label_falls_back_to_raw_url_when_unparseable() {
        let f = sub("not a url", None, false);
        assert_eq!(feed_label(&f), "not a url");
    }

    fn rendered_rows(module: &mut RssTuiModule) -> Vec<String> {
        let (width, height) = (60u16, 20u16);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = MokuTheme::default();
        terminal
            .draw(|frame| module.draw(frame, Rect::new(0, 0, width, height), &theme))
            .unwrap();
        let content = terminal.backend().buffer().content.clone();
        (0..height as usize)
            .map(|y| {
                content[y * width as usize..(y + 1) * width as usize]
                    .iter()
                    .map(|c| c.symbol())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn test_favorite_star_renders_near_the_start_of_its_row() {
        // The panel is narrow (30% of a 60-col test terminal, minus
        // borders) — a long label would clip a *trailing* star, which is
        // exactly the bug being fixed, so the label here is short enough
        // to definitely fit either way; what's under test is the star's
        // position, not clipping itself (covered by the label tests).
        let mut module = RssTuiModule::new();
        module.feeds = vec![sub("https://example.com/feed", Some("Fav"), true)];
        let rows = rendered_rows(&mut module);
        let row = rows
            .iter()
            .find(|r| r.contains("Fav"))
            .expect("feed row visible");
        let star_pos = row.find('★').expect("star should render");
        assert!(
            star_pos < 5,
            "favorite star should be near the start of the row, not clipped at the end: {row:?}"
        );
    }

    #[test]
    fn test_titled_feed_shows_title_not_url() {
        let mut module = RssTuiModule::new();
        module.feeds = vec![sub(
            "https://example.com/feed.xml",
            Some("Cool Blog"),
            false,
        )];
        let content = rendered_rows(&mut module).join("");
        assert!(content.contains("Cool Blog"));
        assert!(!content.contains("example.com/feed.xml"));
    }

    #[test]
    fn test_untitled_feed_shows_domain_not_full_url() {
        let mut module = RssTuiModule::new();
        module.feeds = vec![sub(
            "https://www.youtube.com/feeds/videos.xml?channel_id=abc123",
            None,
            false,
        )];
        let content = rendered_rows(&mut module).join("");
        assert!(content.contains("youtube.com"));
        assert!(!content.contains("channel_id=abc123"));
    }

    #[test]
    fn test_edit_feed_view_shows_both_fields_at_once() {
        let mut module = RssTuiModule::new();
        module.view = RssView::EditFeed {
            url_input: "https://example.com/feed".to_string(),
            name_input: "My Feed".to_string(),
            focus: EditField::Url,
            name_is_suggested: false,
            title_fetch_pending: false,
            editing_index: None,
        };
        let content = rendered_rows(&mut module).join("");
        assert!(content.contains("Add Feed"));
        assert!(
            content.contains("https://example.com/feed"),
            "URL field should be visible"
        );
        assert!(
            content.contains("My Feed"),
            "Name field should be visible at the same time"
        );
    }

    #[test]
    fn test_edit_feed_view_marked_as_editing_when_index_present() {
        let mut module = RssTuiModule::new();
        module.view = RssView::EditFeed {
            url_input: String::new(),
            name_input: "My Name".to_string(),
            focus: EditField::Name,
            name_is_suggested: false,
            title_fetch_pending: false,
            editing_index: Some(0),
        };
        let content = rendered_rows(&mut module).join("");
        assert!(content.contains("Edit Feed"));
        assert!(content.contains("My Name"));
    }

    #[test]
    fn test_edit_feed_view_shows_fetching_indicator_while_title_fetch_pending() {
        let mut module = RssTuiModule::new();
        module.view = RssView::EditFeed {
            url_input: "https://example.com/feed".to_string(),
            name_input: "example.com".to_string(),
            focus: EditField::Name,
            name_is_suggested: true,
            title_fetch_pending: true,
            editing_index: None,
        };
        let content = rendered_rows(&mut module).join("");
        assert!(content.contains("fetching"));
    }

    #[test]
    fn test_preview_shows_selected_articles_summary() {
        let mut module = RssTuiModule::new();
        module.items = vec![FeedItem {
            id: "a".into(),
            feed_title: "Blog".into(),
            title: "An Article".into(),
            link: "https://example.com/a".into(),
            published_at: 1,
            summary: Some("This is the preview text.".to_string()),
        }];
        let content = rendered_rows(&mut module).join("");
        assert!(content.contains("This is the preview text."));
    }

    #[test]
    fn test_preview_shows_placeholder_when_article_has_no_summary() {
        let mut module = RssTuiModule::new();
        module.items = vec![FeedItem {
            id: "a".into(),
            feed_title: "Blog".into(),
            title: "An Article".into(),
            link: "https://example.com/a".into(),
            published_at: 1,
            summary: None,
        }];
        let content = rendered_rows(&mut module).join("");
        assert!(content.contains("No preview available."));
    }
}

#[cfg(test)]
mod confirm_delete_tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use moku_core::security::{SecurityManager, VaultSession};
    use moku_core::{MokuConfig, StorageManager};
    use tempfile::tempdir;

    use super::*;

    async fn create_test_context() -> AppContext {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::mem::forget(temp);

        let config = Arc::new(ArcSwap::from_pointee(MokuConfig::default()));
        let session = Arc::new(VaultSession::new());
        let security = Arc::new(SecurityManager::new_with_root(root.clone()));
        let storage = Arc::new(
            StorageManager::new_with_root(Arc::clone(&session), root)
                .await
                .unwrap(),
        );

        AppContext::new(config, session, security, storage)
    }

    fn module_with_one_feed() -> RssTuiModule {
        let mut module = RssTuiModule::new();
        module.feeds = vec![FeedSubscription {
            url: "https://example.com/feed".to_string(),
            title: Some("Example Feed".to_string()),
            favorite: false,
        }];
        if let RssView::Split { feed_state, .. } = &mut module.view {
            feed_state.select(Some(1)); // display index 1 = the only real feed
        }
        module
    }

    #[tokio::test]
    async fn test_plain_d_does_not_delete_and_opens_confirmation() {
        let mut module = module_with_one_feed();
        let mut ctx = create_test_context().await;
        let event = Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()));
        module.handle_event(&event, &mut ctx).await.unwrap();

        assert_eq!(module.feeds.len(), 1, "plain 'd' must not delete anything");
        assert!(matches!(
            module.view,
            RssView::ConfirmDeleteFeed { index: 0 }
        ));
    }

    #[tokio::test]
    async fn test_shift_d_deletes_immediately() {
        let mut module = module_with_one_feed();
        let mut ctx = create_test_context().await;
        let event = Event::Key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT));
        module.handle_event(&event, &mut ctx).await.unwrap();

        assert!(module.feeds.is_empty(), "Shift+D should delete immediately");
    }

    #[tokio::test]
    async fn test_confirm_delete_yes_deletes() {
        let mut module = module_with_one_feed();
        module.view = RssView::ConfirmDeleteFeed { index: 0 };
        let mut ctx = create_test_context().await;
        let event = Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::empty()));
        module.handle_event(&event, &mut ctx).await.unwrap();

        assert!(module.feeds.is_empty());
        assert!(matches!(module.view, RssView::Split { .. }));
    }

    #[tokio::test]
    async fn test_confirm_delete_no_cancels() {
        let mut module = module_with_one_feed();
        module.view = RssView::ConfirmDeleteFeed { index: 0 };
        let mut ctx = create_test_context().await;
        let event = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        module.handle_event(&event, &mut ctx).await.unwrap();

        assert_eq!(module.feeds.len(), 1, "cancelling must not delete anything");
        assert!(matches!(module.view, RssView::Split { .. }));
    }
}

#[cfg(test)]
mod dashboard_summary_tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use moku_core::security::{SecurityManager, VaultSession};
    use moku_core::{MokuConfig, StorageManager};
    use tempfile::tempdir;

    use super::*;

    async fn create_test_context() -> AppContext {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::mem::forget(temp);

        let config = Arc::new(ArcSwap::from_pointee(MokuConfig::default()));
        let session = Arc::new(VaultSession::new());
        let security = Arc::new(SecurityManager::new_with_root(root.clone()));
        let storage = Arc::new(
            StorageManager::new_with_root(Arc::clone(&session), root)
                .await
                .unwrap(),
        );

        AppContext::new(config, session, security, storage)
    }

    /// A stored `feeds` record that no longer matches the expected shape:
    /// stored-but-unreadable data (RSS is plaintext, so a wrong key can't
    /// be used to simulate it).
    async fn ctx_with_unreadable_feeds() -> AppContext {
        let ctx = create_test_context().await;
        ctx.storage.save("rss", "feeds", &42u32, false).await.unwrap();
        ctx
    }

    #[tokio::test]
    async fn test_unreadable_feeds_make_module_read_only_and_never_overwrite() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut ctx = ctx_with_unreadable_feeds().await;
        let mut module = RssTuiModule::new();
        module.init(&mut ctx).await.unwrap();
        assert!(module.load_error.is_some());

        for (code, mods) in [
            (KeyCode::Char('a'), KeyModifiers::empty()),
            (KeyCode::Char('r'), KeyModifiers::empty()),
            (KeyCode::Char('f'), KeyModifiers::empty()),
            (KeyCode::Char('d'), KeyModifiers::empty()),
            (KeyCode::Char('D'), KeyModifiers::SHIFT),
        ] {
            let ev = Event::Key(KeyEvent::new(code, mods));
            module.handle_event(&ev, &mut ctx).await.unwrap();
        }
        assert!(matches!(module.view, RssView::Split { .. }));
        assert!(!module.is_refreshing, "refresh must not start while read-only");

        let raw: u32 = ctx.storage.load("rss", "feeds").await.unwrap();
        assert_eq!(raw, 42, "the stored record must be untouched");
    }

    #[tokio::test]
    async fn test_fetch_all_refuses_to_write_when_stored_items_are_unreadable() {
        let ctx = create_test_context().await;
        ctx.storage.save("rss", "items", &42u32, false).await.unwrap();
        assert!(RssEngine::fetch_all(&ctx.storage, &ctx.config.load()).await.is_err());
        let raw: u32 = ctx.storage.load("rss", "items").await.unwrap();
        assert_eq!(raw, 42);
    }

    #[tokio::test]
    async fn test_dashboard_summary_reports_rss_load_error() {
        let ctx = ctx_with_unreadable_feeds().await;
        let status = RssTuiModule::new().dashboard_summary(&ctx).await.unwrap();
        assert_eq!(status.tone, moku_core::StatusTone::Error);
    }

    #[tokio::test]
    async fn test_dashboard_summary_reports_feed_and_article_counts() {
        let module = RssTuiModule::new();
        let ctx = create_test_context().await;

        let feeds = vec![FeedSubscription {
            url: "https://a.example/feed".to_string(),
            title: Some("A".to_string()),
            favorite: false,
        }];
        RssEngine::save_feeds(&ctx.storage, &ctx.config.load(), &feeds)
            .await
            .unwrap();

        let status = module.dashboard_summary(&ctx).await.unwrap();
        assert_eq!(status.tone, moku_core::StatusTone::Normal);
        assert_eq!(status.text, "1 feeds, 0 articles");
    }

    #[test]
    fn test_dashboard_summary_never_locked_by_default() {
        // RSS storage is never encrypted by default (the daemon writes it
        // headlessly with the vault always locked), so this module's
        // dashboard summary must not depend on vault-unlock state.
        assert!(!RssTuiModule::new().encrypt_by_default());
    }
}
