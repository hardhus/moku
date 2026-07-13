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

use moku_core::{AppContext, Command, MokuConfig, resolve_event};

use super::SettingsTab;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeneralOption {
    Theme,
    CursorStyle,
}

impl GeneralOption {
    fn label(&self) -> &'static str {
        match self {
            Self::Theme => "Theme",
            Self::CursorStyle => "Cursor Style",
        }
    }
    fn all() -> Vec<Self> {
        vec![Self::Theme, Self::CursorStyle]
    }
}

pub struct GeneralTab {
    state: ListState,
    options: Vec<GeneralOption>,
    key_cache: HashMap<String, String>,
}

impl GeneralTab {
    pub fn new(config: &MokuConfig) -> Self {
        let mut state = ListState::default();
        state.select(Some(0));

        let keys = config.get_module_keys("settings").unwrap_or_default();

        Self {
            state,
            options: GeneralOption::all(),
            key_cache: keys,
        }
    }

    fn next(&mut self) {
        let i = match self.state.selected() {
            Some(i) => (i + 1) % self.options.len(),
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn previous(&mut self) {
        let i = match self.state.selected() {
            Some(i) => (i + self.options.len() - 1) % self.options.len(),
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn change_value(&mut self, ctx: &mut AppContext, direction: i32) {
        if let Some(i) = self.state.selected() {
            match self.options[i] {
                GeneralOption::Theme => {
                    let themes = ["system", "hacker", "light", "pastel"];
                    let current = ctx.config.load().general.theme.clone();
                    let pos = themes.iter().position(|&t| t == current).unwrap_or(0);

                    let new_pos = if direction > 0 {
                        (pos + 1) % themes.len()
                    } else {
                        (pos + themes.len() - 1) % themes.len()
                    };

                    let new_theme = themes[new_pos].to_string();
                    ctx.update_config(|cfg| {
                        cfg.general.theme = new_theme.clone();
                    });
                    ctx.show_info(format!("Theme: {}", new_theme));
                }
                GeneralOption::CursorStyle => {
                    let styles = ["Block", "Bar", "Underline"];
                    let current = ctx.config.load().general.input_cursor_style.clone();
                    let pos = styles.iter().position(|&s| s == current).unwrap_or(0);

                    let new_pos = (pos + 1) % styles.len();
                    let new_style = styles[new_pos].to_string();
                    ctx.update_config(|cfg| {
                        cfg.general.input_cursor_style = new_style;
                    });
                }
            }
        }
    }
}

impl SettingsTab for GeneralTab {
    fn title(&self) -> &str {
        "General"
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<()> {
        let command = resolve_event(event, &ctx.config.load().keys, Some(&self.key_cache));

        match command {
            Command::Up => self.previous(),
            Command::Down => self.next(),
            Command::Right | Command::Confirm => self.change_value(ctx, 1),
            Command::Left => self.change_value(ctx, -1),
            _ => {}
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, config: &MokuConfig) -> Result<()> {
        let theme = config.get_active_theme();

        let items: Vec<ListItem> = self
            .options
            .iter()
            .map(|opt| {
                let value = match opt {
                    GeneralOption::Theme => config.general.theme.clone(),
                    GeneralOption::CursorStyle => config.general.input_cursor_style.clone(),
                };

                let content = format!("{:<15} : [ {} ]", opt.label(), value);
                ListItem::new(Line::from(content)).style(Style::default().fg(theme.base_fg))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" General Appearance Settings ")
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
