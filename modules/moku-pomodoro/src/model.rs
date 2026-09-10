//! Pure TUI-side data: the transient simple-mode form (never persisted
//! itself — only the `Plan` it generates via `engine::build_plan_from_simple_form`
//! is), the settings-form focus/field shape, the per-module keybinding
//! override struct (mirrors `moku-todo`'s `TodoKeyConfig`), and the tree
//! editor's flat node model (mirrors `modules/moku-todo/src/model.rs`'s
//! `Task`/`ViewRow`/`build_view` shape, applied to a `Block` tree instead
//! of a task list).

use std::collections::{HashMap, HashSet};

use moku_pomodoro_daemon::{Block, PhaseKind, Plan, RepeatCount};
use serde::Deserialize;

#[derive(Deserialize, Default)]
pub struct PomodoroKeyConfig {
    pub keys: HashMap<String, String>,
}

/// The "basic" settings shape: fills in a handful of numeric/boolean
/// fields and lets `engine::build_plan_from_simple_form` expand them into
/// one of the canonical `Block` tree shapes. Deliberately supports "no
/// long break at all, just loop work/break forever" as its own default —
/// the simplest real-world case shouldn't need to think about the concept
/// of a long break just to turn it off.
#[derive(Debug, Clone, PartialEq)]
pub struct SimplePresetForm {
    pub work_minutes: u32,
    pub break_minutes: u32,
    pub enable_long_break: bool,
    pub cycles_before_long_break: u32,
    pub long_break_minutes: u32,
    pub infinite: bool,
    pub total_repeats: u32,
}

impl Default for SimplePresetForm {
    fn default() -> Self {
        Self {
            work_minutes: 45,
            break_minutes: 15,
            enable_long_break: false,
            cycles_before_long_break: 3,
            long_break_minutes: 30,
            infinite: true,
            total_repeats: 4,
        }
    }
}

/// Which settings-form field currently has keyboard focus. `Tab` cycles
/// through only the fields actually visible for the current toggle state
/// (`CyclesBeforeLongBreak`/`LongBreakMinutes` only exist while
/// `enable_long_break`; `TotalRepeats` only while `!infinite`) — same
/// conditional-field shape as `moku-volume-daemon`'s `CreateForm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsField {
    WorkMinutes,
    BreakMinutes,
    EnableLongBreak,
    CyclesBeforeLongBreak,
    LongBreakMinutes,
    Infinite,
    TotalRepeats,
}

impl SettingsField {
    /// The next field to focus on `Down`, given which optional fields are
    /// currently visible. Movement in this form (and every other view in
    /// this module) goes through `Command::Up`/`Down` — resolved from
    /// `k`/`j`/arrows, matching this project's list-navigation
    /// convention — never `Tab`.
    pub fn next(self, enable_long_break: bool, infinite: bool) -> SettingsField {
        use SettingsField::*;
        match self {
            WorkMinutes => BreakMinutes,
            BreakMinutes => EnableLongBreak,
            EnableLongBreak => {
                if enable_long_break {
                    CyclesBeforeLongBreak
                } else {
                    Infinite
                }
            }
            CyclesBeforeLongBreak => LongBreakMinutes,
            LongBreakMinutes => Infinite,
            Infinite => {
                if infinite {
                    WorkMinutes
                } else {
                    TotalRepeats
                }
            }
            TotalRepeats => WorkMinutes,
        }
    }

    /// The exact inverse of `next` — focus on `Up`.
    pub fn prev(self, enable_long_break: bool, infinite: bool) -> SettingsField {
        use SettingsField::*;
        match self {
            WorkMinutes => {
                if infinite {
                    Infinite
                } else {
                    TotalRepeats
                }
            }
            BreakMinutes => WorkMinutes,
            EnableLongBreak => BreakMinutes,
            CyclesBeforeLongBreak => EnableLongBreak,
            LongBreakMinutes => CyclesBeforeLongBreak,
            Infinite => {
                if enable_long_break {
                    LongBreakMinutes
                } else {
                    EnableLongBreak
                }
            }
            TotalRepeats => Infinite,
        }
    }
}

