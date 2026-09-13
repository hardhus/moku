use satz_core::{ByteRange, Document};

use crate::model::{Cursor, CursorTarget, RenderBlock};

/// Splits `source` into one `ByteRange` per line, using the same `\n`-scan
/// rule `satz_core::text::LineIndex` uses internally (verified consistent
/// via round-trip tests against `LineIndex::byte_to_position` in
/// `tui_module.rs`'s test module) — `LineIndex` itself doesn't expose its
/// line-start table, so this is computed independently here.
pub fn compute_line_ranges(source: &str) -> Vec<ByteRange> {
    let mut out = Vec::new();
    let bytes = source.as_bytes();
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            let end = if i > start && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            out.push(ByteRange::new(start, end));
            start = i + 1;
        }
    }
    out.push(ByteRange::new(start, source.len()));
    out
}

fn line_text<'s>(source: &'s str, r: ByteRange) -> &'s str {
    &source[r.start..r.end]
}

/// Rests ON the last char of the line (never one past it), like Helix/vim
/// normal mode — `0` for an empty line.
fn clamp_col(line: &str, col: usize) -> usize {
    let n = line.chars().count();
    if n == 0 { 0 } else { col.min(n - 1) }
}

fn is_table_delim_line(blocks: &[RenderBlock], line: usize) -> bool {
    blocks
        .iter()
        .any(|b| matches!(b, RenderBlock::Table { delim_line, .. } if *delim_line == line))
}

/// A GFM table's delimiter row (`|---|---:|`) has no visual row to place a
/// cursor on (tables render as a real `ratatui::Table` widget, header +
/// data rows only) — step one further in the motion's direction when
/// vertical movement would land there.
fn skip_delim(blocks: &[RenderBlock], line_count: usize, mut line: usize, dir: isize) -> usize {
    while is_table_delim_line(blocks, line) {
        let next = line as isize + dir;
        if next < 0 || next as usize >= line_count {
            break;
        }
        line = next as usize;
    }
    line
}

pub fn move_left(c: Cursor) -> Cursor {
    Cursor {
        col: c.col.saturating_sub(1),
        ..c
    }
}

pub fn move_right(lines: &[ByteRange], source: &str, c: Cursor) -> Cursor {
    let len = line_text(source, lines[c.line]).chars().count();
    Cursor {
        col: (c.col + 1).min(len.saturating_sub(1)),
        ..c
    }
}

pub fn move_down(blocks: &[RenderBlock], lines: &[ByteRange], source: &str, c: Cursor) -> Cursor {
    if lines.is_empty() {
        return c;
    }
    let target = (c.line + 1).min(lines.len() - 1);
    let line = skip_delim(blocks, lines.len(), target, 1);
    Cursor {
        line,
        col: clamp_col(line_text(source, lines[line]), c.col),
    }
}

pub fn move_up(blocks: &[RenderBlock], lines: &[ByteRange], source: &str, c: Cursor) -> Cursor {
    if lines.is_empty() {
        return c;
    }
    let target = c.line.saturating_sub(1);
    let line = skip_delim(blocks, lines.len(), target, -1);
    Cursor {
        line,
        col: clamp_col(line_text(source, lines[line]), c.col),
    }
}

pub fn goto_top() -> Cursor {
    Cursor { line: 0, col: 0 }
}

