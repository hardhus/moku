use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Margin, Rect},
    style::Style,
    widgets::{Block, Borders, Paragraph},
};

use moku_core::{AppContext, ModuleId, ModuleMeta, ModuleStatus, MokuTheme, TuiModule, keys_match};
use satz_core::{DocId, Document, Index, parser::structure::parse_structure};

use crate::config::{self, ZkViewColors, ZkViewConfig, ZkViewPalette};
use crate::cursor;
use crate::engine;
use crate::jumplist::{self, JumpAction, JumpListState};
use crate::model::{Cursor, LinkResState, NavFrame, NoteEntry, OpenDoc};
use crate::notelist::{self, NoteListAction, NoteListState};
use crate::references::{self, ReferenceAction, ReferencesState};
use crate::render;
use crate::viewport;

/// A "g"/find-char prefix awaiting its second key times out after this
/// long so a stray keypress doesn't silently swallow the next unrelated
/// one.
const PENDING_CHORD_TIMEOUT: Duration = Duration::from_millis(600);

/// `f`/`F`/`t`/`T` — which direction/style of same-line character search is
/// pending its target char.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FindKind {
    Forward,
    Backward,
    TillForward,
    TillBackward,
}

/// A `g`-prefixed goto chord (`gg`/`ge`/`gs`/`gl`/`gd`/`gr`), or an
/// `f`/`F`/`t`/`T` awaiting its target character.
enum PendingChord {
    Goto,
    FindChar(FindKind),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ViewMode {
    NoteList,
    Document,
    JumpList,
    References,
}

pub struct ZkViewModule {
    vault_root: Option<PathBuf>,
    index: Option<Index>,
    notes: Vec<NoteEntry>,
    note_list: NoteListState,
    view: ViewMode,
    open: Option<OpenDoc>,
    cursor: Cursor,
    scroll_top: usize,
    /// Last known height of the document viewport, refreshed every draw —
    /// used by cursor motions (in `handle_event`, which has no access to
    /// the render area) to keep the viewport following the cursor.
    viewport_height: usize,
    nav_stack: Vec<NavFrame>,
    jump: Option<JumpListState>,
    references: Option<ReferencesState>,
    /// This module's own markdown-specific color palette, resolved from
    /// `[modules.zk_view.colors]` — refreshed whenever the vault (re)loads
    /// (see `load_vault`), since `draw()` has no `AppContext` to read
    /// config from directly.
    palette: ZkViewPalette,
    pending_chord: Option<(PendingChord, Instant)>,
    /// Set right before requesting an editor suspend; consumed at the top
    /// of the next `draw()` call (guaranteed to run before further input is
    /// processed — see `moku-bin/src/app_loop.rs`'s suspend-drain, which
    /// sets `dirty = true`).
    needs_reload_current: bool,
    busy: bool,
    message: Option<(String, Instant)>,
    error: Option<String>,
}

impl ZkViewModule {
    pub fn new() -> Self {
        Self {
            vault_root: None,
            index: None,
            notes: Vec::new(),
            note_list: NoteListState::new(),
            view: ViewMode::NoteList,
            open: None,
            cursor: Cursor::default(),
            scroll_top: 0,
            viewport_height: 20,
            nav_stack: Vec::new(),
            jump: None,
            references: None,
            palette: ZkViewPalette::from_colors(&ZkViewColors::default()),
            pending_chord: None,
            needs_reload_current: false,
            busy: false,
            message: None,
            error: None,
        }
    }

