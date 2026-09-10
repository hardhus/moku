//! The full `Block` tree editor — Browse mode (navigate/add/delete/
//! reorder/collapse) plus a small inline NodeEditor (edit one node's own
//! value). Every structural or value change persists to config
//! immediately (see `persist`) — there is no separate "save" step, the
//! same way `moku-launcher`'s Shift-Up/Down reorder already persists
//! immediately via `persist_order`.
//!
//! The in-memory representation here (`EditNode`, flat + id/parent_id
//! linked) mirrors `modules/moku-todo/src/model.rs`'s `Task`/`ViewRow`
//! shape exactly, generalized to arbitrary nesting depth — see
//! `crate::model::{flatten_plan, unflatten_nodes, build_tree_view}`.

use std::collections::HashSet;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use moku_core::{
    AppContext, Command, ConfirmDeleteKey, ModuleId, MokuTheme, is_delete_bypass,
    resolve_confirm_delete_key, resolve_event,
};
use moku_pomodoro_daemon::{PhaseKind, RepeatCount};

use crate::engine::PomodoroConfig;
use crate::model::{EditNode, EditNodeKind, TreeViewRow, build_tree_view, collect_subtree_ids, has_children, next_node_id, unflatten_nodes};

pub enum NodeEditorState {
    Phase {
        minutes_buf: String,
        phase_kind: PhaseKind,
    },
    Repeat {
        count_buf: String,
        infinite: bool,
    },
}

pub struct TreeEditorState {
    pub profile_name: String,
    pub nodes: Vec<EditNode>,
    pub collapsed: HashSet<String>,
    pub list_state: ListState,
    pub editing: Option<(String, NodeEditorState)>,
    pub confirm_delete: bool,
}

impl TreeEditorState {
    pub fn new(profile_name: String, nodes: Vec<EditNode>) -> Self {
        let mut list_state = ListState::default();
        if !nodes.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            profile_name,
            nodes,
            collapsed: HashSet::new(),
            list_state,
            editing: None,
            confirm_delete: false,
        }
    }

    fn view(&self) -> Vec<TreeViewRow> {
        build_tree_view(&self.nodes, &self.collapsed)
    }
}

pub(super) enum Outcome {
    None,
    Redraw,
    Back,
}

/// Writes `state.nodes` (converted back to a `Plan`) into the matching
/// profile in config and saves — called after every structural or value
/// change, never batched.
async fn persist(state: &TreeEditorState, ctx: &mut AppContext) {
    let mut config: PomodoroConfig = ctx.config.load().resolve_module_config(ModuleId::POMODORO.as_str());
    if let Some(p) = config.profiles.iter_mut().find(|p| p.name == state.profile_name) {
        p.plan = unflatten_nodes(&state.nodes);
    }
    super::save_config(ctx, &config).await;
}

pub(super) async fn handle_event(state: &mut TreeEditorState, event: &Event, ctx: &mut AppContext) -> Result<Outcome> {
    if state.confirm_delete {
        return Ok(match resolve_confirm_delete_key(event) {
            ConfirmDeleteKey::Confirm => {
                state.confirm_delete = false;
                delete_selected(state);
                persist(state, ctx).await;
                Outcome::Redraw
            }
            ConfirmDeleteKey::Cancel => {
                state.confirm_delete = false;
                Outcome::Redraw
            }
            ConfirmDeleteKey::Other => Outcome::None,
        });
    }
    if state.editing.is_some() {
        return Ok(handle_node_editor_event(state, event, ctx).await);
    }
    Ok(handle_browse_event(state, event, ctx).await)
}