/// Text-buffer-backed editable copy of `SimplePresetForm` — numeric
/// fields are typed as strings (so a field can be empty/mid-edit) and only
/// parsed into `SimplePresetForm` on submit.
pub struct SettingsFormState {
    pub work_minutes: String,
    pub break_minutes: String,
    pub enable_long_break: bool,
    pub cycles_before_long_break: String,
    pub long_break_minutes: String,
    pub infinite: bool,
    pub total_repeats: String,
    pub focus: SettingsField,
    pub error: Option<String>,
}

impl SettingsFormState {
    pub fn from_form(form: &SimplePresetForm) -> Self {
        Self {
            work_minutes: form.work_minutes.to_string(),
            break_minutes: form.break_minutes.to_string(),
            enable_long_break: form.enable_long_break,
            cycles_before_long_break: form.cycles_before_long_break.to_string(),
            long_break_minutes: form.long_break_minutes.to_string(),
            infinite: form.infinite,
            total_repeats: form.total_repeats.to_string(),
            focus: SettingsField::WorkMinutes,
            error: None,
        }
    }

    /// Parses every field back into a `SimplePresetForm`, or a
    /// human-readable error naming the first invalid field.
    pub fn try_build(&self) -> Result<SimplePresetForm, String> {
        fn parse_positive(label: &str, s: &str) -> Result<u32, String> {
            let n: u32 = s
                .trim()
                .parse()
                .map_err(|_| format!("{label} must be a whole number"))?;
            if n == 0 {
                return Err(format!("{label} must be at least 1"));
            }
            Ok(n)
        }

        Ok(SimplePresetForm {
            work_minutes: parse_positive("Work minutes", &self.work_minutes)?,
            break_minutes: parse_positive("Break minutes", &self.break_minutes)?,
            enable_long_break: self.enable_long_break,
            cycles_before_long_break: if self.enable_long_break {
                parse_positive("Cycles before long break", &self.cycles_before_long_break)?
            } else {
                1
            },
            long_break_minutes: if self.enable_long_break {
                parse_positive("Long break minutes", &self.long_break_minutes)?
            } else {
                0
            },
            infinite: self.infinite,
            total_repeats: if self.infinite {
                1
            } else {
                parse_positive("Total repeats", &self.total_repeats)?
            },
        })
    }
}

// --- Tree editor: flat node model over a `Block` tree ---

