use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
};

use moku_core::MokuTheme;

use crate::model::Bookmark;

pub fn draw_list(
    frame: &mut Frame,
    area: Rect,
    theme: &MokuTheme,
    items: &[Bookmark],
    state: &mut ListState,
    title: &str,
) {
    let list_items: Vec<ListItem> = items
        .iter()
        .map(|b| {
            let domain = b
                .url
                .replace("https://", "")
                .replace("http://", "")
                .replace("www.", "");
            let domain = domain.split('/').next().unwrap_or(&domain);

            let mut line_spans = vec![Span::styled("🔗 ", Style::default().fg(theme.info))];

            if let Some(ref name) = b.name {
                line_spans.push(Span::styled(
                    format!("{:<20} ", name),
                    Style::default()
                        .fg(theme.base_fg)
                        .add_modifier(Modifier::BOLD),
                ));
                line_spans.push(Span::styled(
                    format!("[{}]", domain),
                    Style::default().fg(theme.border),
                ));
            } else {
                line_spans.push(Span::styled(&b.url, Style::default().fg(theme.base_fg)));
            }

            ListItem::new(Line::from(line_spans))
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", title))
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.base_bg));

    let list = List::new(list_items)
        .block(block)
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(theme.selection_fg)
                .bg(theme.selection_bg),
        )
        .highlight_symbol(">> ");

    frame.render_stateful_widget(list, area, state);
}
