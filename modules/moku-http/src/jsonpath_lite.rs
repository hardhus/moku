//! A deliberately minimal `$.a.b[0].c`-style JSON path evaluator — not a
//! full JSONPath implementation (no wildcards/filters/slices), chosen over
//! pulling in a JSONPath crate since it covers the large majority of real
//! REST API response shapes (plan's explicit scope decision).

use serde_json::Value;

/// Evaluates `path` (e.g. `"$.data.items[0].id"`, or just `"$"` for the
/// whole document) against `value`. Returns `None` if any segment is
/// missing or the wrong shape (object vs array) to continue.
pub fn eval<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let path = path.trim();
    let rest = path.strip_prefix('$')?;
    let rest = rest.strip_prefix('.').unwrap_or(rest);
    if rest.is_empty() {
        return Some(value);
    }

    let mut current = value;
    for segment in rest.split('.') {
        if segment.is_empty() {
            return None;
        }
        let (key, indices) = parse_segment(segment);
        if !key.is_empty() {
            current = current.get(key)?;
        }
        for idx in indices {
            current = current.get(idx)?;
        }
    }
    Some(current)
}

/// Splits `"items[0][1]"` into (`"items"`, `[0, 1]`) and `"[2]"` into
/// (`""`, `[2]`).
fn parse_segment(segment: &str) -> (&str, Vec<usize>) {
    let Some(bracket_start) = segment.find('[') else {
        return (segment, Vec::new());
    };
    let key = &segment[..bracket_start];
    let mut indices = Vec::new();
    let mut rest = &segment[bracket_start..];
    while let Some(open) = rest.find('[') {
        let Some(close_rel) = rest[open..].find(']') else { break };
        let close = open + close_rel;
        if let Ok(n) = rest[open + 1..close].parse::<usize>() {
            indices.push(n);
        }
        rest = &rest[close + 1..];
    }
    (key, indices)
}

/// Renders a JSON value for display/comparison/variable-extraction:
/// strings unwrap to their raw text, everything else becomes its compact
/// JSON form.
pub fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_root_path() {
        let v = json!({"a": 1});
        assert_eq!(eval(&v, "$"), Some(&v));
    }

    #[test]
    fn test_nested_object_path() {
        let v = json!({"a": {"b": {"c": 42}}});
        assert_eq!(eval(&v, "$.a.b.c"), Some(&json!(42)));
    }

    #[test]
    fn test_array_index_path() {
        let v = json!({"items": [10, 20, 30]});
        assert_eq!(eval(&v, "$.items[1]"), Some(&json!(20)));
    }

    #[test]
    fn test_array_of_objects_path() {
        let v = json!({"items": [{"id": "a"}, {"id": "b"}]});
        assert_eq!(eval(&v, "$.items[1].id"), Some(&json!("b")));
    }

    #[test]
    fn test_missing_key_returns_none() {
        let v = json!({"a": 1});
        assert_eq!(eval(&v, "$.missing"), None);
    }

    #[test]
    fn test_out_of_bounds_index_returns_none() {
        let v = json!({"items": [1, 2]});
        assert_eq!(eval(&v, "$.items[5]"), None);
    }

    #[test]
    fn test_value_to_string_unwraps_strings() {
        assert_eq!(value_to_string(&json!("hello")), "hello");
    }

    #[test]
    fn test_value_to_string_stringifies_others() {
        assert_eq!(value_to_string(&json!(42)), "42");
        assert_eq!(value_to_string(&json!(true)), "true");
        assert_eq!(value_to_string(&json!([1, 2])), "[1,2]");
    }
}
