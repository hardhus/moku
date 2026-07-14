use std::time::{Instant, Duration};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Rect, Layout, Constraint},
    style::{Modifier, Style},
    widgets::{Block, Borders, Paragraph},
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
        self.is_running = self.pid
            .map(|p| {
                let mut sys = sysinfo::System::new_all();
                sys.refresh_all();
                sys.process(sysinfo::Pid::from_u32(p)).is_some()
            })
            .unwrap_or(false);
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
                                let _ = std::process::Command::new(exe)
                                    .arg("daemon")
                                    .arg("run")
                                    .spawn();
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
                            let mut sys = sysinfo::System::new_all();
                            sys.refresh_all();
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
        if let Some(msg_time) = self.message_time {
            if msg_time.elapsed() > Duration::from_secs(3) {
                self.message = None;
                self.message_time = None;
            }
        }

        let chunks = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(area);

        let header = Paragraph::new(" MOKU DAEMON MANAGER ")
            .style(Style::default().fg(theme.selection_fg).bg(theme.selection_bg).add_modifier(Modifier::BOLD))
            .alignment(ratatui::layout::Alignment::Center)
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme.border)));
        frame.render_widget(header, chunks[0]);

        let status_str = if self.is_running {
            format!("🟢 RUNNING (PID: {})", self.pid.unwrap_or(0))
        } else {
            "⚫ STOPPED".to_string()
        };

        let last_check_str = format!("Last Checked: {:?}", self.last_checked.elapsed());

        let info_text = format!(
            "\n  Daemon Status:  {}\n\n  {}\n\n  The daemon runs periodic tasks in the background (like RSS feeds).\n  It runs unencrypted so it doesn't need to unlock your vault.\n",
            status_str, last_check_str
        );

        let info = Paragraph::new(info_text)
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme.border)));
        frame.render_widget(info, chunks[1]);

        let msg_content = self.message.clone().unwrap_or_default();
        let msg_widget = Paragraph::new(format!(" {}", msg_content))
            .style(Style::default().fg(theme.selection_fg).add_modifier(Modifier::ITALIC));
        frame.render_widget(msg_widget, chunks[2]);

        let help_text = " [s] Start | [k] Stop | [r] Refresh | [e] Enable Autostart | [d] Disable Autostart | [Esc] Back ";
        let help = Paragraph::new(help_text)
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme.border)));
        frame.render_widget(help, chunks[3]);
    }
}
