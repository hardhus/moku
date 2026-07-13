use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::Event;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState},
};
use serde::Deserialize;

use moku_core::{AppContext, Command, ModuleId, ModuleMeta, MokuTheme, TuiModule, resolve_event};

#[derive(Deserialize, Default)]
struct LauncherKeyConfig {
    pub keys: HashMap<String, String>,
}

pub struct LauncherModule {
    registered_modules: Vec<ModuleId>,
    state: ListState,
}

impl LauncherModule {
    pub fn new() -> Self {
        let modules = ModuleId::all_visible();
        let mut state = ListState::default();
        if !modules.is_empty() {
            state.select(Some(0));
        }
        Self {
            registered_modules: modules,
            state,
        }
    }

    fn next(&mut self) -> bool {
        if self.registered_modules.is_empty() {
            return false;
        }
        let i = match self.state.selected() {
            Some(i) => {
                if i >= self.registered_modules.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
        true
    }

    fn previous(&mut self) -> bool {
        if self.registered_modules.is_empty() {
            return false;
        }
        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    self.registered_modules.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
        true
    }
}

impl Default for LauncherModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for LauncherModule {
    fn id(&self) -> ModuleId {
        ModuleId::LAUNCHER
    }
    fn title(&self) -> &'static str {
        ModuleId::LAUNCHER.title()
    }
}

#[async_trait]
impl TuiModule for LauncherModule {
    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        let module_config: LauncherKeyConfig = ctx
            .config
            .load()
            .resolve_module_config(ModuleId::LAUNCHER.as_str());
        let command = resolve_event(event, &ctx.config.load().keys, Some(&module_config.keys));

        let changed = match command {
            Command::Quit | Command::Back => {
                ctx.quit();
                true
            }
            Command::Down => self.next(),
            Command::Up => self.previous(),
            Command::Confirm => {
                if let Some(index) = self.state.selected() {
                    let module_id = self.registered_modules[index];
                    ctx.navigate_to(module_id);
                }
                true
            }
            _ => false,
        };
        Ok(changed)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let area = centered_rect(60, 50, area);

        let items: Vec<ListItem> = self
            .registered_modules
            .iter()
            .map(|id| ListItem::new(format!("  {}", id.title())))
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .title(" 🚀 Moku Launcher ")
                    .title_alignment(ratatui::layout::Alignment::Center)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border))
                    .style(Style::default().bg(theme.base_bg)),
            )
            .style(Style::default().fg(theme.base_fg))
            .highlight_style(
                Style::default()
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">> ");

        frame.render_stateful_widget(list, area, &mut self.state);
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_launcher_selection_cycle() {
        let mut launcher = LauncherModule::new();
        let initial_len = launcher.registered_modules.len();
        assert!(
            initial_len > 0,
            "At least one module must be visible for testing."
        );

        launcher.state.select(Some(initial_len - 1));
        launcher.next();
        assert_eq!(launcher.state.selected(), Some(0));

        launcher.previous();
        assert_eq!(launcher.state.selected(), Some(initial_len - 1));
    }
}
