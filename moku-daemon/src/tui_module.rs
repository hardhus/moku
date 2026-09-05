use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use moku_core::{
    AppContext, Command, ModuleId, ModuleMeta, ModuleStatus, MokuTheme, TuiModule, resolve_event,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph},
};

const AUTOSTART_ARGS: &[&str] = &["daemon", "start", "--from-autostart"];

pub struct DaemonStatusModule {
    is_running: bool,
    pid: Option<u32>,
    autostart_enabled: bool,
    last_checked: Instant,
    /// Cached alongside the pid/autostart status above, refreshed on the
    /// same `last_checked` throttle — previously re-read and re-parsed
    /// from disk on *every* `draw()` call (i.e. every keystroke while this
    /// screen is open) with no throttle of its own, even though the data
    /// only ever changes once per daemon tick (minutes apart).
    task_statuses: Vec<crate::task_status::TaskStatus>,
    message: Option<String>,
    message_time: Option<Instant>,
}

impl DaemonStatusModule {
    pub fn new() -> Self {
        Self {
            is_running: false,
            pid: None,
            autostart_enabled: false,
            last_checked: Instant::now(),
            task_statuses: Vec::new(),
            message: None,
            message_time: None,
        }
    }

    fn refresh_status(&mut self) {
        self.pid = crate::pid::read();
        self.is_running = self.pid.map(crate::status::pid_is_alive).unwrap_or(false);
        self.autostart_enabled = std::env::current_exe()
            .map(|exe| crate::autostart::is_autostart_enabled(&exe, AUTOSTART_ARGS))
            .unwrap_or(false);
        self.task_statuses = moku_core::dirs::get_data_dir()
            .ok()
            .map(|d| crate::task_status::read_statuses(&d))
            .unwrap_or_default();
        self.last_checked = Instant::now();
    }

    fn show_temp_message(&mut self, msg: impl Into<String>) {
        self.message = Some(msg.into());
        self.message_time = Some(Instant::now());
    }
}

impl Default for DaemonStatusModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for DaemonStatusModule {
    fn id(&self) -> ModuleId {
        ModuleId::DAEMON
    }
    fn title(&self) -> &'static str {
        ModuleId::DAEMON.title()
    }
    fn encrypt_by_default(&self) -> bool {
        false // reads task-status.json, not vault-encrypted storage
    }
}

#[async_trait]
impl TuiModule for DaemonStatusModule {
    async fn init(&mut self, _ctx: &mut AppContext) -> Result<()> {
        self.refresh_status();
        Ok(())
    }

    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        let command = resolve_event(event, &ctx.config.load().keys, None);
        let mut changed = false;

