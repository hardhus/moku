//! `RssView::Detail` (a single article's full text) and
//! `RssView::ConfirmDeleteFeed` (the delete-confirmation prompt) — small
//! enough to share one file. Split out of `tui_module.rs`'s giant
//! `match view` the same way `modules/moku-settings/src/tabs/*.rs` splits
//! one file per tab variant.

use anyhow::Result;
use arboard::Clipboard;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, Clear, ListState, Paragraph, Wrap},
};

use moku_core::{AppContext, MokuTheme};

use super::{Panel, RssView, centered_rect, delete_feed_at, feed_label};
use crate::engine::FeedSubscription;

/// Handles input while `view` is `RssView::Detail`.
pub(super) async fn handle_detail_event(
    view: &mut RssView,
    event: &Event,
    ctx: &mut AppContext,
) -> Result<bool> {
    let RssView::Detail { item } = view else {
        unreachable!(
            "view_detail::handle_detail_event dispatched only when view is RssView::Detail"
        );
    };
    let mut changed = false;

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
                    match Clipboard::new().and_then(|mut c| c.set_text(item.link.clone())) {
                        Ok(_) => ctx.show_info(format!("Copied: {}", item.link)),
                        Err(e) => ctx.show_error(format!("Clipboard error: {}", e)),
                    }
                    changed = true;
                }
                KeyCode::Char('o') => {
                    match moku_core::util::open_url(&item.link) {
                        Ok(_) => ctx.show_info("Opening in browser..."),
                        Err(e) => ctx.show_error(format!("Failed to open: {}", e)),
                    }
                    changed = true;
                }
                _ => {}
            }
        }
    }

    Ok(changed)
}

pub(super) fn draw_detail(view: &mut RssView, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
    let RssView::Detail { item } = view else {
        unreachable!("view_detail::draw_detail dispatched only when view is RssView::Detail");
    };

    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);

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

/// Handles input while `view` is `RssView::ConfirmDeleteFeed`.
pub(super) async fn handle_confirm_delete_event(
    view: &mut RssView,
    feeds: &mut Vec<FeedSubscription>,
    event: &Event,
    ctx: &mut AppContext,
) -> Result<bool> {
    let RssView::ConfirmDeleteFeed { index } = view else {
        unreachable!(
            "view_detail::handle_confirm_delete_event dispatched only when view is RssView::ConfirmDeleteFeed"
        );
    };
    let mut changed = false;

    match moku_core::resolve_confirm_delete_key(event) {
        moku_core::ConfirmDeleteKey::Confirm => {
            let mut feed_state = ListState::default();
            delete_feed_at(feeds, &mut feed_state, *index, ctx).await;
            let mut item_state = ListState::default();
            item_state.select(Some(0));
            *view = RssView::Split {
                active_panel: Panel::Feeds,
                feed_state,
                item_state,
            };
            changed = true;
        }
        moku_core::ConfirmDeleteKey::Cancel => {
            let mut feed_state = ListState::default();
            feed_state.select(Some(*index + 1));
            let mut item_state = ListState::default();
            item_state.select(Some(0));
            *view = RssView::Split {
                active_panel: Panel::Feeds,
                feed_state,
                item_state,
            };
            changed = true;
        }
        moku_core::ConfirmDeleteKey::Other => {}
    }

    Ok(changed)
}

pub(super) fn draw_confirm_delete(
    view: &mut RssView,
    feeds: &[FeedSubscription],
    frame: &mut Frame,
    area: Rect,
    theme: &MokuTheme,
) {
    let RssView::ConfirmDeleteFeed { index } = view else {
        unreachable!(
            "view_detail::draw_confirm_delete dispatched only when view is RssView::ConfirmDeleteFeed"
        );
    };

    let popup_area = centered_rect(60, 20, area);
    frame.render_widget(Clear, popup_area);

    let label = feeds
        .get(*index)
        .map(feed_label)
        .unwrap_or_else(|| "this feed".to_string());

    let block = Block::default()
        .title(" Confirm Delete ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.error));

    let inner_area = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let layout = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(inner_area);

    let message =
        Paragraph::new(format!("Delete '{label}'?")).style(Style::default().fg(theme.base_fg));
    frame.render_widget(message, layout[0]);

    let help_p = Paragraph::new(" [y] Yes  [n] No ").style(
        Style::default()
            .fg(theme.base_fg)
            .add_modifier(Modifier::DIM),
    );
    frame.render_widget(help_p, layout[2]);
}
