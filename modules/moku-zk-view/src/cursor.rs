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

/// Whether `line` has no real screen row of its own to place a cursor on:
/// a GFM table's delimiter row (`|---|---:|`, tables render as a real
/// `ratatui::Table` widget with header + data rows only), or any
/// frontmatter line after its first (frontmatter renders as a short
/// "Label: value" summary, not raw YAML).
fn is_collapsed_line(blocks: &[RenderBlock], line: usize) -> bool {
    blocks.iter().any(|b| match b {
        RenderBlock::Table { delim_line, .. } => *delim_line == line,
        RenderBlock::Frontmatter { first_line, last_line, .. } => line > *first_line && line <= *last_line,
        RenderBlock::Lines { .. } => false,
    })
}

/// Steps one further in the motion's direction while the landing line has
/// no real screen row (see `is_collapsed_line`) — e.g. a table's
/// delimiter row, or the 2nd+ line of a collapsed frontmatter block.
fn skip_collapsed_lines(blocks: &[RenderBlock], line_count: usize, mut line: usize, dir: isize) -> usize {
    while is_collapsed_line(blocks, line) {
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
    let line = skip_collapsed_lines(blocks, lines.len(), target, 1);
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
    let line = skip_collapsed_lines(blocks, lines.len(), target, -1);
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

/// `gs` — first non-whitespace character of the current line (vim's `^`).
pub fn goto_first_non_blank(lines: &[ByteRange], source: &str, c: Cursor) -> Cursor {
    let chars: Vec<char> = line_text(source, lines[c.line]).chars().collect();
    let col = chars.iter().position(|ch| !ch.is_whitespace()).unwrap_or(0);
    Cursor { line: c.line, col }
}

/// `gl` — end of the current line.
pub fn goto_line_end(lines: &[ByteRange], source: &str, c: Cursor) -> Cursor {
    let len = line_text(source, lines[c.line]).chars().count();
    Cursor {
        line: c.line,
        col: len.saturating_sub(1),
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum CharClass {
    Word,
    Punct,
    Space,
}

/// vim/Helix's "small word": alphanumeric/`_` runs, punctuation runs, and
/// whitespace are each their own class — `foo.bar` is three words.
fn classify(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

/// vim/Helix's "WORD" (`W`/`B`/`E`): any non-whitespace run is one word,
/// punctuation included — `foo.bar` is a single WORD.
fn classify_big(c: char) -> CharClass {
    if c.is_whitespace() { CharClass::Space } else { CharClass::Word }
}

/// Word-forward (`w`/`W`) — entirely moku's own logic; satz-core has no
/// word-boundary concept (and shouldn't: that's editor-navigation
/// behavior, not zettelkasten/markdown-model behavior). Generic over
/// `classify` so the same algorithm serves both vim's "small word" (`w`)
/// and "WORD" (`W`) — see `classify`/`classify_big`.
fn word_forward_with(
    classify: fn(char) -> CharClass,
    lines: &[ByteRange],
    source: &str,
    c: Cursor,
) -> Cursor {
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

pub fn word_forward(lines: &[ByteRange], source: &str, c: Cursor) -> Cursor {
    word_forward_with(classify, lines, source, c)
}

pub fn word_forward_big(lines: &[ByteRange], source: &str, c: Cursor) -> Cursor {
    word_forward_with(classify_big, lines, source, c)
}

/// Word-back (`b`/`B`) — symmetric to `word_forward_with`.
fn word_back_with(classify: fn(char) -> CharClass, lines: &[ByteRange], source: &str, c: Cursor) -> Cursor {
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

pub fn word_back(lines: &[ByteRange], source: &str, c: Cursor) -> Cursor {
    word_back_with(classify, lines, source, c)
}

pub fn word_back_big(lines: &[ByteRange], source: &str, c: Cursor) -> Cursor {
    word_back_with(classify_big, lines, source, c)
}

/// `e`/`E` — end of the current word if not already there, else end of
/// the next word (vim/Helix `e` semantics: always advances at least one
/// position, so repeated presses keep moving rather than getting stuck).
fn word_end_forward_with(
    classify: fn(char) -> CharClass,
    lines: &[ByteRange],
    source: &str,
    c: Cursor,
) -> Cursor {
    let mut line = c.line;
    let mut chars: Vec<char> = line_text(source, lines[line]).chars().collect();
    let mut col = c.col;

    if col + 1 < chars.len() {
        col += 1;
    } else if line + 1 < lines.len() {
        line += 1;
        chars = line_text(source, lines[line]).chars().collect();
        col = 0;
    } else {
        return Cursor {
            line,
            col: chars.len().saturating_sub(1),
        };
    }

    loop {
        while col < chars.len() && classify(chars[col]) == CharClass::Space {
            col += 1;
        }
        if col < chars.len() {
            break;
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
            return Cursor { line, col: 0 };
        }
    }

    let target_class = classify(chars[col]);
    while col + 1 < chars.len() && classify(chars[col + 1]) == target_class {
        col += 1;
    }
    Cursor { line, col }
}

pub fn word_end_forward(lines: &[ByteRange], source: &str, c: Cursor) -> Cursor {
    word_end_forward_with(classify, lines, source, c)
}

pub fn word_end_forward_big(lines: &[ByteRange], source: &str, c: Cursor) -> Cursor {
    word_end_forward_with(classify_big, lines, source, c)
}

/// `f<char>` — next occurrence of `target` on the current line, cursor
/// lands ON it. Same-line only (matches vim/Helix `f`).
pub fn find_char_forward(lines: &[ByteRange], source: &str, c: Cursor, target: char) -> Option<Cursor> {
    let chars: Vec<char> = line_text(source, lines[c.line]).chars().collect();
    ((c.col + 1)..chars.len())
        .find(|&i| chars[i] == target)
        .map(|col| Cursor { line: c.line, col })
}

/// `F<char>` — previous occurrence of `target` on the current line.
pub fn find_char_backward(lines: &[ByteRange], source: &str, c: Cursor, target: char) -> Option<Cursor> {
    let chars: Vec<char> = line_text(source, lines[c.line]).chars().collect();
    (0..c.col)
        .rev()
        .find(|&i| chars[i] == target)
        .map(|col| Cursor { line: c.line, col })
}

/// `t<char>` — up to (one before) the next occurrence of `target`.
pub fn till_char_forward(lines: &[ByteRange], source: &str, c: Cursor, target: char) -> Option<Cursor> {
    find_char_forward(lines, source, c, target).map(|found| Cursor {
        line: found.line,
        col: found.col.saturating_sub(1),
    })
}

/// `T<char>` — up to (one after) the previous occurrence of `target`.
pub fn till_char_backward(lines: &[ByteRange], source: &str, c: Cursor, target: char) -> Option<Cursor> {
    find_char_backward(lines, source, c, target).map(|found| Cursor {
        line: found.line,
        col: found.col + 1,
    })
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

    #[test]
    fn test_goto_first_non_blank_skips_leading_whitespace() {
        let source = "    indented text\n";
        let lines = compute_line_ranges(source);
        let c = goto_first_non_blank(&lines, source, Cursor { line: 0, col: 0 });
        assert_eq!(c.col, 4);
    }

    #[test]
    fn test_goto_first_non_blank_blank_line_is_col_zero() {
        let source = "   \n";
        let lines = compute_line_ranges(source);
        let c = goto_first_non_blank(&lines, source, Cursor { line: 0, col: 0 });
        assert_eq!(c.col, 0);
    }

    #[test]
    fn test_goto_line_end_lands_on_last_char() {
        let source = "abcde\n";
        let lines = compute_line_ranges(source);
        let c = goto_line_end(&lines, source, Cursor { line: 0, col: 0 });
        assert_eq!(c.col, 4);
    }

    #[test]
    fn test_word_end_forward_matches_vim_e_semantics() {
        let source = "foo bar baz";
        let lines = compute_line_ranges(source);
        let c0 = Cursor { line: 0, col: 0 };
        let c1 = word_end_forward(&lines, source, c0);
        assert_eq!(c1, Cursor { line: 0, col: 2 }, "end of 'foo'");
        let c2 = word_end_forward(&lines, source, c1);
        assert_eq!(c2, Cursor { line: 0, col: 6 }, "end of 'bar'");
        let c3 = word_end_forward(&lines, source, c2);
        assert_eq!(c3, Cursor { line: 0, col: 10 }, "end of 'baz'");
    }

    #[test]
    fn test_word_forward_vs_word_forward_big_on_punctuated_text() {
        let source = "foo.bar baz";
        let lines = compute_line_ranges(source);
        let c0 = Cursor { line: 0, col: 0 };

        // Small `w` stops at "." (punctuation is its own class).
        let small = word_forward(&lines, source, c0);
        assert_eq!(small.col, 3, "small word stops at the punctuation run");

        // Big `W` treats "foo.bar" as one WORD, skipping straight to "baz".
        let big = word_forward_big(&lines, source, c0);
        assert_eq!(big.col, 8, "WORD motion skips over punctuation entirely");
    }

    #[test]
    fn test_word_end_forward_big_treats_punctuation_as_part_of_word() {
        let source = "foo.bar baz";
        let lines = compute_line_ranges(source);
        let c0 = Cursor { line: 0, col: 0 };
        let big = word_end_forward_big(&lines, source, c0);
        assert_eq!(big.col, 6, "end of the whole 'foo.bar' WORD");
    }

    #[test]
    fn test_word_back_big_skips_over_punctuation() {
        let source = "foo.bar baz";
        let lines = compute_line_ranges(source);
        let start = Cursor { line: 0, col: 8 }; // on 'b' of "baz"
        let big = word_back_big(&lines, source, start);
        assert_eq!(big.col, 0, "back over the whole 'foo.bar' WORD to its start");
    }

    #[test]
    fn test_find_char_forward_and_backward() {
        let source = "abcXdefXghi";
        let lines = compute_line_ranges(source);
        let start = Cursor { line: 0, col: 0 };
        let found = find_char_forward(&lines, source, start, 'X').unwrap();
        assert_eq!(found.col, 3);
        let found2 = find_char_forward(&lines, source, found, 'X').unwrap();
        assert_eq!(found2.col, 7);
        let back = find_char_backward(&lines, source, found2, 'X').unwrap();
        assert_eq!(back.col, 3);
        assert!(find_char_forward(&lines, source, found2, 'Z').is_none());
    }

    #[test]
    fn test_till_char_forward_and_backward_land_one_short() {
        let source = "abcXdefXghi";
        let lines = compute_line_ranges(source);
        let start = Cursor { line: 0, col: 0 };
        let till = till_char_forward(&lines, source, start, 'X').unwrap();
        assert_eq!(till.col, 2);
        let till_back = till_char_backward(&lines, source, Cursor { line: 0, col: 10 }, 'X').unwrap();
        assert_eq!(till_back.col, 8);
    }
}
