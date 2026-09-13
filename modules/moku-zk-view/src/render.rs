use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Row, Table};
use satz_core::ByteRange;
use satz_core::formatter::table::{TableBlock, parse_table_block};
use satz_core::parser::structure::StructureOutput;
use unicode_width::UnicodeWidthStr;

use moku_core::MokuTheme;

use crate::highlight::{LineStyleCtx, style_for_offset};
use crate::model::{Cursor, OpenDoc, RenderBlock};

/// Splits a document's lines into `RenderBlock`s: a run of plain text
/// lines, or a GFM table (rendered later as a real `ratatui::Table`
/// widget). Computed once per open/reload — purely structural, no theme
/// dependency, so it never needs rebuilding on a theme switch.
pub fn build_layout(source: &str, line_ranges: &[ByteRange], structure: &StructureOutput) -> Vec<RenderBlock> {
    let mut tables: Vec<ByteRange> = structure.table_spans.clone();
    tables.sort_by_key(|r| r.start);

    let mut blocks = Vec::new();
    let mut line_cursor = 0usize;

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

pub fn block_row_height(block: &RenderBlock) -> usize {
    match block {
        RenderBlock::Lines { line_count, .. } => (*line_count).max(1),
        RenderBlock::Table { table, .. } => 1 + table.rows.len(),
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
/// inside a `Table` block maps to that block's own top row: a table has
/// no per-source-line rendered position of its own.
pub fn line_to_row(blocks: &[RenderBlock], row_offsets: &[usize], line: usize) -> usize {
    for (i, b) in blocks.iter().enumerate() {
        match b {
            RenderBlock::Lines { first_line, line_count } => {
                if line >= *first_line && line < first_line + line_count {
                    return row_offsets[i] + (line - first_line);
                }
            }
            RenderBlock::Table { first_line, last_line, .. } => {
                if line >= *first_line && line <= *last_line {
                    return row_offsets[i];
                }
            }
        }
    }
    row_offsets.last().copied().unwrap_or(0)
}

fn style_line(source: &str, ctx: &LineStyleCtx, r: ByteRange) -> Line<'static> {
    let text = &source[r.start..r.end];
    if text.is_empty() {
        return Line::from("");
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for (byte_off, ch) in text.char_indices() {
        let style = style_for_offset(ctx, r.start + byte_off);
        match run_style {
            Some(s) if s == style => run.push(ch),
            _ => {
                if !run.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut run), run_style.unwrap()));
                }
                run.push(ch);
                run_style = Some(style);
            }
        }
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style.unwrap()));
    }
    Line::from(spans)
}

fn overlay_cursor(line: Line<'static>, col: usize, theme: &MokuTheme) -> Line<'static> {
    let mut chars: Vec<(char, Style)> = Vec::new();
    for span in &line.spans {
        for c in span.content.chars() {
            chars.push((c, span.style));
        }
    }
    if chars.is_empty() {
        chars.push((' ', Style::default()));
    }
    if col < chars.len() {
        chars[col].1 = Style::default().fg(theme.selection_fg).bg(theme.selection_bg);
    }
    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for (c, style) in chars {
        match run_style {
            Some(s) if s == style => run.push(c),
            _ => {
                if !run.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut run), run_style.unwrap()));
                }
                run.push(c);
                run_style = Some(style);
            }
        }
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style.unwrap()));
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
/// text as styled `Line`s and tables as a real `ratatui::Table` sliced to
/// just the currently-visible rows/header.
#[allow(clippy::too_many_arguments)]
pub fn draw_document(
    frame: &mut Frame,
    area: Rect,
    open: &OpenDoc,
    scroll_top: usize,
    cursor: Cursor,
    show_cursor: bool,
    theme: &MokuTheme,
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
    };

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
                    let mut styled = style_line(&open.source, &ctx, open.line_ranges[line_idx]);
                    if show_cursor && line_idx == cursor.line {
                        styled = overlay_cursor(styled, cursor.col, theme);
                    }
                    lines.push(styled);
                }
                let para = Paragraph::new(lines).style(Style::default().bg(theme.base_bg));
                frame.render_widget(para, rect);
            }
            RenderBlock::Table { table, .. } => {
                let header_visible = scroll_top <= top;
                let skip = scroll_top.saturating_sub(top + 1);
                let take = rect.height as usize;
                let widths = column_widths(table, rect.width);
                let header_row = Row::new(table.header.clone())
                    .style(Style::default().fg(theme.info).add_modifier(Modifier::BOLD));
                let data_take = if header_visible { take.saturating_sub(1) } else { take };
                let rows: Vec<Row> = table
                    .rows
                    .iter()
                    .skip(skip)
                    .take(data_take)
                    .map(|r| Row::new(r.clone()))
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
    use crate::cursor::compute_line_ranges;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn open_doc_for(source: &str) -> OpenDoc {
        let doc = satz_core::parse_document(source, std::path::Path::new("t.md"));
        let structure = satz_core::parser::structure::parse_structure(source);
        let line_ranges = compute_line_ranges(source);
        let blocks = build_layout(source, &line_ranges, &structure);
        let row_offsets = compute_row_offsets(&blocks);
        let rows = total_rows(&blocks);
        OpenDoc {
            doc,
            source: source.to_string(),
            structure,
            line_ranges,
            link_res: Vec::new(),
            blocks,
            row_offsets,
            total_rows: rows,
        }
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
        let theme = MokuTheme::default();

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                draw_document(f, area, &open, 0, Cursor::default(), true, &theme);
            })
            .unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains("Title"));
        assert!(content.contains("22"));
    }

    #[test]
    fn test_draw_document_cursor_overlay_at_correct_cell() {
        let source = "abc\n";
        let open = open_doc_for(source);
        let theme = MokuTheme::default();
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                draw_document(f, area, &open, 0, Cursor { line: 0, col: 1 }, true, &theme);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let cell = buf.cell((1, 0)).unwrap();
        assert_eq!(cell.fg, theme.selection_fg);
        assert_eq!(cell.bg, theme.selection_bg);
        let other = buf.cell((0, 0)).unwrap();
        // The default theme's selection_bg happens to equal its ordinary
        // background (both "Reset"), so bg alone can't distinguish the
        // cursor cell from a plain one — fg does (base_fg=White vs
        // selection_fg=Yellow by default), and is what actually renders
        // visibly distinct in a real terminal either way.
        assert_ne!(other.fg, theme.selection_fg);
    }
}
