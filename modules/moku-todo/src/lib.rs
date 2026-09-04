use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use rand::RngCore;
use rand::rngs::OsRng;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use serde::{Deserialize, Serialize};

use moku_core::{
    AppContext, Command, ModuleId, ModuleMeta, ModuleStatus, MokuTheme, TuiModule, resolve_event,
};

/// A single task. Deliberately shared-friendly: `id`/`parent_id` are the
/// only structure needed for today's nesting, and are also exactly what a
/// future Kanban/Calendar module would need to reference the same
/// records (either by depending on this type directly, or — following
/// the precedent already set by `moku-dashboard` reading this module's
/// own `("todo", "items")` storage key with a compatible shadow struct —
/// by reading the same stored JSON independently). Deliberately NOT
/// included: Kanban-only fields like `project`/`column`, Calendar-only
/// fields like `due` — they'd sit unused in this module today. Adding
/// them later is a non-breaking, purely-additive change (every field
/// here either has no default, because it was always required, or an
/// explicit per-field default for forward compatibility with the old
/// two-field `{title, completed}` shape).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Task {
    /// Stable identity, independent of title/position — this is what
    /// `parent_id` references, and what a future module would key off of.
    #[serde(default = "new_task_id")]
    pub id: String,
    pub title: String,
    pub completed: bool,
    /// `Some(parent.id)` makes this a sub-task. This one field is the
    /// entire nesting mechanism — the list stays flat in storage; the
    /// tree is only built at render/navigation time (see `build_view`).
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

impl Task {
    fn new(title: String, parent_id: Option<String>) -> Self {
        let now = now_secs();
        Self {
            id: new_task_id(),
            title,
            completed: false,
            parent_id,
            created_at: now,
            updated_at: now,
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Matches `modules/moku-secrets/src/model.rs`'s `random_id` exactly —
/// same shape, same rationale (short, not derived from content so
/// renaming a task doesn't change its identity).
fn new_task_id() -> String {
    let mut bytes = [0u8; 8];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Deserialize, Default)]
struct TodoKeyConfig {
    pub keys: HashMap<String, String>,
}

/// One row of the flattened tree view: which `items` index it points at,
/// and how deeply nested it is (0 = root task).
struct ViewRow {
    index: usize,
    depth: usize,
}

/// Flattens `items` (a flat `Vec<Task>` linked only by `parent_id`) into
/// display order: each root task, immediately followed by its children
/// (recursively, same rule) unless its id is in `collapsed`. Sibling
/// order within a parent is just their relative order in `items` — same
/// as today's flat list already relies on `Vec` order for top-level
/// ordering, so no separate "order" field is needed.
fn build_view(items: &[Task], collapsed: &HashSet<String>) -> Vec<ViewRow> {
    let mut view = Vec::new();
    walk_children(items, None, 0, collapsed, &mut view);
    view
}

fn walk_children(
    items: &[Task],
    parent_id: Option<&str>,
    depth: usize,
    collapsed: &HashSet<String>,
    view: &mut Vec<ViewRow>,
) {
    for (index, task) in items.iter().enumerate() {
        if task.parent_id.as_deref() == parent_id {
            view.push(ViewRow { index, depth });
            if !collapsed.contains(&task.id) {
                walk_children(items, Some(task.id.as_str()), depth + 1, collapsed, view);
            }
        }
    }
}

fn has_children(items: &[Task], id: &str) -> bool {
    items.iter().any(|t| t.parent_id.as_deref() == Some(id))
}

/// Collects `id` and every descendant id (recursively) — the full
/// subtree a cascading delete needs to remove.
fn collect_subtree_ids(items: &[Task], id: &str, out: &mut Vec<String>) {
    out.push(id.to_string());
    for task in items {
        if task.parent_id.as_deref() == Some(id) {
            collect_subtree_ids(items, &task.id, out);
        }
    }
}

pub struct TodoModule {
    items: Vec<Task>,
    /// Selection index into the flattened *view* (see `build_view`), not
    /// directly into `items`.
    state: ListState,
    /// Task ids currently collapsed (children hidden). UI-only, not
    /// persisted — collapsing is a viewing convenience, not data.
    collapsed: HashSet<String>,
    input_mode: bool,
    input_buffer: String,
    /// `Some(id)` while composing a sub-task (opened via Tab) — `None`
    /// means the pending input is a new top-level task (opened via `a`).
    input_parent: Option<String>,
}

impl TodoModule {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            state: ListState::default(),
            collapsed: HashSet::new(),
            input_mode: false,
            input_buffer: String::new(),
            input_parent: None,
        }
    }

