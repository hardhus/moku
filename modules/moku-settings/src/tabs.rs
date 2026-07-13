use anyhow::Result;
use crossterm::event::Event;
use ratatui::{Frame, layout::Rect};

pub mod context;
pub mod general;
pub mod keybindings;

use moku_core::{AppContext, MokuConfig};

pub trait SettingsTab {
    fn title(&self) -> &str;

    fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<()>;

    fn draw(&mut self, frame: &mut Frame, area: Rect, config: &MokuConfig) -> Result<()>;
}