    fn show_message(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now()));
    }

    fn module_config(ctx: &AppContext) -> ZkViewConfig {
        ctx.config
            .load()
            .resolve_module_config(ModuleId::ZK_VIEW.as_str())
    }

    // `walk_vault` (inside `build_index`) walks the whole notes directory on
    // disk and parses every file — synchronous and potentially slow for a
    // large vault, so it's offloaded to tokio's blocking-thread pool (same
    // reasoning as `modules/moku-satz`'s own `load`).
    async fn load_vault(&mut self, ctx: &AppContext) {
        let cfg = Self::module_config(ctx);
        self.palette = ZkViewPalette::from_colors(&cfg.colors);
        match config::resolve_vault_root(&cfg) {
            Ok(root) => {
                let root_for_index = root.clone();
                match tokio::task::spawn_blocking(move || engine::build_index(&root_for_index)).await {
                    Ok(Ok(index)) => {
                        self.notes = engine::build_note_list(&index);
                        self.index = Some(index);
                        self.vault_root = Some(root);
                        self.error = None;
                        self.note_list.apply_filter(&self.notes);
                    }
                    Ok(Err(e)) => self.error = Some(e.to_string()),
                    Err(join_err) => self.error = Some(format!("index build task panicked: {join_err}")),
                }
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    async fn reindex_vault(&mut self, ctx: &AppContext) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.load_vault(ctx).await;
        self.busy = false;
        self.show_message("Reindexed.");
    }

    fn open_note_by_idx(&mut self, idx: usize) {
        let Some(note) = self.notes.get(idx) else {
            return;
        };
        let id = note.id.clone();
        self.open_doc(id, 0);
    }

    fn open_doc(&mut self, id: DocId, line: usize) {
        let Some(root) = self.vault_root.clone() else {
            self.error = Some("Vault not loaded.".to_string());
            return;
        };
        let (doc, link_res) = match &self.index {
            Some(index) => match index.get_doc(&id) {
                Some(d) => {
                    let d = d.clone();
                    let res = engine::build_link_resolution_cache(index, &d);
                    (d, res)
                }
                None => {
                    self.error = Some(format!("Document not found: {}", id.as_str()));
                    return;
                }
            },
            None => {
                self.error = Some("Vault not loaded.".to_string());
                return;
            }
        };
        let full_path = root.join(&doc.path);
        let source = match std::fs::read_to_string(&full_path) {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(format!("Failed to read {}: {e}", full_path.display()));
                return;
            }
        };
        self.set_open_doc(doc, source, link_res, line);
        self.view = ViewMode::Document;
    }

    fn set_open_doc(&mut self, doc: Document, source: String, link_res: Vec<LinkResState>, line: usize) {
        let structure = parse_structure(&source);
        let line_ranges = cursor::compute_line_ranges(&source);
        let blocks = render::build_layout(&source, &line_ranges, &structure, &doc.frontmatter);
        let row_offsets = render::compute_row_offsets(&blocks);
        let total_rows = render::total_rows(&blocks);
        let list_marker_ranges = render::compute_list_marker_ranges(&source, &structure);
        let quote_marker_ranges = render::compute_quote_marker_ranges(&source, &line_ranges, &structure);
        let checked_task_tail_ranges = render::compute_checked_task_tail_ranges(&line_ranges, &structure);
        let code_fence_delim_lines = render::compute_code_fence_delim_lines(&line_ranges, &structure);
        let clamped_line = line.min(line_ranges.len().saturating_sub(1));
        self.cursor = Cursor { line: clamped_line, col: 0 };
        self.scroll_top = 0;
        self.open = Some(OpenDoc {
            doc,
            source,
            structure,
            line_ranges,
            link_res,
            blocks,
            row_offsets,
            total_rows,
            list_marker_ranges,
            quote_marker_ranges,
            checked_task_tail_ranges,
            code_fence_delim_lines,
        });
        self.update_scroll_for_cursor();
    }

    fn reload_current_file(&mut self) {
        let Some(root) = self.vault_root.clone() else {
            return;
        };
        let rel_path = match &self.open {
            Some(open) => open.doc.path.clone(),
            None => return,
        };
        let doc = match &mut self.index {
            Some(index) => match engine::reload_single_file(index, &root, &rel_path) {
                Ok(d) => d,
                Err(e) => {
                    self.error = Some(format!("Reload failed: {e}"));
                    return;
                }
            },
            None => return,
        };
        let source = match std::fs::read_to_string(root.join(&rel_path)) {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(format!("Reload failed: {e}"));
                return;
            }
        };
        let link_res = match &self.index {
            Some(index) => engine::build_link_resolution_cache(index, &doc),
            None => Vec::new(),
        };
        let line = self.cursor.line;
        self.set_open_doc(doc, source, link_res, line);
        self.show_message("Reloaded after editor.");
    }

    fn update_scroll_for_cursor(&mut self) {
        let row = match &self.open {
            Some(open) => render::line_to_row(&open.blocks, &open.row_offsets, self.cursor.line),
            None => return,
        };
        let window = self.viewport_height.max(1);
        let margin = (window / 4).min(4);
        self.scroll_top = viewport::recompute_doc_viewport(self.scroll_top, row, window, margin);
    }

    fn with_cursor_motion(&mut self, f: impl FnOnce(&OpenDoc, Cursor) -> Cursor) {
        let cur = self.cursor;
        let new_c = match &self.open {
            Some(open) => f(open, cur),
            None => return,
        };
        self.cursor = new_c;
        self.update_scroll_for_cursor();
    }

    fn follow_link_res(&mut self, res: Option<LinkResState>, ctx: &mut AppContext) {
        match res {
            Some(LinkResState::Resolved { target_id, anchor_line, .. }) => {
                self.navigate_to_doc(target_id, anchor_line)
            }
            Some(LinkResState::AnchorMissing { target_id, .. }) => {
                ctx.show_warning("Heading/block not found — opening document top.");
                self.navigate_to_doc(target_id, 0);
            }
            Some(LinkResState::DocMissing) | None => {
                ctx.show_error("Broken link: target document not found.");
            }
        }
    }

    fn follow_link_under_cursor(&mut self, ctx: &mut AppContext) {
        let cur = self.cursor;
        let Some(open) = &self.open else {
            return;
        };
        let offset = cursor::cursor_to_byte_offset(&open.line_ranges, &open.source, cur);
        let Some(idx) = open.doc.links.iter().position(|l| l.range.contains(offset)) else {
            ctx.show_info("Not on a link.");
            return;
        };
        let res = open.link_res.get(idx).cloned();
        self.follow_link_res(res, ctx);
    }

    fn follow_jump_link(&mut self, link_idx: usize, ctx: &mut AppContext) {
        let res = match &self.open {
            Some(open) => open.link_res.get(link_idx).cloned(),
            None => None,
        };
        self.follow_link_res(res, ctx);
    }

    /// Pushes a "go back" frame for the current document, then opens
    /// `target_id` at `line` — used by `gd` and the jump list.
    fn navigate_to_doc(&mut self, target_id: DocId, line: usize) {
        let cur = self.cursor;
        let scroll = self.scroll_top;
        let frame = match &self.open {
            Some(open) => Some(NavFrame {
                doc_id: open.doc.id.clone(),
                cursor: cur,
                scroll_top: scroll,
            }),
            None => None,
        };
        if let Some(frame) = frame {
            self.nav_stack.push(frame);
        }
        self.open_doc(target_id, line);
    }

    fn open_jump_list(&mut self) {
        let Some(open) = &self.open else {
            return;
        };
        self.jump = Some(JumpListState::new(open));
        self.view = ViewMode::JumpList;
    }

    /// `gr` — "what links here" for the currently open document.
    fn open_references(&mut self) {
        let (Some(index), Some(open)) = (&self.index, &self.open) else {
            return;
        };
        self.references = Some(ReferencesState::new(index, &open.doc.id));
        self.view = ViewMode::References;
    }

    fn apply_find_char(&mut self, kind: FindKind, target: char) {
        let new_c = match &self.open {
            Some(open) => match kind {
                FindKind::Forward => {
                    cursor::find_char_forward(&open.line_ranges, &open.source, self.cursor, target)
                }
                FindKind::Backward => {
                    cursor::find_char_backward(&open.line_ranges, &open.source, self.cursor, target)
                }
                FindKind::TillForward => {
                    cursor::till_char_forward(&open.line_ranges, &open.source, self.cursor, target)
                }
                FindKind::TillBackward => {
                    cursor::till_char_backward(&open.line_ranges, &open.source, self.cursor, target)
                }
            },
            None => None,
        };
        if let Some(c) = new_c {
            self.cursor = c;
            self.update_scroll_for_cursor();
        }
    }

    fn go_back(&mut self) {
        if let Some(frame) = self.nav_stack.pop() {
            self.restore_nav_frame(frame);
        } else {
            self.view = ViewMode::NoteList;
            self.open = None;
        }
    }

    fn restore_nav_frame(&mut self, frame: NavFrame) {
        let Some(root) = self.vault_root.clone() else {
            return;
        };
        let (doc, link_res) = match &self.index {
            Some(index) => match index.get_doc(&frame.doc_id) {
                Some(d) => {
                    let d = d.clone();
                    let res = engine::build_link_resolution_cache(index, &d);
                    (d, res)
                }
                None => return,
            },
            None => return,
        };
        let full_path = root.join(&doc.path);
        let source = match std::fs::read_to_string(&full_path) {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(format!("Failed to reopen {}: {e}", full_path.display()));
                return;
            }
        };
        self.set_open_doc(doc, source, link_res, frame.cursor.line);
        self.cursor = frame.cursor;
        self.scroll_top = frame.scroll_top;
    }

    fn editor_key_matches(&self, ctx: &AppContext, event: &Event) -> bool {
        let Event::Key(k) = event else {
            return false;
        };
        let cfg = Self::module_config(ctx);
        let keys = config::resolve_keys(&cfg);
        match keys.get("open_editor") {
            Some(key_str) => keys_match(*k, key_str),
            None => false,
        }
    }

    fn open_in_editor(&mut self, ctx: &mut AppContext) {
        let Some(root) = self.vault_root.clone() else {
            return;
        };
        let (full_path, line) = match &self.open {
            Some(open) => (root.join(&open.doc.path), self.cursor.line + 1),
            None => return,
        };
        let cfg = Self::module_config(ctx);
        ctx.request_suspend(cfg.editor_command, vec![format!("+{line}"), full_path.display().to_string()]);
        self.needs_reload_current = true;
    }

    fn handle_notelist_event(&mut self, event: &Event, ctx: &mut AppContext) -> bool {
        let Event::Key(key) = event else {
            return false;
        };
        if key.kind != KeyEventKind::Press {
            return false;
        }
        match notelist::handle_key(&mut self.note_list, &self.notes, key) {
            NoteListAction::None => true,
            NoteListAction::Open(idx) => {
                self.open_note_by_idx(idx);
                true
            }
            NoteListAction::Back => {
                ctx.navigate_to(ModuleId::LAUNCHER);
                true
            }
        }
    }

    async fn handle_document_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        let Event::Key(key) = event else {
            return Ok(false);
        };
        if key.kind != KeyEventKind::Press {
            return Ok(false);
        }

        if let Some((chord, at)) = self.pending_chord.take() {
            if at.elapsed() <= PENDING_CHORD_TIMEOUT {
                match chord {
                    PendingChord::Goto => match key.code {
                        KeyCode::Char('g') => {
                            self.with_cursor_motion(|_open, _c| cursor::goto_top());
                            return Ok(true);
                        }
                        // Helix's actual "go to end of file" chord is `ge`
                        // (there's no `G` in Helix at all) — kept alongside
                        // the already-working `G` as a harmless bonus alias.
                        KeyCode::Char('e') => {
                            self.with_cursor_motion(|open, _c| cursor::goto_bottom(&open.line_ranges));
                            return Ok(true);
                        }
                        KeyCode::Char('s') => {
                            self.with_cursor_motion(|open, c| {
                                cursor::goto_first_non_blank(&open.line_ranges, &open.source, c)
                            });
                            return Ok(true);
                        }
                        KeyCode::Char('l') => {
                            self.with_cursor_motion(|open, c| {
                                cursor::goto_line_end(&open.line_ranges, &open.source, c)
                            });
                            return Ok(true);
                        }
                        KeyCode::Char('d') => {
                            self.follow_link_under_cursor(ctx);
                            return Ok(true);
                        }
                        KeyCode::Char('r') => {
                            self.open_references();
                            return Ok(true);
                        }
                        _ => {}
                    },
                    PendingChord::FindChar(kind) => {
                        if let KeyCode::Char(target) = key.code {
                            self.apply_find_char(kind, target);
                            return Ok(true);
                        }
                    }
                }
            }
        }

        if self.editor_key_matches(ctx, event) {
            self.open_in_editor(ctx);
            return Ok(true);
        }

        match key.code {
            KeyCode::Char('h') => self.with_cursor_motion(|_open, c| cursor::move_left(c)),
            KeyCode::Char('l') => {
                self.with_cursor_motion(|open, c| cursor::move_right(&open.line_ranges, &open.source, c))
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.with_cursor_motion(|open, c| cursor::move_down(&open.blocks, &open.line_ranges, &open.source, c))
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.with_cursor_motion(|open, c| cursor::move_up(&open.blocks, &open.line_ranges, &open.source, c))
            }
            KeyCode::Char('w') => {
                self.with_cursor_motion(|open, c| cursor::word_forward(&open.line_ranges, &open.source, c))
            }
            KeyCode::Char('b') => {
                self.with_cursor_motion(|open, c| cursor::word_back(&open.line_ranges, &open.source, c))
            }
            KeyCode::Char('e') => {
                self.with_cursor_motion(|open, c| cursor::word_end_forward(&open.line_ranges, &open.source, c))
            }
            // Shift+w/b/e — vim/Helix's "WORD" motions (punctuation counts
            // as part of the word, e.g. `foo.bar` is one WORD).
            KeyCode::Char('W') => {
                self.with_cursor_motion(|open, c| cursor::word_forward_big(&open.line_ranges, &open.source, c))
            }
            KeyCode::Char('B') => {
                self.with_cursor_motion(|open, c| cursor::word_back_big(&open.line_ranges, &open.source, c))
            }
            KeyCode::Char('E') => {
                self.with_cursor_motion(|open, c| cursor::word_end_forward_big(&open.line_ranges, &open.source, c))
            }
            KeyCode::Char('G') => self.with_cursor_motion(|open, _c| cursor::goto_bottom(&open.line_ranges)),
            KeyCode::Char('g') => self.pending_chord = Some((PendingChord::Goto, Instant::now())),
            KeyCode::Char('f') => {
                self.pending_chord = Some((PendingChord::FindChar(FindKind::Forward), Instant::now()))
            }
            KeyCode::Char('F') => {
                self.pending_chord = Some((PendingChord::FindChar(FindKind::Backward), Instant::now()))
            }
            KeyCode::Char('t') => {
                self.pending_chord = Some((PendingChord::FindChar(FindKind::TillForward), Instant::now()))
            }
            KeyCode::Char('T') => {
                self.pending_chord = Some((PendingChord::FindChar(FindKind::TillBackward), Instant::now()))
            }
            KeyCode::Char('/') => self.open_jump_list(),
            KeyCode::Char('r') => self.reindex_vault(ctx).await,
            KeyCode::Char('q') => ctx.navigate_to(ModuleId::LAUNCHER),
            KeyCode::Esc => self.go_back(),
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn handle_jumplist_event(&mut self, event: &Event, ctx: &mut AppContext) -> bool {
        let Event::Key(key) = event else {
            return false;
        };
        if key.kind != KeyEventKind::Press {
            return false;
        }
        let Some(jump) = &mut self.jump else {
            return false;
        };
        match jumplist::handle_key(jump, key) {
            JumpAction::None => true,
            JumpAction::Close => {
                self.jump = None;
                self.view = ViewMode::Document;
                true
            }
            JumpAction::JumpLink(link_idx) => {
                self.jump = None;
                self.view = ViewMode::Document;
                self.follow_jump_link(link_idx, ctx);
                true
            }
            JumpAction::JumpOffset(offset) => {
                self.jump = None;
                self.view = ViewMode::Document;
                let new_cursor = self
                    .open
                    .as_ref()
                    .map(|open| cursor::byte_offset_to_cursor(&open.line_ranges, &open.source, offset));
                if let Some(c) = new_cursor {
                    self.cursor = c;
                    self.update_scroll_for_cursor();
                }
                true
            }
        }
    }

    fn handle_references_event(&mut self, event: &Event) -> bool {
        let Event::Key(key) = event else {
            return false;
        };
        if key.kind != KeyEventKind::Press {
            return false;
        }
        let Some(refs) = &mut self.references else {
            return false;
        };
        match references::handle_key(refs, key) {
            ReferenceAction::None => true,
            ReferenceAction::Close => {
                self.references = None;
                self.view = ViewMode::Document;
                true
            }
            ReferenceAction::Open(target_id) => {
                self.references = None;
                self.view = ViewMode::Document;
                self.navigate_to_doc(target_id, 0);
                true
            }
        }
    }
}