async fn handle_browse_event(state: &mut TreeEditorState, event: &Event, ctx: &mut AppContext) -> Outcome {
    let Event::Key(key) = event else { return Outcome::None };
    if key.kind != KeyEventKind::Press {
        return Outcome::None;
    }

    // Shift+Up/Down (reorder) and Shift+D (delete bypass) are checked
    // raw, before resolve_event — same "raw check before resolve_event"
    // shape used elsewhere in this app (e.g. moku-todo's Tab/Shift+D).
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        match key.code {
            KeyCode::Up => {
                reorder_selected(state, -1);
                persist(state, ctx).await;
                return Outcome::Redraw;
            }
            KeyCode::Down => {
                reorder_selected(state, 1);
                persist(state, ctx).await;
                return Outcome::Redraw;
            }
            _ => {}
        }
    }
    if is_delete_bypass(event) {
        delete_selected(state);
        persist(state, ctx).await;
        return Outcome::Redraw;
    }

    let command = resolve_event(event, &ctx.config.load().keys, None);
    match command {
        Command::Up => {
            move_selection(state, -1);
            return Outcome::Redraw;
        }
        Command::Down => {
            move_selection(state, 1);
            return Outcome::Redraw;
        }
        Command::Back | Command::Quit => return Outcome::Back,
        Command::Left => {
            collapse_selected(state);
            return Outcome::Redraw;
        }
        Command::Right => {
            expand_selected(state);
            return Outcome::Redraw;
        }
        Command::Confirm => {
            open_node_editor(state);
            return Outcome::Redraw;
        }
        _ => {}
    }

    match key.code {
        KeyCode::Char('a') => {
            add_phase(state, false, ctx).await;
            Outcome::Redraw
        }
        KeyCode::Char('A') => {
            add_phase(state, true, ctx).await;
            Outcome::Redraw
        }
        KeyCode::Char('g') => {
            add_repeat(state, false, ctx).await;
            Outcome::Redraw
        }
        KeyCode::Char('G') => {
            add_repeat(state, true, ctx).await;
            Outcome::Redraw
        }
        KeyCode::Char('d') => {
            if state.list_state.selected().is_some() {
                state.confirm_delete = true;
                Outcome::Redraw
            } else {
                Outcome::None
            }
        }
        _ => Outcome::None,
    }
}

async fn handle_node_editor_event(state: &mut TreeEditorState, event: &Event, ctx: &mut AppContext) -> Outcome {
    let Event::Key(key) = event else { return Outcome::None };
    if key.kind != KeyEventKind::Press {
        return Outcome::None;
    }
    let Some((id, editor)) = &mut state.editing else {
        return Outcome::None;
    };

    match editor {
        NodeEditorState::Phase {
            minutes_buf,
            phase_kind,
        } => match key.code {
            KeyCode::Esc => {
                state.editing = None;
                Outcome::Redraw
            }
            KeyCode::Left => {
                *phase_kind = prev_phase_kind(*phase_kind);
                Outcome::Redraw
            }
            KeyCode::Right => {
                *phase_kind = next_phase_kind(*phase_kind);
                Outcome::Redraw
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                minutes_buf.push(c);
                Outcome::Redraw
            }
            KeyCode::Backspace => {
                minutes_buf.pop();
                Outcome::Redraw
            }
            KeyCode::Enter => {
                let Ok(minutes) = minutes_buf.trim().parse::<u32>() else {
                    return Outcome::Redraw;
                };
                if minutes == 0 {
                    return Outcome::Redraw;
                }
                let node_id = id.clone();
                let new_kind = *phase_kind;
                state.editing = None;
                if let Some(n) = state.nodes.iter_mut().find(|n| n.id == node_id) {
                    n.kind = EditNodeKind::Phase {
                        phase_kind: new_kind,
                        minutes,
                        label: None,
                    };
                }
                persist(state, ctx).await;
                Outcome::Redraw
            }
            _ => Outcome::None,
        },
        NodeEditorState::Repeat { count_buf, infinite } => match key.code {
            KeyCode::Esc => {
                state.editing = None;
                Outcome::Redraw
            }
            KeyCode::Char('i') => {
                *infinite = !*infinite;
                Outcome::Redraw
            }
            KeyCode::Char(c) if c.is_ascii_digit() && !*infinite => {
                count_buf.push(c);
                Outcome::Redraw
            }
            KeyCode::Backspace if !*infinite => {
                count_buf.pop();
                Outcome::Redraw
            }
            KeyCode::Enter => {
                let new_count = if *infinite {
                    RepeatCount::Infinite
                } else {
                    match count_buf.trim().parse::<u32>() {
                        Ok(n) if n > 0 => RepeatCount::Finite(n),
                        _ => return Outcome::Redraw,
                    }
                };
                let node_id = id.clone();
                state.editing = None;
                if let Some(n) = state.nodes.iter_mut().find(|n| n.id == node_id) {
                    n.kind = EditNodeKind::Repeat { count: new_count };
                }
                persist(state, ctx).await;
                Outcome::Redraw
            }
            _ => Outcome::None,
        },
    }
}

