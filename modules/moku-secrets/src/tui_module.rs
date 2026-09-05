use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use zeroize::Zeroizing;

use moku_core::{
    AppContext, Command, ModuleId, ModuleMeta, ModuleStatus, MokuTheme, TuiModule, resolve_event,
};

use crate::engine::{self, PlainFormat};
use crate::generator::{self, CharsetOptions};
use crate::model::SecretEntry;

/// Which field of the (name, value) quick-add form currently has keyboard
/// focus. Both fields are shown at once — `Tab` switches focus, `Enter`
/// submits from either — matching `moku-vault-daemon`'s `CreateForm`/
/// `CreateField` shape, not a sequential per-field wizard. v1's TUI add
/// flow is deliberately scoped to just these two fields —
/// category/username/url/notes stay CLI-only, same kind of scope cut as
/// `VaultManagerModule` not exposing `create`/`resize`.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum AddField {
    Name,
    Value,
}

struct AddState {
    focus: AddField,
    name: String,
    /// Zeroized on drop — this is the secret itself while it's being
    /// typed, not just a display string.
    value: Zeroizing<String>,
    error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ExportKind {
    Encrypted,
    Json,
    Csv,
}

/// Which field of the export form has focus — `Format` isn't a text field
/// (`←`/`→`/`Space` cycle it, same as `CreateForm`'s `Mode`); `Password`
/// only matters (and is only reachable via `Tab`) when `kind ==
/// Encrypted`.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum ExportField {
    Format,
    Path,
    Password,
}

struct ExportState {
    focus: ExportField,
    kind: ExportKind,
    path: String,
    /// Zeroized on drop — a fresh backup password, typed once (no
    /// confirmation field: unlike a vault's own password this only
    /// protects a single throwaway export file, so a typo just means a
    /// bad backup rather than a locked-out vault).
    password: Zeroizing<String>,
    error: Option<String>,
}

pub struct SecretsModule {
    entries: Vec<SecretEntry>,
    state: ListState,
    reveal: bool,
    message: Option<(String, Instant)>,
    add: Option<AddState>,
    export: Option<ExportState>,
    /// `Some(index)` while waiting for the user to confirm deleting that
    /// entry — plain `d` sets this instead of deleting right away;
    /// `Shift+D` (`moku_core::is_delete_bypass`) still deletes immediately.
    confirm_delete: Option<usize>,
}

