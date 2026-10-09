use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use zeroize::{Zeroize, Zeroizing};

use moku_core::{AppContext, ModuleId, ModuleMeta, MokuTheme, TuiModule};
use moku_sys::util::error_chain;
use moku_vault::VaultError;

/// Which input has focus in setup mode (password + confirmation). Login
/// mode only ever uses `Password`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Password,
    Confirm,
}

pub struct LockScreenModule {
    /// The vault password, accumulated keystroke-by-keystroke. It is the
    /// actual secret, not just a display string, so it is wiped with
    /// `zeroize()` (not `clear()`, which only resets the length and leaves
    /// the bytes in the buffer) whenever it's discarded.
    input: Zeroizing<String>,
    /// Setup mode only: the confirmation of a brand-new password. A new
    /// password is always asked twice, since a typo in it would lock the
    /// user out of a vault that has no recovery key yet.
    confirm: Zeroizing<String>,
    focus: Field,
    is_setup_mode: bool,
    /// No `vault/meta.json`, but encrypted records exist on disk: the meta
    /// holding their salt was lost. Setting up a new vault here would make
    /// that data permanently unreadable, so setup is refused and the user
    /// is told to restore `meta.json` from a backup instead.
    orphaned_data: bool,
    error_msg: Option<String>,
}

impl LockScreenModule {
    pub fn new() -> Self {
        Self {
            input: Zeroizing::new(String::new()),
            confirm: Zeroizing::new(String::new()),
            focus: Field::Password,
            is_setup_mode: false,
            orphaned_data: false,
            error_msg: None,
        }
    }

    fn wipe_inputs(&mut self) {
        self.input.zeroize();
        self.confirm.zeroize();
        self.focus = Field::Password;
    }

    fn focused_input(&mut self) -> &mut Zeroizing<String> {
        match self.focus {
            Field::Password => &mut self.input,
            Field::Confirm => &mut self.confirm,
        }
    }

    /// Performs vault initialization or unlocking using the input password.
    /// Once unlocked, calls `ctx.session.unlock(key)`. Navigation logic is handled by app_loop.
    async fn perform_auth(&mut self, ctx: &mut AppContext) {
        if self.orphaned_data {
            return;
        }
        if self.input.trim().is_empty() {
            self.error_msg = Some("Password cannot be empty!".to_string());
            return;
        }
        if self.is_setup_mode && *self.input != *self.confirm {
            self.error_msg = Some("Passwords don't match. Type them again.".to_string());
            self.wipe_inputs();
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
                self.wipe_inputs();
                self.error_msg = None;
            }
            Err(e) => {
                self.error_msg = Some(auth_error_message(&e));
                self.wipe_inputs();
            }
        }
    }

    fn draw_input(
        frame: &mut Frame,
        area: Rect,
        theme: &MokuTheme,
        title: &str,
        value: &str,
        focused: bool,
        has_error: bool,
    ) {
        let masked: String = value.chars().map(|_| '•').collect();
        let border = if has_error {
            theme.error
        } else if focused {
            theme.info
        } else {
            theme.border
        };
        let p = Paragraph::new(masked)
            .block(
                Block::default()
                    .title(title.to_string())
                    .title_alignment(ratatui::layout::Alignment::Center)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border)),
            )
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg));
        frame.render_widget(p, area);
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
        // Only when meta.json is missing: is there encrypted data it would
        // orphan? If that can't even be checked, err on the safe side.
        self.orphaned_data =
            self.is_setup_mode && ctx.storage.has_encrypted_records().await.unwrap_or(true);
        self.wipe_inputs();
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
            KeyCode::Esc => {
                ctx.navigate_to(ModuleId::LAUNCHER);
                self.wipe_inputs();
            }
            _ if self.orphaned_data => return Ok(false),
            KeyCode::Enter => {
                self.perform_auth(ctx).await;
            }
            KeyCode::Tab | KeyCode::BackTab if self.is_setup_mode => {
                self.focus = match self.focus {
                    Field::Password => Field::Confirm,
                    Field::Confirm => Field::Password,
                };
            }
            KeyCode::Char(c) => {
                self.focused_input().push(c);
                self.error_msg = None;
            }
            KeyCode::Backspace => {
                self.focused_input().pop();
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let center = |r: Rect| {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(25),
                    Constraint::Percentage(50),
                    Constraint::Percentage(25),
                ])
                .split(r)[1]
        };

        if self.orphaned_data {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Percentage(30),
                    Constraint::Length(8),
                    Constraint::Percentage(30),
                ])
                .split(area);
            let msg = Paragraph::new(
                "Encrypted data exists, but the vault's meta.json (which holds its salt) is missing. \
                 Restore vault/meta.json from a backup. Creating a new vault here would make the \
                 existing data permanently unreadable, so setup is disabled. [Esc] Back",
            )
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(theme.error).bg(theme.base_bg))
            .block(
                Block::default()
                    .title(" ⚠ Vault metadata missing ")
                    .title_alignment(ratatui::layout::Alignment::Center)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.error)),
            );
            frame.render_widget(msg, center(chunks[1]));
            return;
        }

        let has_error = self.error_msg.is_some();
        if self.is_setup_mode {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Percentage(35),
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(2),
                    Constraint::Length(1),
                    Constraint::Percentage(35),
                ])
                .split(area);
            Self::draw_input(
                frame,
                center(chunks[1]),
                theme,
                " 🆕 Vault Setup: new password ",
                &self.input,
                self.focus == Field::Password,
                has_error,
            );
            Self::draw_input(
                frame,
                center(chunks[2]),
                theme,
                " Confirm password ",
                &self.confirm,
                self.focus == Field::Confirm,
                has_error,
            );
            if let Some(ref err) = self.error_msg {
                frame.render_widget(error_line(err, theme), chunks[3]);
            }
            let hint = Paragraph::new("[Tab] Switch field  [Enter] Create vault  [Esc] Back")
                .style(Style::default().fg(theme.base_fg))
                .alignment(ratatui::layout::Alignment::Center);
            frame.render_widget(hint, chunks[4]);
        } else {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Percentage(40),
                    Constraint::Length(3),
                    Constraint::Length(2),
                    Constraint::Percentage(40),
                ])
                .split(area);
            Self::draw_input(
                frame,
                center(chunks[1]),
                theme,
                " 🔒 Secure Login ",
                &self.input,
                true,
                has_error,
            );
            if let Some(ref err) = self.error_msg {
                frame.render_widget(error_line(err, theme), chunks[2]);
            }
        }
    }
}

