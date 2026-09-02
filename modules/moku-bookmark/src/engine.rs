use anyhow::{Context, Result};

use moku_core::{AppContext, ModuleId};

use crate::model::Bookmark;

pub struct BookmarkEngine;

impl BookmarkEngine {
    /// Loads the entire bookmark list securely from encrypted storage.
    pub async fn load_all(ctx: &AppContext) -> Result<Vec<Bookmark>> {
        let result: Result<Vec<Bookmark>> = ctx
            .storage
            .load(ModuleId::BOOKMARK.as_str(), "bookmarks_encrypted")
            .await;

        match result {
            Ok(items) => Ok(items),
            Err(_) => Ok(Vec::new()),
        }
    }

    /// Atomically saves the bookmark list to disk (encrypted unless
    /// overridden — see moku_core::resolve_encryption).
    pub async fn save_all(ctx: &AppContext, items: &[Bookmark]) -> Result<()> {
        let encrypt = moku_core::resolve_encryption(&ctx.config.load(), ModuleId::BOOKMARK.as_str(), true);
        ctx.storage
            .save(
                ModuleId::BOOKMARK.as_str(),
                "bookmarks_encrypted",
                &items.to_vec(),
                encrypt,
            )
            .await
            .context("Failed to write bookmark data to storage.")
    }

    /// Creates a new bookmark instance.
    pub fn create_bookmark(url: String) -> Result<Bookmark> {
        let trimmed_url = url.trim();
        if trimmed_url.is_empty() {
            anyhow::bail!("URL cannot be empty.");
        }

        Ok(Bookmark::new(trimmed_url.to_string()))
    }

    /// Removes duplicate entries containing the exact same URL case-insensitively.
    /// Returns the number of deleted items.
    pub fn remove_duplicates(items: &mut Vec<Bookmark>) -> usize {
        let initial_len = items.len();
        let mut seen = std::collections::HashSet::new();

        items.retain(|b| seen.insert(b.url.to_lowercase()));

        initial_len - items.len()
    }
}
