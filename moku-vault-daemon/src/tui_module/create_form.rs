//! The new-volume creation form (`CreateForm`) — split out of
//! `tui_module.rs` the same way `modules/moku-settings/src/tabs/*.rs`
//! splits one file per self-contained sub-view.

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::Style,
    text::Line,
    widgets::{Block, Borders, Paragraph},
};
use secrecy::{ExposeSecret, SecretBox};

use moku_core::{AppContext, MokuTheme, SafeKey};

use super::VaultManagerModule;
use crate::registry::{PasswordMode, VolumeSecret};
use crate::size;

/// Which field of the new-volume form currently has keyboard focus. All
/// fields for the current mode are shown at once (`Tab` switches focus,
/// `Enter` submits from any of them) — matches `modules/moku-rss/src/
/// tui_module.rs`'s `EditField`/`EditFeed` shape, not a sequential
/// per-field wizard. `Password`/`ConfirmPassword` only exist (and are only
/// reachable via `Tab`) when `CreateForm::mode` is `Custom` — `Mode` is not
/// a text field, `←`/`→`/`Space` toggle it instead of typing into it.
#[derive(PartialEq, Clone, Copy, Debug)]
enum CreateField {
    Name,
    Size,
    Mode,
    Password,
    ConfirmPassword,
}

/// State for creating a new volume from the TUI. Supports both password
/// modes: `Default` derives the key from moku's already-unlocked app vault
/// with no password field at all (mirrors the mount fast path — see
/// `VaultManagerModule::start_mount_with_key`); `Custom` shows a masked
/// password field *and* a confirmation field, since — unlike `Default`,
/// which is verified against a real, already-unlocked key — a brand new
/// password has nothing to check a typo against except itself. Always
/// created under the current directory (`create_volume`'s own `None`
/// default) — no location field here; `--path` stays a CLI-only option for
/// anyone who wants a different one, keeping this form simple.
pub(super) struct CreateForm {
    focus: CreateField,
    name: String,
    size: String,
    mode: PasswordMode,
    password: String,
    confirm_password: String,
    error: Option<String>,
}

impl CreateForm {
    pub(super) fn new() -> Self {
        Self {
            focus: CreateField::Name,
            name: String::new(),
            size: String::new(),
            mode: PasswordMode::Default,
            password: String::new(),
            confirm_password: String::new(),
            error: None,
        }
    }
}