        match command {
            Command::Quit | Command::Back => {
                ctx.navigate_to(ModuleId::LAUNCHER);
                return Ok(true);
            }
            _ => {}
        }

        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('r') => {
                        self.refresh_status();
                        self.show_temp_message("Status refreshed.");
                        changed = true;
                    }
                    KeyCode::Char('s') => {
                        if self.is_running {
                            self.show_temp_message("Daemon is already running.");
                        } else {
                            if let Ok(exe) = std::env::current_exe() {
                                let mut cmd = std::process::Command::new(exe);
                                cmd.arg("daemon").arg("run");
                                cmd.stdin(std::process::Stdio::null());
                                cmd.stdout(std::process::Stdio::null());
                                cmd.stderr(std::process::Stdio::null());
                                #[cfg(windows)]
                                {
                                    use std::os::windows::process::CommandExt;
                                    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
                                }
                                let _ = cmd.spawn();
                                tokio::time::sleep(Duration::from_millis(200)).await;
                                self.refresh_status();
                                if self.is_running {
                                    self.show_temp_message("Daemon started successfully.");
                                } else {
                                    self.show_temp_message("Daemon starting spawned.");
                                }
                            } else {
                                self.show_temp_message("Failed to find current executable path.");
                            }
                        }
                        changed = true;
                    }
                    KeyCode::Char('k') => {
                        if let Some(pid_val) = self.pid {
                            let sys = crate::status::refresh_single(pid_val);
                            if let Some(process) = sys.process(sysinfo::Pid::from_u32(pid_val)) {
                                process.kill();
                                tokio::time::sleep(Duration::from_millis(150)).await;
                                crate::pid::remove();
                                self.refresh_status();
                                self.show_temp_message("Daemon stopped (process killed).");
                            } else {
                                self.show_temp_message("Daemon process not found.");
                            }
                        } else {
                            self.show_temp_message("Daemon is not running.");
                        }
                        changed = true;
                    }
                    KeyCode::Char('a') => {
                        if let Ok(exe) = std::env::current_exe() {
                            let enable = !self.autostart_enabled;
                            match crate::autostart::set_autostart(enable, &exe, AUTOSTART_ARGS) {
                                Ok(_) => {
                                    self.autostart_enabled = enable;
                                    self.show_temp_message(if enable {
                                        "Autostart enabled."
                                    } else {
                                        "Autostart disabled."
                                    });
                                }
                                Err(e) => self.show_temp_message(format!("Autostart error: {e}")),
                            }
                        } else {
                            self.show_temp_message("Failed to find current executable path.");
                        }
                        changed = true;
                    }
                    _ => {}
                }
            }
        }

        Ok(changed)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        // Best-effort: refresh if this screen happens to redraw (e.g. a
        // toast tick) and the data looks stale. NOT a real timer — in the
        // dirty-flag draw loop, nothing requests a redraw purely to run
        // this check, so it may not fire for a while if nothing else
        // changes. Use [r] to force a refresh.
        if self.last_checked.elapsed() > Duration::from_secs(5) {
            self.refresh_status();
        }

        if let Some(msg_time) = self.message_time {
            if msg_time.elapsed() > Duration::from_secs(3) {
                self.message = None;
                self.message_time = None;
            }
        }

        let chunks = Layout::vertical([
            Constraint::Length(3), // Title
            Constraint::Length(5), // Daemon status panel
            Constraint::Min(0),    // Tasks panel
            Constraint::Length(1), // Message status line
            Constraint::Length(3), // Help bar
        ])
        .split(area);

        // 1. Title Header
        let header = Paragraph::new(" Daemon Manager ")
            .style(
                Style::default()
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg)
                    .add_modifier(Modifier::BOLD),
            )
            .alignment(ratatui::layout::Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border)),
            );
        frame.render_widget(header, chunks[0]);

        // 2. Status Panel
        let daemon_badge = if self.is_running {
            Span::styled(
                format!("● Running (PID: {})", self.pid.unwrap_or(0)),
                Style::default().fg(theme.success),
            )
        } else {
            Span::styled("● Stopped", Style::default().fg(theme.error))
        };
        let autostart_badge = if self.autostart_enabled {
            Span::styled("● Enabled", Style::default().fg(theme.success))
        } else {
            Span::styled("● Disabled", Style::default().fg(theme.error))
        };
        let status_lines = Text::from(vec![
            Line::from(vec![Span::raw("  Daemon:     "), daemon_badge]),
            Line::from(vec![Span::raw("  Autostart:  "), autostart_badge]),
            Line::from(Span::styled(
                format!(
                    "  Last check: {}s ago",
                    self.last_checked.elapsed().as_secs()
                ),
                Style::default().fg(theme.base_fg),
            )),
        ]);
        let info = Paragraph::new(status_lines)
            .style(Style::default().bg(theme.base_bg))
            .block(
                Block::default()
                    .title(" Status ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border)),
            );
        frame.render_widget(info, chunks[1]);

        // 3. Tasks Panel — cached in `self.task_statuses`, refreshed by
        // `refresh_status()` above alongside the pid/autostart check.
        let task_items: Vec<ListItem> = if self.task_statuses.is_empty() {
            vec![ListItem::new(
                "  No task data available. Is the daemon running?",
            )]
        } else {
            self.task_statuses
                .iter()
                .map(|t| {
                    let last = t
                        .last_run_secs
                        .map(format_relative_time)
                        .unwrap_or_else(|| "never".to_string());
                    let status = match &t.last_error {
                        None => format!("OK (processed: {})", t.last_item_count),
                        Some(e) => format!("ERR: {}", &e[..e.len().min(40)]),
                    };
                    ListItem::new(format!(
                        "  • [{}]  Last Run: {}  Status: {}",
                        title_case(&t.id),
                        last,
                        status
                    ))
                })
                .collect()
        };

        let tasks_list = List::new(task_items)
            .block(
                Block::default()
                    .title(" Background Tasks ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border)),
            )
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg));
        frame.render_widget(tasks_list, chunks[2]);

        // 4. Message Line
        let msg_content = self.message.clone().unwrap_or_default();
        let msg_widget = Paragraph::new(format!(" {}", msg_content)).style(
            Style::default()
                .fg(theme.selection_fg)
                .add_modifier(Modifier::ITALIC),
        );
        frame.render_widget(msg_widget, chunks[3]);

        // 5. Help Bar
        let autostart_label = if self.autostart_enabled {
            "Disable Autostart"
        } else {
            "Enable Autostart"
        };
        let help_text =
            format!(" [s] Start | [k] Stop | [a] {autostart_label} | [r] Refresh | [Esc] Back ");
        let help = Paragraph::new(help_text)
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border)),
            );
        frame.render_widget(help, chunks[4]);
    }

    async fn dashboard_summary(&self, _ctx: &AppContext) -> Option<ModuleStatus> {
        let running = crate::pid::read()
            .map(crate::status::pid_is_alive)
            .unwrap_or(false);
        let autostart = std::env::current_exe()
            .map(|exe| crate::autostart::is_autostart_enabled(&exe, AUTOSTART_ARGS))
            .unwrap_or(false);
        let text = format!(
            "{}, autostart {}",
            if running { "Running" } else { "Stopped" },
            if autostart { "on" } else { "off" }
        );
        Some(if running {
            ModuleStatus::normal(text)
        } else {
            ModuleStatus::warning(text)
        })
    }
}