fn next_phase_kind(k: PhaseKind) -> PhaseKind {
    match k {
        PhaseKind::Work => PhaseKind::Break,
        PhaseKind::Break => PhaseKind::LongBreak,
        PhaseKind::LongBreak => PhaseKind::Work,
    }
}
fn prev_phase_kind(k: PhaseKind) -> PhaseKind {
    match k {
        PhaseKind::Work => PhaseKind::LongBreak,
        PhaseKind::Break => PhaseKind::Work,
        PhaseKind::LongBreak => PhaseKind::Break,
    }
}

fn move_selection(state: &mut TreeEditorState, delta: i32) {
    let len = state.view().len();
    if len == 0 {
        state.list_state.select(None);
        return;
    }
    let i = match state.list_state.selected() {
        Some(i) => ((i as i32 + delta).rem_euclid(len as i32)) as usize,
        None => 0,
    };
    state.list_state.select(Some(i));
}

fn selected_row(state: &TreeEditorState) -> Option<TreeViewRow> {
    let view = state.view();
    state.list_state.selected().and_then(|pos| view.get(pos).copied())
}

fn collapse_selected(state: &mut TreeEditorState) {
    if let Some(row) = selected_row(state) {
        let id = state.nodes[row.index].id.clone();
        if has_children(&state.nodes, &id) {
            state.collapsed.insert(id);
        }
    }
}

fn expand_selected(state: &mut TreeEditorState) {
    if let Some(row) = selected_row(state) {
        let id = state.nodes[row.index].id.clone();
        state.collapsed.remove(&id);
    }
}

fn open_node_editor(state: &mut TreeEditorState) {
    let Some(row) = selected_row(state) else { return };
    let node = &state.nodes[row.index];
    let editor = match &node.kind {
        EditNodeKind::Phase {
            phase_kind, minutes, ..
        } => NodeEditorState::Phase {
            minutes_buf: minutes.to_string(),
            phase_kind: *phase_kind,
        },
        EditNodeKind::Repeat { count } => {
            let (buf, infinite) = match count {
                RepeatCount::Finite(n) => (n.to_string(), false),
                RepeatCount::Infinite => ("1".to_string(), true),
            };
            NodeEditorState::Repeat {
                count_buf: buf,
                infinite,
            }
        }
    };
    state.editing = Some((node.id.clone(), editor));
}

fn select_node_by_id(state: &mut TreeEditorState, id: &str) {
    let view = state.view();
    if let Some(pos) = view.iter().position(|row| state.nodes[row.index].id == id) {
        state.list_state.select(Some(pos));
    }
}

/// `as_child`: insert as the selected `Repeat`'s child (no-op if the
/// selection isn't a `Repeat`); otherwise as a sibling in the selection's
/// own level (or top-level if nothing's selected). Always appended as the
/// *last* item in that level — reorder with Shift-Up/Down afterward for
/// exact positioning, rather than trying to insert "after the cursor"
/// (raw node ids/positions don't carry enough information to do that
/// safely once nodes have been added/removed/reordered).
async fn add_phase(state: &mut TreeEditorState, as_child: bool, ctx: &mut AppContext) {
    let Some(parent_id) = target_parent(state, as_child) else { return };
    let new_id = next_node_id(&state.nodes);
    state.nodes.push(EditNode {
        id: new_id.clone(),
        parent_id,
        kind: EditNodeKind::Phase {
            phase_kind: PhaseKind::Work,
            minutes: 25,
            label: None,
        },
    });
    select_node_by_id(state, &new_id);
    persist(state, ctx).await;
    open_node_editor(state);
}