impl VaultManagerModule {
    pub(super) fn handle_create_form_event(
        &mut self,
        event: &Event,
        ctx: &mut AppContext,
    ) -> Result<bool> {
        let Some(form) = &mut self.create_form else {
            return Ok(false);
        };
        let Event::Key(key) = event else {
            return Ok(false);
        };
        if key.kind != KeyEventKind::Press {
            return Ok(false);
        }
        match key.code {
            KeyCode::Esc => self.create_form = None,
            KeyCode::Tab => {
                form.focus = match form.focus {
                    CreateField::Name => CreateField::Size,
                    CreateField::Size => CreateField::Mode,
                    CreateField::Mode => match form.mode {
                        PasswordMode::Custom => CreateField::Password,
                        PasswordMode::Default => CreateField::Name,
                    },
                    CreateField::Password => CreateField::ConfirmPassword,
                    CreateField::ConfirmPassword => CreateField::Name,
                };
            }
            KeyCode::Left | KeyCode::Right if form.focus == CreateField::Mode => {
                form.mode = match form.mode {
                    PasswordMode::Default => PasswordMode::Custom,
                    PasswordMode::Custom => PasswordMode::Default,
                };
                form.error = None;
            }
            KeyCode::Enter => {
                if form.name.trim().is_empty() {
                    form.error = Some("Name cannot be empty.".to_string());
                } else {
                    match size::parse_size(&form.size) {
                        Err(e) => form.error = Some(format!("Invalid size: {e}")),
                        Ok(size_bytes) => {
                            // Doesn't yet consume `size_bytes`/build the
                            // final `VolumeSecret` until every mode-
                            // specific check below passes.
                            let secret = match form.mode {
                                PasswordMode::Custom => {
                                    if form.password.is_empty() {
                                        form.error = Some("Password cannot be empty.".to_string());
                                        None
                                    } else if form.password != form.confirm_password {
                                        form.error = Some("Passwords didn't match.".to_string());
                                        None
                                    } else {
                                        Some(VolumeSecret::Password(form.password.clone()))
                                    }
                                }
                                PasswordMode::Default => match ctx.session.current() {
                                    Some(key) => Some(VolumeSecret::FromAppVault(SecretBox::new(
                                        Box::new(SafeKey(key.expose_secret().0)),
                                    ))),
                                    None => {
                                        form.error = Some(
                                                "Main vault is locked — unlock it first, or switch to a custom password."
                                                    .to_string(),
                                            );
                                        None
                                    }
                                },
                            };
                            if let Some(secret) = secret {
                                let name = form.name.trim().to_string();
                                self.create_form = None;
                                self.start_create(name, size_bytes, secret);
                            }
                        }
                    }
                }
            }
            KeyCode::Char(' ') if form.focus == CreateField::Mode => {
                form.mode = match form.mode {
                    PasswordMode::Default => PasswordMode::Custom,
                    PasswordMode::Custom => PasswordMode::Default,
                };
                form.error = None;
            }
            KeyCode::Char(c) => {
                match form.focus {
                    CreateField::Name => form.name.push(c),
                    CreateField::Size => form.size.push(c),
                    CreateField::Mode => {}
                    CreateField::Password => form.password.push(c),
                    CreateField::ConfirmPassword => form.confirm_password.push(c),
                }
                form.error = None;
            }
            KeyCode::Backspace => {
                match form.focus {
                    CreateField::Name => {
                        form.name.pop();
                    }
                    CreateField::Size => {
                        form.size.pop();
                    }
                    CreateField::Mode => {}
                    CreateField::Password => {
                        form.password.pop();
                    }
                    CreateField::ConfirmPassword => {
                        form.confirm_password.pop();
                    }
                }
                form.error = None;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(super) fn draw_create_form(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &MokuTheme,
        form: &CreateForm,
    ) {
        let chunks = Layout::vertical([
            Constraint::Percentage(30),
            Constraint::Length(9),
            Constraint::Length(2),
            Constraint::Percentage(30),
        ])
        .split(area);
        let box_area = Layout::horizontal([
            Constraint::Percentage(20),
            Constraint::Percentage(60),
            Constraint::Percentage(20),
        ])
        .split(chunks[1])[1];

        let field_style = |focused: bool| {
            if focused {
                Style::default().fg(theme.selection_fg)
            } else {
                Style::default().fg(theme.base_fg)
            }
        };
        let marker = |focused: bool| if focused { ">" } else { " " };

        let mode_label = match form.mode {
            PasswordMode::Default => "Default (your moku vault password)",
            PasswordMode::Custom => "Custom (set below)",
        };

        let mut lines = vec![
            Line::styled(
                format!(
                    "{} Name:     {}",
                    marker(form.focus == CreateField::Name),
                    form.name
                ),
                field_style(form.focus == CreateField::Name),
            ),
            Line::styled(
                format!(
                    "{} Size:     {}",
                    marker(form.focus == CreateField::Size),
                    form.size
                ),
                field_style(form.focus == CreateField::Size),
            ),
            Line::styled(
                format!(
                    "{} Mode:     {}  (←/→ to change)",
                    marker(form.focus == CreateField::Mode),
                    mode_label
                ),
                field_style(form.focus == CreateField::Mode),
            ),
        ];
        if form.mode == PasswordMode::Custom {
            let masked_password: String = form.password.chars().map(|_| '•').collect();
            let masked_confirm: String = form.confirm_password.chars().map(|_| '•').collect();
            lines.push(Line::styled(
                format!(
                    "{} Password: {}",
                    marker(form.focus == CreateField::Password),
                    masked_password
                ),
                field_style(form.focus == CreateField::Password),
            ));
            lines.push(Line::styled(
                format!(
                    "{} Confirm:  {}",
                    marker(form.focus == CreateField::ConfirmPassword),
                    masked_confirm
                ),
                field_style(form.focus == CreateField::ConfirmPassword),
            ));
        }
        if let Some(err) = &form.error {
            lines.push(Line::styled(
                format!("  {err}"),
                Style::default().fg(theme.error),
            ));
        }

        let p = Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" New Volume ")
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.info)),
            )
            .style(Style::default().bg(theme.base_bg));
        frame.render_widget(p, box_area);

        let hint =
            Paragraph::new("[Tab] Switch field  [←/→] Change mode  [Enter] Create  [Esc] Cancel")
                .alignment(Alignment::Center)
                .style(Style::default().fg(theme.base_fg));
        frame.render_widget(hint, chunks[2]);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use moku_core::security::{SecurityManager, VaultSession};
    use moku_core::{MokuConfig, StorageManager, TuiModule};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
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