    async fn save(&self, ctx: &mut AppContext) {
        let encrypt =
            moku_core::resolve_encryption(&ctx.config.load(), ModuleId::TODO.as_str(), true);
        if let Err(e) = ctx
            .storage
            .save(ModuleId::TODO.as_str(), "items", &self.items, encrypt)
            .await
        {
            ctx.show_error(format!("Save error: {}", e));
        }
    }

    fn view(&self) -> Vec<ViewRow> {
        build_view(&self.items, &self.collapsed)
    }

    /// The `items` index the current selection points at, if any.
    fn selected_index(&self) -> Option<usize> {
        let view = self.view();
        self.state
            .selected()
            .and_then(|pos| view.get(pos))
            .map(|row| row.index)
    }

    fn next(&mut self) -> bool {
        let len = self.view().len();
        if len == 0 {
            return false;
        }
        let i = match self.state.selected() {
            Some(i) => {
                if i >= len - 1 {
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
        let len = self.view().len();
        if len == 0 {
            return false;
        }
        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    len - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
        true
    }

    async fn toggle_status(&mut self, ctx: &mut AppContext) -> bool {
        let Some(idx) = self.selected_index() else {
            return false;
        };
        let Some(item) = self.items.get_mut(idx) else {
            return false;
        };
        item.completed = !item.completed;
        item.updated_at = now_secs();
        let msg = if item.completed {
            "Completed"
        } else {
            "Reverted"
        };
        ctx.show_info(format!("Task: {}", msg));
        self.save(ctx).await;
        true
    }

    /// Deletes the selected task AND its entire subtree (cascading, no
    /// confirmation — matches this module's existing no-confirm-on-
    /// delete style, just extended to a whole subtree at once).
    async fn delete_item(&mut self, ctx: &mut AppContext) -> bool {
        let Some(idx) = self.selected_index() else {
            return false;
        };
        let Some(task) = self.items.get(idx) else {
            return false;
        };
        let mut doomed = Vec::new();
        collect_subtree_ids(&self.items, &task.id, &mut doomed);
        let title = task.title.clone();
        let doomed: HashSet<String> = doomed.into_iter().collect();
        self.items.retain(|t| !doomed.contains(&t.id));
        self.collapsed.retain(|id| !doomed.contains(id));

        let new_len = self.view().len();
        if new_len == 0 {
            self.state.select(None);
        } else if let Some(pos) = self.state.selected()
            && pos >= new_len
        {
            self.state.select(Some(new_len - 1));
        }
        ctx.show_info(format!("Deleted: {}", title));
        self.save(ctx).await;
        true
    }

    /// Opens the add-task input. `parent_id: None` adds a new root task
    /// (existing `[a]` behavior); `Some(id)` (from `Tab`) adds a
    /// sub-task under that task instead.
    fn start_add(&mut self, parent_id: Option<String>) {
        self.input_mode = true;
        self.input_buffer.clear();
        self.input_parent = parent_id;
    }

    /// `Tab`: add a sub-task under the selected task, if any is selected
    /// — otherwise falls back to a normal top-level add.
    fn start_add_subtask(&mut self) {
        let parent = self
            .selected_index()
            .and_then(|idx| self.items.get(idx))
            .map(|t| t.id.clone());
        self.start_add(parent);
    }

    async fn add_item(&mut self, ctx: &mut AppContext) {
        if !self.input_buffer.trim().is_empty() {
            let title = self.input_buffer.trim().to_string();
            let parent_id = self.input_parent.take();
            self.items.push(Task::new(title.clone(), parent_id));
            ctx.show_info(format!("'{}' Added", title));
            self.input_buffer.clear();
            let len = self.view().len();
            if len > 0 {
                self.state.select(Some(len - 1));
            }
            self.save(ctx).await;
        }
        self.input_mode = false;
        self.input_parent = None;
    }

    /// `Left`: collapses the selected task if it has children and isn't
    /// already collapsed. No-op on a leaf task.
    fn collapse_selected(&mut self) -> bool {
        let Some(idx) = self.selected_index() else {
            return false;
        };
        let Some(task) = self.items.get(idx) else {
            return false;
        };
        if has_children(&self.items, &task.id) && self.collapsed.insert(task.id.clone()) {
            return true;
        }
        false
    }

    /// `Right`: expands the selected task if it's currently collapsed.
    fn expand_selected(&mut self) -> bool {
        let Some(idx) = self.selected_index() else {
            return false;
        };
        let Some(task) = self.items.get(idx) else {
            return false;
        };
        self.collapsed.remove(&task.id)
    }
}

impl Default for TodoModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for TodoModule {
    fn id(&self) -> ModuleId {
        ModuleId::TODO
    }
    fn title(&self) -> &'static str {
        ModuleId::TODO.title()
    }
}

#[async_trait]
impl TuiModule for TodoModule {
    async fn init(&mut self, ctx: &mut AppContext) -> Result<()> {
        let loaded_items: Result<Vec<Task>> =
            ctx.storage.load(ModuleId::TODO.as_str(), "items").await;

        match loaded_items {
            Ok(items) => {
                self.items = items;
                if !self.items.is_empty() {
                    self.state.select(Some(0));
                }
            }
            Err(_) => {
                self.items = vec![Task::new("Welcome to Moku! 👋".to_string(), None)];
                self.save(ctx).await;
            }
        }
        Ok(())
    }

    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        if self.input_mode {
            let mut changed = false;
            if let Event::Key(key) = event
                && key.kind == KeyEventKind::Press
            {
                match key.code {
                    KeyCode::Enter => {
                        self.add_item(ctx).await;
                        changed = true;
                    }
                    KeyCode::Esc => {
                        self.input_mode = false;
                        self.input_parent = None;
                        self.input_buffer.clear();
                        changed = true;
                    }
                    KeyCode::Char(c) => {
                        self.input_buffer.push(c);
                        changed = true;
                    }
                    KeyCode::Backspace => {
                        self.input_buffer.pop();
                        changed = true;
                    }
                    _ => {}
                }
            }
            return Ok(changed);
        }

        // Tab isn't one of resolve_event's known actions, so it's checked
        // directly here first — same "raw check before resolve_event"
        // shape used elsewhere in this app (e.g. the launcher's
        // Shift+Up/Down reorder).
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Tab
        {
            self.start_add_subtask();
            return Ok(true);
        }

        let module_config: TodoKeyConfig = ctx
            .config
            .load()
            .resolve_module_config(ModuleId::TODO.as_str());
        let command = resolve_event(event, &ctx.config.load().keys, Some(&module_config.keys));

        let changed = match command {
            Command::Quit | Command::Back => {
                ctx.navigate_to(ModuleId::LAUNCHER);
                true
            }
            Command::Up => self.previous(),
            Command::Down => self.next(),
            Command::Left => self.collapse_selected(),
            Command::Right => self.expand_selected(),
            Command::Confirm | Command::Toggle => self.toggle_status(ctx).await,
            Command::Delete => self.delete_item(ctx).await,
            Command::Add => {
                self.start_add(None);
                true
            }
            _ => false,
        };
        Ok(changed)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(3)])
            .split(area);

