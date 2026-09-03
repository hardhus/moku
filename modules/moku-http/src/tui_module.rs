use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
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

use moku_core::{AppContext, Command, ModuleId, ModuleMeta, MokuTheme, TuiModule, resolve_event};

use crate::engine::{self, RunResult};
use crate::model::Collection;

enum Mode {
    /// No collection loaded yet, or the user pressed `[o]` to open a
    /// different one — a plain (unmasked) text input, same shape as the
    /// masked prompts elsewhere in the app but without the `•` rendering.
    EnterPath { input: String },
    Browsing,
}

pub struct HttpModule {
    mode: Mode,
    collection_path: Option<PathBuf>,
    collection: Option<Collection>,
    state: ListState,
    last_results: HashMap<String, RunResult>,
    is_running: bool,
    running: Arc<Mutex<Option<Result<Vec<RunResult>>>>>,
    message: Option<(String, Instant)>,
}

impl HttpModule {
    pub fn new() -> Self {
        Self {
            mode: Mode::EnterPath { input: String::new() },
            collection_path: None,
            collection: None,
            state: ListState::default(),
            last_results: HashMap::new(),
            is_running: false,
            running: Arc::new(Mutex::new(None)),
            message: None,
        }
    }

    fn show_message(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now()));
    }

    fn selected_name(&self) -> Option<String> {
        let collection = self.collection.as_ref()?;
        let i = self.state.selected()?;
        collection.requests.get(i).map(|r| r.name.clone())
    }

    fn select_next(&mut self) {
        let Some(c) = &self.collection else { return };
        if c.requests.is_empty() {
            return;
        }
        let i = match self.state.selected() {
            Some(i) => (i + 1) % c.requests.len(),
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn select_previous(&mut self) {
        let Some(c) = &self.collection else { return };
        if c.requests.is_empty() {
            return;
        }
        let i = match self.state.selected() {
            Some(i) => (i + c.requests.len() - 1) % c.requests.len(),
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn load_path(&mut self, path_str: &str) {
        let path = PathBuf::from(path_str.trim());
        match engine::load_collection(&path) {
            Ok((collection, _raw)) => {
                let count = collection.requests.len();
                self.collection = Some(collection);
                self.collection_path = Some(path);
                self.last_results.clear();
                self.state.select(if count > 0 { Some(0) } else { None });
                self.mode = Mode::Browsing;
                self.show_message(format!("Loaded {count} request(s)."));
            }
            Err(e) => self.show_message(format!("Failed to load: {e}")),
        }
    }

    fn start_run(&mut self, ctx: &AppContext, only: Option<String>) {
        let Some(path) = self.collection_path.clone() else { return };
        if self.is_running {
            return;
        }
        let storage = Arc::clone(&ctx.storage);
        let slot = Arc::clone(&self.running);
        self.is_running = true;
        self.show_message("Running...");
        tokio::spawn(async move {
            let result = engine::run_collection(&path, only.as_deref(), &[], Some(storage.as_ref())).await;
            *slot.lock().unwrap() = Some(result);
        });
    }

    fn poll_run_result(&mut self) -> bool {
        let taken = self.running.lock().unwrap().take();
        let Some(result) = taken else { return false };
        self.is_running = false;
        match result {
            Ok(results) => {
                let any_failed = results.iter().any(|r| !r.all_passed());
                for r in results {
                    self.last_results.insert(r.name.clone(), r);
                }
                self.show_message(if any_failed { "Run complete — some assertions failed." } else { "Run complete — all passed." });
            }
            Err(e) => self.show_message(format!("Run failed: {e}")),
        }
        true
    }

    fn draw_enter_path(&self, frame: &mut Frame, area: Rect, theme: &MokuTheme, input: &str) {
        let chunks = Layout::vertical([Constraint::Percentage(40), Constraint::Length(3), Constraint::Length(2), Constraint::Percentage(40)]).split(area);
        let input_chunk = Layout::horizontal([Constraint::Percentage(15), Constraint::Percentage(70), Constraint::Percentage(15)]).split(chunks[1])[1];

        let p = Paragraph::new(input)
            .block(
                Block::default()
                    .title(" Collection file path (.toml) ")
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.info)),
            )
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg));
        frame.render_widget(p, input_chunk);

        let hint = Paragraph::new("[Enter] load  [Esc] back to launcher").alignment(Alignment::Center).style(Style::default().fg(theme.base_fg));
        frame.render_widget(hint, chunks[2]);
    }
}