/// What the lock screen says when unlocking or creating the vault fails.
/// Only a real password mismatch says "incorrect password"; a damaged or
/// unreadable `meta.json` says so plainly, since retyping the password
/// can't fix it.
fn auth_error_message(e: &VaultError) -> String {
    match e {
        VaultError::WrongPassword => "Incorrect password.".to_string(),
        VaultError::AlreadyInitialized(_) => {
            "A vault already exists here; refusing to overwrite it.".to_string()
        }
        VaultError::NotInitialized => {
            "The vault's meta.json is gone; it may have been moved or deleted.".to_string()
        }
        VaultError::MetadataCorrupt { .. } => {
            "The vault's meta.json is damaged. This is not a password problem.".to_string()
        }
        VaultError::Io { .. } | VaultError::DataDir(_) | VaultError::Write(_) => {
            format!("Couldn't access the vault on disk: {}", error_chain(e))
        }
        _ => format!("Couldn't open the vault: {}", error_chain(e)),
    }
}

fn error_line<'a>(err: &'a str, theme: &MokuTheme) -> Paragraph<'a> {
    Paragraph::new(err)
        .style(
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::ITALIC),
        )
        .alignment(ratatui::layout::Alignment::Center)
        .wrap(ratatui::widgets::Wrap { trim: true })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyEvent, KeyModifiers};
    use moku_config::MokuConfig;
    use moku_store::StorageManager;
    use moku_vault::{SecurityManager, VaultSession};
    use tempfile::tempdir;

    use super::*;

    async fn create_test_context() -> AppContext {
        create_test_context_with_root().await.0
    }

    async fn create_test_context_with_root() -> (AppContext, std::path::PathBuf) {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::mem::forget(temp);

        let config = Arc::new(moku_config::ConfigHandle::detached(MokuConfig::default()));
        let session = Arc::new(VaultSession::new());
        let security = Arc::new(SecurityManager::new_with_root(root.clone()));
        let storage = Arc::new(
            StorageManager::new_with_root(Arc::clone(&session), root.clone())
                .await
                .unwrap(),
        );

        (AppContext::new(config, session, security, storage), root)
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    async fn type_str(module: &mut LockScreenModule, ctx: &mut AppContext, s: &str) {
        for c in s.chars() {
            module.handle_event(&key(KeyCode::Char(c)), ctx).await.unwrap();
        }
    }

    fn rendered(module: &mut LockScreenModule) -> String {
        use ratatui::{Terminal, backend::TestBackend};
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| module.draw(f, Rect::new(0, 0, 100, 30), &MokuTheme::default()))
            .unwrap();
        terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect()
    }

    #[tokio::test]
    async fn test_setup_mismatched_confirmation_is_rejected_and_creates_nothing() {
        let mut module = LockScreenModule::new();
        let mut ctx = create_test_context().await;
        module.init(&mut ctx).await.unwrap();

        type_str(&mut module, &mut ctx, "first-try").await;
        module.handle_event(&key(KeyCode::Tab), &mut ctx).await.unwrap();
        type_str(&mut module, &mut ctx, "first-tyr").await;
        module.handle_event(&key(KeyCode::Enter), &mut ctx).await.unwrap();

        assert!(module.error_msg.as_deref().unwrap().contains("don't match"));
        assert!(!ctx.security.is_vault_initialized(), "no vault from a mismatch");
        assert!(!ctx.session.is_unlocked());
        assert!(module.input.is_empty() && module.confirm.is_empty());
        assert_eq!(module.focus, Field::Password);
    }

    #[tokio::test]
    async fn test_setup_enter_works_from_the_confirm_field() {
        let mut module = LockScreenModule::new();
        let mut ctx = create_test_context().await;
        module.init(&mut ctx).await.unwrap();
        type_str(&mut module, &mut ctx, "pw").await;
        module.handle_event(&key(KeyCode::Tab), &mut ctx).await.unwrap();
        assert_eq!(module.focus, Field::Confirm);
        type_str(&mut module, &mut ctx, "pw").await;
        module.handle_event(&key(KeyCode::Enter), &mut ctx).await.unwrap();
        assert!(ctx.session.is_unlocked());
    }

    #[tokio::test]
    async fn test_setup_screen_shows_both_fields() {
        let mut module = LockScreenModule::new();
        let mut ctx = create_test_context().await;
        module.init(&mut ctx).await.unwrap();
        let content = rendered(&mut module);
        assert!(content.contains("new password"));
        assert!(content.contains("Confirm password"));
    }

    #[tokio::test]
    async fn test_login_mode_tab_does_not_switch_to_a_confirm_field() {
        let mut ctx = create_test_context().await;
        ctx.security
            .initialize_vault(Zeroizing::new("pw".to_string()))
            .await
            .unwrap();
        let mut module = LockScreenModule::new();
        module.init(&mut ctx).await.unwrap();
        module.handle_event(&key(KeyCode::Tab), &mut ctx).await.unwrap();
        assert_eq!(module.focus, Field::Password);
        assert!(!rendered(&mut module).contains("Confirm password"));
    }

    #[tokio::test]
    async fn test_missing_meta_with_encrypted_data_refuses_setup() {
        let mut ctx = create_test_context().await;
        // Encrypted data written under some key, but no meta.json exists.
        let key_bytes = SecurityManager::derive_key("lost", &[5u8; 16]).await.unwrap();
        ctx.session.unlock(key_bytes);
        ctx.storage
            .save("todo", "items", &vec!["x".to_string()], true)
            .await
            .unwrap();
        ctx.session.lock();

        let mut module = LockScreenModule::new();
        module.init(&mut ctx).await.unwrap();
        assert!(module.orphaned_data);
        type_str(&mut module, &mut ctx, "new").await;
        module.handle_event(&key(KeyCode::Enter), &mut ctx).await.unwrap();
        assert!(!ctx.security.is_vault_initialized(), "setup must be refused");
        assert!(rendered(&mut module).contains("meta.json"));
    }

    #[tokio::test]
    async fn test_missing_meta_with_only_plaintext_data_still_allows_setup() {
        let mut ctx = create_test_context().await;
        ctx.storage
            .save("rss", "feeds", &vec!["x".to_string()], false)
            .await
            .unwrap();
        let mut module = LockScreenModule::new();
        module.init(&mut ctx).await.unwrap();
        assert!(module.is_setup_mode);
        assert!(!module.orphaned_data);
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

        type_str(&mut module, &mut ctx, "new-password").await;
        module.handle_event(&key(KeyCode::Tab), &mut ctx).await.unwrap();
        type_str(&mut module, &mut ctx, "new-password").await;
        module.handle_event(&key(KeyCode::Enter), &mut ctx).await.unwrap();

        assert!(module.error_msg.is_none());
        assert!(ctx.session.is_unlocked());
        assert!(ctx.security.is_vault_initialized());
        assert!(module.input.is_empty(), "input should be cleared after a successful auth");
        assert!(module.confirm.is_empty());
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

        assert_eq!(module.error_msg.as_deref(), Some("Incorrect password."));
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

    #[tokio::test]
    async fn test_corrupt_meta_is_not_reported_as_a_wrong_password() {
        let (mut ctx, root) = create_test_context_with_root().await;
        ctx.security
            .initialize_vault(Zeroizing::new("correct-pass".to_string()))
            .await
            .unwrap();
        std::fs::write(root.join("vault").join("meta.json"), b"{ not json").unwrap();
        let mut module = LockScreenModule::new();
        module.init(&mut ctx).await.unwrap();

        type_str(&mut module, &mut ctx, "correct-pass").await;
        module.handle_event(&key(KeyCode::Enter), &mut ctx).await.unwrap();

        let msg = module.error_msg.as_deref().unwrap();
        assert!(msg.contains("damaged"), "got: {msg}");
        assert!(!msg.contains("Incorrect password"));
        assert!(!ctx.session.is_unlocked());
        assert!(rendered(&mut module).contains("damaged"));
    }

    #[test]
    fn test_auth_error_message_distinguishes_error_kinds() {
        assert_eq!(auth_error_message(&VaultError::WrongPassword), "Incorrect password.");
        let corrupt = VaultError::MetadataCorrupt {
            path: "meta.json".into(),
            source: None,
        };
        assert!(auth_error_message(&corrupt).contains("not a password problem"));
        let io = VaultError::Io {
            path: "meta.json".into(),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };
        let msg = auth_error_message(&io);
        assert!(msg.starts_with("Couldn't access the vault on disk"), "got: {msg}");
        assert!(msg.contains("denied"), "the cause must be shown: {msg}");
    }
}
