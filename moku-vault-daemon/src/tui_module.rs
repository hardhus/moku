use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use moku_core::{
    AppContext, Command, ModuleId, ModuleMeta, MokuTheme, SafeKey, TuiModule, resolve_event,
};
use secrecy::SecretBox;

use crate::registry::{self, PasswordMode, VolumeConfig, VolumeSecret};
use crate::worker::{self, StopOutcome};
use crate::{size, status};

mod create_form;
mod mount_prompt;

use create_form::CreateForm;
use mount_prompt::{MountField, PasswordPrompt};

struct VolumeRow {
    cfg: VolumeConfig,
    used_bytes: u64,
    mounted: bool,
}

pub struct VaultManagerModule {
    rows: Vec<VolumeRow>,
    state: ListState,
    message: Option<(String, Instant)>,
    prompt: Option<PasswordPrompt>,
    create_form: Option<CreateForm>,
    /// Volume id pending delete confirmation — `d`/`Command::Delete` sets
    /// this instead of deleting immediately; `Shift+D`
    /// (`moku_core::is_delete_bypass`) skips it entirely. Same shape as
    /// `modules/moku-todo/src/lib.rs`'s `confirm_delete`.
    confirm_delete: Option<String>,
    /// Result of an in-flight mount/unmount/create/delete, written by a
    /// spawned task and picked up at the top of `handle_event` — same
    /// pattern as `modules/moku-settings/src/tabs/storage.rs`'s
    /// `migration_result`, required here for the same reason: `draw()`
    /// only gets `&MokuTheme`, not `&mut AppContext`, so a background task
    /// can't raise a toast directly.
    action_result: Arc<Mutex<Option<String>>>,
    /// True while a `start_*` background task is in flight, cleared when
    /// its result is picked up by `poll_action_result`. Every `start_*`
    /// method checks this first and refuses to spawn a second task while
    /// one is already running — without it, mashing e.g. `u` (unmount)
    /// repeatedly before the row's `mounted` flag refreshes could spawn
    /// several concurrent `stop_mount_process` calls for the same volume
    /// (matches the guard `modules/moku-rss` (`is_refreshing`) and
    /// `modules/moku-http` (`is_running`) already use for their own
    /// single-in-flight-action invariant).
    busy: bool,
}

