use std::collections::HashMap;

use serde::Deserialize;

#[derive(Deserialize, Default, Debug)]
#[serde(default)]
pub struct Collection {
    pub variables: HashMap<String, String>,
    pub requests: Vec<RequestDef>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(default)]
pub struct RequestDef {
    pub name: String,
    pub method: String,
    pub url: String,
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
    pub body_json: Option<toml::Value>,
    pub assertions: Vec<Assertion>,
    pub extract: HashMap<String, String>,
}

impl Default for RequestDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            method: "GET".to_string(),
            url: String::new(),
            headers: HashMap::new(),
            body: None,
            body_json: None,
            assertions: Vec::new(),
            extract: HashMap::new(),
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Assertion {
    Status { equals: u16 },
    Header { name: String, equals: String },
    JsonPath { path: String, equals: String },
    BodyContains { text: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_collection() {
        let toml_src = r#"
[[requests]]
name = "ping"
method = "GET"
url = "https://example.com"
"#;
        let c: Collection = toml::from_str(toml_src).unwrap();
        assert_eq!(c.requests.len(), 1);
        assert_eq!(c.requests[0].name, "ping");
        assert_eq!(c.requests[0].method, "GET");
    }

    #[test]
    fn test_parse_full_collection_with_assertions_and_extract() {
        let toml_src = r#"
[variables]
base_url = "https://example.com"

[[requests]]
name = "login"
method = "POST"
url = "{{base_url}}/login"
headers = { "Content-Type" = "application/json" }

[[requests.assertions]]
type = "status"
equals = 200

[requests.extract]
token = "$.token"
"#;
        let c: Collection = toml::from_str(toml_src).unwrap();
        assert_eq!(c.variables.get("base_url").unwrap(), "https://example.com");
        assert_eq!(c.requests.len(), 1);
        assert_eq!(c.requests[0].assertions.len(), 1);
        assert!(matches!(c.requests[0].assertions[0], Assertion::Status { equals: 200 }));
        assert_eq!(c.requests[0].extract.get("token").unwrap(), "$.token");
    }
}
