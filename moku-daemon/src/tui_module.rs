use std::time::{Instant, Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Rect, Layout, Constraint},
    style::{Modifier, Style},
    widgets::{Block, Borders, Paragraph, List, ListItem},
};
use moku_core::{AppContext, Command, ModuleId, ModuleMeta, MokuTheme, TuiModule, resolve_event};

pub struct DaemonStatusModule {
    is_running: bool,
    pid: Option<u32>,
    last_checked: Instant,
    message: Option<String>,
    message_time: Option<Instant>,
}

impl DaemonStatusModule {
    pub fn new() -> Self {
        Self {
            is_running: false,
            pid: None,
            last_checked: Instant::now(),
            message: None,
            message_time: None,
        }
    }

    fn refresh_status(&mut self) {
        self.pid = crate::pid::read();
        self.is_running = self.pid.map(crate::status::pid_is_alive).unwrap_or(false);
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
                    KeyCode::Char('e') => {
                        if let Ok(exe) = std::env::current_exe() {
                            match crate::autostart::set_autostart(true, &exe, &["daemon", "run"]) {
                                Ok(_) => self.show_temp_message("Autostart enabled."),
                                Err(e) => self.show_temp_message(format!("Autostart error: {e}")),
                            }
                        }
                        changed = true;
                    }
                    KeyCode::Char('d') => {
                        if let Ok(exe) = std::env::current_exe() {
                            match crate::autostart::set_autostart(false, &exe, &["daemon", "run"]) {
                                Ok(_) => self.show_temp_message("Autostart disabled."),
                                Err(e) => self.show_temp_message(format!("Autostart error: {e}")),
                            }
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
            .style(Style::default().fg(theme.selection_fg).bg(theme.selection_bg).add_modifier(Modifier::BOLD))
            .alignment(ratatui::layout::Alignment::Center)
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme.border)));
        frame.render_widget(header, chunks[0]);

        // 2. Status Panel
        let status_str = if self.is_running {
            format!(" [Running] (PID: {})", self.pid.unwrap_or(0))
        } else {
            " [Stopped]".to_string()
        };
        let last_check_str = format!("Last Checked: {}s ago", self.last_checked.elapsed().as_secs());
        let info_text = format!(
            "  Daemon Status: {}\n  {}\n  The daemon runs unencrypted tasks in the background.",
            status_str, last_check_str
        );
        let info = Paragraph::new(info_text)
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
            .block(Block::default().title(" Status ").borders(Borders::ALL).border_style(Style::default().fg(theme.border)));
        frame.render_widget(info, chunks[1]);

        // 3. Tasks Panel
        let data_dir = moku_core::dirs::get_data_dir().ok();
        let task_statuses = data_dir
            .as_deref()
            .map(|d| crate::task_status::read_statuses(d))
            .unwrap_or_default();

        let task_items: Vec<ListItem> = if task_statuses.is_empty() {
            vec![ListItem::new("  No task data available. Is the daemon running?")]
        } else {
            task_statuses
                .iter()
                .map(|t| {
                    let last = t.last_run_secs
                        .map(format_relative_time)
                        .unwrap_or_else(|| "never".to_string());
                    let status = match &t.last_error {
                        None => format!("OK (processed: {})", t.last_item_count),
                        Some(e) => format!("ERR: {}", &e[..e.len().min(40)]),
                    };
                    ListItem::new(format!("  • [{}]  Last Run: {}  Status: {}", title_case(&t.id), last, status))
                })
                .collect()
        };

        let tasks_list = List::new(task_items)
            .block(Block::default().title(" Background Tasks ").borders(Borders::ALL).border_style(Style::default().fg(theme.border)))
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg));
        frame.render_widget(tasks_list, chunks[2]);

        // 4. Message Line
        let msg_content = self.message.clone().unwrap_or_default();
        let msg_widget = Paragraph::new(format!(" {}", msg_content))
            .style(Style::default().fg(theme.selection_fg).add_modifier(Modifier::ITALIC));
        frame.render_widget(msg_widget, chunks[3]);

        // 5. Help Bar
        let help_text = " [s] Start | [k] Stop | [r] Refresh | [e] Enable Autostart | [d] Disable Autostart | [Esc] Back ";
        let help = Paragraph::new(help_text)
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme.border)));
        frame.render_widget(help, chunks[4]);
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

    #[test]
    fn test_title_case() {
        assert_eq!(title_case("rss"), "Rss");
        assert_eq!(title_case(""), "");
        assert_eq!(title_case("a"), "A");
    }
}