/// One node of the tree editor's flat, editable representation of a
/// `Plan`. Storage (`config.toml`, the daemon) never sees this type —
/// `flatten_plan`/`unflatten_nodes` convert at the editor's boundary
/// only. `id`/`parent_id` are ephemeral, valid only within one editing
/// session (a fresh `flatten_plan` call assigns new ones); nothing
/// persists them.
#[derive(Debug, Clone, PartialEq)]
pub struct EditNode {
    pub id: String,
    pub parent_id: Option<String>,
    pub kind: EditNodeKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditNodeKind {
    Phase {
        phase_kind: PhaseKind,
        minutes: u32,
        label: Option<String>,
    },
    Repeat {
        count: RepeatCount,
    },
}

/// `Plan` -> flat editor nodes, depth-first, fresh incrementing ids.
pub fn flatten_plan(plan: &Plan) -> Vec<EditNode> {
    let mut out = Vec::new();
    let mut counter: u64 = 0;

    fn walk(blocks: &[Block], parent_id: Option<&str>, counter: &mut u64, out: &mut Vec<EditNode>) {
        for b in blocks {
            *counter += 1;
            let id = counter.to_string();
            match b {
                Block::Phase {
                    kind,
                    minutes,
                    label,
                } => {
                    out.push(EditNode {
                        id,
                        parent_id: parent_id.map(str::to_string),
                        kind: EditNodeKind::Phase {
                            phase_kind: *kind,
                            minutes: *minutes,
                            label: label.clone(),
                        },
                    });
                }
                Block::Repeat { count, blocks } => {
                    out.push(EditNode {
                        id: id.clone(),
                        parent_id: parent_id.map(str::to_string),
                        kind: EditNodeKind::Repeat { count: *count },
                    });
                    walk(blocks, Some(&id), counter, out);
                }
            }
        }
    }
    walk(plan, None, &mut counter, &mut out);
    out
}

/// Flat editor nodes -> `Plan`. Sibling order under a given parent is the
/// order those nodes appear in `nodes` (preserved via a single pre-
/// indexed parent -> children-indices pass, same O(n) shape as
/// `moku-todo`'s `build_view`, rather than an O(n^2) rescan per level).
pub fn unflatten_nodes(nodes: &[EditNode]) -> Plan {
    let children = index_children(nodes);

    fn build(parent: Option<&str>, nodes: &[EditNode], children: &HashMap<Option<String>, Vec<usize>>) -> Plan {
        let Some(idxs) = children.get(&parent.map(str::to_string)) else {
            return Vec::new();
        };
        idxs.iter()
            .map(|&i| {
                let n = &nodes[i];
                match &n.kind {
                    EditNodeKind::Phase {
                        phase_kind,
                        minutes,
                        label,
                    } => Block::Phase {
                        kind: *phase_kind,
                        minutes: *minutes,
                        label: label.clone(),
                    },
                    EditNodeKind::Repeat { count } => Block::Repeat {
                        count: *count,
                        blocks: build(Some(n.id.as_str()), nodes, children),
                    },
                }
            })
            .collect()
    }
    build(None, nodes, &children)
}

fn index_children(nodes: &[EditNode]) -> HashMap<Option<String>, Vec<usize>> {
    let mut children: HashMap<Option<String>, Vec<usize>> = HashMap::new();
    for (i, n) in nodes.iter().enumerate() {
        children.entry(n.parent_id.clone()).or_default().push(i);
    }
    children
}

/// One row of the tree editor's flattened, collapse-aware display —
/// `index` into `nodes`, `depth` for indentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeViewRow {
    pub index: usize,
    pub depth: usize,
}

/// Depth-first flattening of `nodes` for display, skipping the subtree of
/// any id present in `collapsed` — same shape as `moku-todo`'s
/// `build_view`, generalized to arbitrary depth (a `Repeat` can nest
/// inside a `Repeat` arbitrarily, unlike Todo's fixed parent/child
/// levels).
pub fn build_tree_view(nodes: &[EditNode], collapsed: &HashSet<String>) -> Vec<TreeViewRow> {
    let children = index_children(nodes);
    let mut out = Vec::new();

    fn walk(
        parent: Option<&str>,
        depth: usize,
        nodes: &[EditNode],
        children: &HashMap<Option<String>, Vec<usize>>,
        collapsed: &HashSet<String>,
        out: &mut Vec<TreeViewRow>,
    ) {
        let Some(idxs) = children.get(&parent.map(str::to_string)) else {
            return;
        };
        for &i in idxs {
            out.push(TreeViewRow { index: i, depth });
            let n = &nodes[i];
            if !collapsed.contains(&n.id) {
                walk(Some(n.id.as_str()), depth + 1, nodes, children, collapsed, out);
            }
        }
    }
    walk(None, 0, nodes, &children, collapsed, &mut out);
    out
}

pub fn has_children(nodes: &[EditNode], id: &str) -> bool {
    nodes.iter().any(|n| n.parent_id.as_deref() == Some(id))
}

/// `id` and every descendant's id, depth-first — used for cascading
/// delete (mirrors `moku-todo::model::collect_subtree_ids` exactly).
pub fn collect_subtree_ids(nodes: &[EditNode], id: &str, out: &mut Vec<String>) {
    out.push(id.to_string());
    for n in nodes {
        if n.parent_id.as_deref() == Some(id) {
            collect_subtree_ids(nodes, &n.id, out);
        }
    }
}

