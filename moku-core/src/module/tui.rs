use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::Event;
use ratatui::{Frame, layout::Rect};

use crate::context::AppContext;
use crate::module::{ModuleMeta, ModuleStatus};
use crate::theme::MokuTheme;

/// Blanket-implemented for every `'static` type, so any `TuiModule` impl
/// gets this for free — lets `app_loop` recover a module's concrete type
/// from a type-erased `Box<dyn TuiModule>` (used to hand collected
/// summaries to the Dashboard module) without every module writing this
/// boilerplate itself.
pub trait AsAny: 'static {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

impl<T: 'static> AsAny for T {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// A visible TUI module that handles input and renders on screen.
#[async_trait]
pub trait TuiModule: ModuleMeta + AsAny {
    /// Called when the module gains focus.
    async fn init(&mut self, _ctx: &mut AppContext) -> Result<()> {
        Ok(())
    }

    /// Handles input events. Returns true if redrawing is needed.
    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool>;

    /// Renders the module interface within the given area.
    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme);

    /// A short status line for the Dashboard's overview panel. `None` means
    /// this module doesn't report one and won't appear there. Must not
    /// mutate any state — only ever called to read a fresh summary,
    /// including for modules the user hasn't opened this session.
    async fn dashboard_summary(&self, _ctx: &AppContext) -> Option<ModuleStatus> {
        None
    }

    /// How often this module wants a poll/redraw tick while it is the
    /// *focused* module — `None` (the default, and what every module gets
    /// for free) means never. `app_loop` only ever runs a tick timer for
    /// the currently-focused module's own returned interval, so an idle
    /// module (the common case) costs nothing extra: no wakeup, no
    /// redraw. A module that needs one (e.g. a live countdown) should
    /// return `Some` only while something is actually advancing —
    /// returning `Some` unconditionally would defeat the point.
    fn tick_interval(&self) -> Option<Duration> {
        None
    }

    /// Called on each tick this module asked for via `tick_interval`.
    /// Returns whether a redraw is actually needed (e.g. `false` if the
    /// displayed value hasn't visibly changed since the last tick).
    async fn on_tick(&mut self, _ctx: &mut AppContext) -> Result<bool> {
        Ok(false)
    }
}
