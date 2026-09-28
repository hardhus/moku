use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Row, Table};
use satz_core::{ByteRange, Frontmatter, Heading, LinkKind};
use satz_core::formatter::table::{TableBlock, parse_table_block};
use satz_core::parser::structure::StructureOutput;
use unicode_width::UnicodeWidthStr;

use moku_core::MokuTheme;

use crate::config::ZkViewPalette;
use crate::cursor::cursor_to_byte_offset;
use crate::highlight::{LineStyleCtx, heading_style, style_for_offset};
use crate::model::{Cursor, OpenDoc, RenderBlock};

/// Builds the short "Label: value" summary shown for a YAML frontmatter
/// block instead of raw `---`-fenced YAML — only non-empty fields appear.
fn build_frontmatter_entries(fm: &Frontmatter) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    if let Some(title) = &fm.title {
        let trimmed = title.trim();
        if !trimmed.is_empty() {
            entries.push(("Title".to_string(), trimmed.to_string()));
        }
    }
    if !fm.tags.is_empty() {
        let tags = fm.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" ");
        entries.push(("Tags".to_string(), tags));
    }
    if let Some(date) = &fm.date {
        let trimmed = date.trim();
        if !trimmed.is_empty() {
            entries.push(("Date".to_string(), trimmed.to_string()));
        }
    }
    if !fm.aliases.is_empty() {
        entries.push(("Aliases".to_string(), fm.aliases.join(", ")));
    }
    entries
}

/// Splits a document's lines into `RenderBlock`s: an optional leading
/// frontmatter summary, runs of plain text lines, and GFM tables (rendered
/// later as a real `ratatui::Table` widget). Computed once per open/reload
/// — purely structural, no theme dependency, so it never needs rebuilding
/// on a theme switch.
pub fn build_layout(
    source: &str,
    line_ranges: &[ByteRange],
    structure: &StructureOutput,
    frontmatter: &Frontmatter,
) -> Vec<RenderBlock> {
    let mut blocks = Vec::new();
    let mut line_cursor = 0usize;

    // A YAML frontmatter block always starts on the document's very first
    // line, so this always runs before any table can appear.
    if let Some(fm_range) = structure.frontmatter_range {
        if let Some(last_line) = line_ranges.iter().rposition(|r| r.end <= fm_range.end) {
            blocks.push(RenderBlock::Frontmatter {
                first_line: 0,
                last_line,
                entries: build_frontmatter_entries(frontmatter),
            });
            line_cursor = last_line + 1;
        }
    }

    let mut tables: Vec<ByteRange> = structure.table_spans.clone();
    tables.sort_by_key(|r| r.start);

    for table_span in &tables {
        let table_first_line = line_ranges
            .iter()
            .position(|r| r.start >= table_span.start)
            .unwrap_or(line_ranges.len());
        if table_first_line >= line_ranges.len() {
            continue;
        }
        if table_first_line > line_cursor {
            blocks.push(RenderBlock::Lines {
                first_line: line_cursor,
                line_count: table_first_line - line_cursor,
            });
        }
        match parse_table_block(source, *table_span) {
            Some(table) => {
                let table_last_line = line_ranges
                    .iter()
                    .rposition(|r| r.end <= table_span.end)
                    .unwrap_or(table_first_line);
                let delim_line = (table_first_line + 1).min(table_last_line);
                blocks.push(RenderBlock::Table {
                    first_line: table_first_line,
                    delim_line,
                    last_line: table_last_line,
                    table,
                });
                line_cursor = table_last_line + 1;
            }
            // A span pulldown-cmark flagged as a table but that this
            // stricter re-parse rejects (malformed edge case) is left for
            // the next text segment to pick up — never dropped, never a
            // panic.
            None => {}
        }
    }
    if line_cursor < line_ranges.len() {
        blocks.push(RenderBlock::Lines {
            first_line: line_cursor,
            line_count: line_ranges.len() - line_cursor,
        });
    }
    blocks
}

fn scan_list_marker_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut i = 0;
    if i < bytes.len() && bytes[i].is_ascii_digit() {
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i < bytes.len() && (bytes[i] == b'.' || bytes[i] == b')') {
            i += 1;
        } else {
            return 0;
        }
    } else if i < bytes.len() && matches!(bytes[i], b'-' | b'*' | b'+') {
        i += 1;
    } else {
        return 0;
    }
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    i
}

