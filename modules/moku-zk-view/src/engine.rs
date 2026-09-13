use std::path::Path;

use anyhow::{Context, Result};
use satz_core::{Document, Index, LinkResolution, parse_document, walk_vault};

use crate::model::{LinkResState, NoteEntry};

/// Walks and indexes the vault at `vault_root` — synchronous and
/// potentially slow for a large vault; callers should run this on
/// `tokio::task::spawn_blocking` rather than inline on the TUI event loop
/// (same reasoning as `modules/moku-satz`'s own `build_index` caller).
pub fn build_index(vault_root: &Path) -> Result<Index> {
    let docs = walk_vault(vault_root)
        .with_context(|| format!("failed to walk vault at {}", vault_root.display()))?;
    Ok(Index::build(docs))
}

pub fn build_note_list(index: &Index) -> Vec<NoteEntry> {
    let mut notes: Vec<NoteEntry> = index
        .documents()
        .map(|d| NoteEntry {
            id: d.id.clone(),
            rel_path: d.path.to_string_lossy().replace('\\', "/"),
        })
        .collect();
    notes.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    notes
}

/// Re-reads and re-parses a single file after an external edit (e.g.
/// returning from a suspended editor session), updating the index in
/// place via `Index::replace_doc`. Cheap enough to run synchronously
/// inline (a single file, not a vault walk).
pub fn reload_single_file(index: &mut Index, vault_root: &Path, rel_path: &Path) -> Result<Document> {
    let full = vault_root.join(rel_path);
    let source = std::fs::read_to_string(&full)
        .with_context(|| format!("failed to read {}", full.display()))?;
    let doc = parse_document(&source, rel_path);
    index.replace_doc(doc.clone());
    Ok(doc)
}

/// Resolves every link in `doc` against the index once, so `gd` and the
/// per-character link styling don't re-resolve on every keypress/frame.
/// Mirrors satz-lsp's `handlers/definition.rs` resolution algorithm
/// exactly, just without any LSP types.
pub fn build_link_resolution_cache(index: &Index, doc: &Document) -> Vec<LinkResState> {
    doc.links
        .iter()
        .map(
            |link| match index.resolve_link_full_with_config(link, Some(doc), None) {
                LinkResolution::Resolved { doc: target, anchor } => LinkResState::Resolved {
                    target_id: target.id.clone(),
                    target_path: target.path.clone(),
                    anchor_line: anchor
                        .map(|r| target.line_index.byte_to_position(r.start).line as usize)
                        .unwrap_or(0),
                },
                LinkResolution::AnchorMissing { doc: target } => LinkResState::AnchorMissing {
                    target_id: target.id.clone(),
                    target_path: target.path.clone(),
                },
                LinkResolution::DocMissing => LinkResState::DocMissing,
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write_vault(dir: &Path) {
        std::fs::write(
            dir.join("a.md"),
            "# A\n\nSee [[b#Section]] and [[missing]].\n",
        )
        .unwrap();
        std::fs::write(dir.join("b.md"), "# B\n\n## Section\n\nBody.\n").unwrap();
    }

    #[test]
    fn test_build_link_resolution_cache_resolved_anchor_missing_doc_missing() {
        let vault = tempfile::tempdir().unwrap();
        write_vault(vault.path());
        let index = build_index(vault.path()).unwrap();
        let a = index
            .get_doc(&satz_core::DocId::new("a.md"))
            .unwrap()
            .clone();

        let cache = build_link_resolution_cache(&index, &a);
        assert_eq!(cache.len(), 2);
        assert!(matches!(
            cache[0],
            LinkResState::Resolved { anchor_line: 2, .. }
        ));
        assert!(matches!(cache[1], LinkResState::DocMissing));
    }

    #[test]
    fn test_build_link_resolution_cache_anchor_missing() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(vault.path().join("a.md"), "[[b#Nope]]\n").unwrap();
        std::fs::write(vault.path().join("b.md"), "# B\n").unwrap();
        let index = build_index(vault.path()).unwrap();
        let a = index
            .get_doc(&satz_core::DocId::new("a.md"))
            .unwrap()
            .clone();

        let cache = build_link_resolution_cache(&index, &a);
        assert_eq!(cache.len(), 1);
        assert!(matches!(cache[0], LinkResState::AnchorMissing { .. }));
    }
}
