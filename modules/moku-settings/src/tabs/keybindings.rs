use anyhow::Result;
use crossterm::event::Event;
use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Borders, Paragraph},
};

use moku_core::{AppContext, MokuConfig};

use super::SettingsTab;

pub struct KeybindingsTab;

impl KeybindingsTab {
    pub fn new() -> Self {
        Self
    }
}

impl SettingsTab for KeybindingsTab {
    fn title(&self) -> &str {
        "Keys (Coming Soon)"
    }

    fn handle_event(&mut self, _event: &Event, _ctx: &mut AppContext) -> Result<()> {
        // No interaction yet
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, config: &MokuConfig) -> Result<()> {
        let theme = config.get_active_theme();

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ratatui::style::Style::default().fg(theme.border))
            .title(" Keybindings ");

        let p = Paragraph::new("Key binding configuration will be here.")
            .block(block)
            .style(ratatui::style::Style::default().fg(theme.base_fg));

        frame.render_widget(p, area);
        Ok(())
    }
}
