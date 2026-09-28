//! The mount-password prompt (`PasswordPrompt`) — shown before mounting a
//! volume, always visible even on the no-reprompt fast path (see
//! `VolumeManagerModule::start_mount_with_volume_key`), just without a
//! password field in that case. Split out of `tui_module.rs` the same way
//! `modules/moku-settings/src/tabs/*.rs` splits one file per self-contained
//! sub-view.

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::Style,
    text::Line,
    widgets::{Block, Borders, Paragraph},
};
use zeroize::Zeroizing;

use moku_core::{AppContext, MokuTheme};

use super::VolumeManagerModule;
use crate::registry;

/// Which field of the mount prompt currently has keyboard focus.
/// `Password` only exists (and is only reachable via `Tab`) when
/// `PasswordPrompt::use_app_key` is false — a Default-mode volume with the app
/// vault already unlocked needs no password at all, only a mountpoint to
/// (optionally) confirm or change.
#[derive(PartialEq, Clone, Copy, Debug)]
pub(super) enum MountField {
    Mountpoint,
    Password,
}

/// Self-contained sub-state for a pending mount — no separate `ModuleId`/
/// navigation, mirroring `moku-lock-screen`'s input handling directly
/// (`push`/`pop` a `String`, Enter/Esc). Always shown before mounting,
/// even on the no-reprompt fast path — a mountpoint is picked by default
/// (`VolumeManagerModule::default_mountpoint`) but stays visible and
/// editable rather than being silently auto-chosen, since more than one
/// volume may be mounted and the user may want a specific drive letter.
pub(super) struct PasswordPrompt {
    pub(super) volume_id: String,
    pub(super) display_name: String,
    pub(super) mountpoint: String,
    pub(super) focus: MountField,
    /// Zeroized on drop — the actual mount password while it's being
    /// typed, not just a display string.
    pub(super) input: Zeroizing<String>,
    /// true: a new-scheme Default-mode volume whose key derives from the
    /// app vault. No password field; on Enter the app key is read from
    /// the session AT THAT MOMENT, the volume key is derived, and the app
    /// key is dropped right away. The prompt itself never holds a key, so
    /// a lock (ctrl-l, auto-lock) while it is open really does lock, and
    /// Enter afterwards refuses to mount.
    /// false: a password must be typed (Custom mode, an old-scheme
    /// Default-mode volume with its own vault, or a currently-locked main
    /// vault) — the Password field is shown too.
    pub(super) use_app_key: bool,
}

