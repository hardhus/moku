/// Top-clamped scroll math for a document viewport — deliberately NOT the
/// launcher's circular/ring viewport (`moku-launcher/src/viewport.rs`): a
/// document has a real start and end, nothing wraps. Returns the
/// `scroll_top` needed to keep `cursor_row` within `margin` rows of the
/// visible window's edges, in flattened render-row space (see
/// `render::line_to_row`).
pub fn recompute_doc_viewport(scroll_top: usize, cursor_row: usize, window: usize, margin: usize) -> usize {
    if window == 0 {
        return scroll_top;
    }
    let margin = margin.min(window.saturating_sub(1) / 2);
    if cursor_row < scroll_top + margin {
        cursor_row.saturating_sub(margin)
    } else if cursor_row + margin + 1 > scroll_top + window {
        (cursor_row + margin + 1).saturating_sub(window)
    } else {
        scroll_top
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_scroll_when_cursor_within_margin() {
        assert_eq!(recompute_doc_viewport(5, 10, 20, 3), 5);
    }

    #[test]
    fn test_scrolls_down_when_cursor_near_bottom_edge() {
        let new_top = recompute_doc_viewport(0, 19, 20, 3);
        assert!(new_top > 0);
    }

    #[test]
    fn test_scrolls_up_when_cursor_near_top_edge() {
        let new_top = recompute_doc_viewport(10, 10, 20, 3);
        assert!(new_top < 10);
    }

    #[test]
    fn test_no_wraparound_at_document_start() {
        assert_eq!(recompute_doc_viewport(0, 0, 20, 5), 0);
    }

    #[test]
    fn test_window_larger_than_total_rows_stays_at_zero() {
        assert_eq!(recompute_doc_viewport(0, 2, 50, 5), 0);
    }
}
