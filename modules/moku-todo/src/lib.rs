use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use serde::{Deserialize, Serialize};

use moku_core::{AppContext, Command, ModuleId, ModuleMeta, MokuTheme, TuiModule, resolve_event};

#[derive(Serialize, Deserialize, Clone, Default)]
struct TodoItem {
    title: String,
    completed: bool,
}

#[derive(Deserialize, Default)]
struct TodoKeyConfig {
    pub keys: HashMap<String, String>,
}

pub struct TodoModule {
    items: Vec<TodoItem>,
    state: ListState,
    input_mode: bool,
    input_buffer: String,
}

impl TodoModule {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            state: ListState::default(),
            input_mode: false,
            input_buffer: String::new(),
        }
    }

    async fn save(&self, ctx: &mut AppContext) {
        let encrypt = moku_core::resolve_encryption(&ctx.config.load(), ModuleId::TODO.as_str(), true);
        if let Err(e) = ctx
            .storage
            .save(ModuleId::TODO.as_str(), "items", &self.items, encrypt)
            .await
        {
            ctx.show_error(format!("Save error: {}", e));
        }
    }

    fn next(&mut self) -> bool {
        if self.items.is_empty() {
            return false;
        }
        let i = match self.state.selected() {
            Some(i) => {
                if i >= self.items.len() - 1 {
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
        if self.items.is_empty() {
            return false;
        }
        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    self.items.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
        true
    }

    async fn toggle_status(&mut self, ctx: &mut AppContext) -> bool {
        if let Some(i) = self.state.selected() {
            if let Some(item) = self.items.get_mut(i) {
                item.completed = !item.completed;
                let msg = if item.completed {
                    "Completed"
                } else {
                    "Reverted"
                };
                ctx.show_info(format!("Task: {}", msg));
                self.save(ctx).await;
                return true;
            }
        }
        false
    }

    async fn delete_item(&mut self, ctx: &mut AppContext) -> bool {
        if let Some(i) = self.state.selected() {
            if i < self.items.len() {
                let deleted = self.items.remove(i);
                ctx.show_info(format!("Deleted: {}", deleted.title));

                if self.items.is_empty() {
                    self.state.select(None);
                } else if i >= self.items.len() {
                    self.state.select(Some(self.items.len() - 1));
                }
                self.save(ctx).await;
                return true;
            }
        }
        false
    }

    async fn add_item(&mut self, ctx: &mut AppContext) {
        if !self.input_buffer.trim().is_empty() {
            let title = self.input_buffer.trim().to_string();
            self.items.push(TodoItem {
                title: title.clone(),
                completed: false,
            });
            ctx.show_info(format!("'{}' Added", title));
            self.input_buffer.clear();
            self.state.select(Some(self.items.len() - 1));
            self.save(ctx).await;
        }
        self.input_mode = false;
    }
}

impl Default for TodoModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for TodoModule {
    fn id(&self) -> ModuleId {
        ModuleId::TODO
    }
    fn title(&self) -> &'static str {
        ModuleId::TODO.title()
    }
}

#[async_trait]
impl TuiModule for TodoModule {
    async fn init(&mut self, ctx: &mut AppContext) -> Result<()> {
        let loaded_items: Result<Vec<TodoItem>> =
            ctx.storage.load(ModuleId::TODO.as_str(), "items").await;

        match loaded_items {
            Ok(items) => {
                self.items = items;
                if !self.items.is_empty() {
                    self.state.select(Some(0));
                }
            }
            Err(_) => {
                self.items = vec![TodoItem {
                    title: "Welcome to Moku! 👋".to_string(),
                    completed: false,
                }];
                self.save(ctx).await;
            }
        }
        Ok(())
    }

    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        if self.input_mode {
            let mut changed = false;
            if let Event::Key(key) = event {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Enter => {
                            self.add_item(ctx).await;
                            changed = true;
                        }
                        KeyCode::Esc => {
                            self.input_mode = false;
                            self.input_buffer.clear();
                            changed = true;
                        }
                        KeyCode::Char(c) => {
                            self.input_buffer.push(c);
                            changed = true;
                        }
                        KeyCode::Backspace => {
                            self.input_buffer.pop();
                            changed = true;
                        }
                        _ => {}
                    }
                }
            }
            return Ok(changed);
        }

        let module_config: TodoKeyConfig = ctx
            .config
            .load()
            .resolve_module_config(ModuleId::TODO.as_str());
        let command = resolve_event(event, &ctx.config.load().keys, Some(&module_config.keys));

        let changed = match command {
            Command::Quit | Command::Back => {
                ctx.navigate_to(ModuleId::LAUNCHER);
                true
            }
            Command::Up => self.previous(),
            Command::Down => self.next(),
            Command::Confirm | Command::Toggle => self.toggle_status(ctx).await,
            Command::Delete => self.delete_item(ctx).await,
            Command::Add => {
                self.input_mode = true;
                self.input_buffer.clear();
                true
            }
            _ => false,
        };
        Ok(changed)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(3)])
            .split(area);

        let items: Vec<ListItem> = self
            .items
            .iter()
            .map(|i| {
                let (symbol, color) = if i.completed {
                    ("[x]", theme.success)
                } else {
                    ("[ ]", theme.base_fg)
                };
                let content = Line::from(format!("{} {}", symbol, i.title));
                ListItem::new(content).style(Style::default().fg(color))
            })
            .collect();

        let list_block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", ModuleId::TODO.title()))
            .title_alignment(ratatui::layout::Alignment::Center)
            .border_style(Style::default().fg(theme.border))
            .style(Style::default().bg(theme.base_bg));

        let list = List::new(items)
            .block(list_block)
            .highlight_style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg),
            )
            .highlight_symbol(">> ");

        frame.render_stateful_widget(list, chunks[0], &mut self.state);

        let bottom_content = if self.input_mode {
            Paragraph::new(format!("NEW: {}_", self.input_buffer))
                .style(Style::default().fg(theme.warning))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Add Mode ")
                        .border_style(Style::default().fg(theme.warning)),
                )
        } else {
            Paragraph::new(" [A] Add | [D] Delete | [Space] Toggle | [ESC] Exit ")
                .style(Style::default().fg(theme.base_fg))
                .alignment(ratatui::layout::Alignment::Center)
                .block(
                    Block::default()
                        .borders(Borders::TOP)
                        .border_style(Style::default().fg(theme.border)),
                )
        };

        frame.render_widget(bottom_content, chunks[1]);
    }
}