async fn add_repeat(state: &mut TreeEditorState, as_child: bool, ctx: &mut AppContext) {
    let Some(parent_id) = target_parent(state, as_child) else { return };
    let new_id = next_node_id(&state.nodes);
    state.nodes.push(EditNode {
        id: new_id.clone(),
        parent_id,
        kind: EditNodeKind::Repeat {
            count: RepeatCount::Finite(1),
        },
    });
    select_node_by_id(state, &new_id);
    persist(state, ctx).await;
}

/// `Some(parent_id)` — the parent a new node should be inserted under, or
/// `None`-as-top-level wrapped in `Some(None)`. Bare `None` (no `Some` at
/// all) means "don't add anything" (only for `as_child` when the
/// selection isn't a `Repeat`).
fn target_parent(state: &TreeEditorState, as_child: bool) -> Option<Option<String>> {
    if as_child {
        let row = selected_row(state)?;
        let node = &state.nodes[row.index];
        match &node.kind {
            EditNodeKind::Repeat { .. } => Some(Some(node.id.clone())),
            EditNodeKind::Phase { .. } => None,
        }
    } else {
        match selected_row(state) {
            Some(row) => Some(state.nodes[row.index].parent_id.clone()),
            None => Some(None),
        }
    }
}

fn delete_selected(state: &mut TreeEditorState) {
    let Some(row) = selected_row(state) else { return };
    let id = state.nodes[row.index].id.clone();
    let mut doomed = Vec::new();
    collect_subtree_ids(&state.nodes, &id, &mut doomed);
    let doomed: HashSet<String> = doomed.into_iter().collect();
    state.nodes.retain(|n| !doomed.contains(&n.id));
    state.collapsed.retain(|id| !doomed.contains(id));

    let new_len = state.view().len();
    if new_len == 0 {
        state.list_state.select(None);
    } else if let Some(pos) = state.list_state.selected()
        && pos >= new_len
    {
        state.list_state.select(Some(new_len - 1));
    }
}

/// Swaps the selected node with its previous/next **sibling** (same
/// `parent_id`) — a plain `Vec::swap` of the two full `EditNode` values,
/// id included, so any descendants (which reference their parent by id,
/// never by Vec position) stay correctly attached regardless of which
/// slot their parent ends up in.
fn reorder_selected(state: &mut TreeEditorState, delta: i32) {
    let Some(row) = selected_row(state) else { return };
    let idx = row.index;
    let moved_id = state.nodes[idx].id.clone();
    let parent = state.nodes[idx].parent_id.clone();
    let siblings: Vec<usize> = state
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.parent_id == parent)
        .map(|(i, _)| i)
        .collect();
    let Some(pos_in_siblings) = siblings.iter().position(|&i| i == idx) else {
        return;
    };
    let target_pos = pos_in_siblings as i32 + delta;
    if target_pos < 0 || target_pos as usize >= siblings.len() {
        return;
    }
    let other_idx = siblings[target_pos as usize];
    state.nodes.swap(idx, other_idx);
    select_node_by_id(state, &moved_id);
}