/// Byte ranges of each list item's own marker glyph (`-`/`*`/`+`/`1.`/`1)`
/// + trailing whitespace) — `ListItemSpan::range` is documented (in
/// satz-core) to start exactly at the marker's first byte, so this is a
/// plain prefix scan, not a search.
pub fn compute_list_marker_ranges(source: &str, structure: &StructureOutput) -> Vec<ByteRange> {
    structure
        .list_items
        .iter()
        .filter_map(|item| {
            let end = item.range.end.min(source.len());
            if item.range.start >= end {
                return None;
            }
            let text = &source[item.range.start..end];
            let len = scan_list_marker_len(text);
            (len > 0).then(|| ByteRange::new(item.range.start, item.range.start + len))
        })
        .collect()
}

/// Byte ranges of each blockquote line's leading `>` (one per nesting
/// level, each optionally followed by one space) prefix.
pub fn compute_quote_marker_ranges(source: &str, line_ranges: &[ByteRange], structure: &StructureOutput) -> Vec<ByteRange> {
    let mut out = Vec::new();
    for bq in &structure.blockquote_spans {
        for &line in line_ranges {
            if line.start < bq.start || line.start >= bq.end {
                continue;
            }
            let text = &source[line.start..line.end];
            let bytes = text.as_bytes();
            let mut i = 0;
            while i < bytes.len() && bytes[i] == b'>' {
                i += 1;
                if i < bytes.len() && bytes[i] == b' ' {
                    i += 1;
                }
            }
            if i > 0 {
                out.push(ByteRange::new(line.start, line.start + i));
            }
        }
    }
    out
}

/// Byte ranges from a *checked* task's `[x]` marker to the end of its
/// line — struck through; unchecked tasks are left alone.
pub fn compute_checked_task_tail_ranges(line_ranges: &[ByteRange], structure: &StructureOutput) -> Vec<ByteRange> {
    structure
        .task_markers
        .iter()
        .filter(|t| t.checked)
        .filter_map(|t| {
            line_ranges
                .iter()
                .find(|l| l.contains(t.range.start))
                .map(|l| ByteRange::new(t.range.end, l.end))
        })
        .collect()
}

/// Source line indices that are a fenced code block's opening or closing
/// ` ``` ` delimiter line — rendered as a plain `code_bg` band instead of
/// the literal backticks.
pub fn compute_code_fence_delim_lines(line_ranges: &[ByteRange], structure: &StructureOutput) -> Vec<usize> {
    let mut out = Vec::new();
    for span in &structure.code_fence_spans {
        if let Some(first) = line_ranges.iter().position(|r| r.start >= span.start) {
            out.push(first);
        }
        if let Some(last) = line_ranges.iter().rposition(|r| r.end <= span.end) {
            out.push(last);
        }
    }
    out
}

pub fn block_row_height(block: &RenderBlock) -> usize {
    match block {
        RenderBlock::Lines { line_count, .. } => (*line_count).max(1),
        RenderBlock::Table { table, .. } => 1 + table.rows.len(),
        RenderBlock::Frontmatter { entries, .. } => entries.len().max(1),
    }
}

pub fn compute_row_offsets(blocks: &[RenderBlock]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(blocks.len());
    let mut acc = 0usize;
    for b in blocks {
        offsets.push(acc);
        acc += block_row_height(b);
    }
    offsets
}

pub fn total_rows(blocks: &[RenderBlock]) -> usize {
    blocks.iter().map(block_row_height).sum()
}

/// The render row (flattened scroll-space) that source `line` falls on —
/// used to keep the viewport following the cursor. Every source line
/// inside a `Table`/`Frontmatter` block maps to that block's own top row:
/// neither has a per-source-line rendered position of its own.
pub fn line_to_row(blocks: &[RenderBlock], row_offsets: &[usize], line: usize) -> usize {
    for (i, b) in blocks.iter().enumerate() {
        match b {
            RenderBlock::Lines { first_line, line_count } => {
                if line >= *first_line && line < first_line + line_count {
                    return row_offsets[i] + (line - first_line);
                }
            }
            RenderBlock::Table { first_line, last_line, .. }
            | RenderBlock::Frontmatter { first_line, last_line, .. } => {
                if line >= *first_line && line <= *last_line {
                    return row_offsets[i];
                }
            }
        }
    }
    row_offsets.last().copied().unwrap_or(0)
}

