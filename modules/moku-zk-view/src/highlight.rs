use ratatui::style::{Modifier, Style};
use satz_core::Document;
use satz_core::parser::structure::{EmphasisKind, StructureOutput};

use moku_core::MokuTheme;

use crate::model::LinkResState;

/// Everything `style_for_offset` needs to classify one byte offset —
/// borrowed for the duration of styling one document's visible lines.
pub struct LineStyleCtx<'a> {
    pub doc: &'a Document,
    pub structure: &'a StructureOutput,
    pub link_res: &'a [LinkResState],
    pub theme: &'a MokuTheme,
}

/// Fixed precedence chain (highest first): code > link (resolved/broken) >
/// tag > task checkbox > heading (whole line) > thematic break >
/// blockquote > default — with emphasis (bold/italic) layered on top of
/// whichever color rule above matched, since it's a `Modifier`, not a
/// color, and modifiers compose.
pub fn style_for_offset(ctx: &LineStyleCtx, offset: usize) -> Style {
    if ctx.structure.code_spans.iter().any(|r| r.contains(offset)) {
        return Style::default().fg(ctx.theme.base_fg).bg(ctx.theme.border);
    }
    if let Some((idx, _)) = ctx
        .doc
        .links
        .iter()
        .enumerate()
        .find(|(_, l)| l.range.contains(offset))
    {
        let resolved = matches!(ctx.link_res.get(idx), Some(LinkResState::Resolved { .. }));
        let color = if resolved { ctx.theme.success } else { ctx.theme.error };
        return apply_emphasis(
            ctx,
            offset,
            Style::default().fg(color).add_modifier(Modifier::UNDERLINED),
        );
    }
    if ctx.doc.tags.iter().any(|t| t.range.contains(offset)) {
        return apply_emphasis(ctx, offset, Style::default().fg(ctx.theme.info));
    }
    if let Some(tm) = ctx
        .structure
        .task_markers
        .iter()
        .find(|t| t.range.contains(offset))
    {
        let color = if tm.checked { ctx.theme.success } else { ctx.theme.warning };
        return apply_emphasis(ctx, offset, Style::default().fg(color));
    }
    if let Some(h) = ctx.doc.headings.iter().find(|h| h.range.contains(offset)) {
        return heading_style(ctx.theme, h.level);
    }
    if ctx.structure.rule_spans.iter().any(|r| r.contains(offset)) {
        return Style::default().fg(ctx.theme.border);
    }
    if ctx
        .structure
        .blockquote_spans
        .iter()
        .any(|r| r.contains(offset))
    {
        return apply_emphasis(
            ctx,
            offset,
            Style::default()
                .fg(ctx.theme.base_fg)
                .add_modifier(Modifier::ITALIC | Modifier::DIM),
        );
    }
    apply_emphasis(ctx, offset, Style::default().fg(ctx.theme.base_fg))
}

fn apply_emphasis(ctx: &LineStyleCtx, offset: usize, mut style: Style) -> Style {
    for e in &ctx.structure.emphasis_spans {
        if e.range.contains(offset) {
            style = style.add_modifier(match e.kind {
                EmphasisKind::Italic => Modifier::ITALIC,
                EmphasisKind::Bold => Modifier::BOLD,
            });
        }
    }
    style
}

/// Ratatui text is fixed-width, so heading "size" is conveyed through
/// color/weight/underline instead — cycling through the existing
/// `MokuTheme` palette only, no new theme fields.
fn heading_style(theme: &MokuTheme, level: u8) -> Style {
    match level {
        1 => Style::default()
            .fg(theme.info)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        2 => Style::default()
            .fg(theme.success)
            .add_modifier(Modifier::BOLD | Modifier::ITALIC),
        3 => Style::default().fg(theme.warning).add_modifier(Modifier::BOLD),
        _ => Style::default().fg(theme.base_fg).add_modifier(Modifier::UNDERLINED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use satz_core::parser::structure::parse_structure;

    fn ctx_for<'a>(
        doc: &'a Document,
        structure: &'a StructureOutput,
        link_res: &'a [LinkResState],
        theme: &'a MokuTheme,
    ) -> LineStyleCtx<'a> {
        LineStyleCtx { doc, structure, link_res, theme }
    }

    #[test]
    fn test_heading_level_1_gets_bold_underline() {
        let source = "# Title\n";
        let doc = satz_core::parse_document(source, std::path::Path::new("t.md"));
        let structure = parse_structure(source);
        let theme = MokuTheme::default();
        let ctx = ctx_for(&doc, &structure, &[], &theme);
        let style = style_for_offset(&ctx, 2);
        assert_eq!(style.fg, Some(theme.info));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn test_resolved_vs_broken_link_color() {
        let source = "[[a]] [[b]]\n";
        let doc = satz_core::parse_document(source, std::path::Path::new("t.md"));
        let structure = parse_structure(source);
        let theme = MokuTheme::default();
        let link_res = vec![
            LinkResState::Resolved {
                target_id: satz_core::DocId::new("a.md"),
                target_path: "a.md".into(),
                anchor_line: 0,
            },
            LinkResState::DocMissing,
        ];
        let ctx = ctx_for(&doc, &structure, &link_res, &theme);
        let resolved_offset = doc.links[0].range.start;
        let broken_offset = doc.links[1].range.start;
        assert_eq!(style_for_offset(&ctx, resolved_offset).fg, Some(theme.success));
        assert_eq!(style_for_offset(&ctx, broken_offset).fg, Some(theme.error));
    }

    #[test]
    fn test_task_checkbox_checked_vs_unchecked_color() {
        let source = "- [ ] todo\n- [x] done\n";
        let doc = satz_core::parse_document(source, std::path::Path::new("t.md"));
        let structure = parse_structure(source);
        let theme = MokuTheme::default();
        let ctx = ctx_for(&doc, &structure, &[], &theme);
        let unchecked = structure.task_markers[0].range.start;
        let checked = structure.task_markers[1].range.start;
        assert_eq!(style_for_offset(&ctx, unchecked).fg, Some(theme.warning));
        assert_eq!(style_for_offset(&ctx, checked).fg, Some(theme.success));
    }
}