impl Default for ZkViewModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for ZkViewModule {
    fn id(&self) -> ModuleId {
        ModuleId::ZK_VIEW
    }
    fn title(&self) -> &'static str {
        ModuleId::ZK_VIEW.title()
    }
    fn encrypt_by_default(&self) -> bool {
        false // plain markdown files on disk, not moku's StorageManager
    }
}

#[async_trait]
impl TuiModule for ZkViewModule {
    async fn init(&mut self, ctx: &mut AppContext) -> Result<()> {
        // Only walk the vault the first time this module is entered this
        // session — re-walking on every focus would make simply tabbing
        // back into the viewer unexpectedly slow/reindex-y. Explicit
        // reload paths: 'r' (full reindex) and the automatic single-file
        // reload right after returning from a suspended editor session.
        if self.index.is_none() {
            self.load_vault(ctx).await;
        }
        Ok(())
    }

    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        match self.view {
            ViewMode::NoteList => Ok(self.handle_notelist_event(event, ctx)),
            ViewMode::Document => self.handle_document_event(event, ctx).await,
            ViewMode::JumpList => Ok(self.handle_jumplist_event(event, ctx)),
            ViewMode::References => Ok(self.handle_references_event(event)),
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        if self.needs_reload_current {
            self.reload_current_file();
            self.needs_reload_current = false;
        }
        if let Some((_, at)) = self.message
            && at.elapsed() > Duration::from_secs(6)
        {
            self.message = None;
        }

        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);
        // A small inset keeps the document body from sitting flush against
        // the pane edges. No border here — the list/jump/reference panels
        // already have their own `Block` border for that; this is just
        // breathing room for plain text.
        let doc_area = chunks[0].inner(Margin { horizontal: 2, vertical: 1 });
        self.viewport_height = doc_area.height as usize;