impl VolumeManagerModule {
    pub(super) fn handle_prompt_event(&mut self, event: &Event, ctx: &AppContext) -> Result<bool> {
        let Some(prompt) = &mut self.prompt else {
            return Ok(false);
        };
        let Event::Key(key) = event else {
            return Ok(false);
        };
        if key.kind != KeyEventKind::Press {
            return Ok(false);
        }
        match key.code {
            KeyCode::Enter => {
                let volume_id = prompt.volume_id.clone();
                let display_name = prompt.display_name.clone();
                let mountpoint = prompt.mountpoint.clone();
                if prompt.use_app_key {
                    self.prompt = None;
                    let Some(app_key) = ctx.session.current() else {
                        self.show_message("Main vault is locked, unlock it first.");
                        return Ok(true);
                    };
                    let volume_key = registry::derive_default_volume_master_key(&app_key, &volume_id);
                    drop(app_key);
                    self.start_mount_with_volume_key(volume_id, display_name, mountpoint, volume_key);
                } else {
                    let password = prompt.input.to_string();
                    self.prompt = None;
                    self.start_mount(volume_id, display_name, mountpoint, password);
                }
            }
            KeyCode::Esc => self.prompt = None,
            KeyCode::Tab if !prompt.use_app_key => {
                prompt.focus = match prompt.focus {
                    MountField::Mountpoint => MountField::Password,
                    MountField::Password => MountField::Mountpoint,
                };
            }
            KeyCode::Char(c) => match prompt.focus {
                MountField::Mountpoint => prompt.mountpoint.push(c),
                MountField::Password => prompt.input.push(c),
            },
            KeyCode::Backspace => match prompt.focus {
                MountField::Mountpoint => {
                    prompt.mountpoint.pop();
                }
                MountField::Password => {
                    prompt.input.pop();
                }
            },
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(super) fn draw_prompt(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &MokuTheme,
        prompt: &PasswordPrompt,
    ) {
        let chunks = Layout::vertical([
            Constraint::Percentage(35),
            Constraint::Length(5),
            Constraint::Length(2),
            Constraint::Percentage(35),
        ])
        .split(area);
        let box_area = Layout::horizontal([
            Constraint::Percentage(25),
            Constraint::Percentage(50),
            Constraint::Percentage(25),
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

        let mut lines = vec![Line::styled(
            format!(
                "{} Drive:    {}",
                marker(prompt.focus == MountField::Mountpoint),
                prompt.mountpoint
            ),
            field_style(prompt.focus == MountField::Mountpoint),
        )];
        if !prompt.use_app_key {
            let masked: String = prompt.input.chars().map(|_| '•').collect();
            lines.push(Line::styled(
                format!(
                    "{} Password: {}",
                    marker(prompt.focus == MountField::Password),
                    masked
                ),
                field_style(prompt.focus == MountField::Password),
            ));
        }

        let p = Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!(" Mount '{}' ", prompt.display_name))
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.info)),
            )
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg));
        frame.render_widget(p, box_area);

        let hint = if !prompt.use_app_key {
            "[Tab] Switch field  [Enter] Mount  [Esc] Cancel"
        } else {
            "[Enter] Mount  [Esc] Cancel"
        };
        let hint = Paragraph::new(hint)
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
    use moku_core::{AppContext, MokuConfig, StorageManager, TuiModule};
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

    fn mount_prompt(use_app_key: bool) -> PasswordPrompt {
        PasswordPrompt {
            volume_id: "vol-1".to_string(),
            display_name: "vol-1".to_string(),
            mountpoint: "M:".to_string(),
            focus: MountField::Mountpoint,
            input: Zeroizing::new(String::new()),
            use_app_key,
        }
    }

    #[tokio::test]
    async fn test_mount_prompt_char_input_routes_to_mountpoint_when_no_password_needed() {
        let mut module = VolumeManagerModule::new();
        module.prompt = Some(mount_prompt(true));
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Char('X')), &mut ctx)
            .await
            .unwrap();
        assert_eq!(module.prompt.as_ref().unwrap().mountpoint, "M:X");
    }

    #[tokio::test]
    async fn test_mount_prompt_tab_switches_fields_when_password_needed() {
        let mut module = VolumeManagerModule::new();
        module.prompt = Some(mount_prompt(false));
        let mut ctx = create_test_context().await;

        assert_eq!(
            module.prompt.as_ref().unwrap().focus,
            MountField::Mountpoint
        );
        module
            .handle_event(&key(KeyCode::Tab), &mut ctx)
            .await
            .unwrap();
        assert_eq!(module.prompt.as_ref().unwrap().focus, MountField::Password);
        module
            .handle_event(&key(KeyCode::Tab), &mut ctx)
            .await
            .unwrap();
        assert_eq!(
            module.prompt.as_ref().unwrap().focus,
            MountField::Mountpoint
        );
    }

    #[tokio::test]
    async fn test_mount_prompt_enter_with_key_submits_and_closes() {
        let mut module = VolumeManagerModule::new();
        module.prompt = Some(mount_prompt(true));
        let mut ctx = create_test_context().await;
        let app_key = SecurityManager::derive_key("pw", &[2u8; 16]).await.unwrap();
        ctx.session.unlock(app_key);

        module
            .handle_event(&key(KeyCode::Enter), &mut ctx)
            .await
            .unwrap();
        assert!(module.prompt.is_none());
        assert!(module.busy, "an unlocked vault starts the mount");
    }

    /// Regression: the prompt used to capture the app master key when it
    /// opened, so locking the vault while it was open didn't stop the
    /// mount (and kept the key alive in memory).
    #[tokio::test]
    async fn test_mount_prompt_enter_after_the_vault_was_locked_does_not_mount() {
        let mut module = VolumeManagerModule::new();
        let mut ctx = create_test_context().await;
        let app_key = SecurityManager::derive_key("pw", &[2u8; 16]).await.unwrap();
        ctx.session.unlock(app_key);
        module.prompt = Some(mount_prompt(true));

        ctx.session.lock();
        module
            .handle_event(&key(KeyCode::Enter), &mut ctx)
            .await
            .unwrap();
        assert!(module.prompt.is_none());
        assert!(!module.busy, "no mount may start with a locked vault");
    }

    #[tokio::test]
    async fn test_mount_prompt_enter_without_key_submits_and_closes() {
        let mut module = VolumeManagerModule::new();
        let mut prompt = mount_prompt(false);
        prompt.focus = MountField::Password;
        prompt.input = Zeroizing::new("hunter2".to_string());
        module.prompt = Some(prompt);
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Enter), &mut ctx)
            .await
            .unwrap();
        assert!(module.prompt.is_none());
    }

    #[tokio::test]
    async fn test_mount_prompt_esc_cancels() {
        let mut module = VolumeManagerModule::new();
        module.prompt = Some(mount_prompt(false));
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Esc), &mut ctx)
            .await
            .unwrap();
        assert!(module.prompt.is_none());
    }
}