pub fn goto_bottom(lines: &[ByteRange]) -> Cursor {
    Cursor {
        line: lines.len().saturating_sub(1),
        col: 0,
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum CharClass {
    Word,
    Punct,
    Space,
}

fn classify(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

/// Word-forward (`w`) — entirely moku's own logic; satz-core has no
/// word-boundary concept (and shouldn't: that's editor-navigation
/// behavior, not zettelkasten/markdown-model behavior).
pub fn word_forward(lines: &[ByteRange], source: &str, c: Cursor) -> Cursor {
    let mut line = c.line;
    let mut chars: Vec<char> = line_text(source, lines[line]).chars().collect();
    let mut col = c.col;

    if col < chars.len() {
        let start_class = classify(chars[col]);
        while col < chars.len() && classify(chars[col]) == start_class {
            col += 1;
        }
    }

    loop {
        while col < chars.len() && classify(chars[col]) == CharClass::Space {
            col += 1;
        }
        if col < chars.len() {
            return Cursor { line, col };
        }
        if line + 1 >= lines.len() {
            return Cursor {
                line,
                col: chars.len().saturating_sub(1),
            };
        }
        line += 1;
        chars = line_text(source, lines[line]).chars().collect();
        col = 0;
        if chars.is_empty() {
            // An empty line is itself a word-stop (paragraph break),
            // matching Helix's own `w` behavior.
            return Cursor { line, col: 0 };
        }
    }
}

/// Word-back (`b`) — symmetric to `word_forward`.
pub fn word_back(lines: &[ByteRange], source: &str, c: Cursor) -> Cursor {
    let mut line = c.line;
    let mut chars: Vec<char> = line_text(source, lines[line]).chars().collect();
    let mut col = c.col;

    loop {
        if col == 0 {
            if line == 0 {
                return Cursor { line: 0, col: 0 };
            }
            line -= 1;
            chars = line_text(source, lines[line]).chars().collect();
            col = chars.len();
            if chars.is_empty() {
                return Cursor { line, col: 0 };
            }
            continue;
        }
        col -= 1;
        while col > 0 && classify(chars[col]) == CharClass::Space {
            col -= 1;
        }
        if classify(chars[col]) == CharClass::Space {
            continue;
        }
        let target_class = classify(chars[col]);
        while col > 0 && classify(chars[col - 1]) == target_class {
            col -= 1;
        }
        return Cursor { line, col };
    }
}

pub fn cursor_to_byte_offset(lines: &[ByteRange], source: &str, c: Cursor) -> usize {
    let r = lines[c.line];
    line_text(source, r)
        .char_indices()
        .nth(c.col)
        .map(|(b, _)| r.start + b)
        .unwrap_or(r.end)
}

pub fn byte_offset_to_cursor(lines: &[ByteRange], source: &str, offset: usize) -> Cursor {
    if lines.is_empty() {
        return Cursor::default();
    }
    let line = lines.partition_point(|r| r.end < offset).min(lines.len() - 1);
    let r = lines[line];
    let clamped = offset.clamp(r.start, r.end);
    let col = source[r.start..clamped].chars().count();
    Cursor { line, col }
}

/// What's under the cursor, in the same priority order satz-lsp's own
/// `handlers/references.rs::cursor_target` uses: link > block > heading >
/// tag.
pub fn classify_cursor_target(doc: &Document, offset: usize) -> CursorTarget<'_> {
    if let Some(l) = doc.links.iter().find(|l| l.range.contains(offset)) {
        return CursorTarget::Link(l);
    }
    if let Some(b) = doc.blocks.iter().find(|b| b.range.contains(offset)) {
        return CursorTarget::Block(b);
    }
    if let Some(h) = doc.headings.iter().find(|h| h.range.contains(offset)) {
        return CursorTarget::Heading(h);
    }
    if let Some(t) = doc.tags.iter().find(|t| t.range.contains(offset)) {
        return CursorTarget::Tag(t);
    }
    CursorTarget::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use satz_core::text::LineIndex;

    #[test]
    fn test_compute_line_ranges_matches_line_index_round_trip() {
        let source = "hello\nworld\nfoo";
        let lines = compute_line_ranges(source);
        let line_index = LineIndex::new(source);
        assert_eq!(lines.len(), line_index.line_count());
        for (i, r) in lines.iter().enumerate() {
            let pos = line_index.byte_to_position(r.start);
            assert_eq!(pos.line as usize, i);
        }
    }

    #[test]
    fn test_compute_line_ranges_crlf() {
        let source = "a\r\nb\r\n";
        let lines = compute_line_ranges(source);
        assert_eq!(&source[lines[0].start..lines[0].end], "a");
        assert_eq!(&source[lines[1].start..lines[1].end], "b");
    }

    #[test]
    fn test_word_forward_turkish_text() {
        let source = "ığüşçö bir kelime";
        let lines = compute_line_ranges(source);
        let c = Cursor { line: 0, col: 0 };
        let next = word_forward(&lines, source, c);
        assert_eq!(next, Cursor { line: 0, col: 7 });
    }

    #[test]
    fn test_word_forward_and_back_are_symmetric() {
        let source = "foo bar baz";
        let lines = compute_line_ranges(source);
        let c0 = Cursor { line: 0, col: 0 };
        let c1 = word_forward(&lines, source, c0);
        assert_eq!(c1, Cursor { line: 0, col: 4 });
        let c2 = word_forward(&lines, source, c1);
        assert_eq!(c2, Cursor { line: 0, col: 8 });
        let back1 = word_back(&lines, source, c2);
        assert_eq!(back1, c1);
        let back0 = word_back(&lines, source, back1);
        assert_eq!(back0, c0);
    }

    #[test]
    fn test_goto_top_and_bottom() {
        let source = "a\nb\nc\n";
        let lines = compute_line_ranges(source);
        assert_eq!(goto_top(), Cursor { line: 0, col: 0 });
        assert_eq!(goto_bottom(&lines), Cursor { line: 3, col: 0 });
    }

    #[test]
    fn test_move_down_skips_table_delimiter_line() {
        let source = "text\n| A | B |\n|---|---|\n| 1 | 2 |\nafter\n";
        let lines = compute_line_ranges(source);
        let table = satz_core::formatter::table::parse_table_block(
            source,
            ByteRange::new(source.find("| A").unwrap(), source.find("after").unwrap()),
        )
        .unwrap();
        let blocks = vec![RenderBlock::Table {
            first_line: 1,
            delim_line: 2,
            last_line: 3,
            table,
        }];
        let c = Cursor { line: 1, col: 0 };
        let down = move_down(&blocks, &lines, source, c);
        assert_eq!(down.line, 3, "must skip past the delimiter row (line 2)");
    }

    #[test]
    fn test_cursor_byte_offset_round_trip() {
        let source = "ığüşçö[[x]]";
        let lines = compute_line_ranges(source);
        let bracket = source.find("[[").unwrap();
        let cursor = byte_offset_to_cursor(&lines, source, bracket);
        let offset_back = cursor_to_byte_offset(&lines, source, cursor);
        assert_eq!(offset_back, bracket);
    }

    #[test]
    fn test_classify_cursor_target_priority_link_over_heading() {
        let source = "# [[a]]\n";
        let doc = satz_core::parse_document(source, std::path::Path::new("t.md"));
        let link_range = doc.links[0].range;
        let target = classify_cursor_target(&doc, link_range.start);
        assert!(matches!(target, CursorTarget::Link(_)));
    }

    #[test]
    fn test_classify_cursor_target_none_on_plain_text() {
        let source = "plain text, nothing special\n";
        let doc = satz_core::parse_document(source, std::path::Path::new("t.md"));
        let target = classify_cursor_target(&doc, 2);
        assert!(matches!(target, CursorTarget::None));
    }
}
