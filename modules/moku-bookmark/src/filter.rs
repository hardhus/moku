use crate::model::Bookmark;

pub struct BookmarkFilter;

impl BookmarkFilter {
    /// Fuzzy search: Matches against URL, Title, Domain, or Tags case-insensitively.
    pub fn fuzzy(items: &[Bookmark], query: &str) -> Vec<Bookmark> {
        if query.is_empty() {
            return items.to_vec();
        }

        let q = query.to_lowercase();
        items
            .iter()
            .filter(|b| {
                b.url.to_lowercase().contains(&q)
                    || b.domain.to_lowercase().contains(&q)
                    || b.name
                        .as_ref()
                        .map(|n| n.to_lowercase().contains(&q))
                        .unwrap_or(false)
                    || b.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .cloned()
            .collect()
    }

    /// Specific domain filtering: Returns entries that exactly match the given domain.
    pub fn by_domain(items: &[Bookmark], domain: &str) -> Vec<Bookmark> {
        items
            .iter()
            .filter(|b| b.domain == domain)
            .cloned()
            .collect()
    }
}
