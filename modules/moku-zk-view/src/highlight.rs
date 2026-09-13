use ratatui::style::{Modifier, Style};
use satz_core::{ByteRange, Document};
use satz_core::parser::structure::{EmphasisKind, StructureOutput};

use moku_core::MokuTheme;

use crate::config::ZkViewPalette;
use crate::model::LinkResState;

/// Everything `style_for_offset` needs to classify one byte offset —
/// borrowed for the duration of styling one document's visible lines.
/// Plain/default text and task-checkbox state still come from the shared
/// `MokuTheme` (general app semantics: base text color, pending/done);
/// every markdown-specific color (heading levels, link/tag/code/quote/
/// rule/list-marker) comes from this module's own `ZkViewPalette` instead,
/// since `MokuTheme`'s fixed semantic fields don't have room for e.g.
/// three distinct heading colors. `list_marker_ranges`/`quote_marker_ranges`/
/// `checked_task_tail_ranges` are precomputed once per document open/reload
/// (see `render.rs`'s `compute_*` helpers) rather than re-scanned per char.
pub struct LineStyleCtx<'a> {
    pub doc: &'a Document,
    pub structure: &'a StructureOutput,
    pub link_res: &'a [LinkResState],
    pub theme: &'a MokuTheme,
    pub palette: &'a ZkViewPalette,
    pub list_marker_ranges: &'a [ByteRange],
    pub quote_marker_ranges: &'a [ByteRange],
    pub checked_task_tail_ranges: &'a [ByteRange],
}

/// Fixed precedence chain (highest first): code > list marker > link
/// (resolved/broken) > tag > task checkbox > heading (whole line) >
/// thematic break > blockquote marker (dim) > blockquote text > default —
/// with emphasis (bold/italic) and checked-task strikethrough layered on
/// top of whichever color rule above matched, since those are
/// `Modifier`s, not colors, and modifiers compose (see `apply_modifiers`).
pub fn style_for_offset(ctx: &LineStyleCtx, offset: usize) -> Style {
    if ctx.structure.code_spans.iter().any(|r| r.contains(offset)) {
        return Style::default().fg(ctx.palette.code_fg).bg(ctx.palette.code_bg);
    }
    if ctx.list_marker_ranges.iter().any(|r| r.contains(offset)) {
        return apply_modifiers(ctx, offset, Style::default().fg(ctx.palette.list_marker));
    }
    if let Some((idx, _)) = ctx
        .doc
        .links
        .iter()
        .enumerate()
        .find(|(_, l)| l.range.contains(offset))
    {
        let resolved = matches!(ctx.link_res.get(idx), Some(LinkResState::Resolved { .. }));
        let color = if resolved { ctx.palette.link_resolved } else { ctx.palette.link_broken };
        return apply_modifiers(
            ctx,
            offset,
            Style::default().fg(color).add_modifier(Modifier::UNDERLINED),
        );
    }
    if ctx.doc.tags.iter().any(|t| t.range.contains(offset)) {
        return apply_modifiers(ctx, offset, Style::default().fg(ctx.palette.tag));
    }
    if let Some(tm) = ctx
        .structure
        .task_markers
        .iter()
        .find(|t| t.range.contains(offset))
    {
        let color = if tm.checked { ctx.theme.success } else { ctx.theme.warning };
        return apply_modifiers(ctx, offset, Style::default().fg(color));
    }
    if let Some(h) = ctx.doc.headings.iter().find(|h| h.range.contains(offset)) {
        return heading_style(ctx.palette, h.level);
    }
    if ctx.structure.rule_spans.iter().any(|r| r.contains(offset)) {
        return Style::default().fg(ctx.palette.rule);
    }
    // Checked before the general blockquote rule below — a marker's range
    // is always a subset of some blockquote span, so the general rule
    // would otherwise always win first.
    if ctx.quote_marker_ranges.iter().any(|r| r.contains(offset)) {
        return Style::default().fg(ctx.palette.blockquote).add_modifier(Modifier::DIM);
    }
    if ctx
        .structure
        .blockquote_spans
        .iter()
        .any(|r| r.contains(offset))
    {
        return apply_modifiers(
            ctx,
            offset,
            Style::default().fg(ctx.palette.blockquote).add_modifier(Modifier::ITALIC),
        );
    }
    apply_modifiers(ctx, offset, Style::default().fg(ctx.theme.base_fg))
}

