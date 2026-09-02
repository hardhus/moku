use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph,
        canvas::{Canvas, Line as CanvasLine, Points},
    },
};

use moku_core::{AppContext, Command, ModuleId, ModuleMeta, MokuTheme, TuiModule, resolve_event};
use satz_core::{DocId, Document, Index, VaultGraph};

use crate::engine::{NotesConfig, build_index, ensure_daily_note, resolve_vault_root};

#[derive(Clone, Copy, PartialEq, Eq)]
enum FilterMode {
    All,
    Orphans,
    Broken,
}

impl FilterMode {
    fn label(&self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Orphans => "orphans",
            Self::Broken => "broken links",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::All => Self::Orphans,
            Self::Orphans => Self::Broken,
            Self::Broken => Self::All,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    List,
    Graph,
}

struct NoteRow {
    id: String,
    path: String,
}

fn row_from_doc(doc: &Document) -> NoteRow {
    NoteRow { id: doc.id.as_str().to_string(), path: doc.path.to_string_lossy().replace('\\', "/") }
}

pub struct NotesModule {
    vault_root: Option<PathBuf>,
    index: Option<Index>,
    rows: Vec<NoteRow>,
    state: ListState,
    filter: FilterMode,
    view: View,
    message: Option<(String, Instant)>,
    error: Option<String>,
}

impl NotesModule {
    pub fn new() -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        Self { vault_root: None, index: None, rows: Vec::new(), state, filter: FilterMode::All, view: View::List, message: None, error: None }
    }

    fn load(&mut self, ctx: &AppContext) {
        let config: NotesConfig = ctx.config.load().resolve_module_config("notes");
        let result = resolve_vault_root(&config, None).and_then(|root| build_index(&root).map(|idx| (root, idx)));
        match result {
            Ok((root, index)) => {
                self.vault_root = Some(root);
                self.index = Some(index);
                self.error = None;
                self.apply_filter();
            }
            Err(e) => {
                self.error = Some(e.to_string());
                self.index = None;
                self.rows.clear();
            }
        }
    }

