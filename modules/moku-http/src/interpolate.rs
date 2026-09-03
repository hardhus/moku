use std::collections::HashMap;

/// Replaces every `{{name}}` occurrence in `text` with `vars[name]`.
/// Unresolved names are left as literal `{{name}}` rather than silently
/// disappearing — a typo or missing variable stays visible in the output.
pub fn interpolate(text: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = after[..end].trim();
                match vars.get(name) {
                    Some(value) => out.push_str(value),
                    None => {
                        out.push_str("{{");
                        out.push_str(&after[..end]);
                        out.push_str("}}");
                    }
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Recursively interpolates every string leaf of a `toml::Value` tree
/// (used for `body_json`, which can be an arbitrarily nested table).
pub fn interpolate_toml_value(value: &toml::Value, vars: &HashMap<String, String>) -> toml::Value {
    match value {
        toml::Value::String(s) => toml::Value::String(interpolate(s, vars)),
        toml::Value::Array(items) => toml::Value::Array(items.iter().map(|v| interpolate_toml_value(v, vars)).collect()),
        toml::Value::Table(map) => {
            let mut out = toml::map::Map::new();
            for (k, v) in map {
                out.insert(k.clone(), interpolate_toml_value(v, vars));
            }
            toml::Value::Table(out)
        }
        other => other.clone(),
    }
}

/// Finds every distinct `secrets.NAME` reference inside `{{...}}` spans in
/// `text` (typically a whole collection file's raw source), returning just
/// the bare names (without the `secrets.` prefix).
pub fn find_secret_refs(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        let name = after[..end].trim();
        if let Some(secret_name) = name.strip_prefix("secrets.") {
            let secret_name = secret_name.trim().to_string();
            if !names.contains(&secret_name) {
                names.push(secret_name);
            }
        }
        rest = &after[end + 2..];
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn test_interpolate_single_variable() {
        let v = vars(&[("name", "world")]);
        assert_eq!(interpolate("hello {{name}}", &v), "hello world");
    }

    #[test]
    fn test_interpolate_multiple_variables() {
        let v = vars(&[("a", "1"), ("b", "2")]);
        assert_eq!(interpolate("{{a}}-{{b}}", &v), "1-2");
    }

    #[test]
    fn test_interpolate_leaves_unknown_variable_literal() {
        let v = vars(&[]);
        assert_eq!(interpolate("{{missing}}", &v), "{{missing}}");
    }

    #[test]
    fn test_interpolate_no_placeholders() {
        let v = vars(&[]);
        assert_eq!(interpolate("plain text", &v), "plain text");
    }

    #[test]
    fn test_interpolate_trims_whitespace_in_braces() {
        let v = vars(&[("x", "y")]);
        assert_eq!(interpolate("{{ x }}", &v), "y");
    }

    #[test]
    fn test_find_secret_refs_extracts_bare_names() {
        let names = find_secret_refs("token {{secrets.api_key}} and {{secrets.other}}");
        assert_eq!(names, vec!["api_key".to_string(), "other".to_string()]);
    }

    #[test]
    fn test_find_secret_refs_ignores_non_secret_vars() {
        let names = find_secret_refs("{{base_url}}/{{secrets.token}}");
        assert_eq!(names, vec!["token".to_string()]);
    }

    #[test]
    fn test_find_secret_refs_deduplicates() {
        let names = find_secret_refs("{{secrets.x}} {{secrets.x}}");
        assert_eq!(names, vec!["x".to_string()]);
    }

    #[test]
    fn test_interpolate_toml_value_nested() {
        let v = vars(&[("user", "octocat")]);
        let value: toml::Value = toml::from_str(r#"name = "{{user}}""#).unwrap();
        let interpolated = interpolate_toml_value(&value, &v);
        assert_eq!(interpolated.get("name").unwrap().as_str(), Some("octocat"));
    }
}