        let view = self.view();
        let items: Vec<ListItem> = view
            .iter()
            .map(|row| {
                let task = &self.items[row.index];
                let (symbol, color) = if task.completed {
                    ("[x]", theme.success)
                } else {
                    ("[ ]", theme.base_fg)
                };
                // Plain ASCII only for the collapse indicator (">"/"v") —
                // the same lesson learned the hard way in the launcher:
                // mixed-width Unicode glyphs render at inconsistent
                // terminal cell widths and cause per-row misalignment.
                let marker = if !has_children(&self.items, &task.id) {
                    " "
                } else if self.collapsed.contains(&task.id) {
                    ">"
                } else {
                    "v"
                };
                let indent = "  ".repeat(row.depth);
                let content = Line::from(format!("{indent}{marker} {symbol} {}", task.title));
                ListItem::new(content).style(Style::default().fg(color))
            })
            .collect();

        let list_block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", ModuleId::TODO.title()))
            .title_alignment(ratatui::layout::Alignment::Center)
            .border_style(Style::default().fg(theme.border))
            .style(Style::default().bg(theme.base_bg));

        let list = List::new(items)
            .block(list_block)
            .highlight_style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg),
            )
            .highlight_symbol(">> ");

        frame.render_stateful_widget(list, chunks[0], &mut self.state);

        let bottom_content = if self.input_mode {
            let title = if self.input_parent.is_some() {
                " Add Sub-task "
            } else {
                " Add Task "
            };
            Paragraph::new(format!("NEW: {}_", self.input_buffer))
                .style(Style::default().fg(theme.warning))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(title)
                        .border_style(Style::default().fg(theme.warning)),
                )
        } else {
            Paragraph::new(" [a] Add | [Tab] Sub-task | [Space] Toggle | [<-/->] Collapse | [d] Delete | [Esc] Back ")
                .style(Style::default().fg(theme.base_fg))
                .alignment(ratatui::layout::Alignment::Center)
                .block(Block::default().borders(Borders::TOP).border_style(Style::default().fg(theme.border)))
        };

        frame.render_widget(bottom_content, chunks[1]);
    }

    async fn dashboard_summary(&self, ctx: &AppContext) -> Option<ModuleStatus> {
        let needs_vault =
            moku_core::resolve_encryption(&ctx.config.load(), ModuleId::TODO.as_str(), true);
        if needs_vault && !ctx.session.is_unlocked() {
            return Some(ModuleStatus::locked());
        }
        let items: Vec<Task> = ctx
            .storage
            .load(ModuleId::TODO.as_str(), "items")
            .await
            .unwrap_or_default();
        let done = items.iter().filter(|t| t.completed).count();
        Some(ModuleStatus::normal(format!(
            "{} tasks, {} done",
            items.len(),
            done
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use moku_core::security::{SecurityManager, VaultSession};
    use moku_core::{MokuConfig, StorageManager};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tempfile::tempdir;

    use super::*;

    async fn create_test_context() -> AppContext {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::mem::forget(temp);

        let config = Arc::new(ArcSwap::from_pointee(MokuConfig::default()));
        let session = Arc::new(VaultSession::new());
        let security = Arc::new(SecurityManager::new_with_root(root.clone()));
        let storage = Arc::new(
            StorageManager::new_with_root(Arc::clone(&session), root)
                .await
                .unwrap(),
        );

        AppContext::new(config, session, security, storage)
    }

    fn task(id: &str, title: &str, parent: Option<&str>) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            completed: false,
            parent_id: parent.map(|p| p.to_string()),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn test_new_task_id_is_unique_and_16_hex_chars() {
        let a = new_task_id();
        let b = new_task_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_old_two_field_json_migrates_with_a_fresh_unique_id() {
        let old_json = r#"[{"title":"a","completed":false},{"title":"b","completed":true}]"#;
        let tasks: Vec<Task> = serde_json::from_str(old_json).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_ne!(tasks[0].id, "", "id should be generated, not left empty");
        assert_ne!(
            tasks[0].id, tasks[1].id,
            "each migrated record must get its own unique id"
        );
        assert_eq!(tasks[0].parent_id, None);
        assert_eq!(tasks[0].created_at, 0);
    }

    #[test]
    fn test_build_view_orders_roots_then_each_roots_children_depth_first() {
        let items = vec![
            task("1", "root1", None),
            task("1a", "child of root1", Some("1")),
            task("2", "root2", None),
            task("1a1", "grandchild", Some("1a")),
        ];
        let view = build_view(&items, &HashSet::new());
        let order: Vec<&str> = view.iter().map(|r| items[r.index].id.as_str()).collect();
        assert_eq!(order, vec!["1", "1a", "1a1", "2"]);
        assert_eq!(
            view.iter()
                .find(|r| items[r.index].id == "1a1")
                .unwrap()
                .depth,
            2
        );
    }

    #[test]
    fn test_build_view_skips_children_of_a_collapsed_task() {
        let items = vec![
            task("1", "root", None),
            task("1a", "child", Some("1")),
            task("2", "root2", None),
        ];
        let mut collapsed = HashSet::new();
        collapsed.insert("1".to_string());
        let view = build_view(&items, &collapsed);
        let order: Vec<&str> = view.iter().map(|r| items[r.index].id.as_str()).collect();
        assert_eq!(
            order,
            vec!["1", "2"],
            "collapsed task's child should be hidden from the view"
        );
    }

    #[test]
    fn test_collect_subtree_ids_gets_the_task_and_all_descendants_only() {
        let items = vec![
            task("1", "root", None),
            task("1a", "child", Some("1")),
            task("1a1", "grandchild", Some("1a")),
            task("2", "unrelated root", None),
        ];
        let mut ids = Vec::new();
        collect_subtree_ids(&items, "1", &mut ids);
        let mut ids_sorted = ids.clone();
        ids_sorted.sort();
        assert_eq!(ids_sorted, vec!["1", "1a", "1a1"]);
        assert!(!ids.contains(&"2".to_string()));
    }

    #[test]
    fn test_has_children() {
        let items = vec![task("1", "root", None), task("1a", "child", Some("1"))];
        assert!(has_children(&items, "1"));
        assert!(!has_children(&items, "1a"));
    }

    fn rendered_rows(module: &mut TodoModule) -> Vec<String> {
        let (width, height) = (60u16, 20u16);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = MokuTheme::default();
        terminal
            .draw(|frame| module.draw(frame, Rect::new(0, 0, width, height), &theme))
            .unwrap();
        let content = terminal.backend().buffer().content.clone();
        (0..height as usize)
            .map(|y| {
                content[y * width as usize..(y + 1) * width as usize]
                    .iter()
                    .map(|c| c.symbol())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn test_child_row_indents_further_right_than_its_root() {
        let mut module = TodoModule::new();
        module.items = vec![
            task("1", "Root Task", None),
            task("1a", "Child Task", Some("1")),
        ];
        let rows = rendered_rows(&mut module);
        let root_x = rows
            .iter()
            .find_map(|r| r.find("Root Task"))
            .expect("root row visible");
        let child_x = rows
            .iter()
            .find_map(|r| r.find("Child Task"))
            .expect("child row visible");
        assert!(
            child_x > root_x,
            "a child row should start further right than its root (root_x={root_x}, child_x={child_x})"
        );
    }

    #[test]
    fn test_collapse_indicator_and_checkbox_are_plain_ascii_only() {
        let mut module = TodoModule::new();
        module.items = vec![
            task("1", "Root Task", None),
            task("1a", "Child Task", Some("1")),
        ];
        let rows = rendered_rows(&mut module);
        // Only the task rows' own content (marker + checkbox + title)
        // needs to be pure ASCII — strip the list block's left/right
        // border glyphs (unrelated to the collapse-indicator/checkbox
        // choice this test actually guards) before checking.
        let task_rows: Vec<String> = rows
            .iter()
            .filter(|r| r.contains("Task"))
            .map(|r| {
                let chars: Vec<char> = r.chars().collect();
                chars[1..chars.len().saturating_sub(1)].iter().collect()
            })
            .collect();
        assert_eq!(
            task_rows.len(),
            2,
            "expected exactly the root and child rows to contain a title"
        );
        for row in &task_rows {
            assert!(
                row.is_ascii(),
                "todo row should render using only ASCII glyphs: {row:?}"
            );
        }
        assert!(
            task_rows.iter().any(|r| r.contains("[ ]")),
            "expected the plain ASCII checkbox to still render"
        );
    }

    #[test]
    fn test_collapsed_parent_hides_child_from_render() {
        let mut module = TodoModule::new();
        module.items = vec![
            task("1", "Root Task", None),
            task("1a", "Child Task", Some("1")),
        ];
        module.collapsed.insert("1".to_string());
        let rows = rendered_rows(&mut module);
        let content = rows.join("");
        assert!(content.contains("Root Task"));
        assert!(
            !content.contains("Child Task"),
            "collapsed task's child should not render"
        );
    }

    #[tokio::test]
    async fn test_dashboard_summary_locked_when_vault_not_unlocked() {
        let module = TodoModule::new();
        let ctx = create_test_context().await;
        let status = module.dashboard_summary(&ctx).await.unwrap();
        assert_eq!(status.tone, moku_core::StatusTone::Locked);
    }

    #[tokio::test]
    async fn test_dashboard_summary_reports_counts_when_unlocked() {
        let module = TodoModule::new();
        let ctx = create_test_context().await;
        let key = SecurityManager::derive_key("test-pass", &[7u8; 16])
            .await
            .unwrap();
        ctx.session.unlock(key);

        let items = vec![
            task("1", "one", None),
            task("2", "two", None),
            task("3", "three", None),
        ];
        ctx.storage
            .save(ModuleId::TODO.as_str(), "items", &items, true)
            .await
            .unwrap();

        let status = module.dashboard_summary(&ctx).await.unwrap();
        assert_eq!(status.tone, moku_core::StatusTone::Normal);
        assert_eq!(status.text, "3 tasks, 0 done");
    }
}
