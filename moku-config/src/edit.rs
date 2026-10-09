//! Leaf-level config.toml edits. A config change is computed as the
//! difference between the serialized config before and after it, then
//! applied key by key to the `toml_edit` document parsed from the file, so
//! everything the change didn't touch (comments, key order, formatting,
//! keys moku doesn't know about) stays exactly as the user wrote it.

use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, TableLike, Value};

/// One change at a dotted key path: `Some` sets the value, `None` removes
/// the key.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Change {
    pub path: Vec<String>,
    pub value: Option<toml::Value>,
}

/// The leaf-level changes that turn `before` into `after`. Tables are
/// compared key by key (recursively); any other value, arrays included, is
/// compared and replaced as a whole.
pub(crate) fn diff(before: &toml::Table, after: &toml::Table) -> Vec<Change> {
    let mut out = Vec::new();
    diff_into(&mut Vec::new(), before, after, &mut out);
    out
}

fn diff_into(
    prefix: &mut Vec<String>,
    before: &toml::Table,
    after: &toml::Table,
    out: &mut Vec<Change>,
) {
    for (key, new) in after {
        prefix.push(key.clone());
        match (before.get(key), new) {
            (Some(toml::Value::Table(b)), toml::Value::Table(a)) => diff_into(prefix, b, a, out),
            (Some(old), new) if old == new => {}
            (_, new) => out.push(Change {
                path: prefix.clone(),
                value: Some(new.clone()),
            }),
        }
        prefix.pop();
    }
    for key in before.keys() {
        if !after.contains_key(key) {
            let mut path = prefix.clone();
            path.push(key.clone());
            out.push(Change { path, value: None });
        }
    }
}

/// Applies `changes` to the document, creating any missing parent tables.
pub(crate) fn apply(doc: &mut DocumentMut, changes: &[Change]) {
    for change in changes {
        apply_one(doc.as_table_mut(), false, &change.path, change.value.as_ref());
    }
}

