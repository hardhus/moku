use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, ListState, Paragraph},
};
use serde::{Deserialize, Serialize};

use moku_core::{
    AppContext, Command, ConfigManager, ModuleId, ModuleMeta, MokuConfig, MokuTheme, TuiModule,
    resolve_event,
};

mod fuzzy;
mod meta;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct LauncherModuleConfig {
    /// Existing per-launcher key overrides (`[modules.launcher.keys]`).
    keys: HashMap<String, String>,
    /// User's preferred module order, `ModuleId::as_str()` values,
    /// most-preferred first. Empty = untouched `ModuleId::all_visible()`
    /// order. Written automatically by Shift+Up/Shift+Down (see
    /// `persist_order`), and freely hand-editable in config.toml too.
    order: Vec<String>,
}

/// Active only while the user is typing a `/` search query.
struct FilterState {
    query: String,
    /// Indices into `registered_modules`, best fuzzy match first.
    matches: Vec<usize>,
    list_state: ListState,
    /// Position within `matches` shown at the top row of the list box —
    /// see `recompute_viewport`.
    viewport_top: usize,
}

impl FilterState {
    fn new(len: usize) -> Self {
        let mut list_state = ListState::default();
        if len > 0 {
            list_state.select(Some(0));
        }
        Self {
            query: String::new(),
            matches: (0..len).collect(),
            list_state,
            viewport_top: 0,
        }
    }
}

pub struct LauncherModule {
    registered_modules: Vec<ModuleId>,
    state: ListState,
    /// Absolute index shown at the top row of the list box — see
    /// `recompute_viewport`.
    viewport_top: usize,
    filter: Option<FilterState>,
}

impl LauncherModule {
    /// `extra_visible`: in addition to the static `ModuleId::all_visible()` list,
    /// modules to be shown in the launcher (e.g. successfully loaded Lua plugins).
    /// The combined list is then reordered according to any saved
    /// `[modules.launcher] order` in `config` (see `merge_order`).
    pub fn new(extra_visible: Vec<ModuleId>, config: &MokuConfig) -> Self {
        let settings: LauncherModuleConfig =
            config.resolve_module_config(ModuleId::LAUNCHER.as_str());
        let registered_modules =
            merge_order(ModuleId::all_visible(), extra_visible, &settings.order);
        let mut state = ListState::default();
        if !registered_modules.is_empty() {
            state.select(Some(0));
        }
        Self {
            registered_modules,
            state,
            viewport_top: 0,
            filter: None,
        }
    }

