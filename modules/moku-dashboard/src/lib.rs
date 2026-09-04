use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::Event;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::Style,
    widgets::{Block, Borders, List, ListItem, Paragraph},
};
use serde::Deserialize;

use moku_core::{
    AppContext, Command, ModuleId, ModuleMeta, ModuleStatus, MokuTheme, StatusTone, TuiModule,
    resolve_event,
};

#[derive(Deserialize, Default, Clone)]
struct DashboardKeyConfig {
    #[serde(default)]
    pub keys: HashMap<String, String>,
}

/// A read-only, at-a-glance overview of every other module's status — not
/// a system monitor. Each row comes from that module's own
/// `TuiModule::dashboard_summary`, collected generically by `app_loop`
/// (see `TuiRegistry::collect_dashboard_summaries`) and handed in via
/// `set_summaries`. This module never reaches into another module's
/// storage itself — a new module automatically gains a row here the
/// moment it overrides `dashboard_summary`, with no change needed in this
/// crate.
pub struct DashboardModule {
    statuses: Vec<(ModuleId, ModuleStatus)>,
}

impl DashboardModule {
    pub fn new() -> Self {
        Self {
            statuses: Vec::new(),
        }
    }

    pub fn set_summaries(&mut self, statuses: Vec<(ModuleId, ModuleStatus)>) {
        self.statuses = statuses;
    }
}

impl Default for DashboardModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for DashboardModule {
    fn id(&self) -> ModuleId {
        ModuleId::DASHBOARD
    }

    fn title(&self) -> &'static str {
        ModuleId::DASHBOARD.title()
    }

    fn encrypt_by_default(&self) -> bool {
        false // owns no storage of its own — just displays other modules' summaries
    }
}

#[async_trait]
impl TuiModule for DashboardModule {
    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        let module_config: DashboardKeyConfig = ctx
            .config
            .load()
            .resolve_module_config(ModuleId::DASHBOARD.as_str());
        let command = resolve_event(event, &ctx.config.load().keys, Some(&module_config.keys));

        match command {
            Command::Quit => {
                ctx.quit();
                Ok(true)
            }
            Command::Back => {
                ctx.navigate_to(ModuleId::LAUNCHER);
                Ok(true)
            }
            Command::Refresh => {
                // This module has no way to reach the other modules'
                // storage itself (see the doc comment above) — re-entering
                // itself runs the same summary-collection path app_loop
                // already uses on first entry.
                ctx.navigate_to(ModuleId::DASHBOARD);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(3)])
            .split(area);

        let items: Vec<ListItem> = if self.statuses.is_empty() {
            vec![
                ListItem::new("  No module status available.")
                    .style(Style::default().fg(theme.base_fg)),
            ]
        } else {
            self.statuses
                .iter()
                .map(|(id, status)| {
                    let color = match status.tone {
                        StatusTone::Normal => theme.base_fg,
                        StatusTone::Locked => theme.info,
                        StatusTone::Warning => theme.warning,
                        StatusTone::Error => theme.error,
                    };
                    ListItem::new(format!("  {}: {}", id.title(), status.text))
                        .style(Style::default().fg(color))
                })
                .collect()
        };

        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Dashboard ")
                .title_alignment(Alignment::Center)
                .border_style(Style::default().fg(theme.border))
                .style(Style::default().bg(theme.base_bg)),
        );
        frame.render_widget(list, chunks[0]);

        let footer = Paragraph::new(" [r] Refresh | [Esc] Back | [q] Quit ")
            .style(Style::default().fg(theme.base_fg))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(theme.border)),
            );
        frame.render_widget(footer, chunks[1]);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn rendered_rows(module: &mut DashboardModule) -> Vec<String> {
        let (width, height) = (60u16, 20u16);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = MokuTheme::default();
        terminal
            .draw(|frame| module.draw(frame, Rect::new(0, 0, width, height), &theme))
            .unwrap();
        let content = terminal.backend().buffer().content.clone();
        (0..height as usize)
            .map(|y| {
                content[y * width as usize..(y + 1) * width as usize]
                    .iter()
                    .map(|c| c.symbol())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn test_ram_and_swap_are_never_rendered() {
        let mut module = DashboardModule::new();
        module.set_summaries(vec![(ModuleId::TODO, ModuleStatus::normal("3 tasks"))]);
        let content = rendered_rows(&mut module).join("");
        assert!(!content.contains("RAM"));
        assert!(!content.contains("Swap"));
        assert!(!content.contains("GB"));
    }

    #[test]
    fn test_each_status_renders_as_title_colon_text() {
        let mut module = DashboardModule::new();
        module.set_summaries(vec![
            (ModuleId::TODO, ModuleStatus::normal("3 tasks, 1 done")),
            (ModuleId::SECRETS, ModuleStatus::locked()),
        ]);
        let content = rendered_rows(&mut module).join("");
        assert!(content.contains(&format!("{}: 3 tasks, 1 done", ModuleId::TODO.title())));
        assert!(content.contains(&format!("{}: Locked", ModuleId::SECRETS.title())));
    }

    #[test]
    fn test_row_color_matches_its_tone() {
        let mut module = DashboardModule::new();
        module.set_summaries(vec![
            (ModuleId::TODO, ModuleStatus::normal("ok")),
            (ModuleId::SECRETS, ModuleStatus::locked()),
            (ModuleId::DAEMON, ModuleStatus::warning("Stopped")),
        ]);
        let theme = MokuTheme::default();
        let (width, height) = (60u16, 20u16);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| module.draw(frame, Rect::new(0, 0, width, height), &theme))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();

        let find_row_fg = |needle: &str| -> ratatui::style::Color {
            for y in 0..height {
                let row: String = (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect();
                if row.contains(needle) {
                    return buffer[(1, y)].fg;
                }
            }
            panic!("row containing {needle:?} not found");
        };

        assert_eq!(find_row_fg("ok"), theme.base_fg);
        assert_eq!(find_row_fg("Locked"), theme.info);
        assert_eq!(find_row_fg("Stopped"), theme.warning);
    }
}