/// One piece of a rendered line: either the raw source text (styled
/// per-char as before) or a shorter "concealed" substitute — e.g. a link's
/// display text standing in for its full `[text](url)`/`[[target|text]]`
/// syntax. Concealment is cursor-aware: a span only conceals when the
/// cursor isn't currently positioned inside it (see `build_line_segments`),
/// so moving onto a link reveals its real source syntax.
enum LineSeg {
    Verbatim(ByteRange),
    Concealed { text: String, style: Style },
}

/// A link's fallback label when it has no explicit `|display`/`[text]`:
/// prefer the target document, then a heading/block anchor, then a plain
/// placeholder — never the empty string (which would make the link
/// disappear entirely).
fn link_label(l: &satz_core::Link) -> String {
    if let Some(d) = &l.display {
        return d.clone();
    }
    if !l.target_doc.is_empty() {
        return l.target_doc.clone();
    }
    if let Some(h) = &l.target_heading {
        return format!("#{h}");
    }
    if let Some(b) = &l.target_block {
        return format!("^{b}");
    }
    "(this document)".to_string()
}

/// Splits one line's raw byte range into `Verbatim`/`Concealed` segments.
/// Every link starting on this line conceals to its label UNLESS
/// `cursor_offset` (this line's cursor byte offset, if the cursor is on
/// this line at all) falls inside that exact link's range — footnote
/// references (`[^1]`) are excluded, they're already short enough that
/// hiding them loses more than it gains.
fn build_line_segments(ctx: &LineStyleCtx, r: ByteRange, cursor_offset: Option<usize>) -> Vec<LineSeg> {
    let mut candidates: Vec<(ByteRange, String, Style)> = ctx
        .doc
        .links
        .iter()
        .filter(|l| l.kind != LinkKind::Footnote)
        .filter(|l| l.range.start >= r.start && l.range.start < r.end)
        .filter(|l| !cursor_offset.is_some_and(|off| l.range.contains(off)))
        .map(|l| {
            let text = link_label(l);
            let style = style_for_offset(ctx, l.range.start.max(r.start));
            (l.range, text, style)
        })
        .collect();
    candidates.sort_by_key(|(range, _, _)| range.start);

    let mut segs = Vec::new();
    let mut pos = r.start;
    for (range, text, style) in candidates {
        let start = range.start.max(r.start);
        let end = range.end.min(r.end);
        if start < pos || start >= end {
            continue;
        }
        if start > pos {
            segs.push(LineSeg::Verbatim(ByteRange::new(pos, start)));
        }
        segs.push(LineSeg::Concealed { text, style });
        pos = end;
    }
    if pos < r.end {
        segs.push(LineSeg::Verbatim(ByteRange::new(pos, r.end)));
    }
    segs
}

/// Renders one line to a styled `Line`, plus — when `cursor_offset` is
/// `Some` (this is the cursor's own line) — the *visual* column the
/// cursor should be drawn at. That visual column can differ from the
/// cursor's raw char column whenever an earlier concealed span on the
/// same line rendered fewer characters than its raw source had.
fn render_line(source: &str, ctx: &LineStyleCtx, r: ByteRange, cursor_offset: Option<usize>) -> (Line<'static>, Option<usize>) {
    let segs = build_line_segments(ctx, r, cursor_offset);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut visual_col = 0usize;
    let mut cursor_visual: Option<usize> = None;

    for seg in segs {
        match seg {
            LineSeg::Verbatim(range) => {
                if range.is_empty() {
                    continue;
                }
                let text = &source[range.start..range.end];
                let mut run = String::new();
                let mut run_style: Option<Style> = None;
                for (byte_off, ch) in text.char_indices() {
                    let abs_offset = range.start + byte_off;
                    if cursor_offset == Some(abs_offset) {
                        cursor_visual = Some(visual_col);
                    }
                    let style = style_for_offset(ctx, abs_offset);
                    match run_style {
                        Some(s) if s == style => run.push(ch),
                        _ => {
                            // A non-empty run always has a style (set when its first char was pushed).
                            if let Some(s) = run_style.filter(|_| !run.is_empty()) {
                                spans.push(Span::styled(std::mem::take(&mut run), s));
                            }
                            run.push(ch);
                            run_style = Some(style);
                        }
                    }
                    visual_col += 1;
                }
                if let Some(s) = run_style.filter(|_| !run.is_empty()) {
                    spans.push(Span::styled(run, s));
                }
            }
            LineSeg::Concealed { text, style } => {
                visual_col += text.chars().count();
                spans.push(Span::styled(text, style));
            }
        }
    }

    if cursor_offset.is_some() && cursor_visual.is_none() {
        // Cursor's raw offset never matched a rendered char (e.g. an empty
        // line, or resting exactly at the line's end) — place it at the
        // line's final visual column instead of dropping the overlay.
        cursor_visual = Some(visual_col);
    }

    (Line::from(spans), cursor_visual)
}