impl VaultManagerModule {
    pub fn new() -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        Self {
            rows: Vec::new(),
            state,
            message: None,
            prompt: None,
            create_form: None,
            confirm_delete: None,
            action_result: Arc::new(Mutex::new(None)),
            busy: false,
        }
    }

    async fn refresh(&mut self) {
        let volumes = registry::list_volumes().await.unwrap_or_default();
        self.rows = volumes
            .into_iter()
            .map(|cfg| {
                let used_bytes = registry::usage_bytes(&cfg.id).unwrap_or(0);
                let mounted = status::is_mounted(&cfg.id);
                VolumeRow {
                    cfg,
                    used_bytes,
                    mounted,
                }
            })
            .collect();
        let out_of_range = self
            .state
            .selected()
            .map(|i| i >= self.rows.len())
            .unwrap_or(true);
        if out_of_range && !self.rows.is_empty() {
            self.state.select(Some(0));
        }
    }

    fn selected(&self) -> Option<&VolumeRow> {
        self.state.selected().and_then(|i| self.rows.get(i))
    }

    fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = match self.state.selected() {
            Some(i) => (i + 1) % self.rows.len(),
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn select_previous(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = match self.state.selected() {
            Some(i) => (i + self.rows.len() - 1) % self.rows.len(),
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn show_message(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now()));
    }

    /// A reasonable default mount target — always editable in
    /// `PasswordPrompt` before mounting, this just picks a sensible
    /// starting point. On Windows: `M:` ("moku") if free, since it's easy
    /// to remember and unlikely to collide with anything the user already
    /// has mounted; otherwise the first free letter counting down from Z,
    /// which naturally spreads out further volumes across whatever's free
    /// without any extra bookkeeping (this function just checks the live
    /// filesystem state each time it's called). Elsewhere, a per-volume
    /// folder under the user's home directory.
    #[cfg_attr(windows, allow(unused_variables))]
    fn default_mountpoint(volume_id: &str) -> String {
        #[cfg(windows)]
        {
            if !std::path::Path::new(r"M:\").exists() {
                return "M:".to_string();
            }
            for c in ('D'..='Z').rev() {
                let letter = format!("{c}:");
                if !std::path::Path::new(&format!("{letter}\\")).exists() {
                    return letter;
                }
            }
            "Z:".to_string()
        }
        #[cfg(not(windows))]
        {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            format!("{home}/mnt/{volume_id}")
        }
    }

    fn poll_action_result(&mut self) -> bool {
        let result = self.action_result.lock().unwrap().take();
        if let Some(msg) = result {
            self.busy = false;
            self.show_message(msg);
            true
        } else {
            false
        }
    }

    fn start_mount(
        &mut self,
        volume_id: String,
        display_name: String,
        mountpoint: String,
        password: String,
    ) {
        if self.busy {
            return;
        }
        self.busy = true;
        let slot = Arc::clone(&self.action_result);
        tokio::spawn(async move {
            let msg = match worker::spawn_mount_process(&volume_id, &mountpoint, &password).await {
                Ok(worker::MountOutcome::Ready { pid }) => {
                    format!("Mounted '{display_name}' at {mountpoint} (worker PID: {pid}).")
                }
                Ok(worker::MountOutcome::Failed { message }) => format!("Mount failed: {message}"),
                Err(e) => format!("Mount failed: {e}"),
            };
            *slot.lock().unwrap() = Some(msg);
        });
        self.show_message("Mounting...");
    }

    /// Same as `start_mount`, but for a Default-mode volume mounted with
    /// moku's already-unlocked app-vault key instead of a typed password —
    /// the no-reprompt fast path (see the `KeyCode::Char('m') | KeyCode::
    /// Enter` handler below for when this is used instead of `start_mount`).
    fn start_mount_with_key(
        &mut self,
        volume_id: String,
        display_name: String,
        mountpoint: String,
        key: Arc<SecretBox<SafeKey>>,
    ) {
        if self.busy {
            return;
        }
        self.busy = true;
        let slot = Arc::clone(&self.action_result);
        tokio::spawn(async move {
            let msg =
                match worker::spawn_mount_process_with_key(&volume_id, &mountpoint, key.as_ref())
                    .await
                {
                    Ok(worker::MountOutcome::Ready { pid }) => {
                        format!("Mounted '{display_name}' at {mountpoint} (worker PID: {pid}).")
                    }
                    Ok(worker::MountOutcome::Failed { message }) => {
                        format!("Mount failed: {message}")
                    }
                    Err(e) => format!("Mount failed: {e}"),
                };
            *slot.lock().unwrap() = Some(msg);
        });
        self.show_message("Mounting...");
    }

    fn start_create(&mut self, name: String, size_bytes: u64, secret: VolumeSecret) {
        if self.busy {
            return;
        }
        self.busy = true;
        let slot = Arc::clone(&self.action_result);
        tokio::spawn(async move {
            let msg = match registry::create_volume(&name, size_bytes, secret, None).await {
                Ok(cfg) => format!("Created '{}' (id: {}).", cfg.display_name, cfg.id),
                Err(e) => format!("Create failed: {e}"),
            };
            *slot.lock().unwrap() = Some(msg);
        });
        self.show_message("Creating...");
    }

    fn start_unmount(&mut self, volume_id: String, display_name: String) {
        if self.busy {
            return;
        }
        self.busy = true;
        let slot = Arc::clone(&self.action_result);
        tokio::spawn(async move {
            let msg = match worker::stop_mount_process(&volume_id).await {
                Ok(StopOutcome::NotMounted) => format!("'{display_name}' is not mounted."),
                Ok(StopOutcome::StaleCleanedUp) => {
                    format!("'{display_name}' had a stale mount record; cleaned up.")
                }
                Ok(StopOutcome::Graceful) => format!("Unmounted '{display_name}'."),
                Ok(StopOutcome::Forced) => format!("Unmounted '{display_name}' (forced)."),
                Err(e) => format!("Unmount failed: {e}"),
            };
            *slot.lock().unwrap() = Some(msg);
        });
        self.show_message("Unmounting...");
    }

    /// Opens the delete-confirmation prompt for the selected volume, if
    /// any — mirrors `modules/moku-todo/src/lib.rs`'s
    /// `start_confirm_delete`.
    fn start_confirm_delete(&mut self) -> bool {
        let Some(row) = self.selected() else {
            return false;
        };
        self.confirm_delete = Some(row.cfg.id.clone());
        true
    }

    fn start_delete(&mut self, volume_id: String, display_name: String) {
        if self.busy {
            return;
        }
        self.busy = true;
        let slot = Arc::clone(&self.action_result);
        tokio::spawn(async move {
            // Always attempt an unmount first — a harmless no-op
            // (StopOutcome::NotMounted) if it wasn't mounted, and the
            // required first step if it was: deleting a live-mounted
            // volume's backing files out from under WinFsp is unsafe.
            let _ = worker::stop_mount_process(&volume_id).await;
            let msg = match registry::delete_volume(&volume_id).await {
                Ok(()) => format!("Deleted '{display_name}'."),
                Err(e) => format!("Delete failed: {e}"),
            };
            *slot.lock().unwrap() = Some(msg);
        });
        self.show_message("Deleting...");
    }

    fn draw_confirm_delete(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &MokuTheme,
        display_name: &str,
    ) {
        let chunks = Layout::vertical([
            Constraint::Percentage(40),
            Constraint::Length(4),
            Constraint::Percentage(40),
        ])
        .split(area);
        let box_area = Layout::horizontal([
            Constraint::Percentage(20),
            Constraint::Percentage(60),
            Constraint::Percentage(20),
        ])
        .split(chunks[1])[1];

        let lines = vec![
            Line::styled(
                format!("Delete '{display_name}' and ALL its data?"),
                Style::default().fg(theme.error),
            ),
            Line::raw("This cannot be undone."),
            Line::raw(""),
            Line::styled(
                "[y]/[Enter] Confirm   [n]/[Esc] Cancel",
                Style::default().fg(theme.base_fg),
            ),
        ];
        let p = Paragraph::new(lines)
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .title(" Delete Volume ")
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.error)),
            )
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg));
        frame.render_widget(p, box_area);
    }
}