    fn next(&mut self) -> bool {
        if self.registered_modules.is_empty() {
            return false;
        }
        let i = match self.state.selected() {
            Some(i) => {
                if i >= self.registered_modules.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
        self.viewport_top = recompute_viewport(
            self.viewport_top,
            i,
            self.registered_modules.len(),
            VISIBLE_ROWS,
            SCROLL_MARGIN,
        );
        true
    }

    fn previous(&mut self) -> bool {
        if self.registered_modules.is_empty() {
            return false;
        }
        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    self.registered_modules.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
        self.viewport_top = recompute_viewport(
            self.viewport_top,
            i,
            self.registered_modules.len(),
            VISIBLE_ROWS,
            SCROLL_MARGIN,
        );
        true
    }

    /// Swaps the selected item with its neighbor (`dir`: -1 up, +1 down).
    /// No wraparound — unlike browse-mode next/previous, "move to the
    /// other end of the list" isn't a meaningful reorder gesture. Returns
    /// `false` (no-op) at a boundary, on an empty/singleton list, or if
    /// nothing is selected.
    fn move_selected(&mut self, dir: i32) -> bool {
        let Some(i) = self.state.selected() else {
            return false;
        };
        let len = self.registered_modules.len();
        if len < 2 {
            return false;
        }
        let target = i as i32 + dir;
        if target < 0 || target >= len as i32 {
            return false;
        }
        let target = target as usize;
        self.registered_modules.swap(i, target);
        self.state.select(Some(target));
        self.viewport_top = recompute_viewport(
            self.viewport_top,
            target,
            self.registered_modules.len(),
            VISIBLE_ROWS,
            SCROLL_MARGIN,
        );
        true
    }

    fn enter_filter_mode(&mut self) {
        self.filter = Some(FilterState::new(self.registered_modules.len()));
    }

    fn exit_filter_mode(&mut self) {
        self.filter = None;
    }

    fn recompute_filter_matches(&mut self) {
        let Some(filter) = &self.filter else { return };
        let mut scored: Vec<(usize, i32)> = self
            .registered_modules
            .iter()
            .enumerate()
            .filter_map(|(i, id)| {
                fuzzy::fuzzy_match(&filter.query, id.title()).map(|score| (i, score))
            })
            .collect();
        scored.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
        let matches: Vec<usize> = scored.into_iter().map(|(i, _)| i).collect();

        let Some(filter) = &mut self.filter else {
            return;
        };
        filter
            .list_state
            .select(if matches.is_empty() { None } else { Some(0) });
        filter.matches = matches;
        filter.viewport_top = 0;
    }

    fn filter_push_char(&mut self, c: char) {
        if let Some(filter) = &mut self.filter {
            filter.query.push(c);
        }
        self.recompute_filter_matches();
    }

    fn filter_pop_char(&mut self) {
        if let Some(filter) = &mut self.filter {
            filter.query.pop();
        }
        self.recompute_filter_matches();
    }

    fn filter_move(&mut self, dir: i32) {
        let Some(filter) = &mut self.filter else {
            return;
        };
        if filter.matches.is_empty() {
            return;
        }
        let i = filter.list_state.selected().unwrap_or(0);
        let target = (i as i32 + dir).clamp(0, filter.matches.len() as i32 - 1);
        filter.list_state.select(Some(target as usize));
        filter.viewport_top = recompute_viewport(
            filter.viewport_top,
            target as usize,
            filter.matches.len(),
            VISIBLE_ROWS,
            SCROLL_MARGIN,
        );
    }

    fn filter_selected_module(&self) -> Option<ModuleId> {
        let filter = self.filter.as_ref()?;
        let i = filter.list_state.selected()?;
        let idx = *filter.matches.get(i)?;
        self.registered_modules.get(idx).copied()
    }

    /// Writes the current `registered_modules` order to
    /// `[modules.launcher].order` — in memory immediately (so it takes
    /// effect right away) and to disk (so it survives a restart). Mirrors
    /// `modules/moku-settings/src/tabs/commit.rs`'s `CommitTab::save` /
    /// `modules/moku-settings/src/lib.rs`'s Ctrl+S error-toast shape.
    async fn persist_order(&mut self, ctx: &mut AppContext) {
        let mut settings: LauncherModuleConfig = ctx
            .config
            .load()
            .resolve_module_config(ModuleId::LAUNCHER.as_str());
        settings.order = self
            .registered_modules
            .iter()
            .map(|id| id.as_str().to_string())
            .collect();
        if let Ok(val) = toml::Value::try_from(settings) {
            ctx.update_config(|cfg| {
                cfg.modules
                    .insert(ModuleId::LAUNCHER.as_str().to_string(), val);
            });
            if let Err(e) = ConfigManager::save(&ctx.config.load()).await {
                ctx.show_error(format!("Failed to save module order: {e}"));
            }
        }
    }
}

/// Rows visible in the list box at once, and how many of those rows (at
/// each end) act as a scroll margin — "scrolloff", in editor terms. With
/// 7 and 3 there's exactly one truly free row (the middle) where the
/// cursor can sit without scrolling; everywhere else the cursor stays
/// pinned at row `SCROLL_MARGIN` (or `VISIBLE_ROWS - 1 - SCROLL_MARGIN`
/// near the bottom) and the list scrolls under it instead — except at
/// the true start/end of the list, where there's nothing left to scroll
/// into and the cursor is free to reach row 0 / the last row directly.
const VISIBLE_ROWS: usize = 7;
const SCROLL_MARGIN: usize = 3;

/// Adjusts `viewport_top` (the position shown at row 0) so `selected_pos`
/// stays visible within a `window`-row view over `n` total items, per the
/// scrolloff behavior described on `VISIBLE_ROWS`/`SCROLL_MARGIN`. Pure
/// and stateless given the previous `viewport_top` — callers persist the
/// result and pass it back in next time, which is what gives the cursor
/// its "free" zone (it only ever moves when the margin is actually
/// crossed, not on every keypress).
fn recompute_viewport(
    viewport_top: usize,
    selected_pos: usize,
    n: usize,
    window: usize,
    margin: usize,
) -> usize {
    if n == 0 {
        return 0;
    }
    let window = window.min(n);
    let max_top = n - window;
    let top = viewport_top.min(max_top);
    let cursor_row = selected_pos as i32 - top as i32;
    let low = margin as i32;
    let high = window as i32 - 1 - margin as i32;
    let top = if cursor_row < low {
        selected_pos.saturating_sub(margin)
    } else if cursor_row > high {
        (selected_pos + margin + 1).saturating_sub(window)
    } else {
        top
    };
    top.min(max_top)
}

const MAX_SELECTION_INDENT: usize = 4;
const SELECTION_INDENT_STEP: usize = 2;

/// Leading-space count for the row at `pos` (its position within whatever
/// list is currently displayed — full or filtered), given the selected
/// row's position `selected_pos`. Peaks at `MAX_SELECTION_INDENT` on the
/// selected row itself, tapers by `SELECTION_INDENT_STEP` per row of
/// distance, floors at 0. `None` selection (empty list) means no indent
/// anywhere.
fn selected_indent(pos: usize, selected_pos: Option<usize>) -> usize {
    let Some(selected_pos) = selected_pos else {
        return 0;
    };
    let distance = pos.abs_diff(selected_pos);
    MAX_SELECTION_INDENT.saturating_sub(distance.saturating_mul(SELECTION_INDENT_STEP))
}

/// Orders `all_visible` (+ `extra_visible` appended) according to
/// `saved_order` (a list of `ModuleId::as_str()` values, most-preferred
/// first). Ids in `saved_order` that no longer correspond to a currently
/// visible module are silently skipped. Currently-visible modules not
/// present in `saved_order` (e.g. a newly added module) are appended at
/// the end, in their original relative order — so an existing user's
/// `config.toml` stays forward-compatible with zero action needed.
fn merge_order(
    all_visible: Vec<ModuleId>,
    extra_visible: Vec<ModuleId>,
    saved_order: &[String],
) -> Vec<ModuleId> {
    let mut pool: Vec<ModuleId> = all_visible.into_iter().chain(extra_visible).collect();
    let mut ordered = Vec::with_capacity(pool.len());
    for saved_id in saved_order {
        if let Some(pos) = pool.iter().position(|m| m.as_str() == saved_id) {
            ordered.push(pool.remove(pos));
        }
    }
    ordered.extend(pool);
    ordered
}

impl Default for LauncherModule {
    fn default() -> Self {
        Self::new(Vec::new(), &MokuConfig::default())
    }
}

impl ModuleMeta for LauncherModule {
    fn id(&self) -> ModuleId {
        ModuleId::LAUNCHER
    }
    fn title(&self) -> &'static str {
        ModuleId::LAUNCHER.title()
    }
    fn encrypt_by_default(&self) -> bool {
        false // pure navigation menu, owns no storage
    }
}

#[async_trait]
impl TuiModule for LauncherModule {
    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        // ---------- FILTER MODE: raw key handling, bypasses resolve_event
        // since typed letters must always become literal query text, not
        // navigation commands. ----------
        if self.filter.is_some() {
            if let Event::Key(key) = event
                && (key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat)
            {
                return Ok(match key.code {
                    KeyCode::Esc => {
                        self.exit_filter_mode();
                        true
                    }
                    KeyCode::Enter => {
                        if let Some(id) = self.filter_selected_module() {
                            ctx.navigate_to(id);
                        }
                        self.exit_filter_mode();
                        true
                    }
                    KeyCode::Up => {
                        self.filter_move(-1);
                        true
                    }
                    KeyCode::Down => {
                        self.filter_move(1);
                        true
                    }
                    KeyCode::Backspace => {
                        self.filter_pop_char();
                        true
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.filter_push_char(c);
                        true
                    }
                    _ => false,
                });
            }
            return Ok(false);
        }