impl SecretsModule {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            state: ListState::default(),
            reveal: false,
            message: None,
            add: None,
            export: None,
            confirm_delete: None,
        }
    }

    async fn save(&self, ctx: &mut AppContext) {
        if let Err(e) = engine::save_entries(&ctx.storage, &ctx.config.load(), &self.entries).await
        {
            ctx.show_error(format!("Save error: {e}"));
        }
    }

    fn selected(&self) -> Option<&SecretEntry> {
        self.state.selected().and_then(|i| self.entries.get(i))
    }

    fn select_next(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let i = match self.state.selected() {
            Some(i) => (i + 1) % self.entries.len(),
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn select_previous(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let i = match self.state.selected() {
            Some(i) => (i + self.entries.len() - 1) % self.entries.len(),
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn show_message(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now()));
    }

    async fn delete_selected(&mut self, ctx: &mut AppContext) {
        let Some(i) = self.state.selected() else {
            return;
        };
        if i >= self.entries.len() {
            return;
        }
        let removed = self.entries.remove(i);
        if self.entries.is_empty() {
            self.state.select(None);
        } else if i >= self.entries.len() {
            self.state.select(Some(self.entries.len() - 1));
        }
        self.save(ctx).await;
        self.show_message(format!("Deleted '{}'", removed.name));
    }

    /// `y`/Enter confirms the pending delete (`confirm_delete`), `n`/Esc
    /// cancels — either way returns to the normal list view.
    async fn handle_confirm_delete_key(&mut self, event: &Event, ctx: &mut AppContext) -> bool {
        match moku_core::resolve_confirm_delete_key(event) {
            moku_core::ConfirmDeleteKey::Confirm => {
                if self.confirm_delete.take().is_some() {
                    // `delete_selected` reads `self.state.selected()`,
                    // which still points at the entry pending deletion —
                    // normal navigation is blocked while confirm_delete is
                    // Some, so the selection can't have moved since.
                    self.delete_selected(ctx).await;
                }
            }
            moku_core::ConfirmDeleteKey::Cancel => {
                self.confirm_delete = None;
            }
            moku_core::ConfirmDeleteKey::Other => return false,
        }
        true
    }

    async fn handle_add_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        ctx: &mut AppContext,
    ) -> bool {
        // Taken out of `self` up front so nothing here holds a `&mut
        // self.add` borrow while also calling `self.show_message`/`self.save`
        // (both take `&mut self`).
        let Some(mut add) = self.add.take() else {
            return false;
        };
        let mut keep_open = true;

        match key.code {
            KeyCode::Esc => keep_open = false,
            KeyCode::Tab => {
                add.focus = match add.focus {
                    AddField::Name => AddField::Value,
                    AddField::Value => AddField::Name,
                };
            }
            KeyCode::Enter => {
                let name = add.name.trim().to_string();
                if name.is_empty() {
                    add.error = Some("Name cannot be empty.".to_string());
                } else if engine::find_by_name(&self.entries, &name).is_some() {
                    add.error = Some(format!("'{name}' already exists."));
                } else if add.value.is_empty() {
                    add.error = Some("Value cannot be empty.".to_string());
                } else {
                    keep_open = false;
                    self.entries
                        .push(SecretEntry::new(name.clone(), add.value.to_string()));
                    self.state.select(Some(self.entries.len() - 1));
                    self.save(ctx).await;
                    self.show_message(format!("Added '{name}'"));
                }
            }
            // Ctrl+G on the value field: auto-generate instead of typing.
            KeyCode::Char('g')
                if key.modifiers.contains(KeyModifiers::CONTROL) && add.focus == AddField::Value =>
            {
                match generator::generate_charset_password(&CharsetOptions::default()) {
                    Ok(pw) => add.value = Zeroizing::new(pw),
                    Err(e) => add.error = Some(format!("Generate failed: {e}")),
                }
            }
            KeyCode::Char(c) => {
                match add.focus {
                    AddField::Name => add.name.push(c),
                    AddField::Value => add.value.push(c),
                }
                add.error = None;
            }
            KeyCode::Backspace => {
                match add.focus {
                    AddField::Name => {
                        add.name.pop();
                    }
                    AddField::Value => {
                        add.value.pop();
                    }
                }
                add.error = None;
            }
            _ => {}
        }

        if keep_open {
            self.add = Some(add);
        }
        true
    }

    async fn handle_export_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        let Some(export) = &mut self.export else {
            return false;
        };
        match key.code {
            KeyCode::Esc => self.export = None,
            KeyCode::Tab => {
                export.focus = match export.focus {
                    ExportField::Format => ExportField::Path,
                    ExportField::Path if export.kind == ExportKind::Encrypted => ExportField::Password,
                    ExportField::Path => ExportField::Format,
                    ExportField::Password => ExportField::Format,
                };
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if export.focus == ExportField::Format =>
            {
                export.kind = match export.kind {
                    ExportKind::Encrypted => ExportKind::Json,
                    ExportKind::Json => ExportKind::Csv,
                    ExportKind::Csv => ExportKind::Encrypted,
                };
                export.error = None;
            }
            KeyCode::Enter => {
                if export.path.trim().is_empty() {
                    export.error = Some("Path cannot be empty.".to_string());
                } else {
                    self.finish_export().await;
                }
            }
            KeyCode::Char(c) => {
                match export.focus {
                    ExportField::Format => {}
                    ExportField::Path => export.path.push(c),
                    ExportField::Password => export.password.push(c),
                }
                export.error = None;
            }
            KeyCode::Backspace => {
                match export.focus {
                    ExportField::Format => {}
                    ExportField::Path => {
                        export.path.pop();
                    }
                    ExportField::Password => {
                        export.password.pop();
                    }
                }
                export.error = None;
            }
            _ => return false,
        }
        true
    }

    async fn finish_export(&mut self) {
        let Some(export) = self.export.take() else {
            return;
        };
        let result: Result<()> = match export.kind {
            ExportKind::Json => match engine::export_plain(&self.entries, PlainFormat::Json) {
                Ok(bytes) => tokio::fs::write(&export.path, bytes).await.map_err(Into::into),
                Err(e) => Err(e),
            },
            ExportKind::Csv => match engine::export_plain(&self.entries, PlainFormat::Csv) {
                Ok(bytes) => tokio::fs::write(&export.path, bytes).await.map_err(Into::into),
                Err(e) => Err(e),
            },
            ExportKind::Encrypted => {
                match engine::export_encrypted(&self.entries, &export.password).await {
                    Ok(bytes) => tokio::fs::write(&export.path, bytes).await.map_err(Into::into),
                    Err(e) => Err(e),
                }
            }
        };
        match result {
            Ok(()) => self.show_message(format!(
                "Exported {} entries to {}",
                self.entries.len(),
                export.path
            )),
            Err(e) => self.show_message(format!("Export failed: {e}")),
        }
    }

    fn draw_confirm_delete(&self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let Some(i) = self.confirm_delete else { return };
        let name = self
            .entries
            .get(i)
            .map(|e| e.name.as_str())
            .unwrap_or("this entry");

        let chunks = Layout::vertical([
            Constraint::Percentage(40),
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Percentage(40),
        ])
        .split(area);
        let input_chunk = Layout::horizontal([
            Constraint::Percentage(20),
            Constraint::Percentage(60),
            Constraint::Percentage(20),
        ])
        .split(chunks[1])[1];

        let p = Paragraph::new(format!("Delete '{name}'?"))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .title(" Confirm Delete ")
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.error)),
            )
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg));
        frame.render_widget(p, input_chunk);

        let hint = Paragraph::new("[y] Yes  [n] No")
            .alignment(Alignment::Center)
            .style(Style::default().fg(theme.base_fg));
        frame.render_widget(hint, chunks[2]);
    }

    fn draw_add(&self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let Some(add) = &self.add else { return };
        let chunks = Layout::vertical([
            Constraint::Percentage(35),
            Constraint::Length(4),
            Constraint::Length(2),
            Constraint::Percentage(35),
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

        let masked_value: String = add.value.chars().map(|_| '•').collect();
        let mut lines = vec![
            Line::styled(
                format!("{} Name:  {}", marker(add.focus == AddField::Name), add.name),
                field_style(add.focus == AddField::Name),
            ),
            Line::styled(
                format!("{} Value: {}", marker(add.focus == AddField::Value), masked_value),
                field_style(add.focus == AddField::Value),
            ),
        ];
        if let Some(err) = &add.error {
            lines.push(Line::styled(format!("  {err}"), Style::default().fg(theme.error)));
        }

        let p = Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" New Secret ")
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.info)),
            )
            .style(Style::default().bg(theme.base_bg));
        frame.render_widget(p, box_area);

        let hint = Paragraph::new("[Tab] Switch field  [Ctrl+G] Generate  [Enter] Add  [Esc] Cancel")
            .alignment(Alignment::Center)
            .style(Style::default().fg(theme.base_fg));
        frame.render_widget(hint, chunks[2]);
    }

    fn draw_export(&self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let Some(export) = &self.export else { return };
        let chunks = Layout::vertical([
            Constraint::Percentage(35),
            Constraint::Length(5),
            Constraint::Length(2),
            Constraint::Percentage(35),
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

        let format_label = match export.kind {
            ExportKind::Encrypted => "Encrypted",
            ExportKind::Json => "JSON",
            ExportKind::Csv => "CSV",
        };

        let mut lines = vec![
            Line::styled(
                format!(
                    "{} Format: {}  (\u{2190}/\u{2192} to change)",
                    marker(export.focus == ExportField::Format),
                    format_label
                ),
                field_style(export.focus == ExportField::Format),
            ),
            Line::styled(
                format!("{} Path:   {}", marker(export.focus == ExportField::Path), export.path),
                field_style(export.focus == ExportField::Path),
            ),
        ];
        if export.kind == ExportKind::Encrypted {
            let masked_password: String = export.password.chars().map(|_| '•').collect();
            lines.push(Line::styled(
                format!(
                    "{} Password: {}",
                    marker(export.focus == ExportField::Password),
                    masked_password
                ),
                field_style(export.focus == ExportField::Password),
            ));
        }
        if let Some(err) = &export.error {
            lines.push(Line::styled(format!("  {err}"), Style::default().fg(theme.error)));
        }

        let p = Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Export Secrets ")
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.info)),
            )
            .style(Style::default().bg(theme.base_bg));
        frame.render_widget(p, box_area);

        let hint = Paragraph::new("[Tab] Switch field  [\u{2190}/\u{2192}] Change format  [Enter] Export  [Esc] Cancel")
            .alignment(Alignment::Center)
            .style(Style::default().fg(theme.base_fg));
        frame.render_widget(hint, chunks[2]);
    }
}

