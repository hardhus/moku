//! Small drawing helpers shared by several modules, so the same UI state
//! looks the same everywhere.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::MokuTheme;

/// The persistent banner a module shows while it is read-only because its
/// stored data failed to load (see `StorageManager::load_opt`). Drawn in
/// the theme's error color, above the module's normal content. `area`
/// should be at least 3 rows tall.
pub fn draw_load_error_banner(frame: &mut Frame, area: Rect, theme: &MokuTheme, reason: &str) {
    let text = format!("READ-ONLY, nothing will be saved: {reason}");
    let banner = Paragraph::new(text)
        .style(
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD),
        )
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Load failed ")
                .border_style(Style::default().fg(theme.error)),
        );
    frame.render_widget(banner, area);
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    #[test]
    fn test_load_error_banner_renders_read_only_notice_and_reason() {
        let backend = TestBackend::new(80, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_load_error_banner(frame, Rect::new(0, 0, 80, 4), &MokuTheme::default(), "boom")
            })
            .unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains("Load failed"));
        assert!(content.contains("READ-ONLY"));
    }
}
