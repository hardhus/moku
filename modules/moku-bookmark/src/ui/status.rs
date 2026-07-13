use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use moku_core::MokuTheme;

/// Renders the status bar at the bottom based on the current application state.
pub fn draw_status_bar(frame: &mut Frame, area: Rect, theme: &MokuTheme, mode_name: &str) {
    // Select colors and help text based on the current mode
    let (mode_bg, help_text) = match mode_name {
        "SEARCH" => (
            theme.warning,
            " [/] Type | [ENTER] Apply Filter | [ESC] Exit Search ",
        ),
        "INPUT" => (
            theme.info,
            " [ENTER] Save | [ESC] Cancel | URL format is required ",
        ),
        mode if mode.starts_with("DOMAIN_FILTER") => (
            Color::Magenta,
            " [r] Reset Filter | [ESC] Normal Mode | [c] Copy ",
        ),
        _ => (
            theme.selection_bg,
            " [j/k] Navigate | [/] Search | [a] Add | [p] Paste | [c] Copy | [e/i] Exp/Imp | [x] Clear | [f] Domain ",
        ),
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