/// Approximates where a heading's clean `Heading.text` content starts
/// within its raw source line (first non-`#`, non-whitespace char) — used
/// to translate the cursor's raw column onto the concealed heading text.
fn heading_content_start_col(raw_line: &str) -> usize {
    raw_line
        .chars()
        .position(|c| c != '#' && !c.is_whitespace())
        .unwrap_or(0)
}

fn heading_on_line<'a>(ctx: &LineStyleCtx<'a>, r: ByteRange) -> Option<&'a Heading> {
    ctx.doc.headings.iter().find(|h| r.contains(h.range.start))
}

fn overlay_cursor(line: Line<'static>, col: usize) -> Line<'static> {
    let mut chars: Vec<(char, Style)> = Vec::new();
    for span in &line.spans {
        for c in span.content.chars() {
            chars.push((c, span.style));
        }
    }
    if chars.is_empty() {
        chars.push((' ', Style::default()));
    }
    let idx = col.min(chars.len() - 1);
    // Reverse-video: swaps whatever fg/bg the cell already has, so the
    // cursor stays visible over any theme/background (including a fully
    // transparent one) — the same guarantee a real terminal cursor gives,
    // rather than depending on `selection_bg` being a non-transparent color.
    chars[idx].1 = chars[idx].1.add_modifier(Modifier::REVERSED);

    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for (c, style) in chars {
        match run_style {
            Some(s) if s == style => run.push(c),
            _ => {
                // A non-empty run always has a style (set when its first char was pushed).
                if let Some(s) = run_style.filter(|_| !run.is_empty()) {
                    spans.push(Span::styled(std::mem::take(&mut run), s));
                }
                run.push(c);
                run_style = Some(style);
            }
        }
    }
    if let Some(s) = run_style.filter(|_| !run.is_empty()) {
        spans.push(Span::styled(run, s));
    }
    Line::from(spans)
}

fn column_widths(table: &TableBlock, area_width: u16) -> Vec<Constraint> {
    let col_count = table.alignments.len().max(1);
    let mut widths = vec![3usize; col_count];
    for (i, w) in widths.iter_mut().enumerate() {
        if let Some(cell) = table.header.get(i) {
            *w = (*w).max(UnicodeWidthStr::width(cell.as_str()));
        }
        for row in &table.rows {
            if let Some(cell) = row.get(i) {
                *w = (*w).max(UnicodeWidthStr::width(cell.as_str()));
            }
        }
    }
    let total: usize = widths.iter().sum::<usize>() + col_count * 3;
    if total > area_width as usize && col_count > 0 {
        return (0..col_count)
            .map(|_| Constraint::Ratio(1, col_count as u32))
            .collect();
    }
    widths.into_iter().map(|w| Constraint::Length(w as u16 + 2)).collect()
}

/// Draws the document body: flattens visible `RenderBlock`s (top-clamped
/// scroll, `scroll_top` in render-row space) into `area`, drawing plain
/// text as styled `Line`s (headings shown clean, without their `#`
/// markers; links concealed to their display text unless the cursor sits
/// on them; code-fence delimiter lines and thematic breaks rendered
/// specially), a frontmatter block as a short "Label: value" summary, and
/// tables as a real `ratatui::Table` sliced to just the currently-visible
/// rows/header, with a colored header band and very light zebra-dimmed
/// data rows.
#[allow(clippy::too_many_arguments)]
pub fn draw_document(
    frame: &mut Frame,
    area: Rect,
    open: &OpenDoc,
    scroll_top: usize,
    cursor: Cursor,
    show_cursor: bool,
    theme: &MokuTheme,
    palette: &ZkViewPalette,
) {
    let window = area.height as usize;
    if window == 0 {
        return;
    }
    let visible_end = scroll_top + window;

    let mut segments: Vec<usize> = Vec::new();
    for (i, b) in open.blocks.iter().enumerate() {
        let top = open.row_offsets[i];
        let height = block_row_height(b);
        if top + height <= scroll_top || top >= visible_end {
            continue;
        }
        segments.push(i);
    }
    if segments.is_empty() {
        return;
    }

    let constraints: Vec<Constraint> = segments
        .iter()
        .map(|&i| {
            let top = open.row_offsets[i];
            let height = block_row_height(&open.blocks[i]);
            let visible_top = top.max(scroll_top);
            let visible_bottom = (top + height).min(visible_end);
            Constraint::Length((visible_bottom - visible_top) as u16)
        })
        .collect();

    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let ctx = LineStyleCtx {
        doc: &open.doc,
        structure: &open.structure,
        link_res: &open.link_res,
        theme,
        palette,
        list_marker_ranges: &open.list_marker_ranges,
        quote_marker_ranges: &open.quote_marker_ranges,
        checked_task_tail_ranges: &open.checked_task_tail_ranges,
    };
    let cursor_byte_offset = cursor_to_byte_offset(&open.line_ranges, &open.source, cursor);

    for (seg_idx, &block_idx) in segments.iter().enumerate() {
        let rect = areas[seg_idx];
        let top = open.row_offsets[block_idx];
        match &open.blocks[block_idx] {
            RenderBlock::Lines { first_line, line_count } => {
                let skip = scroll_top.saturating_sub(top);
                let take = rect.height as usize;
                let mut lines: Vec<Line<'static>> = Vec::with_capacity(take);
                for offset in skip..(skip + take).min(*line_count) {
                    let line_idx = first_line + offset;
                    let r = open.line_ranges[line_idx];
                    let is_cursor_line = show_cursor && line_idx == cursor.line;
                    let cursor_offset_here = is_cursor_line.then_some(cursor_byte_offset);

                    let (mut styled, cursor_visual) = if let Some(heading) = heading_on_line(&ctx, r) {
                        // Headings always render clean (no `#` markers) —
                        // there's no hidden information worth revealing on
                        // cursor, unlike a link's target.
                        let raw_line = &open.source[r.start..r.end];
                        let content_start = heading_content_start_col(raw_line);
                        let visual = is_cursor_line.then(|| {
                            let text_len = heading.text.chars().count();
                            cursor.col.saturating_sub(content_start).min(text_len.saturating_sub(1))
                        });
                        (
                            Line::styled(heading.text.clone(), heading_style(palette, heading.level)),
                            visual,
                        )
                    } else if open.code_fence_delim_lines.contains(&line_idx) {
                        // Hide the literal ` ``` ` fence — a plain colored
                        // band top/bottom of the code block, matching the
                        // code content's own background.
                        (
                            Line::styled(" ".repeat(rect.width as usize), Style::default().bg(palette.code_bg)),
                            None,
                        )
                    } else if ctx.structure.rule_spans.iter().any(|rr| rr.contains(r.start)) {
                        // A thematic break drawn full-width instead of its
                        // literal (often just 3-char) `---`/`***` source.
                        (
                            Line::styled("-".repeat(rect.width as usize), Style::default().fg(palette.rule)),
                            None,
                        )
                    } else {
                        render_line(&open.source, &ctx, r, cursor_offset_here)
                    };

                    if let Some(col) = cursor_visual {
                        styled = overlay_cursor(styled, col);
                    }
                    lines.push(styled);
                }
                let para = Paragraph::new(lines).style(Style::default().bg(theme.base_bg));
                frame.render_widget(para, rect);
            }
            RenderBlock::Frontmatter { entries, .. } => {
                let skip = scroll_top.saturating_sub(top);
                let take = rect.height as usize;
                let lines: Vec<Line<'static>> = entries
                    .iter()
                    .skip(skip)
                    .take(take)
                    .map(|(label, value)| {
                        Line::from(vec![
                            Span::styled(
                                format!("{label}: "),
                                Style::default().fg(palette.tag).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(value.clone(), Style::default().fg(theme.base_fg)),
                        ])
                    })
                    .collect();
                let para = Paragraph::new(lines).style(Style::default().bg(theme.base_bg));
                frame.render_widget(para, rect);
            }
            RenderBlock::Table { table, .. } => {
                let header_visible = scroll_top <= top;
                let skip = scroll_top.saturating_sub(top + 1);
                let take = rect.height as usize;
                let widths = column_widths(table, rect.width);
                let header_row = Row::new(table.header.clone()).style(
                    Style::default()
                        .fg(palette.table_header_fg)
                        .bg(palette.table_header_bg)
                        .add_modifier(Modifier::BOLD),
                );
                let data_take = if header_visible { take.saturating_sub(1) } else { take };
                let rows: Vec<Row> = table
                    .rows
                    .iter()
                    .enumerate()
                    .skip(skip)
                    .take(data_take)
                    .map(|(i, r)| {
                        let row = Row::new(r.clone());
                        // Very light zebra: alternate rows just get a touch
                        // dimmer text, no background tint — the app's base
                        // background is deliberately transparent, and a
                        // solid alt-row color would fight that.
                        if i % 2 == 1 {
                            row.style(Style::default().add_modifier(Modifier::DIM))
                        } else {
                            row
                        }
                    })
                    .collect();
                let mut t = Table::new(rows, widths)
                    .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
                    .column_spacing(1);
                if header_visible {
                    t = t.header(header_row);
                }
                frame.render_widget(t, rect);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ZkViewColors;
    use crate::cursor::compute_line_ranges;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn open_doc_for(source: &str) -> OpenDoc {
        let doc = satz_core::parse_document(source, std::path::Path::new("t.md"));
        let structure = satz_core::parser::structure::parse_structure(source);
        let line_ranges = compute_line_ranges(source);
        let blocks = build_layout(source, &line_ranges, &structure, &doc.frontmatter);
        let row_offsets = compute_row_offsets(&blocks);
        let rows = total_rows(&blocks);
        let list_marker_ranges = compute_list_marker_ranges(source, &structure);
        let quote_marker_ranges = compute_quote_marker_ranges(source, &line_ranges, &structure);
        let checked_task_tail_ranges = compute_checked_task_tail_ranges(&line_ranges, &structure);
        let code_fence_delim_lines = compute_code_fence_delim_lines(&line_ranges, &structure);
        OpenDoc {
            doc,
            source: source.to_string(),
            structure,
            line_ranges,
            link_res: Vec::new(),
            blocks,
            row_offsets,
            total_rows: rows,
            list_marker_ranges,
            quote_marker_ranges,
            checked_task_tail_ranges,
            code_fence_delim_lines,
        }
    }

    fn draw(open: &OpenDoc, scroll_top: usize, cursor: Cursor, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let theme = MokuTheme::default();
        let palette = ZkViewPalette::from_colors(&ZkViewColors::default());
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                draw_document(f, area, open, scroll_top, cursor, true, &theme, &palette);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        buf.content.iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn test_build_layout_splits_table_from_surrounding_text() {
        let source = "before\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\nafter\n";
        let open = open_doc_for(source);
        assert_eq!(open.blocks.len(), 3);
        assert!(matches!(open.blocks[0], RenderBlock::Lines { .. }));
        assert!(matches!(open.blocks[1], RenderBlock::Table { .. }));
        assert!(matches!(open.blocks[2], RenderBlock::Lines { .. }));
    }

    #[test]
    fn test_draw_document_renders_heading_and_table_content() {
        let source = "# Title\n\n| A | B |\n|---|---|\n| 1 | 22 |\n";
        let open = open_doc_for(source);
        let content = buffer_text(&draw(&open, 0, Cursor::default(), 40, 10));
        assert!(content.contains("Title"));
        assert!(content.contains("22"));
    }

    #[test]
    fn test_heading_hides_hash_markers() {
        let source = "## Section Title\n";
        let open = open_doc_for(source);
        let content = buffer_text(&draw(&open, 0, Cursor::default(), 40, 5));
        assert!(content.contains("Section Title"));
        assert!(!content.contains('#'));
    }

    #[test]
    fn test_markdown_link_conceals_url_unless_cursor_on_it() {
        let source = "See [Website](https://example.com) here.\n";
        let open = open_doc_for(source);

        // Cursor elsewhere on the line — url must be hidden, only the
        // display text shown.
        let away = buffer_text(&draw(&open, 0, Cursor { line: 0, col: 0 }, 60, 5));
        assert!(away.contains("Website"));
        assert!(!away.contains("example.com"));

        // Cursor exactly on the link — raw syntax (including the url)
        // must be revealed.
        let link_start = source.find("[Website]").unwrap();
        let lines = compute_line_ranges(source);
        let cursor = crate::cursor::byte_offset_to_cursor(&lines, source, link_start);
        let on_link = buffer_text(&draw(&open, 0, cursor, 60, 5));
        assert!(on_link.contains("example.com"));
    }

    #[test]
    fn test_wikilink_display_conceals_target_unless_cursor_on_it() {
        let source = "x [[tlp/sozluk#Olgu|olguların]] y\n";
        let open = open_doc_for(source);
        // Cursor on the leading 'x', clearly off the link — must conceal.
        let away = buffer_text(&draw(&open, 0, Cursor { line: 0, col: 0 }, 60, 5));
        assert!(away.contains("olguların"));
        assert!(!away.contains("tlp/sozluk"));

        // Cursor exactly on the link — raw target must be revealed.
        let link_start = source.find("[[").unwrap();
        let lines = compute_line_ranges(source);
        let cursor = crate::cursor::byte_offset_to_cursor(&lines, source, link_start);
        let on_link = buffer_text(&draw(&open, 0, cursor, 60, 5));
        assert!(on_link.contains("tlp/sozluk"));
    }

    #[test]
    fn test_draw_document_cursor_overlay_uses_reversed_modifier() {
        let source = "abc\n";
        let open = open_doc_for(source);
        let buf = draw(&open, 0, Cursor { line: 0, col: 1 }, 10, 3);
        let cell = buf.cell((1, 0)).unwrap();
        assert!(cell.modifier.contains(Modifier::REVERSED));
        let other = buf.cell((0, 0)).unwrap();
        assert!(!other.modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn test_cursor_overlay_accounts_for_earlier_concealed_link() {
        // "[[a]] b" — the wikilink conceals from 5 raw chars ("[[a]]") down
        // to 1 visual char ("a"), so the cursor resting on the raw 'b'
        // (raw col 6) must land on VISUAL col 2 ("a", space, b), not 6.
        let source = "[[a]] b\n";
        let open = open_doc_for(source);
        let cursor = Cursor { line: 0, col: 6 };
        let buf = draw(&open, 0, cursor, 20, 3);
        let cell = buf.cell((2, 0)).unwrap();
        assert_eq!(cell.symbol(), "b");
        assert!(cell.modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn test_frontmatter_renders_clean_summary_not_raw_yaml() {
        let source = "---\ntitle: My Note\ntags: [a, b]\n---\n\n# Body\n";
        let open = open_doc_for(source);
        assert!(matches!(open.blocks[0], RenderBlock::Frontmatter { .. }));
        let content = buffer_text(&draw(&open, 0, Cursor::default(), 60, 10));
        assert!(content.contains("Title"));
        assert!(content.contains("My Note"));
        assert!(content.contains("#a"));
        assert!(!content.contains("---"));
    }

    #[test]
    fn test_frontmatter_is_a_single_cursor_stop() {
        let source = "---\ntitle: X\ntags: [a]\ndate: 2026-01-01\n---\n\nbody\n";
        let open = open_doc_for(source);
        let RenderBlock::Frontmatter { last_line, .. } = &open.blocks[0] else {
            panic!("expected a Frontmatter block first, got {:?}", {
                match &open.blocks[0] {
                    RenderBlock::Lines { .. } => "Lines",
                    RenderBlock::Table { .. } => "Table",
                    RenderBlock::Frontmatter { .. } => "Frontmatter",
                }
            });
        };
        // Moving down from the frontmatter's first (only reachable) line
        // must skip straight past its remaining raw lines to whatever
        // comes right after the closing fence.
        let start = Cursor { line: 0, col: 0 };
        let next = crate::cursor::move_down(&open.blocks, &open.line_ranges, &open.source, start);
        assert_eq!(next.line, last_line + 1);
    }

    #[test]
    fn test_code_fence_lines_hide_backticks() {
        let source = "```rust\nlet x = 1;\n```\n";
        let open = open_doc_for(source);
        let content = buffer_text(&draw(&open, 0, Cursor::default(), 40, 5));
        assert!(content.contains("let x"));
        assert!(!content.contains("```"));
    }

    #[test]
    fn test_full_width_rule_fills_available_width() {
        let source = "before\n\n---\n\nafter\n";
        let open = open_doc_for(source);
        let buf = draw(&open, 0, Cursor::default(), 20, 10);
        // Row 2 (the rule line) should be filled with '-' across the whole
        // width, not just the raw 3-char "---".
        for x in 0..20u16 {
            assert_eq!(buf.cell((x, 2)).unwrap().symbol(), "-");
        }
    }
}
