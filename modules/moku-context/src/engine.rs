use anyhow::Result;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Configuration settings required for the context module.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContextSettings {
    #[serde(default)]
    pub use_gitignore: bool,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub ignore_dirs: Vec<String>,
}

impl Default for ContextSettings {
    fn default() -> Self {
        Self {
            use_gitignore: true,
            extensions: vec!["rs".to_string(), "toml".to_string(), "txt".to_string()],
            ignore_dirs: vec!["target".to_string(), ".git".to_string()],
        }
    }
}

pub struct ContextEngine {
    pub settings: ContextSettings,
}

impl ContextEngine {
    pub fn new(settings: ContextSettings) -> Self {
        Self { settings }
    }

    /// Scans files and returns the filtered path list.
    pub fn scan_files(&self, root: &Path) -> Vec<PathBuf> {
        let mut builder = WalkBuilder::new(root);
        builder.hidden(false);
        builder.git_ignore(self.settings.use_gitignore);

        builder
            .build()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .map(|e| e.path().to_path_buf())
            .filter(|p| {
                let p_str = p.to_string_lossy();
                let ext_match = p
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|ext| self.settings.extensions.iter().any(|e| e == ext))
                    .unwrap_or(false);

                let is_ignored = self.settings.ignore_dirs.iter().any(|d| p_str.contains(d));
                ext_match && !is_ignored
            })
            .collect()
    }

    /// Reads the file list and returns a combined content string.
    pub fn build_output(&self, root: &Path, files: &[PathBuf]) -> Result<(String, usize)> {
        let mut output = String::new();
        let mut count = 0;

        for path in files {
            if let Ok(text) = fs::read_to_string(&path) {
                let rel_path = path.strip_prefix(root).unwrap_or(path);
                output.push_str(&format!(
                    "\n{}\nFILE: {}\n{}\n{}\n",
                    "=".repeat(50),
                    rel_path.display(),
                    "=".repeat(50),
                    text
                ));
                count += 1;
            }
        }
        Ok((output, count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_engine_file_scanning_logic() {
        let dir = tempdir().unwrap();
        let settings = ContextSettings {
            use_gitignore: true,
            extensions: vec!["rs".to_string()],
            ignore_dirs: vec!["ignored_dir".to_string()],
        };
        let engine = ContextEngine::new(settings);

        // Create test files
        fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("style.css"), "body {}").unwrap(); // Extension mismatch
        let ignored_path = dir.path().join("ignored_dir");
        fs::create_dir(&ignored_path).unwrap();
        fs::write(ignored_path.join("test.rs"), "dummy").unwrap(); // Directory is ignored

        let files = engine.scan_files(dir.path());

        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("main.rs"));
    }
}