        match self.view {
            ViewMode::NoteList => notelist::draw_note_list(
                frame,
                chunks[0],
                &mut self.note_list,
                &self.notes,
                self.error.as_deref(),
                theme,
            ),
            ViewMode::Document => match &self.open {
                Some(open) => render::draw_document(
                    frame,
                    doc_area,
                    open,
                    self.scroll_top,
                    self.cursor,
                    true,
                    theme,
                    &self.palette,
                ),
                None => {
                    let p = Paragraph::new("No document open.").style(Style::default().fg(theme.base_fg));
                    frame.render_widget(p, doc_area);
                }
            },
            ViewMode::JumpList => {
                if let Some(jump) = &mut self.jump {
                    jumplist::draw_jump_list(frame, chunks[0], jump, theme);
                }
            }
            ViewMode::References => {
                if let Some(refs) = &mut self.references {
                    references::draw_references(frame, chunks[0], refs, theme);
                }
            }
        }

        let help = self.message.as_ref().map(|(m, _)| m.clone()).unwrap_or_else(|| {
            match self.view {
                ViewMode::NoteList => " [/] Filter  [Enter] Open  [r] Reindex  [Esc] Back ".to_string(),
                ViewMode::Document => {
                    " hjkl/w/b/e/W/B/E Move  f/F/t/T Find  gg/ge/gs/gl Goto  gd Follow  gr Refs  / Jump  ctrl-e Edit  r Reindex  Esc Back  q Quit "
                        .to_string()
                }
                ViewMode::JumpList => " Type to filter  Enter Jump  Esc Close ".to_string(),
                ViewMode::References => " j/k Move  Enter Open  Esc Close ".to_string(),
            }
        });
        let help_widget = Paragraph::new(help)
            .alignment(Alignment::Left)
            .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border)),
            );
        frame.render_widget(help_widget, chunks[1]);
    }

    // Rebuilds a fresh index instead of relying on `self.index`, so this
    // works even if the module was never opened this session — same
    // reasoning as `modules/moku-satz`'s own `dashboard_summary`.
    async fn dashboard_summary(&self, ctx: &AppContext) -> Option<ModuleStatus> {
        let cfg = Self::module_config(ctx);
        match config::resolve_vault_root(&cfg) {
            Ok(root) => match engine::build_index(&root) {
                Ok(index) => Some(ModuleStatus::normal(format!("{} notes", index.doc_count()))),
                Err(_) => Some(ModuleStatus::warning("Failed to read notes vault")),
            },
            Err(_) => Some(ModuleStatus::warning("Notes vault not configured")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use moku_config::MokuConfig;
    use moku_store::StorageManager;
    use moku_vault::{SecurityManager, VaultSession};
    use tempfile::tempdir;

    async fn create_test_context(vault_path: Option<&std::path::Path>) -> AppContext {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::mem::forget(temp);

        let mut config = MokuConfig::default();
        if let Some(vault) = vault_path {
            let mut table = toml::map::Map::new();
            table.insert(
                "vault_path".to_string(),
                toml::Value::String(vault.to_string_lossy().to_string()),
            );
            config.modules.insert("zk_view".to_string(), toml::Value::Table(table));
        }

        let config = Arc::new(ArcSwap::from_pointee(config));
        let session = Arc::new(VaultSession::new());
        let security = Arc::new(SecurityManager::new_with_root(root.clone()));
        let storage = Arc::new(StorageManager::new_with_root(Arc::clone(&session), root).await.unwrap());

        AppContext::new(config, session, security, storage)
    }

    #[tokio::test]
    async fn test_dashboard_summary_warns_when_vault_not_configured() {
        let module = ZkViewModule::new();
        let ctx = create_test_context(None).await;
        let status = module.dashboard_summary(&ctx).await.unwrap();
        assert_eq!(status.tone, moku_core::StatusTone::Warning);
    }

    #[tokio::test]
    async fn test_dashboard_summary_counts_notes_without_needing_init() {
        let vault = tempdir().unwrap();
        std::fs::write(vault.path().join("a.md"), "# A\n").unwrap();
        std::fs::write(vault.path().join("b.md"), "# B\n").unwrap();

        let module = ZkViewModule::new();
        let ctx = create_test_context(Some(vault.path())).await;
        let status = module.dashboard_summary(&ctx).await.unwrap();
        assert_eq!(status.tone, moku_core::StatusTone::Normal);
        assert_eq!(status.text, "2 notes");
    }

    #[tokio::test]
    async fn test_init_loads_vault_and_note_list_opens_a_document() {
        let vault = tempdir().unwrap();
        std::fs::write(vault.path().join("a.md"), "# A\n\nSee [[b]].\n").unwrap();
        std::fs::write(vault.path().join("b.md"), "# B\n").unwrap();

        let mut module = ZkViewModule::new();
        let mut ctx = create_test_context(Some(vault.path())).await;
        module.init(&mut ctx).await.unwrap();
        assert_eq!(module.notes.len(), 2);

        module.open_note_by_idx(0);
        assert!(module.open.is_some());
        assert_eq!(module.view, ViewMode::Document);
    }

    #[tokio::test]
    async fn test_gd_follows_resolved_link_and_esc_goes_back() {
        let vault = tempdir().unwrap();
        std::fs::write(vault.path().join("a.md"), "See [[b]] here.\n").unwrap();
        std::fs::write(vault.path().join("b.md"), "# B content\n").unwrap();

        let mut module = ZkViewModule::new();
        let mut ctx = create_test_context(Some(vault.path())).await;
        module.init(&mut ctx).await.unwrap();
        let idx = module.notes.iter().position(|n| n.rel_path == "a.md").unwrap();
        module.open_note_by_idx(idx);

        // Move cursor onto the "[[b]]" link (starts at byte 4 on line 0).
        module.cursor = Cursor { line: 0, col: 5 };
        module.follow_link_under_cursor(&mut ctx);
        assert_eq!(
            module.open.as_ref().unwrap().doc.id,
            satz_core::DocId::new("b.md")
        );
        assert_eq!(module.nav_stack.len(), 1);

        module.go_back();
        assert_eq!(module.open.as_ref().unwrap().doc.id, satz_core::DocId::new("a.md"));
        assert!(module.nav_stack.is_empty());
    }

    #[tokio::test]
    async fn test_open_in_editor_requests_suspend_with_line_and_sets_reload_flag() {
        let vault = tempdir().unwrap();
        std::fs::write(vault.path().join("a.md"), "line1\nline2\nline3\n").unwrap();

        let mut module = ZkViewModule::new();
        let mut ctx = create_test_context(Some(vault.path())).await;
        module.init(&mut ctx).await.unwrap();
        module.open_note_by_idx(0);
        module.cursor = Cursor { line: 2, col: 0 };

        module.open_in_editor(&mut ctx);
        assert!(module.needs_reload_current);
        // We can't easily read AppContext's private pending_suspend field
        // from here, so this is exercised end-to-end via app_loop's own
        // tests instead; this test only pins the module-side contract.
    }
}