    fn apply_filter(&mut self) {
        let Some(index) = &self.index else {
            self.rows.clear();
            return;
        };
        let mut rows: Vec<NoteRow> = match self.filter {
            FilterMode::All => index.documents().map(row_from_doc).collect(),
            FilterMode::Orphans => index.orphan_docs().map(row_from_doc).collect(),
            FilterMode::Broken => index.docs_with_broken_links().map(|(doc, _)| row_from_doc(doc)).collect(),
        };
        rows.sort_by(|a, b| a.path.cmp(&b.path));
        self.rows = rows;
        let out_of_range = self.state.selected().map(|i| i >= self.rows.len()).unwrap_or(true);
        if out_of_range && !self.rows.is_empty() {
            self.state.select(Some(0));
        }
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

    fn selected_backlinks(&self) -> Vec<String> {
        let (Some(index), Some(i)) = (&self.index, self.state.selected()) else { return Vec::new() };
        let Some(row) = self.rows.get(i) else { return Vec::new() };
        let id = DocId::new(row.id.clone());
        index.backlinks_of(&id).map(|d| d.as_str().to_string()).collect()
    }

    fn show_message(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now()));
    }

    fn create_daily(&mut self) {
        let Some(root) = self.vault_root.clone() else {
            self.show_message("No vault configured — set [modules.notes] vault_path.");
            return;
        };
        match ensure_daily_note(&root) {
            Ok((path, true)) => self.show_message(format!("Created {}", path.display())),
            Ok((path, false)) => self.show_message(format!("Daily note: {}", path.display())),
            Err(e) => self.show_message(format!("Failed to create daily note: {e}")),
        }
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let chunks = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).split(area);

        let items: Vec<ListItem> = if let Some(err) = &self.error {
            vec![ListItem::new(format!("  Error: {err}")).style(Style::default().fg(theme.error))]
        } else if self.rows.is_empty() {
            vec![ListItem::new("  No notes found (or vault not configured).")]
        } else {
            self.rows.iter().map(|row| ListItem::new(row.path.clone()).style(Style::default().fg(theme.base_fg))).collect()
        };
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" Notes ({}) ", self.filter.label()))
                    .border_style(Style::default().fg(theme.border))
                    .style(Style::default().bg(theme.base_bg)),
            )
            .highlight_style(Style::default().fg(theme.selection_fg).bg(theme.selection_bg).add_modifier(Modifier::BOLD))
            .highlight_symbol(">> ");
        frame.render_stateful_widget(list, chunks[0], &mut self.state);

        let backlinks = self.selected_backlinks();
        let backlink_text =
            if backlinks.is_empty() { "  (none)".to_string() } else { backlinks.iter().map(|b| format!("  {b}")).collect::<Vec<_>>().join("\n") };
        let side = Paragraph::new(backlink_text).style(Style::default().fg(theme.base_fg)).block(
            Block::default().borders(Borders::ALL).title(" Backlinks ").border_style(Style::default().fg(theme.border)).style(Style::default().bg(theme.base_bg)),
        );
        frame.render_widget(side, chunks[1]);
    }

    fn draw_graph(&self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let Some(index) = &self.index else {
            frame.render_widget(Paragraph::new("No vault indexed.").style(Style::default().fg(theme.base_fg)), area);
            return;
        };
        let graph = VaultGraph::build(index);
        let data = graph.to_data();

        // Simple, deterministic circular layout — a full force-directed
        // simulation is explicitly out of scope for v1 (plan Bölüm B):
        // this is a visual overview to browse from, not an editor.
        let n = data.nodes.len().max(1) as f64;
        let mut positions: HashMap<String, (f64, f64)> = HashMap::new();
        for (i, node) in data.nodes.iter().enumerate() {
            let angle = 2.0 * std::f64::consts::PI * (i as f64) / n;
            positions.insert(node.id.clone(), (angle.cos(), angle.sin()));
        }
        let edges = data.edges.clone();
        let node_count = data.nodes.len();
        let edge_count = data.edges.len();

        let canvas = Canvas::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" Graph ({node_count} notes, {edge_count} links) — [g] back to list "))
                    .border_style(Style::default().fg(theme.border)),
            )
            .x_bounds([-1.3, 1.3])
            .y_bounds([-1.3, 1.3])
            .paint(move |ctx| {
                for edge in &edges {
                    if let (Some(&(x1, y1)), Some(&(x2, y2))) = (positions.get(&edge.source), positions.get(&edge.target)) {
                        ctx.draw(&CanvasLine { x1, y1, x2, y2, color: Color::DarkGray });
                    }
                }
                for &(x, y) in positions.values() {
                    ctx.draw(&Points { coords: &[(x, y)], color: Color::Cyan });
                }
            });
        frame.render_widget(canvas, area);
    }
}

impl Default for NotesModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for NotesModule {
    fn id(&self) -> ModuleId {
        ModuleId::NOTES
    }
    fn title(&self) -> &'static str {
        ModuleId::NOTES.title()
    }
    fn encrypt_by_default(&self) -> bool {
        false // plain markdown files on disk, not moku's StorageManager
    }
}

#[async_trait]
impl TuiModule for NotesModule {
    async fn init(&mut self, ctx: &mut AppContext) -> Result<()> {
        self.load(ctx);
        Ok(())
    }

    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        let command = resolve_event(event, &ctx.config.load().keys, None);
        match command {
            Command::Up => self.select_previous(),
            Command::Down => self.select_next(),
            Command::Back | Command::Quit if self.view == View::List => {
                ctx.navigate_to(ModuleId::LAUNCHER);
                return Ok(true);
            }
            _ => {}
        }

        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Esc if self.view == View::Graph => self.view = View::List,
                KeyCode::Char('g') => self.view = if self.view == View::List { View::Graph } else { View::List },
                KeyCode::Char('f') => {
                    self.filter = self.filter.next();
                    self.apply_filter();
                }
                KeyCode::Char('r') => {
                    self.load(ctx);
                    self.show_message("Reloaded.");
                }
                KeyCode::Char('d') => self.create_daily(),
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

        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);

        match self.view {
            View::List => self.draw_list(frame, chunks[0], theme),
            View::Graph => self.draw_graph(frame, chunks[0], theme),
        }

        let help = self
            .message
            .as_ref()
            .map(|(m, _)| m.clone())
            .unwrap_or_else(|| " [f] Filter  [g] Graph  [d] Daily note  [r] Reload  [Esc] Back ".to_string());
        let help_widget = Paragraph::new(help)
            .alignment(Alignment::Left)
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme.border)));
        frame.render_widget(help_widget, chunks[1]);
    }
}