impl Default for VaultManagerModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for VaultManagerModule {
    fn id(&self) -> ModuleId {
        ModuleId::VAULT
    }
    fn title(&self) -> &'static str {
        ModuleId::VAULT.title()
    }
    fn encrypt_by_default(&self) -> bool {
        false // reads volume.json/usage.json directly, not vault-encrypted storage
    }
}

#[async_trait]
impl TuiModule for VaultManagerModule {
    async fn init(&mut self, _ctx: &mut AppContext) -> Result<()> {
        self.refresh().await;
        Ok(())
    }

    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        if self.poll_action_result() {
            self.refresh().await;
            return Ok(true);
        }

        if self.prompt.is_some() {
            return self.handle_prompt_event(event);
        }

        if self.create_form.is_some() {
            return self.handle_create_form_event(event, ctx);
        }

        if let Some(id) = self.confirm_delete.clone() {
            match moku_core::resolve_confirm_delete_key(event) {
                moku_core::ConfirmDeleteKey::Confirm => {
                    self.confirm_delete = None;
                    let display_name = self
                        .rows
                        .iter()
                        .find(|r| r.cfg.id == id)
                        .map(|r| r.cfg.display_name.clone())
                        .unwrap_or_else(|| id.clone());
                    self.start_delete(id, display_name);
                }
                moku_core::ConfirmDeleteKey::Cancel => self.confirm_delete = None,
                moku_core::ConfirmDeleteKey::Other => return Ok(false),
            }
            return Ok(true);
        }

