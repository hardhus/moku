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

struct VolumeRow {
    cfg: VolumeConfig,
    used_bytes: u64,
    mounted: bool,
}

/// Self-contained masked-password sub-state for a pending mount — no
/// separate `ModuleId`/navigation, mirroring `moku-lock-screen`'s input
/// handling directly (`input: String`, char-push/backspace/Enter/Esc, `•`
/// render).
struct PasswordPrompt {
    volume_id: String,
    display_name: String,
    mountpoint: String,
    input: String,
}

/// Which field of the new-volume form currently has keyboard focus. All
/// three are shown at once (`Tab` switches focus, `Enter` submits from
/// any of them) — matches `modules/moku-rss/src/tui_module.rs`'s
/// `EditField`/`EditFeed` shape, not a sequential per-field wizard.
#[derive(PartialEq, Clone, Copy, Debug)]
enum CreateField {
    Name,
    Size,
    Password,
}

/// State for creating a new volume from the TUI. Always `PasswordMode::
/// Custom` (a single masked entry, no separate confirmation field — same
/// one-entry convention `PasswordPrompt` already uses for mounting) and
/// always created under the current directory (`create_volume`'s own
/// `None` default) — no location field here; `--path` stays a CLI-only
/// option for anyone who wants a different one, keeping this form simple.
struct CreateForm {
    focus: CreateField,
    name: String,
    size: String,
    password: String,
    error: Option<String>,
}

impl CreateForm {
    fn new() -> Self {
        Self {
            focus: CreateField::Name,
            name: String::new(),
            size: String::new(),
            password: String::new(),
            error: None,
        }
    }
}

pub struct VaultManagerModule {
    rows: Vec<VolumeRow>,
    state: ListState,
    message: Option<(String, Instant)>,
    prompt: Option<PasswordPrompt>,
    create_form: Option<CreateForm>,
    /// Result of an in-flight mount/unmount/create, written by a spawned
    /// task and picked up at the top of `handle_event` — same pattern as
    /// `modules/moku-settings/src/tabs/storage.rs`'s `migration_result`,
    /// required here for the same reason: `draw()` only gets `&MokuTheme`,
    /// not `&mut AppContext`, so a background task can't raise a toast
    /// directly.
    action_result: Arc<Mutex<Option<String>>>,
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
            action_result: Arc::new(Mutex::new(None)),
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

