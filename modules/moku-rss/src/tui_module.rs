use std::sync::{Arc, Mutex};

use anyhow::Result;
use arboard::Clipboard;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use moku_core::{
    AppContext, Command, ModuleId, ModuleMeta, ModuleStatus, MokuTheme, TuiModule, resolve_event,
};

use crate::engine::{FeedItem, FeedSubscription, RssEngine};

#[derive(PartialEq, Clone, Copy)]
pub enum Panel {
    Feeds,
    Items,
}

/// Which field of the URL+name add/edit form currently has keyboard focus.
/// Both fields are shown at once (`Tab` switches focus, `Enter` submits
/// from either) — unlike `modules/moku-secrets/src/tui_module.rs`'s
/// `AddStage`, this isn't a sequential wizard.
#[derive(PartialEq, Clone, Copy)]
pub enum EditField {
    Url,
    Name,
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
}

pub struct RssTuiModule {
    feeds: Vec<FeedSubscription>,
    items: Vec<FeedItem>,
    view: RssView,
    refresh_result: Arc<Mutex<Option<Result<Vec<FeedItem>, String>>>>,
    is_refreshing: bool,
    /// Set by a background `RssEngine::peek_title` fetch spawned when the
    /// edit modal's focus leaves the URL field — `(url_it_was_fetched_for,
    /// title_or_none)`. Kept on the module (not the transient `RssView`)
    /// since the spawned task outlives any single view value.
    title_suggestion: Arc<Mutex<Option<(String, Option<String>)>>>,
}

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
        }
    }
}

fn get_filtered_items(
    feeds: &[FeedSubscription],
    items: &[FeedItem],
    feed_idx: usize,
) -> Vec<FeedItem> {
    if feed_idx == 0 {
        items.to_vec()
    } else if feed_idx - 1 < feeds.len() {
        let feed = &feeds[feed_idx - 1];
        items
            .iter()
            .filter(|item| matches_feed(item, feed))
            .cloned()
            .collect()
    } else {
        Vec::new()
    }
}

fn matches_feed(item: &FeedItem, feed: &FeedSubscription) -> bool {
    if let Some(ref title) = feed.title {
        if item.feed_title == *title {
            return true;
        }
    }
    if let Ok(feed_url) = reqwest::Url::parse(&feed.url) {
        if let Some(feed_host) = feed_url.host_str() {
            let clean_host = feed_host.strip_prefix("www.").unwrap_or(feed_host);
            if let Ok(item_url) = reqwest::Url::parse(&item.link) {
                if let Some(item_host) = item_url.host_str() {
                    return item_host.contains(clean_host) || clean_host.contains(item_host);
                }
            }
            return item.link.contains(clean_host);
        }
    }
    false
}

/// A URL's host with any leading `www.` stripped (same `reqwest::Url`
/// approach already used by `matches_feed`, not moku-bookmark's separate
/// string-based `extract_domain`). `None` if the URL doesn't parse or has
/// no host.
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

pub enum EditOutcome {
    Added,
    Updated,
    DuplicateUrl,
}

