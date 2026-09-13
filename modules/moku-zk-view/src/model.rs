use std::path::PathBuf;

use satz_core::formatter::table::TableBlock;
use satz_core::model::BlockAnchor;
use satz_core::parser::structure::StructureOutput;
use satz_core::{ByteRange, DocId, Document, Heading, Link, Tag};

pub struct NoteEntry {
    pub id: DocId,
    pub rel_path: String,
}

/// A text-cursor position within an open document's raw source: `line` is
/// a source-line index (into `OpenDoc::line_ranges`), `col` a char index
/// within that line (not a byte offset, not UTF-16 — see cursor.rs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Cursor {
    pub line: usize,
    pub col: usize,
}

/// One entry on the "go back" navigation stack, pushed by `gd` before
/// following a link — pops back to exactly where the reader was.
pub struct NavFrame {
    pub doc_id: DocId,
    pub cursor: Cursor,
    pub scroll_top: usize,
}

/// A link's resolution against the vault index, cached once when a
/// document is opened (not re-resolved on every keypress). `anchor_line`
/// is always present, defaulting to the target document's first line when
/// the link carried no heading/block anchor.
#[derive(Clone, Debug)]
pub enum LinkResState {
    Resolved {
        target_id: DocId,
        target_path: PathBuf,
        anchor_line: usize,
    },
    AnchorMissing {
        target_id: DocId,
        target_path: PathBuf,
    },
    DocMissing,
}

/// What's under the cursor, in the same priority order satz-lsp's own
/// `references.rs` uses: link > block anchor > heading > tag.
pub enum CursorTarget<'a> {
    Link(&'a Link),
    Block(&'a BlockAnchor),
    Heading(&'a Heading),
    Tag(&'a Tag),
    None,
}

#[derive(Clone)]
pub enum JumpItem {
    Link {
        link_idx: usize,
        label: String,
        resolved: bool,
    },
    Tag {
        name: String,
        byte_offset: usize,
    },
    Heading {
        level: u8,
        text: String,
        byte_offset: usize,
    },
}

/// A theme-independent layout segment of a rendered document: either a run
/// of plain source lines (styled fresh per-frame in `render.rs`, since
/// styling depends on the active theme) or a GFM table rendered as a real
/// `ratatui::widgets::Table`. Computed once per document open/reload —
/// never per draw — since it depends only on document structure, not theme.
pub enum RenderBlock {
    Lines {
        first_line: usize,
        line_count: usize,
    },
    Table {
        first_line: usize,
        /// The GFM delimiter row (`|---|---:|`) — never a real screen row,
        /// so line-based cursor motion must step over it (see cursor.rs).
        delim_line: usize,
        last_line: usize,
        table: TableBlock,
    },
    /// The YAML frontmatter block (`---`...`---`), rendered as a short
    /// "Label: value" summary instead of raw YAML — `first_line` is always
    /// `0` when present (a YAML frontmatter block must start on the
    /// document's very first line). `first_line+1..=last_line` are never
    /// real screen rows either (see cursor.rs's `skip_delim`, generalized
    /// to cover this range too).
    Frontmatter {
        first_line: usize,
        last_line: usize,
        entries: Vec<(String, String)>,
    },
}

/// Everything needed to render and navigate one currently-open document,
/// cached at open/reload time (not rebuilt per draw except for the actual
/// styled text, which depends on the active theme).
pub struct OpenDoc {
    pub doc: Document,
    pub source: String,
    pub structure: StructureOutput,
    pub line_ranges: Vec<ByteRange>,
    pub link_res: Vec<LinkResState>,
    pub blocks: Vec<RenderBlock>,
    pub row_offsets: Vec<usize>,
    pub total_rows: usize,
    /// Byte ranges of each list item's own marker glyph (`-`/`*`/`1.` +
    /// following whitespace) — computed once, not per draw, so `-`/`*`
    /// itself can be colored distinctly from the item's text.
    pub list_marker_ranges: Vec<ByteRange>,
    /// Byte ranges of each blockquote line's leading `>` (+ nested `>`)
    /// marker prefix — styled more dimly than the quoted text itself.
    pub quote_marker_ranges: Vec<ByteRange>,
    /// Byte ranges from a *checked* task's `[x]` marker to the end of its
    /// line — struck through, unchecked tasks are left alone.
    pub checked_task_tail_ranges: Vec<ByteRange>,
    /// Source line indices that are a fenced code block's opening/closing
    /// ` ``` ` line — rendered as a plain `code_bg` band instead of the
    /// literal backticks.
    pub code_fence_delim_lines: Vec<usize>,
}