        // Shift+D (moku_core::is_delete_bypass) deletes the selected
        // volume immediately, skipping the confirmation prompt plain `d`
        // shows — same convention as Todo/Bookmark/Secrets/RSS.
        if moku_core::is_delete_bypass(event) {
            if let Some(row) = self.selected() {
                let volume_id = row.cfg.id.clone();
                let display_name = row.cfg.display_name.clone();
                self.start_delete(volume_id, display_name);
            }
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

        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('r') => {
                    self.refresh().await;
                    self.show_message("Refreshed.");
                }
                KeyCode::Char('c') => {
                    self.create_form = Some(CreateForm::new());
                }
                KeyCode::Char('m') | KeyCode::Enter => {
                    if let Some(row) = self.selected() {
                        if row.mounted {
                            self.show_message("Already mounted.");
                        } else {
                            let volume_id = row.cfg.id.clone();
                            let display_name = row.cfg.display_name.clone();
                            let mountpoint = Self::default_mountpoint(&volume_id);

                            // No-reprompt fast path: a Default-mode volume
                            // created under the new scheme (no vault/
                            // meta.json of its own — see
                            // registry::has_own_vault) derives its key from
                            // moku's app vault, so if that vault is already
                            // unlocked there's a real key sitting in
                            // ctx.session right now and nothing left to ask
                            // the user for — the prompt below still opens
                            // (so the mountpoint stays visible/editable),
                            // it just shows no password field and mounts
                            // immediately on Enter if left untouched.
                            let fast_path = row.cfg.password_mode == PasswordMode::Default
                                && ctx.session.is_unlocked()
                                && registry::volume_dir(&volume_id)
                                    .map(|dir| !registry::has_own_vault(&dir))
                                    .unwrap_or(false);
                            let key = fast_path.then(|| ctx.session.current()).flatten();

                            self.prompt = Some(PasswordPrompt {
                                volume_id,
                                display_name,
                                mountpoint,
                                focus: MountField::Mountpoint,
                                input: zeroize::Zeroizing::new(String::new()),
                                key,
                            });
                        }
                    }
                }
                KeyCode::Char('u') => {
                    if let Some(row) = self.selected() {
                        if row.mounted {
                            let volume_id = row.cfg.id.clone();
                            let display_name = row.cfg.display_name.clone();
                            self.start_unmount(volume_id, display_name);
                        } else {
                            self.show_message("Not mounted.");
                        }
                    }
                }
                KeyCode::Char('d') => {
                    self.start_confirm_delete();
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        if let Some((_, at)) = self.message
            && at.elapsed() > Duration::from_secs(6)
        {
            self.message = None;
        }

        if let Some(prompt) = &self.prompt {
            self.draw_prompt(frame, area, theme, prompt);
            return;
        }

        if let Some(form) = &self.create_form {
            self.draw_create_form(frame, area, theme, form);
            return;
        }

        if let Some(id) = &self.confirm_delete {
            let display_name = self
                .rows
                .iter()
                .find(|r| &r.cfg.id == id)
                .map(|r| r.cfg.display_name.as_str())
                .unwrap_or(id.as_str());
            self.draw_confirm_delete(frame, area, theme, display_name);
            return;
        }

        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);

        let items: Vec<ListItem> = if self.rows.is_empty() {
            vec![ListItem::new(
                "  No encrypted volumes yet. Press [c] to create one.",
            )]
        } else {
            self.rows
                .iter()
                .map(|row| {
                    let status = if row.mounted {
                        "🟢 mounted"
                    } else {
                        "⚫ not mounted"
                    };
                    let content = format!(
                        "{}  ({})  {} / {}  [{}]",
                        row.cfg.display_name,
                        row.cfg.id,
                        size::format_size(row.used_bytes),
                        size::format_size(row.cfg.size_limit_bytes),
                        status
                    );
                    ListItem::new(content).style(Style::default().fg(theme.base_fg))
                })
                .collect()
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Encrypted Vaults ")
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

        let help = self
            .message
            .as_ref()
            .map(|(m, _)| m.clone())
            .unwrap_or_else(|| {
                " [Enter]/[m] Mount  [u] Unmount  [c] Create  [d] Delete  [r] Refresh  [Esc] Back "
                    .to_string()
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

    #[cfg(windows)]
    #[test]
    fn test_default_mountpoint_prefers_m_when_free() {
        let result = VaultManagerModule::default_mountpoint("some-volume");
        if !std::path::Path::new(r"M:\").exists() {
            assert_eq!(
                result, "M:",
                "M: is free on this machine and should be preferred"
            );
        } else {
            // M: is already in use on this machine — just confirm we still
            // fall back to a plausible drive-letter mountpoint.
            assert!(result.ends_with(':') && result.len() == 2);
        }
    }

    fn fake_row(id: &str, mounted: bool) -> VolumeRow {
        VolumeRow {
            cfg: VolumeConfig {
                id: id.to_string(),
                display_name: id.to_string(),
                size_limit_bytes: 1024,
                password_mode: PasswordMode::Custom,
                created_at: 0,
            },
            used_bytes: 0,
            mounted,
        }
    }

    #[tokio::test]
    async fn test_busy_guard_blocks_a_second_start_while_one_is_in_flight() {
        let mut module = VaultManagerModule::new();
        assert!(!module.busy);
        module.start_unmount("vol-a".to_string(), "vol-a".to_string());
        assert!(module.busy, "starting an action must set the busy flag");
        // Mashing the same key again before the first task's result lands
        // must not spawn a second one — this is exactly what let a user
        // spam `u` into multiple concurrent stop_mount_process calls for
        // the same volume before this guard existed.
        module.start_unmount("vol-a".to_string(), "vol-a".to_string());
        assert!(module.busy);
    }

    #[test]
    fn test_poll_action_result_clears_busy_flag() {
        let mut module = VaultManagerModule::new();
        module.busy = true;
        *module.action_result.lock().unwrap() = Some("done".to_string());
        module.poll_action_result();
        assert!(
            !module.busy,
            "picking up a finished action's result must clear busy so the next one can start"
        );
    }

    #[tokio::test]
    async fn test_char_d_opens_confirm_delete_for_selected_row() {
        let mut module = VaultManagerModule::new();
        module.rows = vec![fake_row("vol-a", false)];
        module.state.select(Some(0));
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Char('d')), &mut ctx)
            .await
            .unwrap();
        assert_eq!(module.confirm_delete.as_deref(), Some("vol-a"));
    }

    #[tokio::test]
    async fn test_confirm_delete_esc_cancels_without_deleting() {
        let mut module = VaultManagerModule::new();
        module.rows = vec![fake_row("vol-a", false)];
        module.confirm_delete = Some("vol-a".to_string());
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Esc), &mut ctx)
            .await
            .unwrap();
        assert!(module.confirm_delete.is_none());
        assert_eq!(module.rows.len(), 1, "cancelling must not remove the row");
    }

    #[tokio::test]
    async fn test_confirm_delete_n_cancels_without_deleting() {
        let mut module = VaultManagerModule::new();
        module.rows = vec![fake_row("vol-a", false)];
        module.confirm_delete = Some("vol-a".to_string());
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Char('n')), &mut ctx)
            .await
            .unwrap();
        assert!(module.confirm_delete.is_none());
    }

    #[tokio::test]
    async fn test_confirm_delete_enter_closes_prompt_and_starts_delete() {
        let mut module = VaultManagerModule::new();
        module.rows = vec![fake_row("vol-a", false)];
        module.confirm_delete = Some("vol-a".to_string());
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Enter), &mut ctx)
            .await
            .unwrap();
        assert!(module.confirm_delete.is_none());
    }

    #[tokio::test]
    async fn test_shift_d_bypasses_confirmation() {
        let mut module = VaultManagerModule::new();
        module.rows = vec![fake_row("vol-a", false)];
        module.state.select(Some(0));
        let mut ctx = create_test_context().await;

        let event = Event::Key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT));
        module.handle_event(&event, &mut ctx).await.unwrap();

        assert!(
            module.confirm_delete.is_none(),
            "Shift+D must skip the confirmation prompt entirely"
        );
    }
}