impl Default for HttpModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for HttpModule {
    fn id(&self) -> ModuleId {
        ModuleId::HTTP
    }
    fn title(&self) -> &'static str {
        ModuleId::HTTP.title()
    }
    fn encrypt_by_default(&self) -> bool {
        false
    }
}

#[async_trait]
impl TuiModule for HttpModule {
    async fn init(&mut self, _ctx: &mut AppContext) -> Result<()> {
        Ok(())
    }

    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        if self.poll_run_result() {
            return Ok(true);
        }

        let Event::Key(key) = event else { return Ok(false) };
        if key.kind != KeyEventKind::Press {
            return Ok(false);
        }

        if let Mode::EnterPath { input } = &mut self.mode {
            match key.code {
                KeyCode::Enter => {
                    let path = input.clone();
                    self.load_path(&path);
                }
                KeyCode::Esc => {
                    ctx.navigate_to(ModuleId::LAUNCHER);
                }
                KeyCode::Char(c) => input.push(c),
                KeyCode::Backspace => {
                    input.pop();
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

        match key.code {
            KeyCode::Enter | KeyCode::Char('r') => {
                if let Some(name) = self.selected_name() {
                    self.start_run(ctx, Some(name));
                }
            }
            KeyCode::Char('R') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.start_run(ctx, None);
            }
            KeyCode::Char('o') => {
                self.mode = Mode::EnterPath { input: String::new() };
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

        if let Mode::EnterPath { input } = &self.mode {
            self.draw_enter_path(frame, area, theme, input);
            return;
        }

        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);
        let panes = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).split(chunks[0]);

        let items: Vec<ListItem> = match &self.collection {
            Some(c) if !c.requests.is_empty() => c
                .requests
                .iter()
                .map(|r| {
                    let mark = match self.last_results.get(&r.name) {
                        Some(res) if res.all_passed() => "✓",
                        Some(_) => "✗",
                        None => "·",
                    };
                    ListItem::new(format!("{mark} {} [{}]", r.name, r.method)).style(Style::default().fg(theme.base_fg))
                })
                .collect(),
            _ => vec![ListItem::new("  No requests. [o] to open a collection file.")],
        };
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Requests ").border_style(Style::default().fg(theme.border)).style(Style::default().bg(theme.base_bg)))
            .highlight_style(Style::default().fg(theme.selection_fg).bg(theme.selection_bg).add_modifier(Modifier::BOLD))
            .highlight_symbol(">> ");
        frame.render_stateful_widget(list, panes[0], &mut self.state);

        let detail = match self.selected_name().and_then(|name| self.last_results.get(&name)) {
            Some(r) => {
                let status = r.status.map(|s| s.to_string()).unwrap_or_else(|| "-".to_string());
                let mut lines = vec![format!("Status: {status}"), format!("Time:   {:.0}ms", r.duration.as_secs_f64() * 1000.0)];
                if let Some(e) = &r.error {
                    lines.push(format!("Error:  {e}"));
                }
                for a in &r.assertion_results {
                    lines.push(format!("{} {}", if a.passed { "✓" } else { "✗" }, a.description));
                }
                lines.push(String::new());
                lines.push("Body:".to_string());
                lines.push(r.body_preview.clone());
                lines.join("\n")
            }
            None => "  (not run yet — [Enter]/[r] to run selected, [Shift+R] to run all)".to_string(),
        };
        let detail_widget = Paragraph::new(detail)
            .style(Style::default().fg(theme.base_fg))
            .block(Block::default().borders(Borders::ALL).title(" Response ").border_style(Style::default().fg(theme.border)).style(Style::default().bg(theme.base_bg)));
        frame.render_widget(detail_widget, panes[1]);

        let help = self
            .message
            .as_ref()
            .map(|(m, _)| m.clone())
            .unwrap_or_else(|| " [Enter]/[r] Run  [Shift+R] Run all  [o] Open  [Esc] Back ".to_string());
        let help_widget = Paragraph::new(help)
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme.border)));
        frame.render_widget(help_widget, chunks[1]);
    }
}