    async fn create_unlocked_test_context() -> AppContext {
        let ctx = create_test_context().await;
        ctx.session
            .unlock(SecretBox::new(Box::new(SafeKey([7u8; 32]))));
        ctx
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    #[tokio::test]
    async fn test_tab_cycles_focus_in_default_mode_skipping_password_fields() {
        let mut module = VaultManagerModule::new();
        module.create_form = Some(CreateForm::new()); // starts in PasswordMode::Default
        let mut ctx = create_test_context().await;

        assert_eq!(
            module.create_form.as_ref().unwrap().focus,
            CreateField::Name
        );
        module
            .handle_event(&key(KeyCode::Tab), &mut ctx)
            .await
            .unwrap();
        assert_eq!(
            module.create_form.as_ref().unwrap().focus,
            CreateField::Size
        );
        module
            .handle_event(&key(KeyCode::Tab), &mut ctx)
            .await
            .unwrap();
        assert_eq!(
            module.create_form.as_ref().unwrap().focus,
            CreateField::Mode
        );
        module
            .handle_event(&key(KeyCode::Tab), &mut ctx)
            .await
            .unwrap();
        assert_eq!(
            module.create_form.as_ref().unwrap().focus,
            CreateField::Name,
            "Default mode has no password fields to tab into"
        );
    }

    #[tokio::test]
    async fn test_tab_cycles_focus_in_custom_mode_through_password_fields() {
        let mut module = VaultManagerModule::new();
        let mut form = CreateForm::new();
        form.mode = PasswordMode::Custom;
        module.create_form = Some(form);
        let mut ctx = create_test_context().await;

        for _ in 0..3 {
            module
                .handle_event(&key(KeyCode::Tab), &mut ctx)
                .await
                .unwrap();
        }
        assert_eq!(
            module.create_form.as_ref().unwrap().focus,
            CreateField::Password
        );
        module
            .handle_event(&key(KeyCode::Tab), &mut ctx)
            .await
            .unwrap();
        assert_eq!(
            module.create_form.as_ref().unwrap().focus,
            CreateField::ConfirmPassword
        );
        module
            .handle_event(&key(KeyCode::Tab), &mut ctx)
            .await
            .unwrap();
        assert_eq!(
            module.create_form.as_ref().unwrap().focus,
            CreateField::Name
        );
    }

    #[tokio::test]
    async fn test_left_right_and_space_toggle_mode() {
        let mut module = VaultManagerModule::new();
        let mut form = CreateForm::new();
        form.focus = CreateField::Mode;
        module.create_form = Some(form);
        let mut ctx = create_test_context().await;

        assert_eq!(
            module.create_form.as_ref().unwrap().mode,
            PasswordMode::Default
        );
        module
            .handle_event(&key(KeyCode::Right), &mut ctx)
            .await
            .unwrap();
        assert_eq!(
            module.create_form.as_ref().unwrap().mode,
            PasswordMode::Custom
        );
        module
            .handle_event(&key(KeyCode::Char(' ')), &mut ctx)
            .await
            .unwrap();
        assert_eq!(
            module.create_form.as_ref().unwrap().mode,
            PasswordMode::Default
        );
    }

    #[tokio::test]
    async fn test_char_input_routes_to_the_focused_field() {
        let mut module = VaultManagerModule::new();
        module.create_form = Some(CreateForm::new());
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Char('a')), &mut ctx)
            .await
            .unwrap();
        assert_eq!(module.create_form.as_ref().unwrap().name, "a");

