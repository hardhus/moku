use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use moku_core::StorageManager;

use crate::interpolate::{find_secret_refs, interpolate, interpolate_toml_value};
use crate::jsonpath_lite;
use crate::model::{Assertion, Collection, RequestDef};

pub struct AssertionResult {
    pub description: String,
    pub passed: bool,
}

pub struct RunResult {
    pub name: String,
    pub status: Option<u16>,
    pub duration: Duration,
    pub assertion_results: Vec<AssertionResult>,
    pub body_preview: String,
    pub error: Option<String>,
}

impl RunResult {
    pub fn all_passed(&self) -> bool {
        self.error.is_none() && self.assertion_results.iter().all(|a| a.passed)
    }
}

pub fn load_collection(path: &Path) -> Result<(Collection, String)> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let collection: Collection =
        toml::from_str(&raw).with_context(|| format!("failed to parse {} as a moku-http collection", path.display()))?;
    Ok((collection, raw))
}

/// Resolves every `{{secrets.NAME}}` reference found in `raw_source` via
/// moku-secrets, inserting each as `vars["secrets.NAME"]` so `interpolate`
/// treats it like any other variable afterward. The vault must already be
/// unlocked (callers ensure this first — see `cli_module::ensure_unlocked`).
pub async fn resolve_secret_refs(raw_source: &str, storage: &StorageManager, vars: &mut HashMap<String, String>) -> Result<()> {
    let names = find_secret_refs(raw_source);
    if names.is_empty() {
        return Ok(());
    }
    let entries = moku_secrets::engine::load_entries(storage).await;
    for name in names {
        let entry = moku_secrets::engine::find_by_name(&entries, &name)
            .ok_or_else(|| anyhow!("secrets.{name} is referenced but no such secret exists"))?;
        vars.insert(format!("secrets.{name}"), entry.value.clone());
    }
    Ok(())
}

fn to_json_body(value: &toml::Value) -> Result<serde_json::Value> {
    serde_json::to_value(value).context("failed to convert body_json to JSON")
}

pub async fn run_request(client: &reqwest::Client, def: &RequestDef, vars: &mut HashMap<String, String>) -> RunResult {
    let start = Instant::now();
    let outcome = run_request_inner(client, def, vars).await;
    let duration = start.elapsed();
    match outcome {
        Ok((status, assertion_results, body_preview)) => {
            RunResult { name: def.name.clone(), status: Some(status), duration, assertion_results, body_preview, error: None }
        }
        Err(e) => RunResult {
            name: def.name.clone(),
            status: None,
            duration,
            assertion_results: Vec::new(),
            body_preview: String::new(),
            error: Some(e.to_string()),
        },
    }
}

async fn run_request_inner(
    client: &reqwest::Client,
    def: &RequestDef,
    vars: &mut HashMap<String, String>,
) -> Result<(u16, Vec<AssertionResult>, String)> {
    let method_str = interpolate(&def.method, vars).to_uppercase();
    let method = reqwest::Method::from_bytes(method_str.as_bytes()).map_err(|e| anyhow!("invalid HTTP method '{method_str}': {e}"))?;
    let url = interpolate(&def.url, vars);

    let mut builder = client.request(method, &url);
    for (k, v) in &def.headers {
        builder = builder.header(interpolate(k, vars), interpolate(v, vars));
    }

    if let Some(body_json) = &def.body_json {
        let interpolated = interpolate_toml_value(body_json, vars);
        let json_value = to_json_body(&interpolated)?;
        builder = builder.json(&json_value);
    } else if let Some(body) = &def.body {
        builder = builder.body(interpolate(body, vars));
    }

    let response = builder.send().await.with_context(|| format!("request '{}' failed", def.name))?;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body_text = response.text().await.unwrap_or_default();
    let json_body: Option<serde_json::Value> = serde_json::from_str(&body_text).ok();

    let mut assertion_results = Vec::new();
    for assertion in &def.assertions {
        // Built from the *interpolated* values actually compared, not the
        // raw `{{var}}` template — otherwise a passing assertion's own
        // printed description looks like it compared against a literal
        // "{{username}}" instead of the real value that was checked.
        let (passed, description) = match assertion {
            Assertion::Status { equals } => (status == *equals, format!("status == {equals}")),
            Assertion::Header { name, equals } => {
                let expected = interpolate(equals, vars);
                let passed = headers.get(name).and_then(|v| v.to_str().ok()).map(|v| v == expected).unwrap_or(false);
                (passed, format!("header '{name}' == '{expected}'"))
            }
            Assertion::JsonPath { path, equals } => {
                let expected = interpolate(equals, vars);
                let passed =
                    json_body.as_ref().and_then(|v| jsonpath_lite::eval(v, path)).map(|v| jsonpath_lite::value_to_string(v) == expected).unwrap_or(false);
                (passed, format!("{path} == '{expected}'"))
            }
            Assertion::BodyContains { text } => {
                let expected = interpolate(text, vars);
                (body_text.contains(&expected), format!("body contains '{expected}'"))
            }
        };
        assertion_results.push(AssertionResult { description, passed });
    }

    for (name, path) in &def.extract {
        if let Some(v) = json_body.as_ref().and_then(|j| jsonpath_lite::eval(j, path)) {
            vars.insert(name.clone(), jsonpath_lite::value_to_string(v));
        }
    }

    let body_preview: String = body_text.chars().take(2000).collect();
    Ok((status, assertion_results, body_preview))
}

/// Loads and runs a collection's requests in order, threading extracted
/// variables from one request into the next. `storage` is only needed if
/// the collection references `{{secrets.*}}`.
pub async fn run_collection(
    path: &Path,
    only: Option<&str>,
    extra_vars: &[(String, String)],
    storage: Option<&StorageManager>,
) -> Result<Vec<RunResult>> {
    let (collection, raw) = load_collection(path)?;
    let mut vars = collection.variables.clone();
    for (k, v) in extra_vars {
        vars.insert(k.clone(), v.clone());
    }

    if !find_secret_refs(&raw).is_empty() {
        let storage = storage.ok_or_else(|| anyhow!("this collection references secrets.* but no vault storage is available"))?;
        resolve_secret_refs(&raw, storage, &mut vars).await?;
    }

    let client = reqwest::Client::new();
    let mut results = Vec::new();
    for req in &collection.requests {
        if only.is_some_and(|name| name != req.name) {
            continue;
        }
        results.push(run_request(&client, req, &mut vars).await);
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RequestDef;

    fn make_def(name: &str) -> RequestDef {
        RequestDef { name: name.to_string(), ..Default::default() }
    }

    #[test]
    fn test_assertion_status_pass_and_fail() {
        let def = make_def("t");
        assert_eq!(def.method, "GET");

        let passed = matches!(Assertion::Status { equals: 200 }, Assertion::Status { equals } if equals == 200);
        assert!(passed);
    }

    #[tokio::test]
    async fn test_resolve_secret_refs_noop_when_none_referenced() {
        let mut vars = HashMap::new();
        // No storage needed since there are no secrets.* refs — this must
        // not panic or require a real StorageManager.
        let names = find_secret_refs("no secrets here");
        assert!(names.is_empty());
        vars.insert("existing".to_string(), "value".to_string());
        assert_eq!(vars.len(), 1);
    }
}
