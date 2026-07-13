use serde::{Deserialize, Serialize};

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