        module
            .handle_event(&key(KeyCode::Tab), &mut ctx)
            .await
            .unwrap();
        module
            .handle_event(&key(KeyCode::Char('1')), &mut ctx)
            .await
            .unwrap();
        assert_eq!(module.create_form.as_ref().unwrap().size, "1");
    }

    #[tokio::test]
    async fn test_enter_with_empty_name_sets_error_and_keeps_form_open() {
        let mut module = VaultManagerModule::new();
        module.create_form = Some(CreateForm::new());
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Enter), &mut ctx)
            .await
            .unwrap();
        let form = module.create_form.as_ref().expect("form should stay open");
        assert!(form.error.is_some());
    }

    #[tokio::test]
    async fn test_enter_with_invalid_size_sets_error() {
        let mut module = VaultManagerModule::new();
        let mut form = CreateForm::new();
        form.name = "test".to_string();
        form.size = "not-a-size".to_string();
        form.password = "hunter2".to_string();
        module.create_form = Some(form);
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Enter), &mut ctx)
            .await
            .unwrap();
        let form = module.create_form.as_ref().expect("form should stay open");
        assert!(form.error.as_ref().unwrap().contains("Invalid size"));
    }

    #[tokio::test]
    async fn test_enter_custom_mode_empty_password_sets_error() {
        let mut module = VaultManagerModule::new();
        let mut form = CreateForm::new();
        form.name = "test".to_string();
        form.size = "10MiB".to_string();
        form.mode = PasswordMode::Custom;
        module.create_form = Some(form);
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Enter), &mut ctx)
            .await
            .unwrap();
        let form = module.create_form.as_ref().expect("form should stay open");
        assert!(form.error.is_some());
    }

    #[tokio::test]
    async fn test_enter_custom_mode_mismatched_passwords_sets_error() {
        let mut module = VaultManagerModule::new();
        let mut form = CreateForm::new();
        form.name = "test".to_string();
        form.size = "10MiB".to_string();
        form.mode = PasswordMode::Custom;
        form.password = "hunter2".to_string();
        form.confirm_password = "hunter3".to_string();
        module.create_form = Some(form);
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Enter), &mut ctx)
            .await
            .unwrap();
        let form = module.create_form.as_ref().expect("form should stay open");
        assert!(form.error.as_ref().unwrap().contains("didn't match"));
    }

    #[tokio::test]
    async fn test_enter_custom_mode_matching_passwords_submits() {
        let mut module = VaultManagerModule::new();
        let mut form = CreateForm::new();
        form.name = "test".to_string();
        form.size = "10MiB".to_string();
        form.mode = PasswordMode::Custom;
        form.password = "hunter2".to_string();
        form.confirm_password = "hunter2".to_string();
        module.create_form = Some(form);
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Enter), &mut ctx)
            .await
            .unwrap();
        assert!(
            module.create_form.is_none(),
            "matching passwords should submit and close the form"
        );
    }

    #[tokio::test]
    async fn test_enter_default_mode_with_locked_vault_sets_error_and_keeps_form_open() {
        let mut module = VaultManagerModule::new();
        let mut form = CreateForm::new(); // starts in PasswordMode::Default
        form.name = "test".to_string();
        form.size = "10MiB".to_string();
        module.create_form = Some(form);
        let mut ctx = create_test_context().await; // locked — VaultSession::new()

        module
            .handle_event(&key(KeyCode::Enter), &mut ctx)
            .await
            .unwrap();
        let form = module.create_form.as_ref().expect("form should stay open");
        assert!(form.error.as_ref().unwrap().contains("locked"));
    }

    #[tokio::test]
    async fn test_enter_default_mode_with_unlocked_vault_submits_without_a_password() {
        let mut module = VaultManagerModule::new();
        let mut form = CreateForm::new(); // starts in PasswordMode::Default
        form.name = "test".to_string();
        form.size = "10MiB".to_string();
        module.create_form = Some(form);
        let mut ctx = create_unlocked_test_context().await;

        module
            .handle_event(&key(KeyCode::Enter), &mut ctx)
            .await
            .unwrap();
        assert!(
            module.create_form.is_none(),
            "an unlocked vault should let Default mode submit with zero password fields"
        );
    }

    #[tokio::test]
    async fn test_esc_cancels_the_form() {
        let mut module = VaultManagerModule::new();
        module.create_form = Some(CreateForm::new());
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Esc), &mut ctx)
            .await
            .unwrap();
        assert!(module.create_form.is_none());
    }

    fn render_create_form(form: CreateForm) -> String {
        let mut module = VaultManagerModule::new();
        module.create_form = Some(form);

        let (width, height) = (70u16, 20u16);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = MokuTheme::default();
        terminal
            .draw(|frame| module.draw(frame, Rect::new(0, 0, width, height), &theme))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn test_create_form_render_default_mode_hides_password_fields() {
        let mut form = CreateForm::new(); // starts in PasswordMode::Default
        form.name = "my-vault".to_string();
        form.size = "1GB".to_string();
        let content = render_create_form(form);

        assert!(content.contains("New Volume"));
        assert!(content.contains("Name"));
        assert!(content.contains("my-vault"));
        assert!(content.contains("Size"));
        assert!(content.contains("1GB"));
        assert!(content.contains("Mode"));
        assert!(!content.contains("Password"));
    }

    #[test]
    fn test_create_form_render_custom_mode_shows_password_fields() {
        let mut form = CreateForm::new();
        form.mode = PasswordMode::Custom;
        form.name = "my-vault".to_string();
        form.size = "1GB".to_string();
        let content = render_create_form(form);

        assert!(content.contains("Password"));
        assert!(content.contains("Confirm"));
    }
}