fn format_relative_time(unix_secs: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let diff = now.saturating_sub(unix_secs);
    if diff < 10 {
        "just now".to_string()
    } else if diff < 60 {
        format!("{}s ago", diff)
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else {
        format!("{}h ago", diff / 3600)
    }
}

/// Capitalizes the first character of an id string for display (e.g.
/// "rss" -> "Rss"), matching this app's Title Case badge convention.
fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};

    #[test]
    fn test_title_case() {
        assert_eq!(title_case("rss"), "Rss");
        assert_eq!(title_case(""), "");
        assert_eq!(title_case("a"), "A");
    }

    fn rendered_content(module: &mut DaemonStatusModule) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = MokuTheme::default();
        terminal
            .draw(|frame| module.draw(frame, Rect::new(0, 0, 80, 24), &theme))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn test_draw_shows_running_and_autostart_enabled_badges() {
        let mut module = DaemonStatusModule::new();
        module.is_running = true;
        module.pid = Some(4242);
        module.autostart_enabled = true;
        let content = rendered_content(&mut module);
        assert!(content.contains("Running"));
        assert!(content.contains("4242"));
        assert!(content.contains("Enabled"));
        assert!(content.contains("Disable Autostart"));
    }

    #[test]
    fn test_draw_shows_stopped_and_autostart_disabled_badges() {
        let mut module = DaemonStatusModule::new();
        module.is_running = false;
        module.pid = None;
        module.autostart_enabled = false;
        let content = rendered_content(&mut module);
        assert!(content.contains("Stopped"));
        assert!(content.contains("Disabled"));
        assert!(content.contains("Enable Autostart"));
    }
}
