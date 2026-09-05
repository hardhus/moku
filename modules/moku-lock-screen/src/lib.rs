use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, Paragraph},
};
use zeroize::Zeroizing;

use moku_core::{AppContext, ModuleId, ModuleMeta, MokuTheme, TuiModule};

pub struct LockScreenModule {
    /// The vault password, accumulated keystroke-by-keystroke — zeroized
    /// on drop/clear since this is the actual secret, not just a display
    /// string, for as long as this screen is open.
    input: Zeroizing<String>,
    is_setup_mode: bool,
    error_msg: Option<String>,
}

impl LockScreenModule {
    pub fn new() -> Self {
        Self {
            input: Zeroizing::new(String::new()),
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
    fn encrypt_by_default(&self) -> bool {
        // Must never gate on vault unlock itself — that would make the
        // vault unreachable to unlock.
        false
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use moku_core::security::{SecurityManager, VaultSession};
    use moku_core::{MokuConfig, StorageManager};
    use tempfile::tempdir;

    use super::*;

    async fn create_test_context() -> AppContext {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::mem::forget(temp);

        let config = Arc::new(ArcSwap::from_pointee(MokuConfig::default()));
        let session = Arc::new(VaultSession::new());
        let security = Arc::new(SecurityManager::new_with_root(root.clone()));
        let storage = Arc::new(
            StorageManager::new_with_root(Arc::clone(&session), root)
                .await
                .unwrap(),
        );

        AppContext::new(config, session, security, storage)
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    #[tokio::test]
    async fn test_init_sets_setup_mode_when_vault_not_initialized() {
        let mut module = LockScreenModule::new();
        let mut ctx = create_test_context().await;

        module.init(&mut ctx).await.unwrap();
        assert!(module.is_setup_mode);
    }

    #[tokio::test]
    async fn test_init_sets_login_mode_when_vault_already_initialized() {
        let mut module = LockScreenModule::new();
        let mut ctx = create_test_context().await;
        ctx.security
            .initialize_vault(Zeroizing::new("existing-pass".to_string()))
            .await
            .unwrap();

        module.init(&mut ctx).await.unwrap();
        assert!(!module.is_setup_mode);
    }

    #[tokio::test]
    async fn test_enter_with_empty_password_shows_error_and_does_not_unlock_session() {
        let mut module = LockScreenModule::new();
        let mut ctx = create_test_context().await;
        module.init(&mut ctx).await.unwrap();

        module.handle_event(&key(KeyCode::Enter), &mut ctx).await.unwrap();
        assert_eq!(module.error_msg.as_deref(), Some("Password cannot be empty!"));
        assert!(!ctx.session.is_unlocked());
    }

    #[tokio::test]
    async fn test_enter_in_setup_mode_initializes_vault_and_unlocks_session() {
        let mut module = LockScreenModule::new();
        let mut ctx = create_test_context().await;
        module.init(&mut ctx).await.unwrap(); // setup mode: vault not initialized yet

        for c in "new-password".chars() {
            module.handle_event(&key(KeyCode::Char(c)), &mut ctx).await.unwrap();
        }
        module.handle_event(&key(KeyCode::Enter), &mut ctx).await.unwrap();

        assert!(module.error_msg.is_none());
        assert!(ctx.session.is_unlocked());
        assert!(ctx.security.is_vault_initialized());
        assert!(module.input.is_empty(), "input should be cleared after a successful auth");
    }

    #[tokio::test]
    async fn test_enter_in_login_mode_with_correct_password_unlocks_session() {
        let mut ctx = create_test_context().await;
        ctx.security
            .initialize_vault(Zeroizing::new("correct-pass".to_string()))
            .await
            .unwrap();
        let mut module = LockScreenModule::new();
        module.init(&mut ctx).await.unwrap(); // login mode: vault already initialized

        for c in "correct-pass".chars() {
            module.handle_event(&key(KeyCode::Char(c)), &mut ctx).await.unwrap();
        }
        module.handle_event(&key(KeyCode::Enter), &mut ctx).await.unwrap();

        assert!(module.error_msg.is_none());
        assert!(ctx.session.is_unlocked());
    }

    #[tokio::test]
    async fn test_enter_in_login_mode_with_wrong_password_shows_error_and_clears_input() {
        let mut ctx = create_test_context().await;
        ctx.security
            .initialize_vault(Zeroizing::new("correct-pass".to_string()))
            .await
            .unwrap();
        let mut module = LockScreenModule::new();
        module.init(&mut ctx).await.unwrap();

        for c in "wrong-pass".chars() {
            module.handle_event(&key(KeyCode::Char(c)), &mut ctx).await.unwrap();
        }
        module.handle_event(&key(KeyCode::Enter), &mut ctx).await.unwrap();

        assert_eq!(
            module.error_msg.as_deref(),
            Some("Incorrect password or initialization error!")
        );
        assert!(!ctx.session.is_unlocked());
        assert!(module.input.is_empty(), "input should be cleared after a failed auth");
    }

    #[tokio::test]
    async fn test_char_input_accumulates_and_clears_a_prior_error() {
        let mut module = LockScreenModule::new();
        let mut ctx = create_test_context().await;
        module.init(&mut ctx).await.unwrap();
        module.error_msg = Some("stale error".to_string());

        module.handle_event(&key(KeyCode::Char('a')), &mut ctx).await.unwrap();
        assert_eq!(module.input.as_str(), "a");
        assert!(module.error_msg.is_none());

        module.handle_event(&key(KeyCode::Char('b')), &mut ctx).await.unwrap();
        assert_eq!(module.input.as_str(), "ab");
    }

    #[tokio::test]
    async fn test_backspace_removes_last_character() {
        let mut module = LockScreenModule::new();
        let mut ctx = create_test_context().await;
        module.init(&mut ctx).await.unwrap();
        module.input = Zeroizing::new("ab".to_string());

        module.handle_event(&key(KeyCode::Backspace), &mut ctx).await.unwrap();
        assert_eq!(module.input.as_str(), "a");
    }

    #[tokio::test]
    async fn test_esc_navigates_to_launcher_and_clears_input() {
        let mut module = LockScreenModule::new();
        let mut ctx = create_test_context().await;
        module.init(&mut ctx).await.unwrap();
        module.input = Zeroizing::new("partial".to_string());

        module.handle_event(&key(KeyCode::Esc), &mut ctx).await.unwrap();
        assert_eq!(ctx.take_navigation(), Some(ModuleId::LAUNCHER));
        assert!(module.input.is_empty());
    }
}
