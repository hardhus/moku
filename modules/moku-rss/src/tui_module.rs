use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

pub enum RssView {
    Split {
        active_panel: Panel,
        feed_state: ListState,
        item_state: ListState,
    },
    Detail {
        item: FeedItem,
    },
    AddFeed {
        input: String,
    },
}

pub struct RssTuiModule {
    feeds: Vec<FeedSubscription>,
    items: Vec<FeedItem>,
    view: RssView,
    refresh_result: Arc<Mutex<Option<Result<Vec<FeedItem>, String>>>>,
    is_refreshing: bool,
    status_message: Option<(String, Instant)>,
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
            status_message: None,
        }
    }

    fn show_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some((msg.into(), Instant::now()));
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
                    self.show_status("Feeds refreshed successfully.");
                }
                Err(e) => {
                    self.show_status(format!("Refresh error: {}", e));
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
            status_message,
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
                                    *status_message =
                                        Some(("Refreshing feeds...".to_string(), Instant::now()));
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
                                *view = RssView::AddFeed {
                                    input: String::new(),
                                };
                                changed = true;
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
                                                *status_message = Some((
                                                    format!("Save error: {}", e),
                                                    Instant::now(),
                                                ));
                                            } else {
                                                let msg = if feeds[i - 1].favorite {
                                                    "Added to favorites ★"
                                                } else {
                                                    "Removed from favorites"
                                                };
                                                *status_message =
                                                    Some((msg.to_string(), Instant::now()));
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
                                                *status_message = Some((
                                                    format!("Delete error: {}", e),
                                                    Instant::now(),
                                                ));
                                            } else {
                                                *status_message = Some((
                                                    format!("Removed: {}", removed.url),
                                                    Instant::now(),
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
                                                Ok(_) => {
                                                    *status_message = Some((
                                                        format!("Copied: {}", link),
                                                        Instant::now(),
                                                    ))
                                                }
                                                Err(e) => {
                                                    *status_message = Some((
                                                        format!("Clipboard error: {}", e),
                                                        Instant::now(),
                                                    ))
                                                }
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
                                            *status_message = Some((
                                                "Opening in browser...".to_string(),
                                                Instant::now(),
                                            ));
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
                                    Ok(_) => {
                                        *status_message =
                                            Some((format!("Copied: {}", item.link), Instant::now()))
                                    }
                                    Err(e) => {
                                        *status_message = Some((
                                            format!("Clipboard error: {}", e),
                                            Instant::now(),
                                        ))
                                    }
                                }
                                changed = true;
                            }
                            KeyCode::Char('o') => {
                                let _ = moku_core::util::open_url(&item.link);
                                *status_message =
                                    Some(("Opening in browser...".to_string(), Instant::now()));
                                changed = true;
                            }
                            _ => {}
                        }
                    }
                }
            }
            RssView::AddFeed { input } => {
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
                            KeyCode::Backspace => {
                                input.pop();
                                changed = true;
                            }
                            KeyCode::Enter => {
                                let url = input.trim().to_string();
                                if !url.is_empty() {
                                    if !feeds.iter().any(|f| f.url == url) {
                                        feeds.push(FeedSubscription {
                                            url,
                                            title: None,
                                            favorite: false,
                                        });
                                        if let Err(e) = RssEngine::save_feeds(
                                            &ctx.storage,
                                            &ctx.config.load(),
                                            feeds,
                                        )
                                        .await
                                        {
                                            *status_message = Some((
                                                format!("Save failed: {}", e),
                                                Instant::now(),
                                            ));
                                        } else {
                                            *status_message = Some((
                                                "Feed added successfully.".to_string(),
                                                Instant::now(),
                                            ));
                                        }
                                    } else {
                                        *status_message = Some((
                                            "Feed already exists.".to_string(),
                                            Instant::now(),
                                        ));
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
                                changed = true;
                            }
                            KeyCode::Char(c) => {
                                input.push(c);
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
                self.show_status("Feeds refreshed successfully.");
            }
        }

        // Clean status message if expired
        if let Some((_, time)) = self.status_message {
            if time.elapsed() > Duration::from_secs(3) {
                self.status_message = None;
            }
        }

        // Destructure self to allow disjoint field accesses
        let RssTuiModule {
            feeds,
            items,
            view,
            refresh_result: _,
            is_refreshing,
            status_message,
        } = self;

        match view {
            RssView::Split {
                active_panel,
                feed_state,
                item_state,
            } => {
                let chunks = Layout::vertical([
                    Constraint::Min(0),
                    Constraint::Length(1), // status message
                    Constraint::Length(3), // help
                ])
                .split(area);

                let panels =
                    Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)])
                        .split(chunks[0]);

                // Feeds panel
                let mut feed_items = vec![ListItem::new(" * All Feeds")];
                for f in feeds.iter() {
                    let label = f.title.as_deref().unwrap_or(&f.url);
                    let star = if f.favorite { " ★" } else { "" };
                    feed_items.push(ListItem::new(format!(" • {}{}", label, star)));
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

                // Articles panel
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

                frame.render_stateful_widget(item_list, panels[1], item_state);

                // Status message
                if let Some((msg, _)) = status_message {
                    let msg_p = Paragraph::new(format!(" {}", msg)).style(
                        Style::default()
                            .fg(theme.selection_fg)
                            .add_modifier(Modifier::ITALIC),
                    );
                    frame.render_widget(msg_p, chunks[1]);
                }

                // Help bar
                let help_text = if *active_panel == Panel::Feeds {
                    " [Tab] Switch | [a] Add Feed | [d] Delete Feed | [f] Favorite | [r] Refresh | [Esc] Back "
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
                frame.render_widget(help, chunks[2]);
            }
            RssView::Detail { item } => {
                let chunks = Layout::vertical([
                    Constraint::Min(0),
                    Constraint::Length(1), // status message
                    Constraint::Length(3), // help
                ])
                .split(area);

                let block = Block::default()
                    .title(format!(" 📰 {} ", item.feed_title))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border));

                let inner_area = block.inner(chunks[0]);
                frame.render_widget(block, chunks[0]);

                let detail_text = format!("Title:\n{}\n\nLink:\n{}\n", item.title, item.link);

                let p = Paragraph::new(detail_text)
                    .wrap(Wrap { trim: true })
                    .style(Style::default().fg(theme.base_fg).bg(theme.base_bg));
                frame.render_widget(p, inner_area);

                // Status message
                if let Some((msg, _)) = status_message {
                    let msg_p = Paragraph::new(format!(" {}", msg)).style(
                        Style::default()
                            .fg(theme.selection_fg)
                            .add_modifier(Modifier::ITALIC),
                    );
                    frame.render_widget(msg_p, chunks[1]);
                }

                let help = Paragraph::new(" [c] Copy Link | [o] Open Browser | [Esc/q] Back ")
                    .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(theme.border)),
                    );
                frame.render_widget(help, chunks[2]);
            }
            RssView::AddFeed { input } => {
                let popup_area = centered_rect(60, 20, area);
                frame.render_widget(Clear, popup_area);

                let block = Block::default()
                    .title(" 📡 Add New RSS Feed URL ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.selection_bg));

                let inner_area = block.inner(popup_area);
                frame.render_widget(block, popup_area);

                let layout = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Min(0),
                    Constraint::Length(1),
                ])
                .split(inner_area);

                let input_p = Paragraph::new(format!("> {}", input))
                    .style(Style::default().fg(theme.base_fg));
                frame.render_widget(input_p, layout[0]);

                let help_p = Paragraph::new(" [Enter] Save | [Esc] Cancel ").style(
                    Style::default()
                        .fg(theme.base_fg)
                        .add_modifier(Modifier::DIM),
                );
                frame.render_widget(help_p, layout[2]);
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
