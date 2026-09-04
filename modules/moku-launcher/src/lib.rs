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
    /// see `recompute_ring_viewport`. `i64` because it's tracked modulo
    /// `n` (never clamped), so intermediate values can transiently go
    /// negative before `rem_euclid` normalizes them.
    viewport_top: i64,
}

impl FilterState {
    fn new(len: usize) -> Self {
        let mut list_state = ListState::default();
        // Establish the ring's rotated position up front (see
        // recompute_ring_viewport) so the very first render already shows
        // item 0 pinned at RING_MARGIN rather than at row 0 — the rotation
        // isn't something that only kicks in after the first keypress.
        let viewport_top = if len > 0 {
            list_state.select(Some(0));
            recompute_ring_viewport(0, 0, len, RING_MARGIN)
        } else {
            0
        };
        Self {
            query: String::new(),
            matches: (0..len).collect(),
            list_state,
            viewport_top,
        }
    }
}

pub struct LauncherModule {
    registered_modules: Vec<ModuleId>,
    state: ListState,
    /// Absolute index shown at the top row of the list box — see
    /// `recompute_ring_viewport`.
    viewport_top: i64,
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
        // Same up-front rotation as FilterState::new — see there.
        let viewport_top = if !registered_modules.is_empty() {
            state.select(Some(0));
            recompute_ring_viewport(0, 0, registered_modules.len(), RING_MARGIN)
        } else {
            0
        };
        Self {
            registered_modules,
            state,
            viewport_top,
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
        self.viewport_top = recompute_ring_viewport(
            self.viewport_top,
            i,
            self.registered_modules.len(),
            RING_MARGIN,
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
        self.viewport_top = recompute_ring_viewport(
            self.viewport_top,
            i,
            self.registered_modules.len(),
            RING_MARGIN,
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
        self.viewport_top = recompute_ring_viewport(
            self.viewport_top,
            target,
            self.registered_modules.len(),
            RING_MARGIN,
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
        // Re-establish the ring's rotated position immediately (same
        // reasoning as FilterState::new) rather than resetting to 0 and
        // waiting for the next arrow press to correct it.
        filter.viewport_top = recompute_ring_viewport(0, 0, matches.len(), RING_MARGIN);
        filter.matches = matches;
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
        filter.viewport_top = recompute_ring_viewport(
            filter.viewport_top,
            target as usize,
            filter.matches.len(),
            RING_MARGIN,
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

/// How many rows (0-indexed distance from either edge) the cursor stays
/// clear of before the ring starts rotating under it. 2 means the cursor
/// pins at row index 2 (the 3rd row) from the top, and at row index
/// `n - 1 - 2` from the bottom — everywhere in between is a free zone
/// where the cursor moves normally.
const RING_MARGIN: usize = 2;

/// Adjusts `viewport_top` (the item shown at row 0) so the cursor stays
/// within `RING_MARGIN` of either edge, exactly like `recompute_viewport`
/// used to — except the `n`-item list is always shown in full (the
/// window is always `n` rows) and there is no true start/end at all:
/// positions are tracked modulo `n`, never clamped, so "moving past the
/// first item" wraps seamlessly to the last one and vice versa. This is
/// what makes it read as a closed ring/cylinder rather than a bounded
/// list — the cursor never reaches row 0 or the last row exposing a
/// boundary, because there isn't one.
fn recompute_ring_viewport(viewport_top: i64, selected_pos: usize, n: usize, margin: usize) -> i64 {
    if n == 0 {
        return 0;
    }
    let n = n as i64;
    let cursor_row = (selected_pos as i64 - viewport_top).rem_euclid(n);
    let low = margin as i64;
    let high = n - 1 - margin as i64;
    if cursor_row < low {
        (selected_pos as i64 - low).rem_euclid(n)
    } else if cursor_row > high {
        (selected_pos as i64 - high).rem_euclid(n)
    } else {
        viewport_top.rem_euclid(n)
    }
}

const MAX_SELECTION_INDENT: usize = 4;

/// Leading-space count for a row `distance` steps away from the cursor's
/// row. Quadratic rather than linear falloff — not a real 3D perspective
/// (ratatui/a terminal can't do that), but a curved rather than
/// stair-step taper reads a little more like a rounded surface. Floors
/// at 0; `has_selection = false` (nothing selected, e.g. an empty list)
/// means no indent anywhere.
fn selected_indent(distance: usize, has_selection: bool) -> usize {
    if !has_selection {
        return 0;
    }
    let d = distance as f32;
    (MAX_SELECTION_INDENT as f32 - d * d * 0.6).max(0.0).round() as usize
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

        // Size the list to fit every item at once — nothing is ever
        // hidden — instead of the terminal's full height. Always based on
        // the full (unfiltered) module count so the box doesn't resize/
        // reflow while typing a search query — only which item lands at
        // the cursor's row changes.
        let list_height = (self.registered_modules.len().max(1) as u16) + 2;
        let search_height = if filtering { 3 } else { 0 };
        let content_height = 3 + list_height + search_height + 1;
        let top_pad = area.height.saturating_sub(content_height) / 2;

        let rows = Layout::vertical([
            Constraint::Length(top_pad),       // 0. top spacer (vertical centering)
            Constraint::Length(3),             // 1. Header
            Constraint::Length(list_height),   // 2. Module list (fits every item)
            Constraint::Length(search_height), // 3. Search input
            Constraint::Length(1),             // 4. Status/help bar
            Constraint::Min(0),                // 5. bottom spacer
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

        // 2. Module list — a closed ring: every item is always shown (the
        // box is sized to fit them all), and the list has no true start
        // or end — positions wrap circularly (see `recompute_ring_viewport`).
        // The cursor moves freely near the middle, and once it's within
        // RING_MARGIN rows of either edge, it stays put and the whole
        // ring rotates by one step under it instead — including right
        // past the first/last item, which is what makes it read as a
        // cylinder rather than a list with edges. No icons or numbers
        // (mixed-width emoji glyphs render at inconsistent terminal cell
        // widths across icons/fonts, which is what caused the earlier
        // per-row misalignment; plain ASCII titles render at a fully
        // consistent width everywhere). Every row starts at the same
        // column (`base_indent`, from the longest title, so the whole
        // block reads as centered) plus an additional indent — and a
        // dimmer color the further a row sits from the cursor — that
        // peaks/is brightest on the cursor's row and tapers off over its
        // neighbors, suggesting a little depth without any real
        // perspective (a terminal can't do that).
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
        let n_display = display_indices.len() as i64;

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
        // Where the cursor actually lands this frame — usually pinned at
        // RING_MARGIN from an edge, free to sit anywhere in the middle —
        // this is the taper/dimming's reference point.
        let cursor_row =
            selected_pos.map(|p| (p as i64 - viewport_top).rem_euclid(n_display.max(1)));

        for r in 0..inner_height {
            let text = (n_display > 0).then(|| {
                let idx = display_indices[(viewport_top + r as i64).rem_euclid(n_display) as usize];
                let id = self.registered_modules[idx];
                let distance = cursor_row
                    .map(|c| (r as i64 - c).unsigned_abs() as usize)
                    .unwrap_or(usize::MAX);
                let indent = base_indent + selected_indent(distance, cursor_row.is_some());
                format!("{}{}", " ".repeat(indent), id.title())
            });
            let distance = cursor_row
                .map(|c| (r as i64 - c).unsigned_abs())
                .unwrap_or(u64::MAX);
            let style = if distance == 0 {
                Style::default()
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg)
                    .add_modifier(Modifier::BOLD)
            } else if distance <= 2 {
                Style::default().fg(theme.base_fg).bg(theme.base_bg)
            } else {
                Style::default()
                    .fg(theme.base_fg)
                    .bg(theme.base_bg)
                    .add_modifier(Modifier::DIM)
            };
            let row_area = Rect {
                x: list_inner.x,
                y: list_inner.y + r as u16,
                width: list_inner.width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(text.unwrap_or_default()).style(style),
                row_area,
            );
        }

        // 3. Search input — only occupies space while filtering.
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
            frame.render_widget(input, rows[3]);
        }

        // 4. Status/help bar — plain ASCII only (no arrow glyphs). Mixed-
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
        frame.render_widget(status, rows[4]);
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
    fn test_selected_indent_peaks_at_zero_distance_and_tapers_off() {
        assert_eq!(selected_indent(0, true), MAX_SELECTION_INDENT);
        assert!(selected_indent(1, true) < MAX_SELECTION_INDENT);
        assert!(selected_indent(1, true) > selected_indent(2, true));
        assert_eq!(selected_indent(3, true), 0); // far enough away, floored at 0
        assert_eq!(selected_indent(9, true), 0); // far away, floored at 0
        assert_eq!(selected_indent(0, false), 0); // nothing selected
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
        // Fresh launcher: Dashboard (index 0) selected, ring already
        // rotated by the constructor so its cursor row is fixed at
        // RING_MARGIN. Notes and Vault both land far enough from that row
        // (distance >= 3) to have zero extra indent regardless of their
        // very different title lengths ("Notes" vs "Encrypted Vaults").
        let mut launcher = launcher();

        let short_title = ModuleId::NOTES.title(); // "Notes"
        let long_title = ModuleId::VAULT.title(); // "Encrypted Vaults"
        assert!(launcher.registered_modules.contains(&ModuleId::NOTES));
        assert!(launcher.registered_modules.contains(&ModuleId::VAULT));

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
    fn test_draw_list_rows_have_no_number_prefix() {
        let mut launcher = launcher();
        let content = rendered_content(&mut launcher);
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
    fn test_all_items_always_rendered_simultaneously() {
        // Nothing is ever hidden — every module title should be present
        // in the rendered content regardless of which one is selected.
        let mut launcher = launcher();
        for i in [0usize, 5, 9] {
            launcher.state.select(Some(i));
            let content = rendered_content(&mut launcher);
            for id in &launcher.registered_modules.clone() {
                assert!(
                    content.contains(id.title()),
                    "{} missing while item {i} selected",
                    id.title()
                );
            }
        }
    }

    #[test]
    fn test_first_item_selected_does_not_render_at_the_top_row() {
        // Fresh launcher: Dashboard (index 0) is selected, and the
        // constructor already rotates the ring so it sits at RING_MARGIN,
        // never at row 0 — there's no "true start" to expose.
        let mut launcher = launcher();
        let (width, height) = (100usize, 30usize);
        let dashboard_row = find_row(&mut launcher, width, height, "Dashboard")
            .expect("Dashboard should be visible");
        let list_top = find_row(&mut launcher, width, height, "Modules (").unwrap() + 1;
        assert_eq!(
            dashboard_row - list_top,
            RING_MARGIN,
            "the first item should sit at the pinned margin row, not row 0"
        );
    }

    #[test]
    fn test_ring_rotates_one_step_keeping_cursor_row_fixed() {
        // The exact KARE2 -> KARE3 transition confirmed with the user:
        // Daemon (index 5) selected, then one more Down to Vault (index
        // 6) — the cursor's screen row must not change, but the ring
        // rotates so HTTP (the item that was two rows above Daemon) now
        // appears at the very top.
        let mut launcher = launcher();
        for _ in 0..5 {
            launcher.next(); // Dashboard -> ... -> Daemon
        }
        assert_eq!(launcher.state.selected(), Some(5));
        let (width, height) = (100usize, 30usize);
        let list_top = find_row(&mut launcher, width, height, "Modules (").unwrap() + 1;
        let daemon_row =
            find_row(&mut launcher, width, height, "Daemon Status").unwrap() - list_top;

        launcher.next(); // Daemon -> Vault
        assert_eq!(launcher.state.selected(), Some(6));
        let list_top = find_row(&mut launcher, width, height, "Modules (").unwrap() + 1;
        let vault_row =
            find_row(&mut launcher, width, height, "Encrypted Vaults").unwrap() - list_top;
        let http_row = find_row(&mut launcher, width, height, "API Client").unwrap() - list_top;

        assert_eq!(
            vault_row, daemon_row,
            "the cursor's screen row should not move once scrolling has engaged"
        );
        assert_eq!(
            http_row, 0,
            "the ring should have rotated so the wrapped-around item lands at the very top"
        );
    }

    #[test]
    fn test_recompute_ring_viewport_stays_put_in_the_free_zone() {
        // n=10, margin=2: cursor_row in [2, 7] is free — moving within it
        // never changes viewport_top.
        assert_eq!(recompute_ring_viewport(8, 1, 10, 2), 8);
        assert_eq!(recompute_ring_viewport(8, 5, 10, 2), 8);
    }

    #[test]
    fn test_recompute_ring_viewport_rotates_forward_past_the_bottom_margin() {
        // The exact KARE1 -> KARE3 sequence confirmed with the user.
        assert_eq!(recompute_ring_viewport(0, 0, 10, 2), 8); // KARE1: Dashboard selected fresh
        assert_eq!(recompute_ring_viewport(8, 6, 10, 2), 9); // KARE3: one more Down past the margin
    }

    #[test]
    fn test_recompute_ring_viewport_rotates_backward_past_the_top_margin() {
        assert_eq!(recompute_ring_viewport(8, 0, 10, 2), 8); // still in free zone at row 2
        assert_eq!(recompute_ring_viewport(8, 9, 10, 2), 7); // wraps past Dashboard to the last item
    }

    #[test]
    fn test_recompute_ring_viewport_handles_empty_and_singleton_lists() {
        assert_eq!(recompute_ring_viewport(5, 0, 0, 2), 0);
        // A single item with a margin larger than the list: must not
        // panic, and settles on a stable value.
        assert_eq!(recompute_ring_viewport(0, 0, 1, 2), 0);
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
