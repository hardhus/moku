use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

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
    pub fn new(title: String, parent_id: Option<String>) -> Self {
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

pub(crate) fn now_secs() -> u64 {
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

/// One row of the flattened tree view: which `items` index it points at,
/// and how deeply nested it is (0 = root task).
pub(crate) struct ViewRow {
    pub(crate) index: usize,
    pub(crate) depth: usize,
}

/// Flattens `items` (a flat `Vec<Task>` linked only by `parent_id`) into
/// display order: each root task, immediately followed by its children
/// (recursively, same rule) unless its id is in `collapsed`. Sibling
/// order within a parent is just their relative order in `items` — same
/// as today's flat list already relies on `Vec` order for top-level
/// ordering, so no separate "order" field is needed.
pub(crate) fn build_view(items: &[Task], collapsed: &HashSet<String>) -> Vec<ViewRow> {
    // Pre-index parent -> children once (O(n)) instead of having
    // `walk_children` rescan the whole slice at every recursion level
    // (which was O(n^2) overall for a tree of n tasks).
    let mut children_of: HashMap<Option<&str>, Vec<usize>> = HashMap::new();
    for (index, task) in items.iter().enumerate() {
        children_of
            .entry(task.parent_id.as_deref())
            .or_default()
            .push(index);
    }

    let mut view = Vec::with_capacity(items.len());
    walk_children(items, &children_of, None, 0, collapsed, &mut view);
    view
}

fn walk_children<'a>(
    items: &'a [Task],
    children_of: &HashMap<Option<&'a str>, Vec<usize>>,
    parent_id: Option<&'a str>,
    depth: usize,
    collapsed: &HashSet<String>,
    view: &mut Vec<ViewRow>,
) {
    let Some(indices) = children_of.get(&parent_id) else {
        return;
    };
    for &index in indices {
        let task = &items[index];
        view.push(ViewRow { index, depth });
        if !collapsed.contains(&task.id) {
            walk_children(
                items,
                children_of,
                Some(task.id.as_str()),
                depth + 1,
                collapsed,
                view,
            );
        }
    }
}

pub(crate) fn has_children(items: &[Task], id: &str) -> bool {
    items.iter().any(|t| t.parent_id.as_deref() == Some(id))
}

/// Collects `id` and every descendant id (recursively) — the full
/// subtree a cascading delete needs to remove.
pub(crate) fn collect_subtree_ids(items: &[Task], id: &str, out: &mut Vec<String>) {
    out.push(id.to_string());
    for task in items {
        if task.parent_id.as_deref() == Some(id) {
            collect_subtree_ids(items, &task.id, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
