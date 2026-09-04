use serde::{Deserialize, Serialize};

/// Shared mode-label strings. `lib.rs` builds the mode-name string shown
/// in the status bar/input title from these; `ui/status.rs` and
/// `ui/input.rs` match against the same constants, so the two can never
/// drift out of sync (as `"INPUT"` vs. `"ADD URL"` previously did).
pub const MODE_NORMAL: &str = "Normal";
pub const MODE_SEARCH: &str = "Search";
pub const MODE_INPUT: &str = "Add Bookmark";
pub const MODE_DOMAIN_FILTER_PREFIX: &str = "Domain Filter";
pub const MODE_CONFIRM_DELETE: &str = "Confirm Delete";

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Bookmark {
    pub url: String,
    pub domain: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub created_at: u64,
}

impl Bookmark {
    pub fn new(url: String) -> Self {
        let domain = extract_domain(&url);

        Self {
            url,
            domain,
            name: None,
            description: None,
            tags: Vec::new(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }
}

fn extract_domain(url: &str) -> String {
    let d = url
        .replace("https://", "")
        .replace("http://", "")
        .replace("www.", "");

    d.split('/').next().unwrap_or(&d).to_string()
}
