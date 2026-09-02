use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const DEFAULT_MODEL: &str = "gemini-3-flash-preview";
const DEFAULT_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct CommitSettings {
    pub char_limit: usize,
    /// Full API endpoint override (assumes the same `?key=<api_key>` query
    /// auth style as Gemini). Leave unset to use the default Gemini
    /// endpoint with `model` substituted in.
    pub api_url: Option<String>,
    /// Model name substituted into the default Gemini endpoint. Ignored if
    /// `api_url` is also set (that fully replaces the endpoint).
    pub model: Option<String>,
    /// Overrides the built-in Conventional-Commits system prompt. Include
    /// the literal `{diff}` placeholder where the (possibly truncated)
    /// git diff should be inserted.
    pub prompt_template: Option<String>,
    /// Gemini `generationConfig.temperature`. Left unset (API default) if
    /// not provided.
    pub temperature: Option<f32>,
}

impl Default for CommitSettings {
    fn default() -> Self {
        Self {
            char_limit: 20_000,
            api_url: None,
            model: None,
            prompt_template: None,
            temperature: None,
        }
    }
}

#[derive(Serialize)]
struct GeminiRequest {
    contents: Vec<Content>,
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig>,
}
#[derive(Serialize)]
struct GenerationConfig {
    temperature: f32,
}
#[derive(Serialize)]
struct Content {
    parts: Vec<Part>,
}
#[derive(Serialize)]
struct Part {
    text: String,
}
#[derive(Deserialize)]
struct GeminiResponse {
    candidates: Vec<Candidate>,
}
#[derive(Deserialize)]
struct Candidate {
    content: ContentResponse,
}
#[derive(Deserialize)]
struct ContentResponse {
    parts: Vec<PartResponse>,
}
#[derive(Deserialize)]
struct PartResponse {
    text: String,
}

pub struct CommitEngine {
    pub settings: CommitSettings,
}

impl CommitEngine {
    pub fn new(settings: CommitSettings) -> Self {
        Self { settings }
    }

    pub fn get_staged_diff(&self) -> Result<Option<String>> {
        let output = Command::new("git")
            .args(["diff", "--staged"])
            .output()
            .context("Failed to execute git command. Is git installed and in PATH?")?;

        let diff = String::from_utf8_lossy(&output.stdout).to_string();

        if diff.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(diff))
    }

    pub async fn generate_commit_message(&self, api_key: &str, diff: &str) -> Result<String> {
        let truncated_diff = if diff.len() > self.settings.char_limit {
            format!(
                "{}\n\n[... DIFF TRUNCATED (Limit: {}) ...]",
                &diff[..self.settings.char_limit],
                self.settings.char_limit
            )
        } else {
            diff.to_string()
        };

        let prompt = build_prompt(&self.settings, &truncated_diff);
        let url = build_api_url(&self.settings, api_key);

        let client = reqwest::Client::new();
        let response = client
            .post(&url)
            .json(&GeminiRequest {
                contents: vec![Content {
                    parts: vec![Part { text: prompt }],
                }],
                generation_config: self.settings.temperature.map(|temperature| GenerationConfig { temperature }),
            })
            .send()
            .await
            .context("API request failed. Check your internet connection.")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            bail!("API Error: {}", error_text);
        }

        let body: GeminiResponse = response
            .json()
            .await
            .context("Failed to parse API response (JSON Parse Error).")?;

        let text = body
            .candidates
            .first()
            .and_then(|c| c.content.parts.first())
            .map(|p| p.text.trim().to_string())
            .context("API returned an invalid or empty commit message.")?;

        Ok(text)
    }
}

fn build_api_url(settings: &CommitSettings, api_key: &str) -> String {
    match &settings.api_url {
        Some(custom_url) => format!("{custom_url}?key={api_key}"),
        None => {
            let model = settings.model.as_deref().unwrap_or(DEFAULT_MODEL);
            format!("{DEFAULT_API_BASE}/{model}:generateContent?key={api_key}")
        }
    }
}

