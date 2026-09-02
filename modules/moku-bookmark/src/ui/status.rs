use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use moku_core::MokuTheme;

use crate::model::{MODE_DOMAIN_FILTER_PREFIX, MODE_INPUT, MODE_SEARCH};

/// Renders the status bar at the bottom based on the current application state.
pub fn draw_status_bar(frame: &mut Frame, area: Rect, theme: &MokuTheme, mode_name: &str) {
    // Select colors and help text based on the current mode
    let (mode_bg, help_text) = if mode_name == MODE_SEARCH {
        (
            theme.warning,
            " [/] Type | [Enter] Apply Filter | [Esc] Exit Search ",
        )
    } else if mode_name == MODE_INPUT {
        (
            theme.info,
            " [Enter] Save | [Esc] Cancel | URL format is required ",
        )
    } else if mode_name.starts_with(MODE_DOMAIN_FILTER_PREFIX) {
        (
            theme.warning,
            " [r] Reset Filter | [Esc] Normal Mode | [c] Copy ",
        )
    } else {
        (
            theme.selection_bg,
            " [j/k] Navigate | [/] Search | [a] Add | [p] Paste | [c] Copy | [e/i] Exp/Imp | [x] Clear | [f] Domain ",
        )
    };

    // Construct the status bar using styled spans
    let spans = vec![
        // Left side: Mode label
        Span::styled(
            format!(" {} ", mode_name),
            Style::default()
                .bg(mode_bg)
                .fg(theme.base_bg)
                .add_modifier(Modifier::BOLD),
        ),
        // Spacer
        Span::raw(" "),
        // Right side: Shortcut help text
        Span::styled(help_text, Style::default().fg(theme.base_fg)),
    ];

    let p = Paragraph::new(Line::from(spans))
        .alignment(Alignment::Left)
        .style(Style::default().bg(theme.base_bg));

    frame.render_widget(p, area);
}
