use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use moku_core::{AppContext, Command, MokuConfig, resolve_event};

use super::SettingsTab;

/// (module id, ModuleMeta::encrypt_by_default()) — kept in sync with the
/// overrides in moku-todo/moku-bookmark/moku-rss/moku-secrets (see Faz 5)
/// and with moku-bin/src/config_cmd.rs's identical list for the CLI
/// equivalent. `secrets` is included so `[m]`/`[Shift+M]` can also
/// re-key it to the current per-module HKDF storage key scheme (see
/// `moku-core/src/storage/keys.rs`) — without it here, a user's real
/// secrets would never get an in-app trigger to migrate off the legacy
/// raw-master-key scheme.
const ENCRYPTABLE_MODULES: &[(&str, bool)] =
    &[("todo", true), ("bookmark", true), ("rss", false), ("secrets", true)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageOption {
    GlobalDefault,
    Module(usize),
    AutoLockTimeout,
}

impl StorageOption {
    fn all() -> Vec<Self> {
        let mut v = vec![Self::GlobalDefault];
        v.extend((0..ENCRYPTABLE_MODULES.len()).map(Self::Module));
        v.push(Self::AutoLockTimeout);
        v
    }

    fn label(&self) -> String {
        match self {
            Self::GlobalDefault => "Global Default".to_string(),
            Self::Module(i) => format!("  {}", ENCRYPTABLE_MODULES[*i].0),
            Self::AutoLockTimeout => "Auto-Lock Timeout".to_string(),
        }
    }
}

pub struct StorageTab {
    state: ListState,
    options: Vec<StorageOption>,
    key_cache: HashMap<String, String>,
    status_message: Option<(String, Instant)>,
    migration_result: Arc<Mutex<Option<String>>>,
    vault_unlocked: bool,
    /// Guards against mashing `m`/`Shift+M` before a prior migration's
    /// result has landed — without it, two concurrent
    /// `migrate_module_encryption` calls for the same module race on the
    /// same sled keys (same pattern/reasoning as
    /// `moku-vault-daemon`'s `busy` flag).
    busy: bool,
}

impl StorageTab {
    pub fn new(config: &MokuConfig) -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        let keys = config.get_module_keys("settings").unwrap_or_default();

        Self {
            state,
            options: StorageOption::all(),
            key_cache: keys,
            status_message: None,
            migration_result: Arc::new(Mutex::new(None)),
            vault_unlocked: false,
            busy: false,
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

    fn show_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some((msg.into(), Instant::now()));
    }

    fn set_module_override(&self, ctx: &mut AppContext, module: &str, value: Option<bool>) {
        ctx.update_config(|cfg| {
            let table = cfg
                .modules
                .entry(module.to_string())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            if let toml::Value::Table(t) = table {
                match value {
                    Some(v) => {
                        t.insert("encrypt".to_string(), toml::Value::Boolean(v));
                    }
                    None => {
                        t.remove("encrypt");
                    }
                }
            }
        });
    }

    fn change_value(&mut self, ctx: &mut AppContext, direction: i32) {
        let Some(i) = self.state.selected() else { return };
        match self.options[i] {
            StorageOption::GlobalDefault => {
                let current = ctx.config.load().storage.default_encrypt;
                ctx.update_config(|cfg| cfg.storage.default_encrypt = !current);
            }
            StorageOption::Module(idx) => {
                let (module, _) = ENCRYPTABLE_MODULES[idx];
                let current = ctx
                    .config
                    .load()
                    .modules
                    .get(module)
                    .and_then(|v| v.get("encrypt"))
                    .and_then(|v| v.as_bool());
                // Tri-state cycle: inherit(None) -> On -> Off -> inherit
                let next = match (current, direction >= 0) {
                    (None, true) => Some(true),
                    (Some(true), true) => Some(false),
                    (Some(false), true) => None,
                    (None, false) => Some(false),
                    (Some(false), false) => Some(true),
                    (Some(true), false) => None,
                };
                self.set_module_override(ctx, module, next);
            }
            StorageOption::AutoLockTimeout => {
                let step: i64 = 30;
                let current = ctx.config.load().storage.auto_lock_timeout as i64;
                let next = (current + direction as i64 * step).clamp(0, 3600) as u64;
                ctx.update_config(|cfg| cfg.storage.auto_lock_timeout = next);
            }
        }
    }

    /// Spawns the migration(s) in the background (sled I/O is local/fast,
    /// but this still avoids blocking the render loop) and stores a
    /// summary in `migration_result` for `handle_event` to pick up on the
    /// next event, mirroring modules/moku-rss/src/tui_module.rs's refresh
    /// pattern — SettingsTab::draw() only gets `&MokuConfig`, not
    /// `&mut AppContext`, so a toast can't be raised directly from there.
    fn migrate(&mut self, ctx: &AppContext, modules: Vec<(&'static str, bool)>) {
        if self.busy {
            return;
        }
        self.busy = true;

        let storage = Arc::clone(&ctx.storage);
        let session = Arc::clone(&ctx.session);
        let slot = Arc::clone(&self.migration_result);

        tokio::spawn(async move {
            let mut lines = Vec::new();
            for (module, target) in modules {
                if target && !session.is_unlocked() {
                    lines.push(format!("{module}: vault must be unlocked to migrate to encrypted"));
                    continue;
                }
                match storage.migrate_module_encryption(module, target).await {
                    Ok(report) => lines.push(format!(
                        "{module}: {} migrated, {} skipped, {} error(s)",
                        report.migrated,
                        report.skipped,
                        report.errors.len()
                    )),
                    Err(e) => lines.push(format!("{module}: {e}")),
                }
            }
            *slot.lock().unwrap() = Some(lines.join(" | "));
        });

        self.show_status("Migrating...");
    }

    fn poll_migration_result(&mut self) {
        let result = self.migration_result.lock().unwrap().take();
        if let Some(msg) = result {
            self.busy = false;
            self.show_status(msg);
        }
    }
}

impl SettingsTab for StorageTab {
    fn title(&self) -> &str {
        "Storage & Security"
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<()> {
        self.poll_migration_result();
        self.vault_unlocked = ctx.session.is_unlocked();

        if let Some((_, at)) = self.status_message {
            if at.elapsed() > Duration::from_secs(6) {
                self.status_message = None;
            }
        }

        let command = resolve_event(event, &ctx.config.load().keys, Some(&self.key_cache));

        match command {
            Command::Up => self.select_previous(),
            Command::Down => self.select_next(),
            Command::Right | Command::Confirm => self.change_value(ctx, 1),
            Command::Left => self.change_value(ctx, -1),
            _ => {}
        }

        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('l') if key.modifiers.is_empty() => {
                        if ctx.session.is_unlocked() {
                            ctx.session.lock();
                            self.show_status("Vault locked.");
                        } else {
                            self.show_status("Vault is already locked.");
                        }
                    }
                    KeyCode::Char('m') if key.modifiers.is_empty() => {
                        if let Some(StorageOption::Module(idx)) = self.state.selected().map(|i| self.options[i]) {
                            let (module, module_default) = ENCRYPTABLE_MODULES[idx];
                            let target = moku_core::resolve_encryption(&ctx.config.load(), module, module_default);
                            self.migrate(ctx, vec![(module, target)]);
                        }
                    }
                    KeyCode::Char('M') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        let cfg = ctx.config.load();
                        let modules: Vec<(&'static str, bool)> = ENCRYPTABLE_MODULES
                            .iter()
                            .map(|(m, d)| (*m, moku_core::resolve_encryption(&cfg, m, *d)))
                            .collect();
                        drop(cfg);
                        self.migrate(ctx, modules);
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, config: &MokuConfig) -> Result<()> {
        let theme = config.get_active_theme();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(2)])
            .split(area);

        let items: Vec<ListItem> = self
            .options
            .iter()
            .map(|opt| {
                let value_str = match opt {
                    StorageOption::GlobalDefault => {
                        if config.storage.default_encrypt { "Encrypted" } else { "Plaintext" }.to_string()
                    }
                    StorageOption::Module(idx) => {
                        let (module, module_default) = ENCRYPTABLE_MODULES[*idx];
                        let override_value = config
                            .modules
                            .get(module)
                            .and_then(|v| v.get("encrypt"))
                            .and_then(|v| v.as_bool());
                        match override_value {
                            None => format!(
                                "Inherit ({})",
                                if moku_core::resolve_encryption(config, module, module_default) {
                                    "encrypted"
                                } else {
                                    "plaintext"
                                }
                            ),
                            Some(true) => "Encrypted (override)".to_string(),
                            Some(false) => "Plaintext (override)".to_string(),
                        }
                    }
                    StorageOption::AutoLockTimeout => {
                        if config.storage.auto_lock_timeout == 0 {
                            "Off".to_string()
                        } else {
                            format!("{}s", config.storage.auto_lock_timeout)
                        }
                    }
                };
                let content = format!("{:<20}: [ {} ]", opt.label(), value_str);
                ListItem::new(Line::from(content)).style(Style::default().fg(theme.base_fg))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Storage & Security ")
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

        frame.render_stateful_widget(list, chunks[0], &mut self.state);

        let vault_state = if self.vault_unlocked { "Unlocked" } else { "Locked" };
        let (status, status_color) = self
            .status_message
            .as_ref()
            .map(|(m, _)| (m.clone(), theme.info))
            .unwrap_or_else(|| {
                (
                    format!(
                        "Vault: {vault_state} | [m] Migrate selected | [Shift+M] Migrate all | [l] Lock vault now"
                    ),
                    theme.base_fg,
                )
            });
        let status_widget = Paragraph::new(status).style(Style::default().fg(status_color));
        frame.render_widget(status_widget, chunks[1]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
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

    #[tokio::test]
    async fn test_busy_guard_blocks_a_second_migrate_while_one_is_in_flight() {
        let mut tab = StorageTab::new(&MokuConfig::default());
        let ctx = create_test_context().await;
        assert!(!tab.busy);

        tab.migrate(&ctx, vec![("todo", false)]);
        assert!(tab.busy, "starting a migration must set the busy flag");

        // Mashing 'm'/'Shift+M' again before the first migration's result
        // lands must not spawn a second concurrent
        // migrate_module_encryption call for the same module.
        tab.migrate(&ctx, vec![("todo", false)]);
        assert!(tab.busy);
    }

    #[test]
    fn test_poll_migration_result_clears_busy_flag() {
        let mut tab = StorageTab::new(&MokuConfig::default());
        tab.busy = true;
        *tab.migration_result.lock().unwrap() = Some("done".to_string());
        tab.poll_migration_result();
        assert!(
            !tab.busy,
            "picking up a finished migration's result must clear busy so the next one can start"
        );
    }
}
