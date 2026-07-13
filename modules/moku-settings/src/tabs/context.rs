use std::collections::HashMap;

use anyhow::Result;
use crossterm::event::Event;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState},
};
use serde::{Deserialize, Serialize};

use moku_core::{AppContext, Command, MokuConfig, resolve_event};

use super::SettingsTab;

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ContextModuleSettings {
    pub use_gitignore: bool,
    pub char_limit: usize,
}

impl Default for ContextModuleSettings {
    fn default() -> Self {
        Self {
            use_gitignore: false,
            char_limit: 5000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextOption {
    GitIgnore,
    CommitLimit,
}

impl ContextOption {
    fn label(&self) -> &'static str {
        match self {
            Self::GitIgnore => "Git Ignore",
            Self::CommitLimit => "Commit Limit",
        }
    }
    fn all() -> Vec<Self> {
        vec![Self::GitIgnore, Self::CommitLimit]
    }
}

pub struct ContextTab {
    state: ListState,
    options: Vec<ContextOption>,
    key_cache: HashMap<String, String>,
}

impl ContextTab {
    pub fn new(config: &MokuConfig) -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        let keys = config.get_module_keys("settings").unwrap_or_default();

        Self {
            state,
            options: ContextOption::all(),
            key_cache: keys,
        }
    }

    fn select_next(&mut self) {
        let i = match self.state.selected() {
            Some(i) => (i + 1) % self.options.len(),
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn select_previous(&mut self) {
        let i = match self.state.selected() {
            Some(i) => (i + self.options.len() - 1) % self.options.len(),
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn update_config_value(&self, ctx: &mut AppContext, settings: ContextModuleSettings) {
        if let Ok(val) = toml::Value::try_from(settings) {
            ctx.update_config(|cfg| {
                cfg.modules.insert("context".to_string(), val);
            });
        }
    }

    fn change_value(&mut self, ctx: &mut AppContext, direction: i32) {
        if let Some(i) = self.state.selected() {
            let selected_option = self.options[i];

            let mut settings: ContextModuleSettings =
                ctx.config.load().resolve_module_config("context");

            match selected_option {
                ContextOption::GitIgnore => {
                    settings.use_gitignore = !settings.use_gitignore;
                    let status = if settings.use_gitignore {
                        "Enabled"
                    } else {
                        "Disabled"
                    };
                    ctx.show_info(format!("GitIgnore: {}", status));
                }
                ContextOption::CommitLimit => {
                    let step = 500;
                    let current = settings.char_limit as i32;
                    let new_val = (current + (direction * step)).clamp(1000, 100_000);
                    settings.char_limit = new_val as usize;
                }
            }

            self.update_config_value(ctx, settings);
        }
    }
}

impl SettingsTab for ContextTab {
    fn title(&self) -> &str {
        "AI & Context"
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<()> {
        let command = resolve_event(event, &ctx.config.load().keys, Some(&self.key_cache));

        match command {
            Command::Up => self.select_previous(),
            Command::Down => self.select_next(),
            Command::Right | Command::Confirm => self.change_value(ctx, 1),
            Command::Left => self.change_value(ctx, -1),
            _ => {}
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, config: &MokuConfig) -> Result<()> {
        let theme = config.get_active_theme();

        let settings: ContextModuleSettings = config.resolve_module_config("context");

        let items: Vec<ListItem> = self
            .options
            .iter()
            .map(|opt| {
                let value_str = match opt {
                    ContextOption::GitIgnore => if settings.use_gitignore {
                        "Enabled"
                    } else {
                        "Disabled"
                    }
                    .to_string(),
                    ContextOption::CommitLimit => format!("{} chars", settings.char_limit),
                };

                let content = format!("{:<15} : [ {} ]", opt.label(), value_str);
                ListItem::new(Line::from(content)).style(Style::default().fg(theme.base_fg))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" AI Context Settings ")
                    .border_style(Style::default().fg(theme.border))
                    .style(Style::default().bg(theme.base_bg)),
            )
            .highlight_style(
                Style::default()
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">> ");

        frame.render_stateful_widget(list, area, &mut self.state);
        Ok(())
    }
}
