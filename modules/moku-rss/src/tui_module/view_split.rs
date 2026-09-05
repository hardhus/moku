//! `RssView::Split` — the main two-panel feed/article browser. Split out
//! of `tui_module.rs`'s giant `match view` the same way
//! `modules/moku-settings/src/tabs/*.rs` splits one file per tab variant.

use std::sync::Arc;

use anyhow::Result;
use arboard::Clipboard;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};

use moku_core::{AppContext, Command, ModuleId, MokuTheme};

use super::view_edit_feed::EditField;
use super::{Panel, RssView, delete_feed_at, feed_label};
use crate::engine::{FeedItem, FeedSubscription, RssEngine};

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

/// Handles input while `view` is `RssView::Split`. Takes each field it
/// needs as an explicit parameter (rather than `&mut RssTuiModule`) so the
/// caller can destructure `self` into disjoint borrows first — same
/// reason the count is over clippy's default threshold.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_event(
    view: &mut RssView,
    feeds: &mut Vec<FeedSubscription>,
    items: &mut Vec<FeedItem>,
    refresh_result: &super::RefreshResultSlot,
    is_refreshing: &mut bool,
    event: &Event,
    ctx: &mut AppContext,
    command: Command,
) -> Result<bool> {
    let RssView::Split {
        active_panel,
        feed_state,
        item_state,
    } = view
    else {
        unreachable!("view_split::handle_event dispatched only when view is RssView::Split");
    };
    let mut changed = false;

    // Shift+D bypasses the confirmation prompt entirely and deletes
    // immediately — checked as a raw key before the normal dispatch, same
    // shape as other raw Shift-key checks in this app.
    if *active_panel == Panel::Feeds
        && moku_core::is_delete_bypass(event)
        && let Some(i) = feed_state.selected()
        && i > 0
        && i - 1 < feeds.len()
    {
        delete_feed_at(feeds, feed_state, i - 1, ctx).await;
        return Ok(true);
    }

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
                            let res = RssEngine::fetch_all(&storage, &config.load()).await;
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
                                if let Err(e) =
                                    RssEngine::save_feeds(&ctx.storage, &ctx.config.load(), feeds)
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
                                *view = RssView::ConfirmDeleteFeed { index: i - 1 };
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
                                    Err(e) => ctx.show_error(format!("Clipboard error: {}", e)),
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
                                match moku_core::util::open_url(&filtered[i].link) {
                                    Ok(_) => ctx.show_info("Opening in browser..."),
                                    Err(e) => ctx.show_error(format!("Failed to open: {}", e)),
                                }
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

    Ok(changed)
}

pub(super) fn draw(
    view: &mut RssView,
    feeds: &[FeedSubscription],
    items: &[FeedItem],
    is_refreshing: bool,
    frame: &mut Frame,
    area: Rect,
    theme: &MokuTheme,
) {
    let RssView::Split {
        active_panel,
        feed_state,
        item_state,
    } = view
    else {
        unreachable!("view_split::draw dispatched only when view is RssView::Split");
    };

    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);

    let panels = Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(chunks[0]);

    // Feeds panel — favorite marker goes at the FRONT of the line, not
    // appended at the end: a long label (or a narrow panel) means
    // trailing content gets clipped, so a trailing star was effectively
    // never visible.
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

    // Articles column: list on top, preview of the selected article below.
    let article_area =
        Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)]).split(panels[1]);

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
    if is_refreshing {
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
