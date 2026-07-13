use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct CommitSettings {
    pub char_limit: usize,
}

impl Default for CommitSettings {
    fn default() -> Self {
        Self { char_limit: 20_000 }
    }
}

#[derive(Serialize)]
struct GeminiRequest {
    contents: Vec<Content>,
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

        let prompt = format!(
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
        );

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-flash-preview:generateContent?key={}",
            api_key
        );

        let client = reqwest::Client::new();
        let response = client
            .post(&url)
            .json(&GeminiRequest {
                contents: vec![Content {
                    parts: vec![Part { text: prompt }],
                }],
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