fn build_prompt(settings: &CommitSettings, truncated_diff: &str) -> String {
    match &settings.prompt_template {
        Some(template) => template.replace("{diff}", truncated_diff),
        None => format!(
            r#"
You are a Senior Software Engineer acting as an AI commit message generator.
Your task is to analyze the provided git diff and generate a structured, professional commit message following the Conventional Commits specification.

--- GUIDELINES ---
1. **Structure:**
   <type>(<scope>): <subject>
   <BLANK LINE>
   - <Detail bullet point 1>
   - <Detail bullet point 2>
   - ...

2. **Subject Line Rules:**
   - Use imperative mood ("add", "fix", "change" -> NOT "added", "fixed").
   - Max 50 characters.
   - NO period at the end.
   - Must be concise but descriptive.

3. **Body/Details Rules:**
   - Analyze the diff deeply. Do not just say "updated file". Explain *what* logic changed.
   - Use a bulleted list (-) for details.
   - If multiple logical changes exist, list them separately.
   - Wrap lines at 72 characters if possible.

4. **Types:**
   - feat: New feature
   - fix: Bug fix
   - refactor: Code change that neither fixes a bug nor adds a feature
   - chore: Build process, auxiliary tools, dependencies
   - docs: Documentation only
   - style: Formatting, missing semi-colons, etc.
   - perf: Code change that improves performance

5. **Output Format:**
   - RETURN ONLY THE RAW COMMIT MESSAGE.
   - NO markdown blocks (```), NO introductory text, NO explanations.

--- EXAMPLES ---

Input: (Diff showing a new struct in `auth.rs` and a login function)
Output:
feat(auth): implement user login system

- Add User struct with JWT token generation.
- Implement login function in auth controller.
- Update error handling for invalid credentials.

Input: (Diff showing a fix in `physics.rs` for collision calculation)
Output:
fix(physics): resolve collision overlapping bug

- Correct velocity calculation in collision_solver.
- Add boundary check for rigid bodies.

--- GIT DIFF TO ANALYZE ---
{}
"#,
            truncated_diff
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_api_url_default() {
        let settings = CommitSettings::default();
        let url = build_api_url(&settings, "KEY123");
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-flash-preview:generateContent?key=KEY123"
        );
    }

    #[test]
    fn test_build_api_url_custom_model_uses_default_endpoint() {
        let settings = CommitSettings {
            model: Some("gemini-2.0-flash".to_string()),
            ..CommitSettings::default()
        };
        let url = build_api_url(&settings, "KEY123");
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent?key=KEY123"
        );
    }

    #[test]
    fn test_build_api_url_custom_api_url_overrides_everything() {
        let settings = CommitSettings {
            api_url: Some("https://my-proxy.internal/v1/generate".to_string()),
            model: Some("ignored-model".to_string()),
            ..CommitSettings::default()
        };
        let url = build_api_url(&settings, "KEY123");
        assert_eq!(url, "https://my-proxy.internal/v1/generate?key=KEY123");
    }

    #[test]
    fn test_build_prompt_default_includes_diff() {
        let settings = CommitSettings::default();
        let prompt = build_prompt(&settings, "diff --git a/x b/x");
        assert!(prompt.contains("diff --git a/x b/x"));
        assert!(prompt.contains("Conventional Commits"));
    }

    #[test]
    fn test_build_prompt_custom_template_substitutes_diff_placeholder() {
        let settings = CommitSettings {
            prompt_template: Some("Summarize this diff:\n{diff}\nBe brief.".to_string()),
            ..CommitSettings::default()
        };
        let prompt = build_prompt(&settings, "diff --git a/x b/x");
        assert_eq!(prompt, "Summarize this diff:\ndiff --git a/x b/x\nBe brief.");
    }
}
