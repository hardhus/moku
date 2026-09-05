//! `RssView::EditFeed` — the add/edit-feed form. Split out of
//! `tui_module.rs`'s giant `match view` the same way
//! `modules/moku-settings/src/tabs/*.rs` splits one file per tab variant:
//! this view already had its own self-contained state, draw block, and
//! event-handling block.

use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, Clear, ListState, Paragraph},
};

use moku_core::{AppContext, MokuTheme};

use super::{Panel, RssView, domain_of};
use crate::engine::{FeedSubscription, RssEngine};

/// Which field of the URL+name add/edit form currently has keyboard focus.
/// Both fields are shown at once (`Tab` switches focus, `Enter` submits
/// from either) — unlike `modules/moku-secrets/src/tui_module.rs`'s
/// `AddStage`, this isn't a sequential wizard.
#[derive(PartialEq, Clone, Copy)]
pub enum EditField {
    Url,
    Name,
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

/// Handles input while `view` is `RssView::EditFeed`.
pub(super) async fn handle_event(
    view: &mut RssView,
    feeds: &mut Vec<FeedSubscription>,
    title_suggestion: &super::TitleSuggestionSlot,
    event: &Event,
    ctx: &mut AppContext,
) -> Result<bool> {
    let RssView::EditFeed {
        url_input,
        name_input,
        focus,
        name_is_suggested,
        title_fetch_pending,
        editing_index,
    } = view
    else {
        unreachable!("view_edit_feed::handle_event dispatched only when view is RssView::EditFeed");
    };
    let mut changed = false;

    // Apply a background title-suggestion fetch's result, if one just
    // finished for the URL currently in the field and the user hasn't
    // typed a name of their own since it was kicked off (see the Tab
    // handler below).
    let got_suggestion = {
        let mut slot = title_suggestion.lock().unwrap();
        slot.take()
    };
    if let Some((for_url, title)) = got_suggestion
        && for_url == url_input.trim()
    {
        *title_fetch_pending = false;
        if *name_is_suggested && let Some(t) = title {
            *name_input = t;
        }
        changed = true;
    }

    if let Event::Key(key) = event
        && key.kind == KeyEventKind::Press
    {
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
                // Moving from the URL field to the Name field: give an
                // instant domain-based suggestion (no network needed),
                // then try to upgrade it to the feed's real title in the
                // background — same fetch machinery the [r] refresh
                // already uses.
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
                            ctx.show_warning("A feed with this URL already exists.");
                        }
                        EditOutcome::Added => {
                            if let Err(e) =
                                RssEngine::save_feeds(&ctx.storage, &ctx.config.load(), feeds).await
                            {
                                ctx.show_error(format!("Save failed: {}", e));
                            } else {
                                ctx.show_info("Feed added.");
                            }
                        }
                        EditOutcome::Updated => {
                            if let Err(e) =
                                RssEngine::save_feeds(&ctx.storage, &ctx.config.load(), feeds).await
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

    Ok(changed)
}

pub(super) fn draw(view: &mut RssView, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
    let RssView::EditFeed {
        url_input,
        name_input,
        focus,
        name_is_suggested: _,
        title_fetch_pending,
        editing_index,
    } = view
    else {
        unreachable!("view_edit_feed::draw dispatched only when view is RssView::EditFeed");
    };

    let popup_area = super::centered_rect(60, 20, area);
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

    let help_p = Paragraph::new(" [Tab] Switch field  [Enter] Save  [Esc] Cancel ").style(
        Style::default()
            .fg(theme.base_fg)
            .add_modifier(Modifier::DIM),
    );
    frame.render_widget(help_p, layout[3]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(url: &str, title: Option<&str>) -> FeedSubscription {
        FeedSubscription {
            url: url.to_string(),
            title: title.map(str::to_string),
            favorite: false,
        }
    }

    #[test]
    fn test_apply_edit_adds_new_feed() {
        let mut feeds = vec![sub("https://a.example/feed", None)];
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
            sub("https://a.example/feed", None),
            sub("https://b.example/feed", Some("B")),
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
            sub("https://a.example/feed", None),
            sub("https://b.example/feed", None),
        ];
        let outcome = apply_edit(&mut feeds, None, "https://a.example/feed".to_string(), None);
        assert!(matches!(outcome, EditOutcome::DuplicateUrl));
        assert_eq!(feeds.len(), 2, "nothing should be added on a duplicate");
    }

    #[test]
    fn test_apply_edit_editing_a_feed_with_its_own_unchanged_url_is_not_a_duplicate() {
        let mut feeds = vec![sub("https://a.example/feed", Some("A"))];
        let outcome = apply_edit(
            &mut feeds,
            Some(0),
            "https://a.example/feed".to_string(),
            Some("A Renamed".to_string()),
        );
        assert!(matches!(outcome, EditOutcome::Updated));
        assert_eq!(feeds[0].title.as_deref(), Some("A Renamed"));
    }
}