impl Default for SecretsModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for SecretsModule {
    fn id(&self) -> ModuleId {
        ModuleId::SECRETS
    }
    fn title(&self) -> &'static str {
        ModuleId::SECRETS.title()
    }
}

#[async_trait]
impl TuiModule for SecretsModule {
    async fn init(&mut self, ctx: &mut AppContext) -> Result<()> {
        self.entries = engine::load_entries(&ctx.storage).await;
        if !self.entries.is_empty() {
            self.state.select(Some(0));
        }
        Ok(())
    }

    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        let Event::Key(key) = event else {
            return Ok(false);
        };
        if key.kind != KeyEventKind::Press {
            return Ok(false);
        }

        if self.add.is_some() {
            return Ok(self.handle_add_key(*key, ctx).await);
        }
        if self.export.is_some() {
            return Ok(self.handle_export_key(*key).await);
        }
        if self.confirm_delete.is_some() {
            return Ok(self.handle_confirm_delete_key(event, ctx).await);
        }

        // Shift+D bypasses the confirmation prompt entirely and deletes
        // immediately — checked as a raw key before the normal dispatch,
        // same shape as other raw Shift-key checks in this app.
        if moku_core::is_delete_bypass(event) {
            self.delete_selected(ctx).await;
            return Ok(true);
        }