        // ---------- BROWSE MODE ----------
        // Shift+Up/Shift+Down reorder, checked before resolve_event (whose
        // hardcoded fallback maps bare Up/Down regardless of modifiers) —
        // same "raw check takes precedence" precedent as moku-http's
        // `KeyCode::Char('R') if modifiers.contains(SHIFT)` and
        // moku-settings' Ctrl+S. Always consumed (never falls through to
        // plain navigation) so a no-op at a boundary doesn't also move
        // the cursor via the ordinary Up/Down path.
        if let Event::Key(key) = event
            && (key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat)
            && key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Up | KeyCode::Down)
        {
            let dir = if key.code == KeyCode::Up { -1 } else { 1 };
            let moved = self.move_selected(dir);
            if moved {
                self.persist_order(ctx).await;
            }
            return Ok(moved);
        }

        let module_config: LauncherModuleConfig = ctx
            .config
            .load()
            .resolve_module_config(ModuleId::LAUNCHER.as_str());
        let command = resolve_event(event, &ctx.config.load().keys, Some(&module_config.keys));

        let changed = match command {
            Command::Quit | Command::Back => {
                ctx.quit();
                true
            }
            Command::Down => self.next(),
            Command::Up => self.previous(),
            Command::Search => {
                self.enter_filter_mode();
                true
            }
            Command::Confirm => {
                if let Some(index) = self.state.selected() {
                    let module_id = self.registered_modules[index];
                    ctx.navigate_to(module_id);
                }
                true
            }
            _ => false,
        };
        Ok(changed)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let filtering = self.filter.is_some();

        // Horizontally center a content-sized box instead of stretching
        // every row to the terminal's full width — the same "unnecessarily
        // big frame" complaint the vertical fix addressed, just sideways.
        // BOX_WIDTH is picked to comfortably fit the longest line we ever
        // render (the browse-mode status/help text, ~81 cols with its
        // badge) without wrapping/clipping; bump it if a longer string is
        // ever added below.
        const BOX_WIDTH: u16 = 84;
        let box_width = BOX_WIDTH.min(area.width);
        let left_pad = area.width.saturating_sub(box_width) / 2;
        let area = Layout::horizontal([
            Constraint::Length(left_pad),
            Constraint::Length(box_width),
            Constraint::Min(0),
        ])
        .split(area)[1];

