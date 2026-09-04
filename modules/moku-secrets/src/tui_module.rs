use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use moku_core::{
    AppContext, Command, ModuleId, ModuleMeta, ModuleStatus, MokuTheme, TuiModule, resolve_event,
};

use crate::engine::{self, PlainFormat};
use crate::generator::{self, CharsetOptions};
use crate::model::SecretEntry;

/// Which field of the (name, value) quick-add flow is currently being
/// typed. v1's TUI add flow is deliberately scoped to just these two
/// fields — category/username/url/notes stay CLI-only, same kind of
/// scope cut as `VaultManagerModule` not exposing `create`/`resize`.
#[derive(PartialEq, Eq)]
enum AddStage {
    Name,
    Value,
}

struct AddState {
    stage: AddStage,
    name: String,
    value: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExportKind {
    Encrypted,
    Json,
    Csv,
}

enum ExportStage {
    ChooseFormat,
    Path,
    Password,
}

struct ExportState {
    stage: ExportStage,
    kind: Option<ExportKind>,
    path: String,
    password: String,
}

pub struct SecretsModule {
    entries: Vec<SecretEntry>,
    state: ListState,
    reveal: bool,
    message: Option<(String, Instant)>,
    add: Option<AddState>,
    export: Option<ExportState>,
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
            KeyCode::Enter => match add.stage {
                AddStage::Name => {
                    if add.name.trim().is_empty() {
                        self.show_message("Name cannot be empty.");
                    } else if engine::find_by_name(&self.entries, &add.name).is_some() {
                        self.show_message(format!("'{}' already exists.", add.name));
                    } else {
                        add.stage = AddStage::Value;
                    }
                }
                AddStage::Value => {
                    keep_open = false;
                    let name = add.name.trim().to_string();
                    if add.value.is_empty() {
                        self.show_message("Value cannot be empty — entry not added.");
                    } else {
                        self.entries
                            .push(SecretEntry::new(name.clone(), add.value.clone()));
                        self.state.select(Some(self.entries.len() - 1));
                        self.save(ctx).await;
                        self.show_message(format!("Added '{name}'"));
                    }
                }
            },
            // Ctrl+G on the value field: auto-generate instead of typing.
            KeyCode::Char('g')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && add.stage == AddStage::Value =>
            {
                match generator::generate_charset_password(&CharsetOptions::default()) {
                    Ok(pw) => add.value = pw,
                    Err(e) => self.show_message(format!("Generate failed: {e}")),
                }
            }
            KeyCode::Char(c) => match add.stage {
                AddStage::Name => add.name.push(c),
                AddStage::Value => add.value.push(c),
            },
            KeyCode::Backspace => match add.stage {
                AddStage::Name => {
                    add.name.pop();
                }
                AddStage::Value => {
                    add.value.pop();
                }
            },
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
        match &export.stage {
            ExportStage::ChooseFormat => match key.code {
                KeyCode::Char('1') => {
                    export.kind = Some(ExportKind::Encrypted);
                    export.stage = ExportStage::Path;
                }
                KeyCode::Char('2') => {
                    export.kind = Some(ExportKind::Json);
                    export.stage = ExportStage::Path;
                }
                KeyCode::Char('3') => {
                    export.kind = Some(ExportKind::Csv);
                    export.stage = ExportStage::Path;
                }
                KeyCode::Esc => self.export = None,
                _ => return false,
            },
            ExportStage::Path => match key.code {
                KeyCode::Enter => {
                    if export.path.trim().is_empty() {
                        self.show_message("Path cannot be empty.");
                    } else if export.kind == Some(ExportKind::Encrypted) {
                        export.stage = ExportStage::Password;
                    } else {
                        self.finish_export().await;
                    }
                }
                KeyCode::Esc => self.export = None,
                KeyCode::Char(c) => export.path.push(c),
                KeyCode::Backspace => {
                    export.path.pop();
                }
                _ => return false,
            },
            ExportStage::Password => match key.code {
                KeyCode::Enter => self.finish_export().await,
                KeyCode::Esc => self.export = None,
                KeyCode::Char(c) => export.password.push(c),
                KeyCode::Backspace => {
                    export.password.pop();
                }
                _ => return false,
            },
        }
        true
    }

    async fn finish_export(&mut self) {
        let Some(export) = self.export.take() else {
            return;
        };
        let Some(kind) = export.kind else { return };
        let result = match kind {
            ExportKind::Json => engine::export_plain(&self.entries, PlainFormat::Json)
                .and_then(|b| std::fs::write(&export.path, b).map_err(Into::into)),
            ExportKind::Csv => engine::export_plain(&self.entries, PlainFormat::Csv)
                .and_then(|b| std::fs::write(&export.path, b).map_err(Into::into)),
            ExportKind::Encrypted => {
                match engine::export_encrypted(&self.entries, &export.password).await {
                    Ok(bytes) => std::fs::write(&export.path, bytes).map_err(Into::into),
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

    fn draw_add(&self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let Some(add) = &self.add else { return };
        let chunks = Layout::vertical([
            Constraint::Percentage(40),
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Percentage(40),
        ])
        .split(area);
        let input_chunk = Layout::horizontal([
            Constraint::Percentage(25),
            Constraint::Percentage(50),
            Constraint::Percentage(25),
        ])
        .split(chunks[1])[1];

        let (title, shown) = match add.stage {
            AddStage::Name => (" New secret — name ", add.name.clone()),
            AddStage::Value => (
                " New secret — value (Ctrl+G to generate) ",
                "•".repeat(add.value.chars().count()),
            ),
        };
        let p = Paragraph::new(shown)
            .block(
                Block::default()
                    .title(title)
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.info)),
            )
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg));
        frame.render_widget(p, input_chunk);

        let hint = Paragraph::new("[Enter] next/confirm  [Esc] cancel")
            .alignment(Alignment::Center)
            .style(Style::default().fg(theme.base_fg));
        frame.render_widget(hint, chunks[2]);
    }

    fn draw_export(&self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let Some(export) = &self.export else { return };
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

        let (title, shown) = match export.stage {
            ExportStage::ChooseFormat => {
                (" Export — [1] Encrypted  [2] JSON  [3] CSV ", String::new())
            }
            ExportStage::Path => (" Export — output file path ", export.path.clone()),
            ExportStage::Password => (
                " Export — new password for this backup ",
                "•".repeat(export.password.chars().count()),
            ),
        };
        let p = Paragraph::new(shown)
            .block(
                Block::default()
                    .title(title)
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.info)),
            )
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg));
        frame.render_widget(p, input_chunk);

        let hint = Paragraph::new("[Enter] confirm  [Esc] cancel")
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
                    stage: AddStage::Name,
                    name: String::new(),
                    value: String::new(),
                });
            }
            KeyCode::Char('d') => self.delete_selected(ctx).await,
            KeyCode::Char('r') => self.reveal = !self.reveal,
            KeyCode::Char('e') => {
                self.export = Some(ExportState {
                    stage: ExportStage::ChooseFormat,
                    kind: None,
                    path: String::new(),
                    password: String::new(),
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
