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
use secrecy::{ExposeSecret, SecretBox};

use crate::registry::{self, PasswordMode, VolumeConfig, VolumeSecret};
use crate::worker::{self, StopOutcome};
use crate::{size, status};

struct VolumeRow {
    cfg: VolumeConfig,
    used_bytes: u64,
    mounted: bool,
}

/// Which field of the mount prompt currently has keyboard focus.
/// `Password` only exists (and is only reachable via `Tab`) when
/// `PasswordPrompt::key` is `None` — a Default-mode volume with the app
/// vault already unlocked needs no password at all, only a mountpoint to
/// (optionally) confirm or change.
#[derive(PartialEq, Clone, Copy, Debug)]
enum MountField {
    Mountpoint,
    Password,
}

/// Self-contained sub-state for a pending mount — no separate `ModuleId`/
/// navigation, mirroring `moku-lock-screen`'s input handling directly
/// (`push`/`pop` a `String`, Enter/Esc). Always shown before mounting,
/// even on the no-reprompt fast path — a mountpoint is picked by default
/// (`VaultManagerModule::default_mountpoint`) but stays visible and
/// editable rather than being silently auto-chosen, since more than one
/// volume may be mounted and the user may want a specific drive letter.
struct PasswordPrompt {
    volume_id: String,
    display_name: String,
    mountpoint: String,
    focus: MountField,
    input: String,
    /// Some(key): the app vault's already-unlocked master key (Default
    /// mode, verified when the main vault was unlocked) — no password
    /// field is shown or needed, Enter mounts with just the mountpoint.
    /// None: a password must be typed (Custom mode, an old-scheme
    /// Default-mode volume with its own vault, or a currently-locked main
    /// vault) — the Password field is shown too.
    key: Option<Arc<SecretBox<SafeKey>>>,
}

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
struct CreateForm {
    focus: CreateField,
    name: String,
    size: String,
    mode: PasswordMode,
    password: String,
    confirm_password: String,
    error: Option<String>,
}

impl CreateForm {
    fn new() -> Self {
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
        if prompt.key.is_none() {
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

        let hint = if prompt.key.is_none() {
            "[Tab] Switch field  [Enter] Mount  [Esc] Cancel"
        } else {
            "[Enter] Mount  [Esc] Cancel"
        };
        let hint = Paragraph::new(hint)
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
                    match prompt.key.clone() {
                        Some(key) => {
                            self.prompt = None;
                            self.start_mount_with_key(volume_id, display_name, mountpoint, key);
                        }
                        None => {
                            let password = prompt.input.clone();
                            self.prompt = None;
                            self.start_mount(volume_id, display_name, mountpoint, password);
                        }
                    }
                }
                KeyCode::Esc => self.prompt = None,
                KeyCode::Tab if prompt.key.is_none() => {
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
                                            form.error =
                                                Some("Password cannot be empty.".to_string());
                                            None
                                        } else if form.password != form.confirm_password {
                                            form.error =
                                                Some("Passwords didn't match.".to_string());
                                            None
                                        } else {
                                            Some(VolumeSecret::Password(form.password.clone()))
                                        }
                                    }
                                    PasswordMode::Default => match ctx.session.current() {
                                        Some(key) => {
                                            Some(VolumeSecret::FromAppVault(SecretBox::new(
                                                Box::new(SafeKey(key.expose_secret().0)),
                                            )))
                                        }
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
                                input: String::new(),
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

    async fn create_unlocked_test_context() -> AppContext {
        let ctx = create_test_context().await;
        ctx.session
            .unlock(SecretBox::new(Box::new(SafeKey([7u8; 32]))));
        ctx
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

    fn mount_prompt(key: Option<Arc<SecretBox<SafeKey>>>) -> PasswordPrompt {
        PasswordPrompt {
            volume_id: "vol-1".to_string(),
            display_name: "vol-1".to_string(),
            mountpoint: "M:".to_string(),
            focus: MountField::Mountpoint,
            input: String::new(),
            key,
        }
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

    #[tokio::test]
    async fn test_mount_prompt_char_input_routes_to_mountpoint_when_no_password_needed() {
        let mut module = VaultManagerModule::new();
        module.prompt = Some(mount_prompt(Some(Arc::new(SecretBox::new(Box::new(
            SafeKey([1u8; 32]),
        ))))));
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Char('X')), &mut ctx)
            .await
            .unwrap();
        assert_eq!(module.prompt.as_ref().unwrap().mountpoint, "M:X");
    }

    #[tokio::test]
    async fn test_mount_prompt_tab_switches_fields_when_password_needed() {
        let mut module = VaultManagerModule::new();
        module.prompt = Some(mount_prompt(None));
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
        let mut module = VaultManagerModule::new();
        module.prompt = Some(mount_prompt(Some(Arc::new(SecretBox::new(Box::new(
            SafeKey([2u8; 32]),
        ))))));
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Enter), &mut ctx)
            .await
            .unwrap();
        assert!(module.prompt.is_none());
    }

    #[tokio::test]
    async fn test_mount_prompt_enter_without_key_submits_and_closes() {
        let mut module = VaultManagerModule::new();
        let mut prompt = mount_prompt(None);
        prompt.focus = MountField::Password;
        prompt.input = "hunter2".to_string();
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
        let mut module = VaultManagerModule::new();
        module.prompt = Some(mount_prompt(None));
        let mut ctx = create_test_context().await;

        module
            .handle_event(&key(KeyCode::Esc), &mut ctx)
            .await
            .unwrap();
        assert!(module.prompt.is_none());
    }
}
