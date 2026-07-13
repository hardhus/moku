use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, Paragraph},
};

use moku_core::{AppContext, ModuleId, ModuleMeta, MokuTheme, TuiModule};

pub struct LockScreenModule {
    input: String,
    is_setup_mode: bool,
    error_msg: Option<String>,
}

impl LockScreenModule {
    pub fn new() -> Self {
        Self {
            input: String::new(),
            is_setup_mode: false,
            error_msg: None,
        }
    }

    /// Performs vault initialization or unlocking using the input password.
    /// Once unlocked, calls `ctx.session.unlock(key)`. Navigation logic is handled by app_loop.
    async fn perform_auth(&mut self, ctx: &mut AppContext) {
        if self.input.trim().is_empty() {
            self.error_msg = Some("Password cannot be empty!".to_string());
            return;
        }

        let result = if self.is_setup_mode {
            ctx.security.initialize_vault(self.input.clone()).await
        } else {
            ctx.security.unlock_vault(self.input.clone()).await
        };

        match result {
            Ok(key) => {
                ctx.session.unlock(key);
                ctx.show_info("Session Unlocked 🔓");
                self.input.clear();
                self.error_msg = None;
            }
            Err(_) => {
                self.error_msg = Some("Incorrect password or initialization error!".to_string());
                self.input.clear();
            }
        }
    }
}

impl Default for LockScreenModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for LockScreenModule {
    fn id(&self) -> ModuleId {
        ModuleId::LOCK_SCREEN
    }
    fn title(&self) -> &'static str {
        ModuleId::LOCK_SCREEN.title()
    }
}

#[async_trait]
impl TuiModule for LockScreenModule {
    async fn init(&mut self, ctx: &mut AppContext) -> Result<()> {
        self.is_setup_mode = !ctx.security.is_vault_initialized();
        self.input.clear();
        self.error_msg = None;
        Ok(())
    }

    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        let Event::Key(key) = event else {
            return Ok(false);
        };
        if key.kind != KeyEventKind::Press {
            return Ok(false);
        }

        match key.code {
            KeyCode::Enter => {
                self.perform_auth(ctx).await;
            }
            KeyCode::Esc => {
                ctx.navigate_to(ModuleId::LAUNCHER);
                self.input.clear();
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                self.error_msg = None;
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(40),
                Constraint::Length(3),
                Constraint::Length(2),
                Constraint::Percentage(40),
            ])
            .split(area);

        let input_chunk = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(25),
                Constraint::Percentage(50),
                Constraint::Percentage(25),
            ])
            .split(chunks[1])[1];

        let masked_input: String = self.input.chars().map(|_| '•').collect();
        let title = if self.is_setup_mode {
            " 🆕 Vault Setup "
        } else {
            " 🔒 Secure Login "
        };
        let border_style = if self.error_msg.is_some() {
            theme.error
        } else {
            theme.info
        };

        let p = Paragraph::new(masked_input)
            .block(
                Block::default()
                    .title(title)
                    .title_alignment(ratatui::layout::Alignment::Center)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_style)),
            )
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg));

        frame.render_widget(p, input_chunk);

        if let Some(ref err) = self.error_msg {
            let error_p = Paragraph::new(err.as_str())
                .style(
                    Style::default()
                        .fg(theme.error)
                        .add_modifier(Modifier::ITALIC),
                )
                .alignment(ratatui::layout::Alignment::Center);
            frame.render_widget(error_p, chunks[2]);
        }
    }
}