        let command = resolve_event(event, &ctx.config.load().keys, None);
        match command {
            Command::Up => self.select_previous(),
            Command::Down => self.select_next(),
            Command::Back | Command::Quit => {
                ctx.navigate_to(ModuleId::LAUNCHER);
                return Ok(true);
            }
            _ => {}
        }

        match key.code {
            KeyCode::Char('a') => {
                self.add = Some(AddState {
                    focus: AddField::Name,
                    name: String::new(),
                    value: Zeroizing::new(String::new()),
                    error: None,
                });
            }
            KeyCode::Char('d') => {
                if self.state.selected().is_some() {
                    self.confirm_delete = self.state.selected();
                }
            }
            KeyCode::Char('r') => self.reveal = !self.reveal,
            KeyCode::Char('e') => {
                self.export = Some(ExportState {
                    focus: ExportField::Format,
                    kind: ExportKind::Encrypted,
                    path: String::new(),
                    password: Zeroizing::new(String::new()),
                    error: None,
                });
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        if let Some((_, at)) = self.message
            && at.elapsed() > Duration::from_secs(6)
        {
            self.message = None;
        }

        if self.add.is_some() {
            self.draw_add(frame, area, theme);
            return;
        }
        if self.export.is_some() {
            self.draw_export(frame, area, theme);
            return;
        }
        if self.confirm_delete.is_some() {
            self.draw_confirm_delete(frame, area, theme);
            return;
        }

        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);
        let panes = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(chunks[0]);

        let items: Vec<ListItem> = if self.entries.is_empty() {
            vec![ListItem::new("  No secrets yet. [a] to add one.")]
        } else {
            self.entries
                .iter()
                .map(|e| {
                    ListItem::new(format!(
                        "{} [{}]",
                        e.name,
                        e.category.as_deref().unwrap_or("-")
                    ))
                    .style(Style::default().fg(theme.base_fg))
                })
                .collect()
        };
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Secrets ")
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
        frame.render_stateful_widget(list, panes[0], &mut self.state);

        let detail = if let Some(entry) = self.selected() {
            let masked = "•".repeat(entry.value.chars().count());
            let value_line = if self.reveal {
                entry.value.as_str()
            } else {
                masked.as_str()
            };
            let totp_line = entry
                .totp_seed
                .as_ref()
                .map(|seed| match engine::totp_code_now(seed) {
                    Ok(code) => format!("TOTP:     {code}"),
                    Err(e) => format!("TOTP:     error ({e})"),
                })
                .unwrap_or_else(|| "TOTP:     -".to_string());
            format!(
                "Name:     {}\nCategory: {}\nUsername: {}\nURL:      {}\nValue:    {}\n{}\nNotes:    {}",
                entry.name,
                entry.category.as_deref().unwrap_or("-"),
                entry.username.as_deref().unwrap_or("-"),
                entry.url.as_deref().unwrap_or("-"),
                value_line,
                totp_line,
                entry.notes.as_deref().unwrap_or("-"),
            )
        } else {
            "  (nothing selected)".to_string()
        };
        let detail_widget = Paragraph::new(detail)
            .style(Style::default().fg(theme.base_fg))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Details ")
                    .border_style(Style::default().fg(theme.border))
                    .style(Style::default().bg(theme.base_bg)),
            );
        frame.render_widget(detail_widget, panes[1]);