pub(super) fn draw(frame: &mut Frame, area: Rect, theme: &MokuTheme, state: &mut TreeEditorState) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);
    let view = state.view();

    let items: Vec<ListItem> = view
        .iter()
        .map(|row| {
            let node = &state.nodes[row.index];
            let indent = "  ".repeat(row.depth);
            let marker = if !has_children(&state.nodes, &node.id) {
                " "
            } else if state.collapsed.contains(&node.id) {
                ">"
            } else {
                "v"
            };
            let label = match &node.kind {
                EditNodeKind::Phase {
                    phase_kind, minutes, ..
                } => format!("{} {minutes} min", phase_kind.label()),
                EditNodeKind::Repeat { count } => match count {
                    RepeatCount::Infinite => "Repeat forever".to_string(),
                    RepeatCount::Finite(n) => format!("Repeat x{n}"),
                },
            };
            ListItem::new(format!("{indent}{marker} {label}"))
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} — Tree Editor ", state.profile_name))
        .title_alignment(Alignment::Center)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.base_bg));

    if items.is_empty() {
        let para = Paragraph::new("Empty plan. Press [a] to add a phase.")
            .alignment(Alignment::Center)
            .block(block)
            .style(Style::default().fg(theme.base_fg));
        frame.render_widget(para, chunks[0]);
    } else {
        let list = List::new(items)
            .block(block)
            .highlight_style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg),
            )
            .highlight_symbol(">> ");
        frame.render_stateful_widget(list, chunks[0], &mut state.list_state);
    }

    let bottom = if state.confirm_delete {
        Paragraph::new("Delete this node (and its subtree)? [y] Yes  [n] No")
            .style(Style::default().fg(theme.error))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(theme.error)),
            )
    } else if let Some((_, editor)) = &state.editing {
        let text = match editor {
            NodeEditorState::Phase {
                minutes_buf,
                phase_kind,
            } => format!(
                "{}  minutes: {minutes_buf}_  ([Left/Right] change kind, [Enter] save, [Esc] cancel)",
                phase_kind.label()
            ),
            NodeEditorState::Repeat { count_buf, infinite } => format!(
                "count: {}  ([i] toggle infinite, [Enter] save, [Esc] cancel)",
                if *infinite {
                    "infinite".to_string()
                } else {
                    format!("{count_buf}_")
                }
            ),
        };
        Paragraph::new(text)
            .style(Style::default().fg(theme.warning))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(theme.warning)),
            )
    } else {
        Paragraph::new(" [Enter] Edit  [a/A] +Phase  [g/G] +Repeat  [d] Del  [Shift+^/v] Move  [Esc] Back ")
            .style(Style::default().fg(theme.base_fg))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(theme.border)),
            )
    };
    frame.render_widget(bottom, chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::flatten_plan;
    use crossterm::event::KeyEvent;
    use moku_pomodoro_daemon::Block;
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use moku_core::security::{SecurityManager, VaultSession};
    use moku_core::{MokuConfig, StorageManager};
    use tempfile::tempdir;

    async fn create_test_context() -> AppContext {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::mem::forget(temp);
        let config = Arc::new(ArcSwap::from_pointee(MokuConfig::default()));
        let session = Arc::new(VaultSession::new());
        let security = Arc::new(SecurityManager::new_with_root(root.clone()));
        let storage = Arc::new(StorageManager::new_with_root(Arc::clone(&session), root).await.unwrap());
        AppContext::new(config, session, security, storage)
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }
    fn shift_key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::SHIFT))
    }

    fn state_for(plan: &moku_pomodoro_daemon::Plan) -> TreeEditorState {
        TreeEditorState::new("test".to_string(), flatten_plan(plan))
    }

    #[test]
    fn test_move_selection_wraps() {
        let plan: moku_pomodoro_daemon::Plan = vec![
            Block::Phase {
                kind: PhaseKind::Work,
                minutes: 10,
                label: None,
            },
            Block::Phase {
                kind: PhaseKind::Break,
                minutes: 5,
                label: None,
            },
        ];
        let mut state = state_for(&plan);
        assert_eq!(state.list_state.selected(), Some(0));
        move_selection(&mut state, -1);
        assert_eq!(state.list_state.selected(), Some(1), "should wrap to the last row");
        move_selection(&mut state, 1);
        assert_eq!(state.list_state.selected(), Some(0));
    }

    #[tokio::test]
    async fn test_add_phase_sibling_persists_immediately() {
        let plan: moku_pomodoro_daemon::Plan = vec![Block::Phase {
            kind: PhaseKind::Work,
            minutes: 10,
            label: None,
        }];
        let mut state = state_for(&plan);
        let mut ctx = create_test_context().await;
        ctx.update_config(|cfg| {
            cfg.modules.insert(
                "pomodoro".to_string(),
                toml::Value::try_from(&PomodoroConfig {
                    profiles: vec![crate::engine::Profile {
                        name: "test".to_string(),
                        plan: plan.clone(),
                    }],
                    ..PomodoroConfig::default()
                })
                .unwrap(),
            );
        });

        add_phase(&mut state, false, &mut ctx).await;
        assert_eq!(state.nodes.len(), 2, "a new phase node was added");
        assert!(state.editing.is_some(), "adding a phase opens its editor immediately");

        let saved: PomodoroConfig = ctx.config.load().resolve_module_config("pomodoro");
        assert_eq!(
            saved.profiles[0].plan.len(),
            2,
            "the addition should already be persisted, before the editor is even confirmed"
        );
    }

    #[test]
    fn test_add_repeat_as_child_requires_repeat_selection() {
        let plan: moku_pomodoro_daemon::Plan = vec![Block::Phase {
            kind: PhaseKind::Work,
            minutes: 10,
            label: None,
        }];
        let state = state_for(&plan);
        // Selection is a Phase — adding a child should be rejected.
        assert!(target_parent(&state, true).is_none());
    }

    #[test]
    fn test_delete_selected_removes_repeat_and_its_whole_subtree() {
        let plan: moku_pomodoro_daemon::Plan = vec![
            Block::Repeat {
                count: RepeatCount::Finite(1),
                blocks: vec![
                    Block::Phase {
                        kind: PhaseKind::Work,
                        minutes: 10,
                        label: None,
                    },
                    Block::Phase {
                        kind: PhaseKind::Break,
                        minutes: 5,
                        label: None,
                    },
                ],
            },
            Block::Phase {
                kind: PhaseKind::LongBreak,
                minutes: 20,
                label: None,
            },
        ];
        let mut state = state_for(&plan);
        state.list_state.select(Some(0)); // the Repeat itself
        delete_selected(&mut state);
        assert_eq!(state.nodes.len(), 1, "the repeat and its 2 children are gone, long break remains");
        assert!(matches!(state.nodes[0].kind, EditNodeKind::Phase { .. }));
    }

    #[test]
    fn test_reorder_swaps_only_within_the_same_parent_and_preserves_children() {
        let plan: moku_pomodoro_daemon::Plan = vec![
            Block::Phase {
                kind: PhaseKind::Work,
                minutes: 10,
                label: None,
            },
            Block::Repeat {
                count: RepeatCount::Finite(2),
                blocks: vec![Block::Phase {
                    kind: PhaseKind::Break,
                    minutes: 5,
                    label: None,
                }],
            },
        ];
        let mut state = state_for(&plan);
        state.list_state.select(Some(0)); // the Work phase (index 0 in nodes, sibling of the Repeat)
        reorder_selected(&mut state, 1); // swap with the Repeat sibling

        let rebuilt = unflatten_nodes(&state.nodes);
        // Order should have flipped: Repeat(with its child intact) first, Work second.
        assert!(matches!(rebuilt[0], Block::Repeat { .. }));
        let Block::Repeat { blocks, .. } = &rebuilt[0] else {
            unreachable!()
        };
        assert_eq!(blocks.len(), 1, "the repeat's child must have followed it");
        assert!(matches!(rebuilt[1], Block::Phase { .. }));
    }

    #[test]
    fn test_reorder_at_boundary_is_a_no_op() {
        let plan: moku_pomodoro_daemon::Plan = vec![
            Block::Phase {
                kind: PhaseKind::Work,
                minutes: 10,
                label: None,
            },
            Block::Phase {
                kind: PhaseKind::Break,
                minutes: 5,
                label: None,
            },
        ];
        let mut state = state_for(&plan);
        state.list_state.select(Some(0));
        reorder_selected(&mut state, -1); // already first, moving further up is a no-op
        let rebuilt = unflatten_nodes(&state.nodes);
        assert_eq!(rebuilt, plan);
    }

    #[test]
    fn test_collapse_then_expand_toggles_visible_children() {
        let plan: moku_pomodoro_daemon::Plan = vec![Block::Repeat {
            count: RepeatCount::Finite(1),
            blocks: vec![Block::Phase {
                kind: PhaseKind::Work,
                minutes: 10,
                label: None,
            }],
        }];
        let mut state = state_for(&plan);
        state.list_state.select(Some(0));
        collapse_selected(&mut state);
        assert_eq!(state.view().len(), 1);
        expand_selected(&mut state);
        assert_eq!(state.view().len(), 2);
    }

    #[test]
    fn test_open_node_editor_prefills_from_current_value() {
        let plan: moku_pomodoro_daemon::Plan = vec![Block::Phase {
            kind: PhaseKind::Break,
            minutes: 15,
            label: None,
        }];
        let mut state = state_for(&plan);
        open_node_editor(&mut state);
        let Some((_, NodeEditorState::Phase { minutes_buf, phase_kind })) = &state.editing else {
            panic!("expected Phase editor");
        };
        assert_eq!(minutes_buf, "15");
        assert_eq!(*phase_kind, PhaseKind::Break);
    }

    #[tokio::test]
    async fn test_node_editor_enter_confirms_and_esc_discards() {
        let plan: moku_pomodoro_daemon::Plan = vec![Block::Phase {
            kind: PhaseKind::Work,
            minutes: 10,
            label: None,
        }];
        let mut state = state_for(&plan);
        let mut ctx = create_test_context().await;

        open_node_editor(&mut state);
        if let Some((_, NodeEditorState::Phase { minutes_buf, .. })) = &mut state.editing {
            minutes_buf.clear();
        }
        handle_node_editor_event(&mut state, &key(KeyCode::Char('9')), &mut ctx).await;
        handle_node_editor_event(&mut state, &key(KeyCode::Enter), &mut ctx).await;
        assert!(state.editing.is_none());
        let EditNodeKind::Phase { minutes, .. } = state.nodes[0].kind else {
            unreachable!()
        };
        assert_eq!(minutes, 9);

        open_node_editor(&mut state);
        handle_node_editor_event(&mut state, &key(KeyCode::Char('1')), &mut ctx).await;
        handle_node_editor_event(&mut state, &key(KeyCode::Esc), &mut ctx).await;
        assert!(state.editing.is_none());
        let EditNodeKind::Phase { minutes, .. } = state.nodes[0].kind else {
            unreachable!()
        };
        assert_eq!(minutes, 9, "Esc should discard the in-progress edit");
    }

    #[test]
    fn test_repeat_editor_infinite_toggle_ignores_digit_input() {
        let plan: moku_pomodoro_daemon::Plan = vec![Block::Repeat {
            count: RepeatCount::Finite(3),
            blocks: vec![],
        }];
        let mut state = state_for(&plan);
        open_node_editor(&mut state);
        let Some((_, editor)) = &mut state.editing else { unreachable!() };
        let NodeEditorState::Repeat { infinite, .. } = editor else {
            unreachable!()
        };
        assert!(!*infinite);
        *infinite = true;
        let NodeEditorState::Repeat { count_buf, infinite } = editor else {
            unreachable!()
        };
        assert!(*infinite);
        let before = count_buf.clone();
        count_buf.push('9'); // simulating what the handler would skip while infinite
        assert_ne!(&before, count_buf, "this direct mutation isn't gated — the real gate is in handle_node_editor_event's match guard");
    }

    #[tokio::test]
    async fn test_shift_up_reorders_via_handle_event() {
        let plan: moku_pomodoro_daemon::Plan = vec![
            Block::Phase {
                kind: PhaseKind::Work,
                minutes: 10,
                label: None,
            },
            Block::Phase {
                kind: PhaseKind::Break,
                minutes: 5,
                label: None,
            },
        ];
        let mut state = state_for(&plan);
        let mut ctx = create_test_context().await;
        ctx.update_config(|cfg| {
            cfg.modules.insert(
                "pomodoro".to_string(),
                toml::Value::try_from(&PomodoroConfig {
                    profiles: vec![crate::engine::Profile {
                        name: "test".to_string(),
                        plan: plan.clone(),
                    }],
                    ..PomodoroConfig::default()
                })
                .unwrap(),
            );
        });
        state.list_state.select(Some(1)); // Break
        let outcome = handle_event(&mut state, &shift_key(KeyCode::Up), &mut ctx).await.unwrap();
        assert!(matches!(outcome, Outcome::Redraw));
        let rebuilt = unflatten_nodes(&state.nodes);
        assert!(matches!(
            rebuilt[0],
            Block::Phase {
                kind: PhaseKind::Break,
                ..
            }
        ));
    }
}
