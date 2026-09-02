use std::collections::HashMap;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind};
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

/// Mirrors modules/moku-commit/src/engine.rs::CommitSettings field-for-field
/// (moku-settings doesn't depend on moku-commit, matching the existing
/// ContextModuleSettings shadow-struct pattern in tabs/context.rs).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default)]
struct CommitModuleSettings {
    char_limit: usize,
    api_url: Option<String>,
    model: Option<String>,
    prompt_template: Option<String>,
    temperature: Option<f32>,
}

impl Default for CommitModuleSettings {
    fn default() -> Self {
        Self {
            char_limit: 20_000,
            api_url: None,
            model: None,
            prompt_template: None,
            temperature: None,
        }
    }
}

const DEFAULT_MODEL_LABEL: &str = "gemini-3-flash-preview (default)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitOption {
    Model,
    ApiUrl,
    CharLimit,
    Temperature,
}

impl CommitOption {
    fn label(&self) -> &'static str {
        match self {
            Self::Model => "Model",
            Self::ApiUrl => "API URL",
            Self::CharLimit => "Diff Char Limit",
            Self::Temperature => "Temperature",
        }
    }
    fn all() -> Vec<Self> {
        vec![Self::Model, Self::ApiUrl, Self::CharLimit, Self::Temperature]
    }
    fn is_text(&self) -> bool {
        matches!(self, Self::Model | Self::ApiUrl)
    }
}

pub struct CommitTab {
    state: ListState,
    options: Vec<CommitOption>,
    key_cache: HashMap<String, String>,
    /// Text being typed for Model/ApiUrl, if currently editing.
    editing: Option<String>,
}

impl CommitTab {
    pub fn new(config: &MokuConfig) -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        let keys = config.get_module_keys("settings").unwrap_or_default();

        Self {
            state,
            options: CommitOption::all(),
            key_cache: keys,
            editing: None,
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

    fn save(&self, ctx: &mut AppContext, settings: CommitModuleSettings) {
        if let Ok(val) = toml::Value::try_from(settings) {
            ctx.update_config(|cfg| {
                cfg.modules.insert("commit".to_string(), val);
            });
        }
    }

    fn start_editing(&mut self, ctx: &AppContext) {
        let Some(i) = self.state.selected() else { return };
        let settings: CommitModuleSettings = ctx.config.load().resolve_module_config("commit");
        self.editing = Some(match self.options[i] {
            CommitOption::Model => settings.model.clone().unwrap_or_default(),
            CommitOption::ApiUrl => settings.api_url.clone().unwrap_or_default(),
            _ => return,
        });
    }

    fn confirm_editing(&mut self, ctx: &mut AppContext) {
        let (Some(i), Some(buffer)) = (self.state.selected(), self.editing.take()) else { return };
        let mut settings: CommitModuleSettings = ctx.config.load().resolve_module_config("commit");
        let trimmed = buffer.trim();
        let value = if trimmed.is_empty() { None } else { Some(trimmed.to_string()) };
        match self.options[i] {
            CommitOption::Model => settings.model = value,
            CommitOption::ApiUrl => settings.api_url = value,
            _ => {}
        }
        self.save(ctx, settings);
    }

    fn change_numeric(&mut self, ctx: &mut AppContext, direction: i32) {
        let Some(i) = self.state.selected() else { return };
        let mut settings: CommitModuleSettings = ctx.config.load().resolve_module_config("commit");
        match self.options[i] {
            CommitOption::CharLimit => {
                let step = 1000;
                let current = settings.char_limit as i32;
                settings.char_limit = (current + direction * step).clamp(1000, 200_000) as usize;
            }
            CommitOption::Temperature => {
                // Cycle: None (API default) -> 0.0 -> 0.1 -> ... -> 2.0 -> None
                let step = 0.1_f32;
                settings.temperature = match settings.temperature {
                    None if direction > 0 => Some(0.0),
                    None => Some(2.0),
                    Some(t) => {
                        let next = t + direction as f32 * step;
                        if next < -step / 2.0 || next > 2.0 + step / 2.0 {
                            None
                        } else {
                            Some(next.clamp(0.0, 2.0))
                        }
                    }
                };
            }
            _ => {}
        }
        self.save(ctx, settings);
    }
}

impl SettingsTab for CommitTab {
    fn title(&self) -> &str {
        "AI Commit"
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<()> {
        if self.editing.is_some() {
            if let Event::Key(key) = event {
                if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
                    match key.code {
                        KeyCode::Enter => self.confirm_editing(ctx),
                        KeyCode::Esc => self.editing = None,
                        KeyCode::Char(c) => {
                            if let Some(buf) = self.editing.as_mut() {
                                buf.push(c);
                            }
                        }
                        KeyCode::Backspace => {
                            if let Some(buf) = self.editing.as_mut() {
                                buf.pop();
                            }
                        }
                        _ => {}
                    }
                }
            }
            return Ok(());
        }

        let command = resolve_event(event, &ctx.config.load().keys, Some(&self.key_cache));
        let selected_is_text = self
            .state
            .selected()
            .map(|i| self.options[i].is_text())
            .unwrap_or(false);

        match command {
            Command::Up => self.select_previous(),
            Command::Down => self.select_next(),
            Command::Confirm if selected_is_text => self.start_editing(ctx),
            Command::Right if !selected_is_text => self.change_numeric(ctx, 1),
            Command::Left if !selected_is_text => self.change_numeric(ctx, -1),
            _ => {}
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, config: &MokuConfig) -> Result<()> {
        let theme = config.get_active_theme();
        let settings: CommitModuleSettings = config.resolve_module_config("commit");
        let editing_index = self.editing.is_some().then_some(self.state.selected()).flatten();

        let items: Vec<ListItem> = self
            .options
            .iter()
            .enumerate()
            .map(|(i, opt)| {
                let value_str = if Some(i) == editing_index {
                    format!("{}_", self.editing.as_deref().unwrap_or(""))
                } else {
                    match opt {
                        CommitOption::Model => settings
                            .model
                            .clone()
                            .unwrap_or_else(|| DEFAULT_MODEL_LABEL.to_string()),
                        CommitOption::ApiUrl => settings
                            .api_url
                            .clone()
                            .unwrap_or_else(|| "(default Gemini endpoint)".to_string()),
                        CommitOption::CharLimit => format!("{} chars", settings.char_limit),
                        CommitOption::Temperature => settings
                            .temperature
                            .map(|t| format!("{t:.1}"))
                            .unwrap_or_else(|| "(API default)".to_string()),
                    }
                };

                let content = format!("{:<17}: [ {} ]", opt.label(), value_str);
                ListItem::new(Line::from(content)).style(Style::default().fg(theme.base_fg))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" AI Commit Settings (prompt_template: config.toml only) ")
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
