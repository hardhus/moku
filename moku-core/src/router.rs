use anyhow::Result;
use crossterm::event::Event;
use ratatui::{Frame, layout::Rect};

use crate::context::AppContext;
use crate::module::{ModuleId, TuiRegistry};
use crate::theme::MokuTheme;

/// Routes events/draw calls to the active TUI module.
/// Split-pane support only requires changing internals (e.g., `focused: ModuleId` to `panes: Vec<PaneSlot>` and `active_slot: usize`).
pub struct Router {
    focused: ModuleId,
}

impl Router {
    pub fn new(initial: ModuleId) -> Self {
        Self { focused: initial }
    }

    pub fn focused(&self) -> ModuleId {
        self.focused
    }

    pub fn switch_to(&mut self, id: ModuleId) {
        self.focused = id;
    }

    pub async fn dispatch_event(
        &mut self,
        registry: &mut TuiRegistry,
        event: &Event,
        ctx: &mut AppContext,
    ) -> Result<bool> {
        match registry.get_mut(self.focused) {
            Some(module) => module.handle_event(event, ctx).await,
            None => Ok(false),
        }
    }

    pub fn draw(
        &self,
        registry: &mut TuiRegistry,
        frame: &mut Frame,
        area: Rect,
        theme: &MokuTheme,
    ) {
        if let Some(module) = registry.get_mut(self.focused) {
            module.draw(frame, area, theme);
        }
    }
}
