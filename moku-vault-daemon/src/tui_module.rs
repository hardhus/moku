use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use moku_core::{AppContext, Command, ModuleId, ModuleMeta, MokuTheme, TuiModule, resolve_event};

use crate::registry::{self, VolumeConfig};
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

pub struct VaultManagerModule {
    rows: Vec<VolumeRow>,
    state: ListState,
    message: Option<(String, Instant)>,
    prompt: Option<PasswordPrompt>,
    /// Result of an in-flight mount/unmount, written by a spawned task and
    /// picked up at the top of `handle_event` — same pattern as
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
                KeyCode::Char('m') | KeyCode::Enter => {
                    if let Some(row) = self.selected() {
                        if row.mounted {
                            self.show_message("Already mounted.");
                        } else {
                            let mountpoint = Self::default_mountpoint(&row.cfg.id);
                            self.prompt = Some(PasswordPrompt {
                                volume_id: row.cfg.id.clone(),
                                display_name: row.cfg.display_name.clone(),
                                mountpoint,
                                input: String::new(),
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

        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);

        let items: Vec<ListItem> = if self.rows.is_empty() {
            vec![ListItem::new(
                "  No encrypted volumes yet. Create one with: moku vault create <name> --size 10GB",
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
                " [Enter]/[m] Mount  [u] Unmount  [r] Refresh  [Esc] Back ".to_string()
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