        let help = self
            .message
            .as_ref()
            .map(|(m, _)| m.clone())
            .unwrap_or_else(|| {
                " [a] Add  [d] Delete  [r] Reveal  [e] Export  [Esc] Back ".to_string()
            });
        let help_widget = Paragraph::new(help)
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border)),
            );
        frame.render_widget(help_widget, chunks[1]);
    }

    async fn dashboard_summary(&self, ctx: &AppContext) -> Option<ModuleStatus> {
        let needs_vault =
            moku_core::resolve_encryption(&ctx.config.load(), ModuleId::SECRETS.as_str(), true);
        if needs_vault && !ctx.session.is_unlocked() {
            return Some(ModuleStatus::locked());
        }
        let entries = engine::load_entries(&ctx.storage).await;
        Some(ModuleStatus::normal(format!("{} secrets", entries.len())))
    }
}

#[cfg(test)]
mod confirm_delete_tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use moku_core::security::{SecurityManager, VaultSession};
    use moku_core::{MokuConfig, StorageManager};
    use tempfile::tempdir;

    use super::*;
    use crate::model::SecretEntry;

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

    async fn module_with_one_entry() -> (SecretsModule, AppContext) {
        let mut module = SecretsModule::new();
        let ctx = create_test_context().await;
        let key = SecurityManager::derive_key("test-pass", &[9u8; 16])
            .await
            .unwrap();
        ctx.session.unlock(key);
        module.entries = vec![SecretEntry::new(
            "github".to_string(),
            "hunter2".to_string(),
        )];
        module.state.select(Some(0));
        (module, ctx)
    }

    #[tokio::test]
    async fn test_plain_d_does_not_delete_and_opens_confirmation() {
        let (mut module, mut ctx) = module_with_one_entry().await;
        let event = Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()));
        module.handle_event(&event, &mut ctx).await.unwrap();

        assert_eq!(
            module.entries.len(),
            1,
            "plain 'd' must not delete anything"
        );
        assert_eq!(module.confirm_delete, Some(0));
    }

    #[tokio::test]
    async fn test_shift_d_deletes_immediately() {
        let (mut module, mut ctx) = module_with_one_entry().await;
        let event = Event::Key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT));
        module.handle_event(&event, &mut ctx).await.unwrap();

        assert!(
            module.entries.is_empty(),
            "Shift+D should delete immediately"
        );
    }

    #[tokio::test]
    async fn test_confirm_delete_yes_deletes() {
        let (mut module, mut ctx) = module_with_one_entry().await;
        module.confirm_delete = Some(0);
        let event = Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::empty()));
        module.handle_event(&event, &mut ctx).await.unwrap();

        assert!(module.entries.is_empty());
        assert!(module.confirm_delete.is_none());
    }

    #[tokio::test]
    async fn test_confirm_delete_no_cancels() {
        let (mut module, mut ctx) = module_with_one_entry().await;
        module.confirm_delete = Some(0);
        let event = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        module.handle_event(&event, &mut ctx).await.unwrap();

        assert_eq!(
            module.entries.len(),
            1,
            "cancelling must not delete anything"
        );
        assert!(module.confirm_delete.is_none());
    }
}

#[cfg(test)]
mod dashboard_summary_tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use moku_core::security::{SecurityManager, VaultSession};
    use moku_core::{MokuConfig, StorageManager};
    use tempfile::tempdir;

    use super::*;
    use crate::model::SecretEntry;

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

    #[tokio::test]
    async fn test_dashboard_summary_locked_when_vault_not_unlocked() {
        let module = SecretsModule::new();
        let ctx = create_test_context().await;
        let status = module.dashboard_summary(&ctx).await.unwrap();
        assert_eq!(status.tone, moku_core::StatusTone::Locked);
    }

    #[tokio::test]
    async fn test_dashboard_summary_reports_count_when_unlocked() {
        let module = SecretsModule::new();
        let ctx = create_test_context().await;
        let key = SecurityManager::derive_key("test-pass", &[5u8; 16])
            .await
            .unwrap();
        ctx.session.unlock(key);

        let entries = vec![
            SecretEntry::new("github".to_string(), "hunter2".to_string()),
            SecretEntry::new("aws".to_string(), "sekrit".to_string()),
        ];
        engine::save_entries(&ctx.storage, &ctx.config.load(), &entries)
            .await
            .unwrap();

        let status = module.dashboard_summary(&ctx).await.unwrap();
        assert_eq!(status.tone, moku_core::StatusTone::Normal);
        assert_eq!(status.text, "2 secrets");
    }
}

