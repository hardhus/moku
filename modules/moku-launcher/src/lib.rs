use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
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
        }
    }
}

pub struct LauncherModule {
    registered_modules: Vec<ModuleId>,
    state: ListState,
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
        let rows = Layout::vertical([
            Constraint::Length(3),                             // 1. Header
            Constraint::Min(0),                                // 2. Module list
            Constraint::Length(1),                             // 3. Detail line
            Constraint::Length(if filtering { 3 } else { 0 }), // 4. Search input
            Constraint::Length(1),                             // 5. Status/help bar
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
        frame.render_widget(header, rows[0]);

        // 2. Module list — no icons or numbers (mixed-width emoji glyphs
        // render at inconsistent terminal cell widths across icons/fonts,
        // which is what caused the earlier per-row misalignment; plain
        // ASCII titles render at a fully consistent width everywhere).
        // Selection is shown by color (highlight_style below) AND by a
        // graduated indent that peaks on the selected row and tapers off
        // over its immediate neighbors, all center-aligned.
        let display_indices: Vec<usize> = match &self.filter {
            Some(f) => f.matches.clone(),
            None => (0..self.registered_modules.len()).collect(),
        };
        let selected_pos = match &self.filter {
            Some(f) => f.list_state.selected(),
            None => self.state.selected(),
        };
        let items: Vec<ListItem> = display_indices
            .iter()
            .enumerate()
            .map(|(pos, &idx)| {
                let id = self.registered_modules[idx];
                let indent = selected_indent(pos, selected_pos);
                let text = format!("{}{}", " ".repeat(indent), id.title());
                ListItem::new(Line::from(text).alignment(Alignment::Center))
            })
            .collect();
        let list = List::new(items)
            .block(
                Block::default()
                    .title(format!(" Modules ({}) ", self.registered_modules.len()))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border))
                    .style(Style::default().bg(theme.base_bg)),
            )
            .style(Style::default().fg(theme.base_fg))
            .highlight_style(
                Style::default()
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("");

        if let Some(f) = &mut self.filter {
            frame.render_stateful_widget(list, rows[1], &mut f.list_state);
        } else {
            frame.render_stateful_widget(list, rows[1], &mut self.state);
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
        frame.render_widget(detail, rows[2]);

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
            frame.render_widget(input, rows[3]);
        }

        // 5. Status/help bar
        let (badge, badge_bg, help) = if filtering {
            (
                " SEARCH ",
                theme.warning,
                " Type to filter · ↑↓ Move · Enter Open · Esc Cancel ",
            )
        } else {
            (
                " BROWSE ",
                theme.selection_bg,
                " ↑↓/jk Move · Enter Open · / Search · Shift+↑↓ Reorder · Esc/q Quit ",
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
}