/// `inline`: whether `table` is an inline table (`{ .. }`), whose children
/// must be inline too.
fn apply_one(
    table: &mut dyn TableLike,
    inline: bool,
    path: &[String],
    value: Option<&toml::Value>,
) {
    let Some((key, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        match value {
            None => {
                table.remove(key);
            }
            Some(v) => set_leaf(table, inline, key, v),
        }
        return;
    }
    let needs_new = !matches!(table.get(key), Some(item) if item.is_table_like());
    if needs_new {
        if value.is_none() {
            // Removing something under a parent that doesn't exist: done.
            return;
        }
        let item = if inline {
            Item::Value(Value::InlineTable(InlineTable::new()))
        } else {
            let mut t = Table::new();
            // `[modules.pomodoro]` should not also print an empty `[modules]`.
            t.set_implicit(true);
            Item::Table(t)
        };
        table.insert(key, item);
    }
    let child_inline = inline || matches!(table.get(key), Some(Item::Value(_)));
    let Some(child) = table.get_mut(key).and_then(Item::as_table_like_mut) else {
        return;
    };
    apply_one(child, child_inline, rest, value);
}

fn set_leaf(table: &mut dyn TableLike, inline: bool, key: &str, v: &toml::Value) {
    // A value that is only being changed is replaced in place, so the key
    // keeps its comments above it and the value keeps its trailing comment.
    if let Some(Item::Value(old)) = table.get_mut(key) {
        let decor = old.decor().clone();
        *old = to_value(v);
        *old.decor_mut() = decor;
        return;
    }
    let item = if inline { Item::Value(to_value(v)) } else { to_item(v) };
    table.insert(key, item);
}

/// A value as a standalone document item: tables become `[table]`
/// sections and arrays of tables become `[[array]]` entries, the way a
/// person would write them.
fn to_item(v: &toml::Value) -> Item {
    match v {
        toml::Value::Table(t) => Item::Table(to_table(t)),
        toml::Value::Array(items)
            if !items.is_empty() && items.iter().all(|i| matches!(i, toml::Value::Table(_))) =>
        {
            let mut aot = ArrayOfTables::new();
            for i in items {
                if let toml::Value::Table(t) = i {
                    aot.push(to_table(t));
                }
            }
            Item::ArrayOfTables(aot)
        }
        other => Item::Value(to_value(other)),
    }
}

fn to_table(t: &toml::Table) -> Table {
    let mut out = Table::new();
    // Scalars first, then sub-tables, so the table's own keys are written
    // under its header rather than after a nested `[sub.table]`.
    for (k, v) in t {
        if !matches!(v, toml::Value::Table(_)) {
            out.insert(k, to_item(v));
        }
    }
    for (k, v) in t {
        if let toml::Value::Table(sub) = v {
            out.insert(k, Item::Table(to_table(sub)));
        }
    }
    out
}

/// A value in inline form (`{ a = 1 }`, `[1, 2]`), for inside an inline
/// table or array.
fn to_value(v: &toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::from(s.as_str()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => Value::from(*f),
        toml::Value::Boolean(b) => Value::from(*b),
        toml::Value::Datetime(dt) => dt
            .to_string()
            .parse::<Value>()
            .unwrap_or_else(|_| Value::from(dt.to_string())),
        toml::Value::Array(items) => {
            let mut a = Array::new();
            for i in items {
                a.push(to_value(i));
            }
            Value::Array(a)
        }
        toml::Value::Table(t) => {
            let mut it = InlineTable::new();
            for (k, v) in t {
                it.insert(k, to_value(v));
            }
            Value::InlineTable(it)
        }
    }
}

/// Applies `changes` (paths relative to `table`) to a plain `toml::Table`
/// — used to merge a module's typed settings back into its raw table.
pub(crate) fn apply_to_table(table: &mut toml::Table, changes: &[Change]) {
    for change in changes {
        set_in_table(table, &change.path, change.value.as_ref());
    }
}

fn set_in_table(table: &mut toml::Table, path: &[String], value: Option<&toml::Value>) {
    let Some((key, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        match value {
            Some(v) => {
                table.insert(key.clone(), v.clone());
            }
            None => {
                table.remove(key);
            }
        }
        return;
    }
    let entry = table
        .entry(key.clone())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !entry.is_table() {
        *entry = toml::Value::Table(toml::Table::new());
    }
    if let toml::Value::Table(child) = entry {
        set_in_table(child, rest, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(src: &str) -> toml::Table {
        toml::from_str(src).unwrap()
    }

    #[test]
    fn test_diff_reports_only_changed_leaves() {
        let before = table("a = 1\n[s]\nx = 1\ny = 2\n");
        let after = table("a = 1\n[s]\nx = 1\ny = 3\nz = 4\n");
        let changes = diff(&before, &after);
        assert_eq!(
            changes,
            vec![
                Change {
                    path: vec!["s".into(), "y".into()],
                    value: Some(toml::Value::Integer(3)),
                },
                Change {
                    path: vec!["s".into(), "z".into()],
                    value: Some(toml::Value::Integer(4)),
                },
            ]
        );
    }

    #[test]
    fn test_apply_keeps_comments_and_untouched_keys() {
        let src = "# top comment\n[storage]\n# why 300\nauto_lock_timeout = 300 # seconds\ndefault_encrypt = true\nmystery = \"kept\"\n";
        let mut doc: DocumentMut = src.parse().unwrap();
        apply(
            &mut doc,
            &[Change {
                path: vec!["storage".into(), "auto_lock_timeout".into()],
                value: Some(toml::Value::Integer(600)),
            }],
        );
        assert_eq!(doc.to_string(), src.replace("= 300", "= 600"));
    }

    #[test]
    fn test_apply_creates_a_new_nested_table_as_a_section() {
        let mut doc: DocumentMut = "[general]\ntheme = \"system\"\n".parse().unwrap();
        apply(
            &mut doc,
            &[Change {
                path: vec!["modules".into(), "launcher".into(), "order".into()],
                value: Some(toml::Value::Array(vec![toml::Value::String("todo".into())])),
            }],
        );
        let out = doc.to_string();
        assert!(out.contains("[modules.launcher]\norder = [\"todo\"]"), "{out}");
        assert!(!out.contains("[modules]\n"), "{out}");
    }

    #[test]
    fn test_apply_removes_a_key() {
        let mut doc: DocumentMut = "[modules.rss]\nencrypt = true\nother = 1\n".parse().unwrap();
        apply(
            &mut doc,
            &[Change {
                path: vec!["modules".into(), "rss".into(), "encrypt".into()],
                value: None,
            }],
        );
        assert_eq!(doc.to_string(), "[modules.rss]\nother = 1\n");
    }
}