/// The next unused node id, for inserting a fresh node into an
/// already-flattened, possibly-edited list (ids are plain incrementing
/// counters — see `flatten_plan` — so this is just "max existing + 1",
/// tolerant of ids `flatten_plan` never assigned in this exact shape).
pub fn next_node_id(nodes: &[EditNode]) -> String {
    let max = nodes
        .iter()
        .filter_map(|n| n.id.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    (max + 1).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tab_order_skips_hidden_fields_when_long_break_off_and_infinite() {
        let f = SettingsField::EnableLongBreak;
        assert_eq!(
            f.next(false, true),
            SettingsField::Infinite,
            "long break off should skip straight to Infinite"
        );
        assert_eq!(
            SettingsField::Infinite.next(false, true),
            SettingsField::WorkMinutes,
            "infinite on should skip TotalRepeats and wrap"
        );
    }

    #[test]
    fn test_tab_order_visits_all_fields_when_long_break_on_and_finite() {
        let mut f = SettingsField::WorkMinutes;
        let mut seen = vec![f];
        // 7 distinct fields are visible in this mode (WorkMinutes,
        // BreakMinutes, EnableLongBreak, CyclesBeforeLongBreak,
        // LongBreakMinutes, Infinite, TotalRepeats) — 7 `next()` calls are
        // needed to cycle all the way back around to the start.
        for _ in 0..7 {
            f = f.next(true, false);
            seen.push(f);
        }
        assert!(seen.contains(&SettingsField::CyclesBeforeLongBreak));
        assert!(seen.contains(&SettingsField::LongBreakMinutes));
        assert!(seen.contains(&SettingsField::TotalRepeats));
        assert_eq!(seen.last(), Some(&SettingsField::WorkMinutes));
    }

    #[test]
    fn test_try_build_rejects_non_numeric_field() {
        let mut state = SettingsFormState::from_form(&SimplePresetForm::default());
        state.work_minutes = "abc".to_string();
        assert!(state.try_build().is_err());
    }

    #[test]
    fn test_try_build_rejects_zero() {
        let mut state = SettingsFormState::from_form(&SimplePresetForm::default());
        state.work_minutes = "0".to_string();
        assert!(state.try_build().is_err());
    }

    #[test]
    fn test_try_build_round_trips_a_valid_form() {
        let original = SimplePresetForm {
            work_minutes: 20,
            break_minutes: 5,
            enable_long_break: true,
            cycles_before_long_break: 4,
            long_break_minutes: 25,
            infinite: false,
            total_repeats: 6,
        };
        let state = SettingsFormState::from_form(&original);
        let rebuilt = state.try_build().unwrap();
        assert_eq!(rebuilt, original);
    }

    // --- Tree editor model ---

    fn work(minutes: u32) -> Block {
        Block::Phase {
            kind: PhaseKind::Work,
            minutes,
            label: None,
        }
    }
    fn brk(minutes: u32) -> Block {
        Block::Phase {
            kind: PhaseKind::Break,
            minutes,
            label: None,
        }
    }
    fn long_break(minutes: u32) -> Block {
        Block::Phase {
            kind: PhaseKind::LongBreak,
            minutes,
            label: None,
        }
    }
    fn repeat(count: RepeatCount, blocks: Vec<Block>) -> Block {
        Block::Repeat { count, blocks }
    }

    fn round_trip(plan: &Plan) -> Plan {
        unflatten_nodes(&flatten_plan(plan))
    }

    #[test]
    fn test_flatten_unflatten_round_trips_simplest_shape() {
        let plan: Plan = vec![repeat(RepeatCount::Infinite, vec![work(45), brk(15)])];
        assert_eq!(round_trip(&plan), plan);
    }

    #[test]
    fn test_flatten_unflatten_round_trips_standard_nested_shape() {
        let plan: Plan = vec![repeat(
            RepeatCount::Infinite,
            vec![
                repeat(RepeatCount::Finite(3), vec![work(45), brk(15)]),
                long_break(30),
            ],
        )];
        assert_eq!(round_trip(&plan), plan);
    }

    #[test]
    fn test_flatten_unflatten_round_trips_doubly_nested_shape() {
        let plan: Plan = vec![
            repeat(
                RepeatCount::Finite(2),
                vec![
                    repeat(RepeatCount::Finite(3), vec![work(45), brk(15)]),
                    long_break(30),
                ],
            ),
            long_break(60),
        ];
        assert_eq!(round_trip(&plan), plan);
    }

    #[test]
    fn test_flatten_unflatten_round_trips_fully_explicit_decreasing_break_shape() {
        let mut plan: Plan = Vec::new();
        for i in 0..8u32 {
            plan.push(work(40));
            plan.push(brk(20 - i * 2));
        }
        assert_eq!(round_trip(&plan), plan);
    }

    #[test]
    fn test_flatten_preserves_sibling_order() {
        let plan: Plan = vec![work(10), brk(5), long_break(20)];
        let nodes = flatten_plan(&plan);
        let view = build_tree_view(&nodes, &HashSet::new());
        let ordered_minutes: Vec<u32> = view
            .iter()
            .map(|row| match &nodes[row.index].kind {
                EditNodeKind::Phase { minutes, .. } => *minutes,
                EditNodeKind::Repeat { .. } => panic!("no repeats in this plan"),
            })
            .collect();
        assert_eq!(ordered_minutes, vec![10, 5, 20]);
    }

    #[test]
    fn test_build_tree_view_skips_collapsed_subtree() {
        let plan: Plan = vec![repeat(RepeatCount::Finite(3), vec![work(45), brk(15)])];
        let nodes = flatten_plan(&plan);
        let repeat_id = nodes[0].id.clone();
        assert!(has_children(&nodes, &repeat_id));

        let full_view = build_tree_view(&nodes, &HashSet::new());
        assert_eq!(full_view.len(), 3, "repeat + its 2 children");

        let mut collapsed = HashSet::new();
        collapsed.insert(repeat_id);
        let collapsed_view = build_tree_view(&nodes, &collapsed);
        assert_eq!(collapsed_view.len(), 1, "children hidden while collapsed");
    }

    #[test]
    fn test_build_tree_view_depth_increases_for_nested_repeats() {
        let plan: Plan = vec![repeat(
            RepeatCount::Infinite,
            vec![repeat(RepeatCount::Finite(3), vec![work(45)])],
        )];
        let nodes = flatten_plan(&plan);
        let view = build_tree_view(&nodes, &HashSet::new());
        assert_eq!(view[0].depth, 0, "outer repeat");
        assert_eq!(view[1].depth, 1, "inner repeat");
        assert_eq!(view[2].depth, 2, "work phase");
    }

    #[test]
    fn test_collect_subtree_ids_gets_node_and_all_descendants_only() {
        let plan: Plan = vec![
            repeat(RepeatCount::Finite(1), vec![work(45), brk(15)]),
            long_break(30),
        ];
        let nodes = flatten_plan(&plan);
        let repeat_id = nodes[0].id.clone();
        let long_break_id = nodes[3].id.clone();

        let mut doomed = Vec::new();
        collect_subtree_ids(&nodes, &repeat_id, &mut doomed);
        assert_eq!(doomed.len(), 3, "the repeat plus its 2 children");
        assert!(!doomed.contains(&long_break_id));
    }

    #[test]
    fn test_next_node_id_is_higher_than_every_existing_id() {
        let plan: Plan = vec![work(10), brk(5)];
        let nodes = flatten_plan(&plan);
        let next = next_node_id(&nodes);
        let existing_max: u64 = nodes.iter().filter_map(|n| n.id.parse().ok()).max().unwrap();
        assert_eq!(next.parse::<u64>().unwrap(), existing_max + 1);
    }

    #[test]
    fn test_deleting_a_repeats_subtree_via_unflatten_removes_all_its_phases() {
        let plan: Plan = vec![
            repeat(RepeatCount::Finite(1), vec![work(45), brk(15)]),
            long_break(30),
        ];
        let mut nodes = flatten_plan(&plan);
        let repeat_id = nodes[0].id.clone();
        let mut doomed = Vec::new();
        collect_subtree_ids(&nodes, &repeat_id, &mut doomed);
        let doomed: HashSet<String> = doomed.into_iter().collect();
        nodes.retain(|n| !doomed.contains(&n.id));

        let rebuilt = unflatten_nodes(&nodes);
        assert_eq!(rebuilt, vec![long_break(30)]);
    }
}