    /// A reasonable default mount target: on Windows, the first free
    /// drive letter counting down from Z; elsewhere, a per-volume folder
    /// under the user's home directory. Not user-configurable yet — a
    /// possible future refinement, matching the plan's v1 scope note.
    #[cfg_attr(windows, allow(unused_variables))]
    fn default_mountpoint(volume_id: &str) -> String {
        #[cfg(windows)]
        {
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
        let slot = Arc::clone(&self.action_result);
        tokio::spawn(async move {
            let msg = match worker::spawn_mount_process(&volume_id, &mountpoint, &password).await {
                Ok(worker::MountOutcome::Ready { pid }) => {
                    format!("Mounted '{display_name}' at {mountpoint} (worker PID: {pid}).")
                }
                Ok(worker::MountOutcome::Failed { message }) => format!("Mount failed: {message}"),
                Ok(worker::MountOutcome::TimedOut { pid }) => {
                    format!(
                        "'{display_name}' is still starting (worker PID: {pid}) — check back shortly."
                    )
                }
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
        let slot = Arc::clone(&self.action_result);
        tokio::spawn(async move {
            let msg = match worker::spawn_mount_process_with_key(
                &volume_id,
                &mountpoint,
                key.as_ref(),
            )
            .await
            {
                Ok(worker::MountOutcome::Ready { pid }) => {
                    format!("Mounted '{display_name}' at {mountpoint} (worker PID: {pid}).")
                }
                Ok(worker::MountOutcome::Failed { message }) => {
                    format!("Mount failed: {message}")
                }
                Ok(worker::MountOutcome::TimedOut { pid }) => format!(
                    "'{display_name}' is still starting (worker PID: {pid}) — check back shortly."
                ),
                Err(e) => format!("Mount failed: {e}"),
            };
            *slot.lock().unwrap() = Some(msg);
        });
        self.show_message("Mounting...");
    }

    fn start_create(&mut self, name: String, size_bytes: u64, password: String) {
        let slot = Arc::clone(&self.action_result);
        tokio::spawn(async move {
            let msg = match registry::create_volume(
                &name,
                size_bytes,
                VolumeSecret::Password(password),
                None,
            )
            .await
            {
                Ok(cfg) => format!("Created '{}' (id: {}).", cfg.display_name, cfg.id),
                Err(e) => format!("Create failed: {e}"),
            };
            *slot.lock().unwrap() = Some(msg);
        });
        self.show_message("Creating...");
    }

    fn start_unmount(&mut self, volume_id: String, display_name: String) {
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

    fn draw_prompt(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &MokuTheme,
        prompt: &PasswordPrompt,
    ) {
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

        let masked: String = prompt.input.chars().map(|_| '•').collect();
        let p = Paragraph::new(masked)
            .block(
                Block::default()
                    .title(format!(" Password for '{}' ", prompt.display_name))
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.info)),
            )
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg));
        frame.render_widget(p, input_chunk);

        let hint = Paragraph::new(format!(
            "Mounting at {} — [Enter] confirm  [Esc] cancel",
            prompt.mountpoint
        ))
        .alignment(Alignment::Center)
        .style(Style::default().fg(theme.base_fg));
        frame.render_widget(hint, chunks[2]);
    }

    fn draw_create_form(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &MokuTheme,
        form: &CreateForm,
    ) {
        let chunks = Layout::vertical([
            Constraint::Percentage(35),
            Constraint::Length(6),
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
        let masked_password: String = form.password.chars().map(|_| '•').collect();

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
                    "{} Password: {}",
                    marker(form.focus == CreateField::Password),
                    masked_password
                ),
                field_style(form.focus == CreateField::Password),
            ),
        ];
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

        let hint = Paragraph::new("[Tab] Switch field  [Enter] Create  [Esc] Cancel")
            .alignment(Alignment::Center)
            .style(Style::default().fg(theme.base_fg));
        frame.render_widget(hint, chunks[2]);
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

        if let Some(prompt) = &mut self.prompt {
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
                    let password = prompt.input.clone();
                    self.prompt = None;
                    self.start_mount(volume_id, display_name, mountpoint, password);
                }
                KeyCode::Esc => self.prompt = None,
                KeyCode::Char(c) => prompt.input.push(c),
                KeyCode::Backspace => {
                    prompt.input.pop();
                }
                _ => return Ok(false),
            }
            return Ok(true);
        }

        if let Some(form) = &mut self.create_form {
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
                        CreateField::Size => CreateField::Password,
                        CreateField::Password => CreateField::Name,
                    };
                }
                KeyCode::Enter => {
                    if form.name.trim().is_empty() {
                        form.error = Some("Name cannot be empty.".to_string());
                    } else if form.password.is_empty() {
                        form.error = Some("Password cannot be empty.".to_string());
                    } else {
                        match size::parse_size(&form.size) {
                            Ok(size_bytes) => {
                                let name = form.name.trim().to_string();
                                let password = form.password.clone();
                                self.create_form = None;
                                self.start_create(name, size_bytes, password);
                            }
                            Err(e) => form.error = Some(format!("Invalid size: {e}")),
                        }
                    }
                }
                KeyCode::Char(c) => {
                    match form.focus {
                        CreateField::Name => form.name.push(c),
                        CreateField::Size => form.size.push(c),
                        CreateField::Password => form.password.push(c),
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
                        CreateField::Password => {
                            form.password.pop();
                        }
                    }
                    form.error = None;
                }
                _ => return Ok(false),
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
                            // the user for.
                            let fast_path = row.cfg.password_mode == PasswordMode::Default
                                && ctx.session.is_unlocked()
                                && registry::volume_dir(&volume_id)
                                    .map(|dir| !registry::has_own_vault(&dir))
                                    .unwrap_or(false);

                            match fast_path.then(|| ctx.session.current()).flatten() {
                                Some(key) => {
                                    self.start_mount_with_key(
                                        volume_id,
                                        display_name,
                                        mountpoint,
                                        key,
                                    );
                                }
                                None => {
                                    self.prompt = Some(PasswordPrompt {
                                        volume_id,
                                        display_name,
                                        mountpoint,
                                        input: String::new(),
                                    });
                                }
                            }
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
                " [Enter]/[m] Mount  [u] Unmount  [c] Create  [r] Refresh  [Esc] Back ".to_string()
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
mod create_form_tests {
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

    #[tokio::test]
    async fn test_tab_cycles_focus_through_all_three_fields() {
        let mut module = VaultManagerModule::new();
        module.create_form = Some(CreateForm::new());
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
            CreateField::Password
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
    async fn test_enter_with_empty_password_sets_error() {
        let mut module = VaultManagerModule::new();
        let mut form = CreateForm::new();
        form.name = "test".to_string();
        form.size = "10MiB".to_string();
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

    #[test]
    fn test_create_form_render_shows_all_three_fields() {
        let mut module = VaultManagerModule::new();
        let mut form = CreateForm::new();
        form.name = "my-vault".to_string();
        form.size = "1GB".to_string();
        module.create_form = Some(form);

        let (width, height) = (70u16, 20u16);
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

        assert!(content.contains("New Volume"));
        assert!(content.contains("Name"));
        assert!(content.contains("my-vault"));
        assert!(content.contains("Size"));
        assert!(content.contains("1GB"));
        assert!(content.contains("Password"));
    }
}