/// The save-decision core of the add/edit-feed flow, kept free of I/O and
/// `ctx` so it's directly unit-testable — same "pure core + thin
/// side-effecting caller" split already used by `engine::merge_feed_entries`.
pub fn apply_edit(
    feeds: &mut Vec<FeedSubscription>,
    editing_index: Option<usize>,
    url: String,
    title: Option<String>,
) -> EditOutcome {
    let duplicate = feeds
        .iter()
        .enumerate()
        .any(|(i, f)| f.url == url && Some(i) != editing_index);
    if duplicate {
        return EditOutcome::DuplicateUrl;
    }
    match editing_index {
        Some(i) => {
            feeds[i].url = url;
            feeds[i].title = title;
            EditOutcome::Updated
        }
        None => {
            feeds.push(FeedSubscription {
                url,
                title,
                favorite: false,
            });
            EditOutcome::Added
        }
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
        self.feeds = RssEngine::load_feeds(&ctx.storage).await;
        self.items = RssEngine::load_items(&ctx.storage).await;
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

        // Destructure self to allow disjoint mutable borrows of fields
        let RssTuiModule {
            feeds,
            items,
            view,
            refresh_result,
            is_refreshing,
            title_suggestion,
        } = self;

        match view {
            RssView::Split {
                active_panel,
                feed_state,
                item_state,
            } => {
                match command {
                    Command::Quit | Command::Back => {
                        ctx.navigate_to(ModuleId::LAUNCHER);
                        return Ok(true);
                    }
                    Command::Up => {
                        if *active_panel == Panel::Feeds {
                            if !feeds.is_empty() || feed_state.selected().is_some() {
                                let i = match feed_state.selected() {
                                    Some(i) => {
                                        if i == 0 {
                                            feeds.len()
                                        } else {
                                            i - 1
                                        }
                                    }
                                    None => 0,
                                };
                                feed_state.select(Some(i));
                                let filtered_len = get_filtered_items(feeds, items, i).len();
                                if filtered_len > 0 {
                                    let item_sel = item_state.selected().unwrap_or(0);
                                    item_state.select(Some(item_sel.min(filtered_len - 1)));
                                } else {
                                    item_state.select(None);
                                }
                                changed = true;
                            }
                        } else {
                            let feed_idx = feed_state.selected().unwrap_or(0);
                            let filtered = get_filtered_items(feeds, items, feed_idx);
                            if !filtered.is_empty() {
                                let i = match item_state.selected() {
                                    Some(i) => {
                                        if i == 0 {
                                            filtered.len() - 1
                                        } else {
                                            i - 1
                                        }
                                    }
                                    None => 0,
                                };
                                item_state.select(Some(i));
                                changed = true;
                            }
                        }
                    }
                    Command::Down => {
                        if *active_panel == Panel::Feeds {
                            let i = match feed_state.selected() {
                                Some(i) => {
                                    if i >= feeds.len() {
                                        0
                                    } else {
                                        i + 1
                                    }
                                }
                                None => 0,
                            };
                            feed_state.select(Some(i));
                            let filtered_len = get_filtered_items(feeds, items, i).len();
                            if filtered_len > 0 {
                                let item_sel = item_state.selected().unwrap_or(0);
                                item_state.select(Some(item_sel.min(filtered_len - 1)));
                            } else {
                                item_state.select(None);
                            }
                            changed = true;
                        } else {
                            let feed_idx = feed_state.selected().unwrap_or(0);
                            let filtered = get_filtered_items(feeds, items, feed_idx);
                            if !filtered.is_empty() {
                                let i = match item_state.selected() {
                                    Some(i) => {
                                        if i >= filtered.len() - 1 {
                                            0
                                        } else {
                                            i + 1
                                        }
                                    }
                                    None => 0,
                                };
                                item_state.select(Some(i));
                                changed = true;
                            }
                        }
                    }
                    _ => {}
                }

                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Tab => {
                                *active_panel = match active_panel {
                                    Panel::Feeds => Panel::Items,
                                    Panel::Items => Panel::Feeds,
                                };
                                changed = true;
                            }
                            KeyCode::Char('r') => {
                                if !*is_refreshing {
                                    *is_refreshing = true;
                                    ctx.show_info("Refreshing feeds...");
                                    let storage = Arc::clone(&ctx.storage);
                                    let config = Arc::clone(&ctx.config);
                                    let result_slot = Arc::clone(refresh_result);

                                    tokio::spawn(async move {
                                        let res =
                                            RssEngine::fetch_all(&storage, &config.load()).await;
                                        if let Err(e) = res {
                                            let mut slot = result_slot.lock().unwrap();
                                            *slot = Some(Err(e.to_string()));
                                        } else {
                                            let all_items = RssEngine::load_items(&storage).await;
                                            let mut slot = result_slot.lock().unwrap();
                                            *slot = Some(Ok(all_items));
                                        }
                                    });
                                    changed = true;
                                }
                            }
                            KeyCode::Char('a') => {
                                *view = RssView::EditFeed {
                                    url_input: String::new(),
                                    name_input: String::new(),
                                    focus: EditField::Url,
                                    name_is_suggested: true,
                                    title_fetch_pending: false,
                                    editing_index: None,
                                };
                                changed = true;
                            }
                            KeyCode::Char('e') => {
                                if *active_panel == Panel::Feeds {
                                    if let Some(i) = feed_state.selected() {
                                        if i > 0 && i - 1 < feeds.len() {
                                            let f = &feeds[i - 1];
                                            let name_input = f.title.clone().unwrap_or_default();
                                            let name_is_suggested = name_input.is_empty();
                                            *view = RssView::EditFeed {
                                                url_input: f.url.clone(),
                                                name_input,
                                                focus: EditField::Url,
                                                name_is_suggested,
                                                title_fetch_pending: false,
                                                editing_index: Some(i - 1),
                                            };
                                            changed = true;
                                        }
                                    }
                                }
                            }
                            KeyCode::Char('f') => {
                                if *active_panel == Panel::Feeds {
                                    if let Some(i) = feed_state.selected() {
                                        if i > 0 && i - 1 < feeds.len() {
                                            feeds[i - 1].favorite = !feeds[i - 1].favorite;
                                            if let Err(e) = RssEngine::save_feeds(
                                                &ctx.storage,
                                                &ctx.config.load(),
                                                feeds,
                                            )
                                            .await
                                            {
                                                ctx.show_error(format!("Save error: {}", e));
                                            } else if feeds[i - 1].favorite {
                                                ctx.show_info("Added to favorites.");
                                            } else {
                                                ctx.show_info("Removed from favorites.");
                                            }
                                            changed = true;
                                        }
                                    }
                                }
                            }
                            KeyCode::Char('d') => {
                                if *active_panel == Panel::Feeds {
                                    if let Some(i) = feed_state.selected() {
                                        if i > 0 && i - 1 < feeds.len() {
                                            let removed = feeds.remove(i - 1);
                                            if let Err(e) = RssEngine::save_feeds(
                                                &ctx.storage,
                                                &ctx.config.load(),
                                                feeds,
                                            )
                                            .await
                                            {
                                                ctx.show_error(format!("Delete error: {}", e));
                                            } else {
                                                ctx.show_info(format!(
                                                    "Removed: {}",
                                                    feed_label(&removed)
                                                ));
                                                feed_state.select(Some((i - 1).max(0)));
                                            }
                                            changed = true;
                                        }
                                    }
                                }
                            }
                            KeyCode::Char('c') => {
                                if *active_panel == Panel::Items {
                                    let feed_idx = feed_state.selected().unwrap_or(0);
                                    let filtered = get_filtered_items(feeds, items, feed_idx);
                                    if let Some(i) = item_state.selected() {
                                        if i < filtered.len() {
                                            let link = &filtered[i].link;
                                            match Clipboard::new()
                                                .and_then(|mut c| c.set_text(link.to_string()))
                                            {
                                                Ok(_) => ctx.show_info(format!("Copied: {}", link)),
                                                Err(e) => ctx
                                                    .show_error(format!("Clipboard error: {}", e)),
                                            }
                                            changed = true;
                                        }
                                    }
                                }
                            }
                            KeyCode::Char('o') => {
                                if *active_panel == Panel::Items {
                                    let feed_idx = feed_state.selected().unwrap_or(0);
                                    let filtered = get_filtered_items(feeds, items, feed_idx);
                                    if let Some(i) = item_state.selected() {
                                        if i < filtered.len() {
                                            let _ = moku_core::util::open_url(&filtered[i].link);
                                            ctx.show_info("Opening in browser...");
                                            changed = true;
                                        }
                                    }
                                }
                            }
                            KeyCode::Enter => {
                                if *active_panel == Panel::Items {
                                    let feed_idx = feed_state.selected().unwrap_or(0);
                                    let filtered = get_filtered_items(feeds, items, feed_idx);
                                    if let Some(i) = item_state.selected() {
                                        if i < filtered.len() {
                                            *view = RssView::Detail {
                                                item: filtered[i].clone(),
                                            };
                                            changed = true;
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            RssView::Detail { item } => {
                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') => {
                                let mut feed_state = ListState::default();
                                feed_state.select(Some(0));
                                let mut item_state = ListState::default();
                                item_state.select(Some(0));
                                *view = RssView::Split {
                                    active_panel: Panel::Items,
                                    feed_state,
                                    item_state,
                                };
                                changed = true;
                            }
                            KeyCode::Char('c') => {
                                match Clipboard::new()
                                    .and_then(|mut c| c.set_text(item.link.clone()))
                                {
                                    Ok(_) => ctx.show_info(format!("Copied: {}", item.link)),
                                    Err(e) => ctx.show_error(format!("Clipboard error: {}", e)),
                                }
                                changed = true;
                            }
                            KeyCode::Char('o') => {
                                let _ = moku_core::util::open_url(&item.link);
                                ctx.show_info("Opening in browser...");
                                changed = true;
                            }
                            _ => {}
                        }
                    }
                }
            }
            RssView::EditFeed {
                url_input,
                name_input,
                focus,
                name_is_suggested,
                title_fetch_pending,
                editing_index,
            } => {
                // Apply a background title-suggestion fetch's result, if
                // one just finished for the URL currently in the field and
                // the user hasn't typed a name of their own since it was
                // kicked off (see the Tab handler below).
                let got_suggestion = {
                    let mut slot = title_suggestion.lock().unwrap();
                    slot.take()
                };
                if let Some((for_url, title)) = got_suggestion {
                    if for_url == url_input.trim() {
                        *title_fetch_pending = false;
                        if *name_is_suggested && let Some(t) = title {
                            *name_input = t;
                        }
                        changed = true;
                    }
                }

                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Esc => {
                                let mut feed_state = ListState::default();
                                feed_state.select(Some(0));
                                let mut item_state = ListState::default();
                                item_state.select(Some(0));
                                *view = RssView::Split {
                                    active_panel: Panel::Feeds,
                                    feed_state,
                                    item_state,
                                };
                                changed = true;
                            }
                            KeyCode::Tab => {
                                let switching_to_name = *focus == EditField::Url;
                                *focus = match focus {
                                    EditField::Url => EditField::Name,
                                    EditField::Name => EditField::Url,
                                };
                                // Moving from the URL field to the Name
                                // field: give an instant domain-based
                                // suggestion (no network needed), then try
                                // to upgrade it to the feed's real title in
                                // the background — same fetch machinery the
                                // [r] refresh already uses.
                                if switching_to_name && *name_is_suggested {
                                    let trimmed = url_input.trim().to_string();
                                    if !trimmed.is_empty() {
                                        *name_input = domain_of(&trimmed).unwrap_or_default();
                                        if !*title_fetch_pending {
                                            *title_fetch_pending = true;
                                            let slot = Arc::clone(title_suggestion);
                                            let fetch_url = trimmed;
                                            tokio::spawn(async move {
                                                let title = RssEngine::peek_title(&fetch_url).await;
                                                let mut slot = slot.lock().unwrap();
                                                *slot = Some((fetch_url, title));
                                            });
                                        }
                                    }
                                }
                                changed = true;
                            }
                            KeyCode::Backspace => {
                                match focus {
                                    EditField::Url => {
                                        url_input.pop();
                                    }
                                    EditField::Name => {
                                        name_input.pop();
                                        *name_is_suggested = false;
                                    }
                                }
                                changed = true;
                            }
                            KeyCode::Enter => {
                                if url_input.trim().is_empty() {
                                    ctx.show_warning("URL cannot be empty.");
                                } else {
                                    let url = url_input.trim().to_string();
                                    let name = name_input.trim();
                                    let title = if name.is_empty() {
                                        None
                                    } else {
                                        Some(name.to_string())
                                    };
                                    let outcome = apply_edit(feeds, *editing_index, url, title);
                                    match outcome {
                                        EditOutcome::DuplicateUrl => {
                                            ctx.show_warning(
                                                "A feed with this URL already exists.",
                                            );
                                        }
                                        EditOutcome::Added => {
                                            if let Err(e) = RssEngine::save_feeds(
                                                &ctx.storage,
                                                &ctx.config.load(),
                                                feeds,
                                            )
                                            .await
                                            {
                                                ctx.show_error(format!("Save failed: {}", e));
                                            } else {
                                                ctx.show_info("Feed added.");
                                            }
                                        }
                                        EditOutcome::Updated => {
                                            if let Err(e) = RssEngine::save_feeds(
                                                &ctx.storage,
                                                &ctx.config.load(),
                                                feeds,
                                            )
                                            .await
                                            {
                                                ctx.show_error(format!("Save failed: {}", e));
                                            } else {
                                                ctx.show_info("Feed updated.");
                                            }
                                        }
                                    }
                                    let mut feed_state = ListState::default();
                                    feed_state.select(Some(0));
                                    let mut item_state = ListState::default();
                                    item_state.select(Some(0));
                                    *view = RssView::Split {
                                        active_panel: Panel::Feeds,
                                        feed_state,
                                        item_state,
                                    };
                                }
                                changed = true;
                            }
                            KeyCode::Char(c) => {
                                match focus {
                                    EditField::Url => url_input.push(c),
                                    EditField::Name => {
                                        name_input.push(c);
                                        *name_is_suggested = false;
                                    }
                                }
                                changed = true;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        Ok(changed)
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

        // Destructure self to allow disjoint field accesses
        let RssTuiModule {
            feeds,
            items,
            view,
            refresh_result: _,
            is_refreshing,
            title_suggestion: _,
        } = self;

        match view {
            RssView::Split {
                active_panel,
                feed_state,
                item_state,
            } => {
                let chunks =
                    Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);

                let panels =
                    Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)])
                        .split(chunks[0]);

                // Feeds panel — favorite marker goes at the FRONT of the
                // line, not appended at the end: a long label (or a narrow
                // panel) means trailing content gets clipped, so a
                // trailing star was effectively never visible.
                let mut feed_items = vec![ListItem::new(" * All Feeds")];
                for f in feeds.iter() {
                    let marker = if f.favorite { "★" } else { " " };
                    feed_items.push(ListItem::new(format!(" {} {}", marker, feed_label(f))));
                }

                let feed_border_style = if *active_panel == Panel::Feeds {
                    Style::default()
                        .fg(theme.selection_bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.border)
                };

                let feed_list = List::new(feed_items)
                    .block(
                        Block::default()
                            .title(" 📡 Feeds ")
                            .borders(Borders::ALL)
                            .border_style(feed_border_style)
                            .style(Style::default().bg(theme.base_bg)),
                    )
                    .style(Style::default().fg(theme.base_fg))
                    .highlight_style(
                        Style::default()
                            .fg(theme.selection_fg)
                            .bg(theme.selection_bg)
                            .add_modifier(Modifier::BOLD),
                    );

                frame.render_stateful_widget(feed_list, panels[0], feed_state);

                // Articles column: list on top, preview of the selected
                // article below.
                let article_area =
                    Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)])
                        .split(panels[1]);

                let selected_feed_idx = feed_state.selected().unwrap_or(0);
                let filtered = get_filtered_items(feeds, items, selected_feed_idx);

                let item_items: Vec<ListItem> = filtered
                    .iter()
                    .map(|i| ListItem::new(format!("[{}] {}", i.feed_title, i.title)))
                    .collect();

                let item_border_style = if *active_panel == Panel::Items {
                    Style::default()
                        .fg(theme.selection_bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.border)
                };

                let mut title = " 📰 Articles ".to_string();
                if *is_refreshing {
                    title.push_str("(Refreshing...) ");
                }

                let item_list = List::new(item_items)
                    .block(
                        Block::default()
                            .title(title)
                            .borders(Borders::ALL)
                            .border_style(item_border_style)
                            .style(Style::default().bg(theme.base_bg)),
                    )
                    .style(Style::default().fg(theme.base_fg))
                    .highlight_style(
                        Style::default()
                            .fg(theme.selection_fg)
                            .bg(theme.selection_bg)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol(">> ");

                frame.render_stateful_widget(item_list, article_area[0], item_state);

                let preview_text = match item_state.selected().and_then(|i| filtered.get(i)) {
                    Some(item) => format!(
                        "{}\n{}\n\n{}",
                        item.feed_title,
                        item.title,
                        item.summary.as_deref().unwrap_or("No preview available.")
                    ),
                    None => "No article selected.".to_string(),
                };
                let preview = Paragraph::new(preview_text)
                    .wrap(Wrap { trim: true })
                    .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
                    .block(
                        Block::default()
                            .title(" Preview ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(theme.border)),
                    );
                frame.render_widget(preview, article_area[1]);

                // Help bar
                let help_text = if *active_panel == Panel::Feeds {
                    " [Tab] Switch | [a] Add Feed | [e] Edit Feed | [d] Delete Feed | [f] Favorite | [r] Refresh | [Esc] Back "
                } else {
                    " [Tab] Switch | [Enter] Read | [c] Copy Link | [o] Open Browser | [r] Refresh | [Esc] Back "
                };
                let help = Paragraph::new(help_text)
                    .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(theme.border)),
                    );
                frame.render_widget(help, chunks[1]);
            }
            RssView::Detail { item } => {
                let chunks =
                    Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);

                let block = Block::default()
                    .title(format!(" 📰 {} ", item.feed_title))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border));

                let inner_area = block.inner(chunks[0]);
                frame.render_widget(block, chunks[0]);

                let detail_text = match item.summary.as_deref() {
                    Some(summary) => format!(
                        "Title:\n{}\n\nLink:\n{}\n\nSummary:\n{}\n",
                        item.title, item.link, summary
                    ),
                    None => format!("Title:\n{}\n\nLink:\n{}\n", item.title, item.link),
                };

                let p = Paragraph::new(detail_text)
                    .wrap(Wrap { trim: true })
                    .style(Style::default().fg(theme.base_fg).bg(theme.base_bg));
                frame.render_widget(p, inner_area);

                let help = Paragraph::new(" [c] Copy Link | [o] Open Browser | [Esc/q] Back ")
                    .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(theme.border)),
                    );
                frame.render_widget(help, chunks[1]);
            }
            RssView::EditFeed {
                url_input,
                name_input,
                focus,
                name_is_suggested: _,
                title_fetch_pending,
                editing_index,
            } => {
                let popup_area = centered_rect(60, 20, area);
                frame.render_widget(Clear, popup_area);

                let title = if editing_index.is_some() {
                    " Edit Feed "
                } else {
                    " Add Feed "
                };
                let block = Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.selection_bg));

                let inner_area = block.inner(popup_area);
                frame.render_widget(block, popup_area);

                let layout = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Min(0),
                    Constraint::Length(1),
                ])
                .split(inner_area);

                let field_style = |focused: bool| {
                    if focused {
                        Style::default().fg(theme.selection_fg)
                    } else {
                        Style::default().fg(theme.base_fg)
                    }
                };

                let url_focused = *focus == EditField::Url;
                let url_p = Paragraph::new(format!(
                    "{} URL:  {}",
                    if url_focused { ">" } else { " " },
                    url_input
                ))
                .style(field_style(url_focused));
                frame.render_widget(url_p, layout[0]);

                let name_focused = *focus == EditField::Name;
                let fetching = if *title_fetch_pending {
                    " (fetching...)"
                } else {
                    ""
                };
                let name_p = Paragraph::new(format!(
                    "{} Name: {}{}",
                    if name_focused { ">" } else { " " },
                    name_input,
                    fetching
                ))
                .style(field_style(name_focused));
                frame.render_widget(name_p, layout[1]);

                let help_p = Paragraph::new(" [Tab] Switch field  [Enter] Save  [Esc] Cancel ")
                    .style(
                        Style::default()
                            .fg(theme.base_fg)
                            .add_modifier(Modifier::DIM),
                    );
                frame.render_widget(help_p, layout[3]);
            }
        }
    }

    async fn dashboard_summary(&self, ctx: &AppContext) -> Option<ModuleStatus> {
        let feeds = RssEngine::load_feeds(&ctx.storage).await;
        let items = RssEngine::load_items(&ctx.storage).await;
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

    #[test]
    fn test_apply_edit_adds_new_feed() {
        let mut feeds = vec![sub("https://a.example/feed", None, false)];
        let outcome = apply_edit(
            &mut feeds,
            None,
            "https://b.example/feed".to_string(),
            Some("B".to_string()),
        );
        assert!(matches!(outcome, EditOutcome::Added));
        assert_eq!(feeds.len(), 2);
        assert_eq!(feeds[1].title.as_deref(), Some("B"));
    }

    #[test]
    fn test_apply_edit_updates_existing_feed_at_its_index() {
        let mut feeds = vec![
            sub("https://a.example/feed", None, false),
            sub("https://b.example/feed", Some("B"), false),
        ];
        let outcome = apply_edit(
            &mut feeds,
            Some(1),
            "https://b-new.example/feed".to_string(),
            Some("B Renamed".to_string()),
        );
        assert!(matches!(outcome, EditOutcome::Updated));
        assert_eq!(feeds.len(), 2, "editing must not add a new entry");
        assert_eq!(feeds[1].url, "https://b-new.example/feed");
        assert_eq!(feeds[1].title.as_deref(), Some("B Renamed"));
        assert_eq!(
            feeds[0].url, "https://a.example/feed",
            "other feed untouched"
        );
    }

    #[test]
    fn test_apply_edit_rejects_duplicate_url_against_other_feeds() {
        let mut feeds = vec![
            sub("https://a.example/feed", None, false),
            sub("https://b.example/feed", None, false),
        ];
        let outcome = apply_edit(&mut feeds, None, "https://a.example/feed".to_string(), None);
        assert!(matches!(outcome, EditOutcome::DuplicateUrl));
        assert_eq!(feeds.len(), 2, "nothing should be added on a duplicate");
    }

    #[test]
    fn test_apply_edit_editing_a_feed_with_its_own_unchanged_url_is_not_a_duplicate() {
        let mut feeds = vec![sub("https://a.example/feed", Some("A"), false)];
        let outcome = apply_edit(
            &mut feeds,
            Some(0),
            "https://a.example/feed".to_string(),
            Some("A Renamed".to_string()),
        );
        assert!(matches!(outcome, EditOutcome::Updated));
        assert_eq!(feeds[0].title.as_deref(), Some("A Renamed"));
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