        // Size the list to a fixed, content-appropriate row count (capped
        // at VISIBLE_ROWS) instead of stretching to fit every item at
        // once — a near-empty screen-tall box is exactly the
        // "unnecessarily big frame" look this replaces, and a fixed cap
        // is also what makes the scrolloff behavior below actually
        // engage. Always based on the full (unfiltered) module count so
        // the box doesn't resize/reflow while typing a search query —
        // only which rows are visible inside it changes.
        let list_height = (VISIBLE_ROWS.min(self.registered_modules.len().max(1)) as u16) + 2;
        let search_height = if filtering { 3 } else { 0 };
        let content_height = 3 + list_height + 1 + search_height + 1;
        let top_pad = area.height.saturating_sub(content_height) / 2;

        let rows = Layout::vertical([
            Constraint::Length(top_pad),       // 0. top spacer (vertical centering)
            Constraint::Length(3),             // 1. Header
            Constraint::Length(list_height),   // 2. Module list (tight-fit)
            Constraint::Length(1),             // 3. Detail line
            Constraint::Length(search_height), // 4. Search input
            Constraint::Length(1),             // 5. Status/help bar
            Constraint::Min(0),                // 6. bottom spacer
        ])
        .split(area);

        // 1. Header
        let header = Paragraph::new(" 🚀 Moku Launcher ")
            .style(
                Style::default()
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg)
                    .add_modifier(Modifier::BOLD),
            )
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border)),
            );
        frame.render_widget(header, rows[1]);

        // 2. Module list — a bounded "scrolloff" view: the cursor moves
        // freely within the box, but once it comes within SCROLL_MARGIN
        // rows of either edge, the list scrolls under it instead of
        // letting it go further — except at the true start/end of the
        // list, where there's nothing left to scroll into and the cursor
        // reaches row 0 / the last row directly (see `recompute_viewport`
        // for the exact rule). No icons or numbers (mixed-width emoji
        // glyphs render at inconsistent terminal cell widths across
        // icons/fonts, which is what caused the earlier per-row
        // misalignment; plain ASCII titles render at a fully consistent
        // width everywhere). Every row starts at the same column
        // (`base_indent`, from the longest title, so the whole block
        // reads as centered) plus an additional indent that peaks on the
        // cursor's row and tapers off over its neighbors.
        let display_indices: Vec<usize> = match &self.filter {
            Some(f) => f.matches.clone(),
            None => (0..self.registered_modules.len()).collect(),
        };
        let selected_pos = match &self.filter {
            Some(f) => f.list_state.selected(),
            None => self.state.selected(),
        };
        let viewport_top = match &self.filter {
            Some(f) => f.viewport_top,
            None => self.viewport_top,
        };

        let list_block = Block::default()
            .title(format!(" Modules ({}) ", self.registered_modules.len()))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border))
            .style(Style::default().bg(theme.base_bg));
        let list_inner = list_block.inner(rows[2]);
        frame.render_widget(list_block, rows[2]);

        let inner_height = list_inner.height as usize;
        let content_width = list_inner.width as usize;
        let max_title_len = self
            .registered_modules
            .iter()
            .map(|id| id.title().chars().count())
            .max()
            .unwrap_or(0);
        let base_indent = content_width.saturating_sub(max_title_len) / 2;
        // Where the cursor actually lands this frame — varies (row 0 near
        // the true start, the last row near the true end, otherwise
        // pinned at SCROLL_MARGIN) — this is the taper's reference point,
        // not a fixed row.
        let cursor_row = selected_pos.map(|p| p as i32 - viewport_top as i32);

        for r in 0..inner_height {
            let display_pos = viewport_top + r;
            let text = display_indices.get(display_pos).map(|&idx| {
                let id = self.registered_modules[idx];
                let distance = cursor_row
                    .map(|c| (r as i32 - c).unsigned_abs() as usize)
                    .unwrap_or(usize::MAX);
                let indent = base_indent + selected_indent(distance, Some(0));
                format!("{}{}", " ".repeat(indent), id.title())
            });
            let is_cursor_row = cursor_row.is_some_and(|c| c == r as i32);
            let row_area = Rect {
                x: list_inner.x,
                y: list_inner.y + r as u16,
                width: list_inner.width,
                height: 1,
            };
            let style = if is_cursor_row {
                Style::default()
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.base_fg).bg(theme.base_bg)
            };
            frame.render_widget(
                Paragraph::new(text.unwrap_or_default()).style(style),
                row_area,
            );
        }

        // 3. Detail line — the selected module's one-line description.
        let selected_id = if self.filter.is_some() {
            self.filter_selected_module()
        } else {
            self.state
                .selected()
                .and_then(|i| self.registered_modules.get(i))
                .copied()
        };
        let detail_text = selected_id
            .map(|id| format!(" {} — {}", meta::icon_for(id), meta::description_for(id)))
            .unwrap_or_default();
        let detail =
            Paragraph::new(detail_text).style(Style::default().fg(theme.base_fg).bg(theme.base_bg));
        frame.render_widget(detail, rows[3]);

        // 4. Search input — only occupies space while filtering.
        if let Some(f) = &self.filter {
            let (body, style) = if f.query.is_empty() {
                (
                    "Type to filter modules...".to_string(),
                    Style::default()
                        .fg(theme.border)
                        .add_modifier(Modifier::ITALIC),
                )
            } else {
                (format!("{}_", f.query), Style::default().fg(theme.base_fg))
            };
            let input = Paragraph::new(body).style(style).block(
                Block::default()
                    .title(" Search ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.info).add_modifier(Modifier::BOLD))
                    .style(Style::default().bg(theme.base_bg)),
            );
            frame.render_widget(input, rows[4]);
        }

        // 5. Status/help bar — plain ASCII only (no arrow glyphs). Mixed-
        // width Unicode characters (↑/↓ included) render at inconsistent
        // cell widths across terminals/fonts — the exact bug that caused
        // the earlier per-row icon misalignment — and were cutting this
        // text off mid-word on some terminals.
        let (badge, badge_bg, help) = if filtering {
            (
                " SEARCH ",
                theme.warning,
                " Type to filter | [Up/Dn] Move | [Enter] Open | [Esc] Cancel ",
            )
        } else {
            (
                " BROWSE ",
                theme.selection_bg,
                " [j/k] Move | [/] Search | [Shift+Up/Dn] Sort | [Enter] Open | [q] Quit ",
            )
        };
        let status_line = Line::from(vec![
            Span::styled(
                badge,
                Style::default()
                    .bg(badge_bg)
                    .fg(theme.base_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(help, Style::default().fg(theme.base_fg)),
        ]);
        let status = Paragraph::new(status_line).style(Style::default().bg(theme.base_bg));
        frame.render_widget(status, rows[5]);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn launcher() -> LauncherModule {
        LauncherModule::new(Vec::new(), &MokuConfig::default())
    }

    #[test]
    fn test_launcher_selection_cycle() {
        let mut launcher = launcher();
        let initial_len = launcher.registered_modules.len();
        assert!(
            initial_len > 0,
            "At least one module must be visible for testing."
        );

        launcher.state.select(Some(initial_len - 1));
        launcher.next();
        assert_eq!(launcher.state.selected(), Some(0));

        launcher.previous();
        assert_eq!(launcher.state.selected(), Some(initial_len - 1));
    }

    #[test]
    fn test_merge_order_empty_saved_order_keeps_original() {
        let all = vec![ModuleId::TODO, ModuleId::RSS, ModuleId::NOTES];
        let merged = merge_order(all.clone(), Vec::new(), &[]);
        assert_eq!(merged, all);
    }

    #[test]
    fn test_merge_order_puts_saved_first_in_saved_order() {
        let all = vec![
            ModuleId::TODO,
            ModuleId::RSS,
            ModuleId::NOTES,
            ModuleId::SECRETS,
        ];
        let saved = vec!["secrets".to_string(), "todo".to_string()];
        let merged = merge_order(all, Vec::new(), &saved);
        assert_eq!(
            merged,
            vec![
                ModuleId::SECRETS,
                ModuleId::TODO,
                ModuleId::RSS,
                ModuleId::NOTES
            ]
        );
    }

    #[test]
    fn test_merge_order_skips_unknown_saved_id_without_panicking() {
        let all = vec![ModuleId::TODO, ModuleId::RSS];
        let saved = vec!["some-removed-module".to_string(), "rss".to_string()];
        let merged = merge_order(all, Vec::new(), &saved);
        assert_eq!(merged, vec![ModuleId::RSS, ModuleId::TODO]);
    }

    #[test]
    fn test_merge_order_appends_newly_visible_module_not_in_saved_order() {
        let all = vec![ModuleId::TODO, ModuleId::RSS, ModuleId::HTTP];
        let saved = vec!["rss".to_string()];
        let merged = merge_order(all, Vec::new(), &saved);
        assert_eq!(merged, vec![ModuleId::RSS, ModuleId::TODO, ModuleId::HTTP]);
    }

    #[test]
    fn test_move_selected_swaps_and_follows_selection() {
        let mut launcher = launcher();
        let first = launcher.registered_modules[0];
        let second = launcher.registered_modules[1];
        launcher.state.select(Some(0));

        assert!(launcher.move_selected(1));
        assert_eq!(launcher.registered_modules[0], second);
        assert_eq!(launcher.registered_modules[1], first);
        assert_eq!(launcher.state.selected(), Some(1));
    }

    #[test]
    fn test_move_selected_noop_at_boundaries() {
        let mut launcher = launcher();
        let len = launcher.registered_modules.len();

        launcher.state.select(Some(0));
        assert!(
            !launcher.move_selected(-1),
            "moving up from the top should no-op"
        );

        launcher.state.select(Some(len - 1));
        assert!(
            !launcher.move_selected(1),
            "moving down from the bottom should no-op"
        );
    }

    #[test]
    fn test_enter_filter_mode_starts_with_full_list_and_exit_clears_it() {
        let mut launcher = launcher();
        let len = launcher.registered_modules.len();
        launcher.enter_filter_mode();
        assert!(launcher.filter.is_some());
        assert_eq!(launcher.filter.as_ref().unwrap().matches.len(), len);

        launcher.exit_filter_mode();
        assert!(launcher.filter.is_none());
    }

    #[test]
    fn test_filter_push_and_pop_recompute_matches() {
        let mut launcher = launcher();
        launcher.enter_filter_mode();
        for c in "rss".chars() {
            launcher.filter_push_char(c);
        }
        let matched_id = launcher.filter_selected_module();
        assert_eq!(matched_id, Some(ModuleId::RSS));

        launcher.filter_pop_char();
        launcher.filter_pop_char();
        launcher.filter_pop_char();
        assert_eq!(launcher.filter.as_ref().unwrap().query, "");
        assert_eq!(
            launcher.filter.as_ref().unwrap().matches.len(),
            launcher.registered_modules.len()
        );
    }

    #[test]
    fn test_filter_with_no_matches_selects_nothing_without_panicking() {
        let mut launcher = launcher();
        launcher.enter_filter_mode();
        for c in "zzzzqqqq".chars() {
            launcher.filter_push_char(c);
        }
        assert!(launcher.filter.as_ref().unwrap().matches.is_empty());
        assert_eq!(launcher.filter_selected_module(), None);
    }

    #[test]
    fn test_filter_move_clamps_at_ends() {
        let mut launcher = launcher();
        launcher.enter_filter_mode();
        let len = launcher.filter.as_ref().unwrap().matches.len();

        launcher.filter_move(-1);
        assert_eq!(
            launcher.filter.as_ref().unwrap().list_state.selected(),
            Some(0)
        );

        for _ in 0..(len + 5) {
            launcher.filter_move(1);
        }
        assert_eq!(
            launcher.filter.as_ref().unwrap().list_state.selected(),
            Some(len - 1)
        );
    }

    fn rendered_content(launcher: &mut LauncherModule) -> String {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = MokuTheme::default();
        terminal
            .draw(|frame| launcher.draw(frame, Rect::new(0, 0, 100, 30), &theme))
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
    fn test_draw_browse_mode_shows_titles_and_badge() {
        let mut launcher = launcher();
        let content = rendered_content(&mut launcher);
        assert!(content.contains("RSS Feed Reader"));
        assert!(content.contains("BROWSE"));
    }

    #[test]
    fn test_draw_filter_mode_narrows_visible_titles() {
        let mut launcher = launcher();
        launcher.enter_filter_mode();
        for c in "rss".chars() {
            launcher.filter_push_char(c);
        }
        let content = rendered_content(&mut launcher);
        assert!(content.contains("RSS Feed Reader"));
        assert!(!content.contains("Bookmark"));
        assert!(content.contains("rss_"));
        assert!(content.contains("SEARCH"));
    }

    #[test]
    fn test_selected_indent_peaks_at_selection_and_tapers_off() {
        assert_eq!(selected_indent(3, Some(3)), MAX_SELECTION_INDENT);
        assert_eq!(
            selected_indent(2, Some(3)),
            MAX_SELECTION_INDENT - SELECTION_INDENT_STEP
        );
        assert_eq!(
            selected_indent(4, Some(3)),
            MAX_SELECTION_INDENT - SELECTION_INDENT_STEP
        );
        assert_eq!(selected_indent(0, Some(3)), 0); // distance 3, floored at 0
        assert_eq!(selected_indent(9, Some(3)), 0); // far away, floored at 0
        assert_eq!(selected_indent(0, None), 0); // nothing selected
    }

    /// Column offset of the first occurrence of `title` in the rendered
    /// buffer (scanning row by row). Ratatui's own `Alignment::Center`
    /// splits its auto-padding around whatever string it's given, so a
    /// row's manually-prepended indent spaces shift where the *visible*
    /// title text lands rather than adding a literally-matchable N-space
    /// prefix — this measures the real rendered position instead of
    /// assuming a fixed padding string.
    fn title_start_x(launcher: &mut LauncherModule, title: &str) -> Option<usize> {
        let (width, height) = (100usize, 30usize);
        let backend = TestBackend::new(width as u16, height as u16);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = MokuTheme::default();
        terminal
            .draw(|frame| {
                launcher.draw(frame, Rect::new(0, 0, width as u16, height as u16), &theme)
            })
            .unwrap();
        let content = terminal.backend().buffer().content.clone();
        (0..height).find_map(|y| {
            let row: String = content[y * width..(y + 1) * width]
                .iter()
                .map(|c| c.symbol())
                .collect();
            row.find(title)
        })
    }

    #[test]
    fn test_draw_selected_row_title_shifts_right_relative_to_unselected() {
        let mut launcher = launcher();
        let title = launcher.registered_modules[0].title().to_string();

        launcher.state.select(Some(0));
        let x_selected =
            title_start_x(&mut launcher, &title).expect("title visible while selected");

        launcher.state.select(Some(6)); // distance 6 from index 0 => zero indent
        let x_far =
            title_start_x(&mut launcher, &title).expect("title visible while far from selection");

        assert!(
            x_selected > x_far,
            "selected row's title should render further right than its own unselected position (selected_x={x_selected}, far_x={x_far})"
        );
    }

    #[test]
    fn test_unselected_rows_of_different_lengths_share_the_same_starting_column() {
        let mut launcher = launcher();
        // Select index 0 so every other row here is at distance >= 2 (zero
        // extra indent) — differing only in their own title length.
        launcher.state.select(Some(0));

        let short_title = ModuleId::BOOKMARK.title(); // "Bookmark"
        let long_title = ModuleId::RSS.title(); // "RSS Feed Reader"
        assert!(launcher.registered_modules.contains(&ModuleId::BOOKMARK));
        assert!(launcher.registered_modules.contains(&ModuleId::RSS));

        let x_short = title_start_x(&mut launcher, short_title).expect("short title visible");
        let x_long = title_start_x(&mut launcher, long_title).expect("long title visible");

        assert_eq!(
            x_short, x_long,
            "unselected rows of different title lengths should start at the same column (short_x={x_short}, long_x={x_long})"
        );
    }

    #[test]
    fn test_content_is_vertically_padded_from_the_top_in_a_tall_terminal() {
        let mut launcher = launcher();
        let (width, height) = (100usize, 50usize); // far taller than the content needs
        let backend = TestBackend::new(width as u16, height as u16);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = MokuTheme::default();
        terminal
            .draw(|frame| {
                launcher.draw(frame, Rect::new(0, 0, width as u16, height as u16), &theme)
            })
            .unwrap();
        let content = terminal.backend().buffer().content.clone();
        let header_row = (0..height).find(|&y| {
            let row: String = content[y * width..(y + 1) * width]
                .iter()
                .map(|c| c.symbol())
                .collect();
            row.contains("Moku Launcher")
        });
        assert!(
            header_row.is_some_and(|y| y > 0),
            "header should be pushed down from the very top row in a tall terminal (found at {header_row:?}) — content should be vertically centered, not pinned to the top"
        );
    }

    #[test]
    fn test_list_box_does_not_stretch_to_fill_a_tall_terminal() {
        let mut launcher = launcher();
        let (width, height) = (100u16, 50u16); // far taller than 10 items need
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = MokuTheme::default();
        terminal
            .draw(|frame| launcher.draw(frame, Rect::new(0, 0, width, height), &theme))
            .unwrap();
        let content = terminal.backend().buffer().content.clone();
        let w = width as usize;
        // The status bar ("BROWSE") should land well before the last row —
        // if the list still stretched via Min(0), it (and everything below
        // it) would be pushed all the way down to the bottom of the screen.
        let status_row = (0..height as usize).find(|&y| {
            let row: String = content[y * w..(y + 1) * w]
                .iter()
                .map(|c| c.symbol())
                .collect();
            row.contains("BROWSE")
        });
        assert!(
            status_row.is_some_and(|y| y < height as usize - 5),
            "status bar should sit well above the bottom of a tall terminal (found at {status_row:?} of {height} rows) — the list shouldn't stretch to fill all remaining space"
        );
    }

    #[test]
    fn test_draw_list_rows_have_no_icon_or_number_prefix() {
        let mut launcher = launcher();
        let content = rendered_content(&mut launcher);
        // Every icon appears at most once (the single detail-line icon for
        // the selected module) — never once per row.
        for id in ModuleId::all_visible() {
            let icon = meta::icon_for(id);
            assert!(
                content.matches(icon).count() <= 1,
                "icon {icon} for {} appears more than once — looks like it leaked into list rows",
                id.as_str()
            );
        }
        // No "N. " numbering prefix anywhere in the rendered content.
        for n in 1..=launcher.registered_modules.len() {
            assert!(
                !content.contains(&format!("{n}. ")),
                "found leftover numbering prefix \"{n}. \""
            );
        }
    }

    fn find_row(
        launcher: &mut LauncherModule,
        width: usize,
        height: usize,
        text: &str,
    ) -> Option<usize> {
        let backend = TestBackend::new(width as u16, height as u16);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = MokuTheme::default();
        terminal
            .draw(|frame| {
                launcher.draw(frame, Rect::new(0, 0, width as u16, height as u16), &theme)
            })
            .unwrap();
        let content = terminal.backend().buffer().content.clone();
        (0..height).find(|&y| {
            let row: String = content[y * width..(y + 1) * width]
                .iter()
                .map(|c| c.symbol())
                .collect();
            row.contains(text)
        })
    }

    #[test]
    fn test_cursor_moves_freely_near_the_true_start_of_the_list() {
        // Within SCROLL_MARGIN of the true top, there's nothing to scroll
        // into yet, so the cursor's screen row should track its absolute
        // index directly (row 0 for item 0, row 1 for item 1, ...).
        let mut launcher = launcher();
        let (width, height) = (100usize, 30usize);
        for i in 0..SCROLL_MARGIN {
            launcher.state.select(Some(i));
            launcher.viewport_top = recompute_viewport(
                launcher.viewport_top,
                i,
                launcher.registered_modules.len(),
                VISIBLE_ROWS,
                SCROLL_MARGIN,
            );
            let title = launcher.registered_modules[i].title().to_string();
            let row = find_row(&mut launcher, width, height, &title)
                .expect("selected title should be visible");
            let list_top = find_row(&mut launcher, width, height, "Modules (").unwrap() + 1;
            assert_eq!(
                row - list_top,
                i,
                "item {i} near the true start should sit at its own absolute row"
            );
        }
    }

    #[test]
    fn test_cursor_pins_at_the_margin_row_once_scrolling_engages() {
        let mut launcher = launcher();
        let (width, height) = (100usize, 30usize);
        // Index comfortably past the margin on both sides (well within a
        // 10-item list with VISIBLE_ROWS=7): scrolling should have already
        // engaged, pinning the cursor at row SCROLL_MARGIN from the list's
        // own top border.
        let i = 5;
        launcher.state.select(Some(i));
        launcher.viewport_top = recompute_viewport(
            launcher.viewport_top,
            i,
            launcher.registered_modules.len(),
            VISIBLE_ROWS,
            SCROLL_MARGIN,
        );
        let title = launcher.registered_modules[i].title().to_string();
        let row = find_row(&mut launcher, width, height, &title)
            .expect("selected title should be visible");
        let list_top = find_row(&mut launcher, width, height, "Modules (").unwrap() + 1;
        assert_eq!(
            row - list_top,
            SCROLL_MARGIN,
            "cursor should be pinned at the scrolloff margin row once scrolled"
        );
    }

    #[test]
    fn test_cursor_reaches_the_last_row_at_the_true_end_of_the_list() {
        let mut launcher = launcher();
        let (width, height) = (100usize, 30usize);
        let last = launcher.registered_modules.len() - 1;
        launcher.state.select(Some(last));
        launcher.viewport_top = recompute_viewport(
            launcher.viewport_top,
            last,
            launcher.registered_modules.len(),
            VISIBLE_ROWS,
            SCROLL_MARGIN,
        );
        let title = launcher.registered_modules[last].title().to_string();
        let row = find_row(&mut launcher, width, height, &title)
            .expect("selected title should be visible");
        let list_top = find_row(&mut launcher, width, height, "Modules (").unwrap() + 1;
        assert_eq!(
            row - list_top,
            VISIBLE_ROWS - 1,
            "the last item should reach the box's last row, not stop short"
        );
    }

    #[test]
    fn test_recompute_viewport_stays_put_within_the_free_zone() {
        // n=10, window=7, margin=3: row 3 (cursor_row = selected - top) is
        // the only truly free row — as long as the cursor would land
        // exactly there, the existing viewport_top is left untouched.
        assert_eq!(recompute_viewport(0, 3, 10, 7, 3), 0);
        assert_eq!(recompute_viewport(1, 4, 10, 7, 3), 1); // unrelated prior offset preserved
    }

    #[test]
    fn test_recompute_viewport_scrolls_forward_past_the_bottom_margin() {
        assert_eq!(recompute_viewport(0, 4, 10, 7, 3), 1);
        assert_eq!(recompute_viewport(1, 6, 10, 7, 3), 3); // clamped at max_top = n - window
    }

    #[test]
    fn test_recompute_viewport_scrolls_backward_past_the_top_margin() {
        assert_eq!(recompute_viewport(3, 3, 10, 7, 3), 0);
        assert_eq!(recompute_viewport(3, 0, 10, 7, 3), 0);
    }

    #[test]
    fn test_recompute_viewport_handles_empty_and_short_lists() {
        assert_eq!(recompute_viewport(5, 0, 0, 7, 3), 0);
        // n shorter than the window: max_top is 0, everything stays put.
        assert_eq!(recompute_viewport(0, 1, 2, 7, 3), 0);
    }

    #[test]
    fn test_status_bar_help_text_is_not_truncated() {
        let mut launcher = launcher();
        let content = rendered_content(&mut launcher);
        assert!(
            content.contains("[q] Quit"),
            "browse-mode status bar should show the full \"[q] Quit\" hint, not a truncated fragment"
        );
        assert!(
            content.contains("[Shift+Up/Dn] Sort"),
            "browse-mode status bar should show the full reorder hint"
        );

        launcher.enter_filter_mode();
        let content = rendered_content(&mut launcher);
        assert!(
            content.contains("[Esc] Cancel"),
            "search-mode status bar should show the full cancel hint"
        );
    }

    #[test]
    fn test_box_does_not_stretch_across_a_very_wide_terminal() {
        let mut launcher = launcher();
        let (width, height) = (300usize, 30usize);
        let backend = TestBackend::new(width as u16, height as u16);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = MokuTheme::default();
        terminal
            .draw(|frame| {
                launcher.draw(frame, Rect::new(0, 0, width as u16, height as u16), &theme)
            })
            .unwrap();
        let content = terminal.backend().buffer().content.clone();
        let far_right_blank =
            (0..height).all(|y| content[y * width + (width - 10)].symbol().trim().is_empty());
        assert!(
            far_right_blank,
            "content should not stretch across a very wide (300-column) terminal"
        );
    }
}
