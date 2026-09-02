use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, Gauge, Paragraph},
};
use serde::Deserialize;
use sysinfo::{MemoryRefreshKind, RefreshKind, System};

use moku_core::{AppContext, Command, ModuleId, ModuleMeta, MokuTheme, TuiModule, resolve_event};

#[derive(Deserialize, Default)]
struct SimpleTodo {
    completed: bool,
}

#[derive(Deserialize, Default, Clone)]
struct DashboardKeyConfig {
    #[serde(default)]
    pub keys: HashMap<String, String>,
}

pub struct DashboardModule {
    sys: System,
    todo_count: usize,
    completed_count: usize,
}

impl DashboardModule {
    pub fn new() -> Self {
        let sys = System::new_with_specifics(
            RefreshKind::nothing().with_memory(MemoryRefreshKind::everything()),
        );

        Self {
            sys,
            todo_count: 0,
            completed_count: 0,
        }
    }

    async fn refresh_data(&mut self, ctx: &AppContext) {
        self.sys.refresh_memory();

        let todo_data: Result<Vec<SimpleTodo>> =
            ctx.storage.load(ModuleId::TODO.as_str(), "items").await;

        match todo_data {
            Ok(todos) => {
                self.todo_count = todos.len();
                self.completed_count = todos.iter().filter(|t| t.completed).count();
            }
            Err(_) => {
                self.todo_count = 0;
                self.completed_count = 0;
            }
        }
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
        false // read-only system stats, owns no storage
    }
}

#[async_trait]
impl TuiModule for DashboardModule {
    async fn init(&mut self, ctx: &mut AppContext) -> Result<()> {
        self.refresh_data(ctx).await;
        Ok(())
    }

    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        let mut changed = false;

        let module_config: DashboardKeyConfig = ctx
            .config
            .load()
            .resolve_module_config(ModuleId::DASHBOARD.as_str());

        let overrides = Some(&module_config.keys);

        let command = resolve_event(event, &ctx.config.load().keys, overrides);

        match command {
            Command::Quit => {
                ctx.quit();
                return Ok(true);
            }
            Command::Back => {
                ctx.navigate_to(ModuleId::LAUNCHER);
                return Ok(true);
            }
            Command::Refresh => {
                self.refresh_data(ctx).await;
                changed = true;
            }
            _ => {}
        }

        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
                if key.code == KeyCode::Char('r') {
                    self.refresh_data(ctx).await;
                    ctx.show_info("Dashboard Updated 🔄");
                    changed = true;
                }
            }
        }

        Ok(changed)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6), // Todo Info
                Constraint::Length(3), // RAM
                Constraint::Length(3), // Swap
                Constraint::Min(0),    // Spacer
                Constraint::Length(3), // Footer
            ])
            .split(area);

        let stats_text = format!(
            "Total Tasks:  {}\nCompleted:    {}\nTo do:      {}",
            self.todo_count,
            self.completed_count,
            self.todo_count.saturating_sub(self.completed_count)
        );

        let stats_block = Paragraph::new(stats_text)
            .block(
                Block::default()
                    .title(" 📝 Moku Summary ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border))
                    .style(Style::default().bg(theme.base_bg)),
            )
            .style(Style::default().fg(theme.info).add_modifier(Modifier::BOLD));

        frame.render_widget(stats_block, chunks[0]);

        // RAM and Swap Gauge renders...
        let total_mem = self.sys.total_memory() as f64;
        let used_mem = self.sys.used_memory() as f64;
        let mem_percent = if total_mem > 0.0 {
            (used_mem / total_mem * 100.0) as u16
        } else {
            0
        };

        let gauge = Gauge::default()
            .block(
                Block::default()
                    .title(" 🧠 RAM Usage ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border)),
            )
            .gauge_style(Style::default().fg(theme.warning))
            .percent(mem_percent)
            .label(format!(
                "{:.1} GB / {:.1} GB",
                used_mem / 1024.0 / 1024.0 / 1024.0,
                total_mem / 1024.0 / 1024.0 / 1024.0
            ));

        frame.render_widget(gauge, chunks[1]);

        let total_swap = self.sys.total_swap() as f64;
        let used_swap = self.sys.used_swap() as f64;
        let swap_percent = if total_swap > 0.0 {
            (used_swap / total_swap * 100.0) as u16
        } else {
            0
        };

        let swap_gauge = Gauge::default()
            .block(
                Block::default()
                    .title(" 💾 Swap Usage ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border)),
            )
            .gauge_style(Style::default().fg(theme.error))
            .percent(swap_percent);

        frame.render_widget(swap_gauge, chunks[2]);

        let footer = Paragraph::new("Refresh: [r] | Menu: [ESC]")
            .style(Style::default().fg(theme.base_fg))
            .alignment(ratatui::layout::Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(theme.border)),
            );

        frame.render_widget(footer, chunks[4]);
    }
}
