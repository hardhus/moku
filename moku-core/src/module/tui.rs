use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::Event;
use ratatui::{Frame, layout::Rect};

use crate::context::AppContext;
use crate::module::ModuleMeta;
use crate::theme::MokuTheme;

/// A visible TUI module that handles input and renders on screen.
#[async_trait]
pub trait TuiModule: ModuleMeta {
    /// Called when the module gains focus.
    async fn init(&mut self, _ctx: &mut AppContext) -> Result<()> {
        Ok(())
    }

    /// Handles input events. Returns true if redrawing is needed.
    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool>;

    /// Renders the module interface within the given area.
    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme);
}
