use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Borders, Paragraph},
};

use moku_core::MokuTheme;

use crate::model::{MODE_INPUT, MODE_SEARCH};

/// Customized input field for the Bookmark module.
/// Dynamically adjusts its visual style based on the active mode (Search, Add, Filter).
pub fn draw_input(frame: &mut Frame, area: Rect, theme: &MokuTheme, buffer: &str, mode_name: &str) {
    let border_color = if mode_name == MODE_SEARCH {
        theme.info
    } else if mode_name == MODE_INPUT {
        theme.warning
    } else {
        theme.border
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", mode_name))
        .border_style(
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(theme.base_bg));

    let display_text = if buffer.is_empty() && mode_name == MODE_SEARCH {
        "Start typing...".to_string()
    } else {
        format!("{}_", buffer)
    };

    let text_style = if buffer.is_empty() && mode_name == MODE_SEARCH {
        Style::default()
            .fg(theme.border)
            .add_modifier(Modifier::ITALIC)
    } else {
        Style::default().fg(theme.base_fg)
    };

    let paragraph = Paragraph::new(display_text).block(block).style(text_style);

    frame.render_widget(paragraph, area);
}