#[cfg(test)]
mod add_export_form_tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use moku_core::security::{SecurityManager, VaultSession};
    use moku_core::{MokuConfig, StorageManager};
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

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    fn new_add_module() -> SecretsModule {
        let mut module = SecretsModule::new();
        module.add = Some(AddState {
            focus: AddField::Name,
            name: String::new(),
            value: Zeroizing::new(String::new()),
            error: None,
        });
        module
    }

    fn new_export_module() -> SecretsModule {
        let mut module = SecretsModule::new();
        module.export = Some(ExportState {
            focus: ExportField::Format,
            kind: ExportKind::Encrypted,
            path: String::new(),
            password: Zeroizing::new(String::new()),
            error: None,
        });
        module
    }

    #[tokio::test]
    async fn test_add_tab_cycles_focus_between_name_and_value() {
        let mut module = new_add_module();
        let mut ctx = create_test_context().await;

        assert_eq!(module.add.as_ref().unwrap().focus, AddField::Name);
        module.handle_event(&key(KeyCode::Tab), &mut ctx).await.unwrap();
        assert_eq!(module.add.as_ref().unwrap().focus, AddField::Value);
        module.handle_event(&key(KeyCode::Tab), &mut ctx).await.unwrap();
        assert_eq!(module.add.as_ref().unwrap().focus, AddField::Name);
    }

    #[tokio::test]
    async fn test_add_char_input_routes_to_the_focused_field() {
        let mut module = new_add_module();
        let mut ctx = create_test_context().await;

        module.handle_event(&key(KeyCode::Char('g')), &mut ctx).await.unwrap();
        assert_eq!(module.add.as_ref().unwrap().name, "g");

        module.handle_event(&key(KeyCode::Tab), &mut ctx).await.unwrap();
        module.handle_event(&key(KeyCode::Char('h')), &mut ctx).await.unwrap();
        assert_eq!(module.add.as_ref().unwrap().value.as_str(), "h");
    }

    #[tokio::test]
    async fn test_add_enter_with_empty_name_sets_error_and_keeps_form_open() {
        let mut module = new_add_module();
        let mut ctx = create_test_context().await;

        module.handle_event(&key(KeyCode::Enter), &mut ctx).await.unwrap();
        let add = module.add.as_ref().expect("form should stay open");
        assert!(add.error.is_some());
    }

    #[tokio::test]
    async fn test_add_enter_with_empty_value_sets_error_and_keeps_form_open() {
        let mut module = new_add_module();
        module.add.as_mut().unwrap().name = "github".to_string();
        let mut ctx = create_test_context().await;

        module.handle_event(&key(KeyCode::Enter), &mut ctx).await.unwrap();
        let add = module.add.as_ref().expect("form should stay open");
        assert!(add.error.as_ref().unwrap().contains("Value"));
    }

    #[tokio::test]
    async fn test_add_enter_with_valid_fields_submits_and_closes() {
        let mut module = new_add_module();
        {
            let add = module.add.as_mut().unwrap();
            add.name = "github".to_string();
            add.value = Zeroizing::new("hunter2".to_string());
        }
        let mut ctx = create_test_context().await;

        module.handle_event(&key(KeyCode::Enter), &mut ctx).await.unwrap();
        assert!(module.add.is_none(), "valid fields should submit and close the form");
        assert_eq!(module.entries.len(), 1);
        assert_eq!(module.entries[0].name, "github");
        assert_eq!(module.entries[0].value.as_str(), "hunter2");
    }

    #[tokio::test]
    async fn test_add_esc_cancels_the_form() {
        let mut module = new_add_module();
        let mut ctx = create_test_context().await;

        module.handle_event(&key(KeyCode::Esc), &mut ctx).await.unwrap();
        assert!(module.add.is_none());
    }

    #[test]
    fn test_add_render_shows_masked_value_not_plaintext() {
        let mut module = new_add_module();
        module.add.as_mut().unwrap().value = Zeroizing::new("hunter2".to_string());

        let (width, height) = (60u16, 20u16);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = MokuTheme::default();
        terminal
            .draw(|frame| module.draw(frame, Rect::new(0, 0, width, height), &theme))
            .unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(!content.contains("hunter2"));
        assert!(content.contains("•"));
    }

    #[tokio::test]
    async fn test_export_tab_cycles_through_password_when_encrypted() {
        let mut module = new_export_module(); // kind starts as Encrypted
        let mut ctx = create_test_context().await;

        assert_eq!(module.export.as_ref().unwrap().focus, ExportField::Format);
        module.handle_event(&key(KeyCode::Tab), &mut ctx).await.unwrap();
        assert_eq!(module.export.as_ref().unwrap().focus, ExportField::Path);
        module.handle_event(&key(KeyCode::Tab), &mut ctx).await.unwrap();
        assert_eq!(module.export.as_ref().unwrap().focus, ExportField::Password);
        module.handle_event(&key(KeyCode::Tab), &mut ctx).await.unwrap();
        assert_eq!(module.export.as_ref().unwrap().focus, ExportField::Format);
    }

    #[tokio::test]
    async fn test_export_tab_skips_password_when_not_encrypted() {
        let mut module = new_export_module();
        module.export.as_mut().unwrap().kind = ExportKind::Json;
        let mut ctx = create_test_context().await;

        module.handle_event(&key(KeyCode::Tab), &mut ctx).await.unwrap();
        assert_eq!(module.export.as_ref().unwrap().focus, ExportField::Path);
        module.handle_event(&key(KeyCode::Tab), &mut ctx).await.unwrap();
        assert_eq!(
            module.export.as_ref().unwrap().focus,
            ExportField::Format,
            "JSON/CSV exports have no password field to tab into"
        );
    }

    #[tokio::test]
    async fn test_export_left_right_cycles_format() {
        let mut module = new_export_module();
        let mut ctx = create_test_context().await;

        assert_eq!(module.export.as_ref().unwrap().kind, ExportKind::Encrypted);
        module.handle_event(&key(KeyCode::Right), &mut ctx).await.unwrap();
        assert_eq!(module.export.as_ref().unwrap().kind, ExportKind::Json);
        module.handle_event(&key(KeyCode::Right), &mut ctx).await.unwrap();
        assert_eq!(module.export.as_ref().unwrap().kind, ExportKind::Csv);
        module.handle_event(&key(KeyCode::Right), &mut ctx).await.unwrap();
        assert_eq!(module.export.as_ref().unwrap().kind, ExportKind::Encrypted);
    }

    #[tokio::test]
    async fn test_export_enter_with_empty_path_sets_error_and_keeps_form_open() {
        let mut module = new_export_module();
        let mut ctx = create_test_context().await;

        module.handle_event(&key(KeyCode::Enter), &mut ctx).await.unwrap();
        let export = module.export.as_ref().expect("form should stay open");
        assert!(export.error.as_ref().unwrap().contains("Path"));
    }

    #[tokio::test]
    async fn test_export_esc_cancels_the_form() {
        let mut module = new_export_module();
        let mut ctx = create_test_context().await;

        module.handle_event(&key(KeyCode::Esc), &mut ctx).await.unwrap();
        assert!(module.export.is_none());
    }

    #[test]
    fn test_export_render_hides_password_field_for_non_encrypted_format() {
        let mut module = new_export_module();
        module.export.as_mut().unwrap().kind = ExportKind::Json;

        let (width, height) = (60u16, 20u16);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = MokuTheme::default();
        terminal
            .draw(|frame| module.draw(frame, Rect::new(0, 0, width, height), &theme))
            .unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(!content.contains("Password"));
    }

    #[test]
    fn test_export_render_shows_password_field_for_encrypted_format() {
        let mut module = new_export_module(); // kind starts as Encrypted

        let (width, height) = (60u16, 20u16);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = MokuTheme::default();
        terminal
            .draw(|frame| module.draw(frame, Rect::new(0, 0, width, height), &theme))
            .unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains("Password"));
    }
}