fn apply_modifiers(ctx: &LineStyleCtx, offset: usize, mut style: Style) -> Style {
    for e in &ctx.structure.emphasis_spans {
        if e.range.contains(offset) {
            style = style.add_modifier(match e.kind {
                EmphasisKind::Italic => Modifier::ITALIC,
                EmphasisKind::Bold => Modifier::BOLD,
            });
        }
    }
    if ctx.checked_task_tail_ranges.iter().any(|r| r.contains(offset)) {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    style
}

/// Ratatui text is fixed-width, so heading "size" is conveyed through
/// color/weight instead — from this module's own `ZkViewPalette`
/// (config-overridable), not `MokuTheme`. No underline: it read as a
/// "warning" indicator rather than a title, and (combined with the
/// heading line's own leading space) produced a stray-looking underscore
/// artifact — bold/italic/color alone carry the hierarchy.
pub fn heading_style(palette: &ZkViewPalette, level: u8) -> Style {
    match level {
        1 => Style::default().fg(palette.heading1).add_modifier(Modifier::BOLD),
        2 => Style::default()
            .fg(palette.heading2)
            .add_modifier(Modifier::BOLD | Modifier::ITALIC),
        3 => Style::default().fg(palette.heading3).add_modifier(Modifier::BOLD),
        _ => Style::default().fg(palette.heading_default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ZkViewColors;
    use satz_core::parser::structure::parse_structure;

    fn ctx_for<'a>(
        doc: &'a Document,
        structure: &'a StructureOutput,
        link_res: &'a [LinkResState],
        theme: &'a MokuTheme,
        palette: &'a ZkViewPalette,
    ) -> LineStyleCtx<'a> {
        LineStyleCtx {
            doc,
            structure,
            link_res,
            theme,
            palette,
            list_marker_ranges: &[],
            quote_marker_ranges: &[],
            checked_task_tail_ranges: &[],
        }
    }

    #[test]
    fn test_heading_level_1_gets_bold_no_underline() {
        let source = "# Title\n";
        let doc = satz_core::parse_document(source, std::path::Path::new("t.md"));
        let structure = parse_structure(source);
        let theme = MokuTheme::default();
        let palette = ZkViewPalette::from_colors(&ZkViewColors::default());
        let ctx = ctx_for(&doc, &structure, &[], &theme, &palette);
        let style = style_for_offset(&ctx, 2);
        assert_eq!(style.fg, Some(palette.heading1));
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(!style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn test_resolved_vs_broken_link_color() {
        let source = "[[a]] [[b]]\n";
        let doc = satz_core::parse_document(source, std::path::Path::new("t.md"));
        let structure = parse_structure(source);
        let theme = MokuTheme::default();
        let palette = ZkViewPalette::from_colors(&ZkViewColors::default());
        let link_res = vec![
            LinkResState::Resolved {
                target_id: satz_core::DocId::new("a.md"),
                target_path: "a.md".into(),
                anchor_line: 0,
            },
            LinkResState::DocMissing,
        ];
        let ctx = ctx_for(&doc, &structure, &link_res, &theme, &palette);
        let resolved_offset = doc.links[0].range.start;
        let broken_offset = doc.links[1].range.start;
        assert_eq!(style_for_offset(&ctx, resolved_offset).fg, Some(palette.link_resolved));
        assert_eq!(style_for_offset(&ctx, broken_offset).fg, Some(palette.link_broken));
    }

    #[test]
    fn test_task_checkbox_checked_vs_unchecked_color() {
        let source = "- [ ] todo\n- [x] done\n";
        let doc = satz_core::parse_document(source, std::path::Path::new("t.md"));
        let structure = parse_structure(source);
        let theme = MokuTheme::default();
        let palette = ZkViewPalette::from_colors(&ZkViewColors::default());
        let ctx = ctx_for(&doc, &structure, &[], &theme, &palette);
        let unchecked = structure.task_markers[0].range.start;
        let checked = structure.task_markers[1].range.start;
        assert_eq!(style_for_offset(&ctx, unchecked).fg, Some(theme.warning));
        assert_eq!(style_for_offset(&ctx, checked).fg, Some(theme.success));
    }

    #[test]
    fn test_code_span_uses_palette_bg_and_fg() {
        let source = "text `code` more\n";
        let doc = satz_core::parse_document(source, std::path::Path::new("t.md"));
        let structure = parse_structure(source);
        let theme = MokuTheme::default();
        let palette = ZkViewPalette::from_colors(&ZkViewColors::default());
        let ctx = ctx_for(&doc, &structure, &[], &theme, &palette);
        let offset = structure.code_spans[0].start;
        let style = style_for_offset(&ctx, offset);
        assert_eq!(style.fg, Some(palette.code_fg));
        assert_eq!(style.bg, Some(palette.code_bg));
    }

    #[test]
    fn test_list_marker_gets_accent_color() {
        let source = "- item one\n";
        let doc = satz_core::parse_document(source, std::path::Path::new("t.md"));
        let structure = parse_structure(source);
        let theme = MokuTheme::default();
        let palette = ZkViewPalette::from_colors(&ZkViewColors::default());
        let marker_ranges = vec![ByteRange::new(0, 2)];
        let ctx = LineStyleCtx {
            doc: &doc,
            structure: &structure,
            link_res: &[],
            theme: &theme,
            palette: &palette,
            list_marker_ranges: &marker_ranges,
            quote_marker_ranges: &[],
            checked_task_tail_ranges: &[],
        };
        assert_eq!(style_for_offset(&ctx, 0).fg, Some(palette.list_marker));
        assert_ne!(style_for_offset(&ctx, 3).fg, Some(palette.list_marker));
    }

    #[test]
    fn test_checked_task_tail_gets_strikethrough() {
        let source = "- [x] done text\n";
        let doc = satz_core::parse_document(source, std::path::Path::new("t.md"));
        let structure = parse_structure(source);
        let theme = MokuTheme::default();
        let palette = ZkViewPalette::from_colors(&ZkViewColors::default());
        let marker_end = structure.task_markers[0].range.end;
        let tail = vec![ByteRange::new(marker_end, source.len())];
        let ctx = LineStyleCtx {
            doc: &doc,
            structure: &structure,
            link_res: &[],
            theme: &theme,
            palette: &palette,
            list_marker_ranges: &[],
            quote_marker_ranges: &[],
            checked_task_tail_ranges: &tail,
        };
        let style = style_for_offset(&ctx, marker_end + 2);
        assert!(style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn test_quote_marker_is_dimmer_than_quoted_text() {
        let source = "> quoted text\n";
        let doc = satz_core::parse_document(source, std::path::Path::new("t.md"));
        let structure = parse_structure(source);
        let theme = MokuTheme::default();
        let palette = ZkViewPalette::from_colors(&ZkViewColors::default());
        let marker = vec![ByteRange::new(0, 2)]; // "> "
        let ctx = LineStyleCtx {
            doc: &doc,
            structure: &structure,
            link_res: &[],
            theme: &theme,
            palette: &palette,
            list_marker_ranges: &[],
            quote_marker_ranges: &marker,
            checked_task_tail_ranges: &[],
        };
        let marker_style = style_for_offset(&ctx, 0);
        let text_style = style_for_offset(&ctx, 5);
        assert!(marker_style.add_modifier.contains(Modifier::DIM));
        assert!(!text_style.add_modifier.contains(Modifier::DIM));
        assert!(text_style.add_modifier.contains(Modifier::ITALIC));
    }
}
