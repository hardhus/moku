use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use regex::Regex;

use crate::model::Bookmark;

pub struct BookmarkIO;

impl BookmarkIO {
    /// Exports the bookmark list as a JSON file.
    pub fn export_json(items: &[Bookmark], filename: &str) -> Result<()> {
        let json = serde_json::to_string_pretty(items).context("JSON serialization error")?;
        fs::write(filename, json).context(format!("Failed to write to file: {}", filename))?;
        Ok(())
    }

    /// Imports the bookmark list from a JSON file.
    pub fn import_json(filename: &str) -> Result<Vec<Bookmark>> {
        let data = fs::read_to_string(filename)
            .context(format!("File not found or unreadable: {}", filename))?;

        let items: Vec<Bookmark> = serde_json::from_str(&data)
            .context("File is not in valid Moku Bookmark JSON format")?;
        Ok(items)
    }

    /// Imports bookmarks from an HTML (Netscape Bookmark) file.
    pub fn import_html(filename: &str) -> Result<Vec<Bookmark>> {
        let content =
            fs::read_to_string(filename).context(format!("File not found: {}", filename))?;

        // Regex to capture <a href="URL">TITLE</a> tags (?i for case-insensitive)
        let re = Regex::new(r#"(?i)<a[^>]*href="([^"]+)"[^>]*>([^<]*)</a>"#)
            .context("Regex compilation error")?;

        let mut items = Vec::new();

        for cap in re.captures_iter(&content) {
            if let (Some(url_match), Some(title_match)) = (cap.get(1), cap.get(2)) {
                let url = url_match.as_str().to_string();
                let title = title_match.as_str().trim().to_string();

                let mut bm = Bookmark::new(url);
                if !title.is_empty() {
                    bm.name = Some(title);
                }
                items.push(bm);
            }
        }

        Ok(items)
    }

    /// Automatically selects the appropriate import method (JSON or HTML) based on file extension.
    pub fn import_file(filename: &str) -> Result<Vec<Bookmark>> {
        let path = Path::new(filename);
        match path.extension().and_then(|e| e.to_str()) {
            Some("json") => Self::import_json(filename),
            Some("html") | Some("htm") => Self::import_html(filename),
            _ => anyhow::bail!("Unsupported file format. Please use .json or .html."),
        }
    }
}
