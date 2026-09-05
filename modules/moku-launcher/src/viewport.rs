use ratatui::style::Color;

/// How many rows (0-indexed distance from either edge) the cursor stays
/// clear of before the ring starts rotating under it. 3 means the cursor
/// pins at row index 3 (the 4th row) from the top, and at row index
/// `window - 1 - 3` from the bottom — everywhere in between is a free
/// zone where the cursor moves normally.
pub const RING_MARGIN: usize = 3;

/// Adjusts `viewport_top` (the item shown at row 0) so the cursor stays
/// within `RING_MARGIN` of either edge of a `window`-row view, exactly
/// like the old bounded `recompute_viewport` used to — except positions
/// are tracked modulo the *total* `n` (never clamped to `[0, n-window]`),
/// so "moving past the first item" wraps seamlessly to the last one and
/// vice versa, however many are shown at once. When `window == n` (the
/// common case — everything fits), this reduces to true circular
/// wraparound with no start/end at all, which is what makes it read as a
/// closed ring/cylinder rather than a bounded list. When `window < n`
/// (more modules than fit on screen), the same rotate-under-the-cursor
/// rule crops to a scrolling window — "findable if you scroll enough" —
/// while still never exposing a hard boundary at the *pinning* edges.
pub fn recompute_ring_viewport(
    viewport_top: i64,
    selected_pos: usize,
    n: usize,
    window: usize,
    margin: usize,
) -> i64 {
    if n == 0 {
        return 0;
    }
    let n = n as i64;
    let window = (window.min(n as usize)).max(1) as i64;
    let cursor_row = (selected_pos as i64 - viewport_top).rem_euclid(n);
    let low = margin as i64;
    let high = window - 1 - margin as i64;
    if cursor_row < low {
        (selected_pos as i64 - low).rem_euclid(n)
    } else if cursor_row > high {
        (selected_pos as i64 - high).rem_euclid(n)
    } else {
        viewport_top.rem_euclid(n)
    }
}

pub const MAX_SELECTION_INDENT: usize = 4;

/// Leading-space count for a row `distance` steps away from the cursor's
/// row. Quadratic rather than linear falloff — not a real 3D perspective
/// (ratatui/a terminal can't do that), but a curved rather than
/// stair-step taper reads a little more like a rounded surface. Floors
/// at 0; `has_selection = false` (nothing selected, e.g. an empty list)
/// means no indent anywhere.
pub fn selected_indent(distance: usize, has_selection: bool) -> usize {
    if !has_selection {
        return 0;
    }
    let d = distance as f32;
    (MAX_SELECTION_INDENT as f32 - d * d * 0.6).max(0.0).round() as usize
}

/// Never dims a row below this fraction of its base brightness — every
/// row stays visible, however far it sits from the cursor.
const FADE_FLOOR: f32 = 0.4;

/// Darkens `base` a little more for each step of `distance` from the
/// cursor's row, reaching `FADE_FLOOR` brightness at `max_distance` and
/// never going dimmer than that. Unlike `Modifier::DIM` (a fixed,
/// terminal-dependent, effectively two-tier reduction), this computes a
/// real `Color::Rgb` per row — genuinely continuous, and entirely within
/// what a terminal can already do (this is just picking different literal
/// colors, not a perspective/3D effect, which a terminal really can't do).
pub fn fade_color(base: Color, distance: usize, max_distance: usize) -> Color {
    let (r, g, b) = color_to_rgb(base);
    let t = (distance as f32 / (max_distance.max(1) as f32)).min(1.0);
    let factor = 1.0 - t * (1.0 - FADE_FLOOR);
    Color::Rgb(
        (r as f32 * factor).round() as u8,
        (g as f32 * factor).round() as u8,
        (b as f32 * factor).round() as u8,
    )
}

/// Approximates a ratatui `Color` as an RGB triple for fading purposes.
/// `Rgb` passes through exactly; named ANSI colors map to their standard
/// terminal-palette equivalents; anything ambiguous (`Reset`, `Indexed`)
/// falls back to a neutral mid-gray rather than guessing.
fn color_to_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (205, 49, 49),
        Color::Green => (13, 188, 121),
        Color::Yellow => (229, 229, 16),
        Color::Blue => (36, 114, 200),
        Color::Magenta => (188, 63, 188),
        Color::Cyan => (17, 168, 205),
        Color::Gray => (192, 192, 192),
        Color::DarkGray => (102, 102, 102),
        Color::LightRed => (241, 76, 76),
        Color::LightGreen => (35, 209, 139),
        Color::LightYellow => (245, 245, 67),
        Color::LightBlue => (59, 142, 234),
        Color::LightMagenta => (214, 112, 214),
        Color::LightCyan => (41, 184, 219),
        Color::White => (255, 255, 255),
        Color::Reset | Color::Indexed(_) => (170, 170, 170),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_selected_indent_peaks_at_zero_distance_and_tapers_off() {
        assert_eq!(selected_indent(0, true), MAX_SELECTION_INDENT);
        assert!(selected_indent(1, true) < MAX_SELECTION_INDENT);
        assert!(selected_indent(1, true) > selected_indent(2, true));
        assert_eq!(selected_indent(3, true), 0); // far enough away, floored at 0
        assert_eq!(selected_indent(9, true), 0); // far away, floored at 0
        assert_eq!(selected_indent(0, false), 0); // nothing selected
    }

    #[test]
    fn test_recompute_ring_viewport_stays_put_in_the_free_zone() {
        // n=11, window=11 (no cropping), margin=3: cursor_row in [3, 7] is
        // free — moving within it never changes viewport_top.
        assert_eq!(recompute_ring_viewport(8, 1, 11, 11, 3), 8);
        assert_eq!(recompute_ring_viewport(8, 3, 11, 11, 3), 8);
    }

    #[test]
    fn test_recompute_ring_viewport_rotates_forward_past_the_bottom_margin() {
        // The exact KARE1 -> KARE3 sequence confirmed with the user (11
        // modules now that Vault Security is included, margin 3).
        assert_eq!(recompute_ring_viewport(0, 0, 11, 11, 3), 8); // KARE1: Dashboard selected fresh
        assert_eq!(recompute_ring_viewport(8, 5, 11, 11, 3), 9); // KARE3: one more Down past the margin
    }

    #[test]
    fn test_recompute_ring_viewport_rotates_backward_past_the_top_margin() {
        assert_eq!(recompute_ring_viewport(8, 0, 11, 11, 3), 8); // still in free zone at row 3
        assert_eq!(recompute_ring_viewport(8, 10, 11, 11, 3), 7); // wraps past Dashboard to the last item
    }

    #[test]
    fn test_recompute_ring_viewport_handles_empty_and_singleton_lists() {
        assert_eq!(recompute_ring_viewport(5, 0, 0, 11, 3), 0);
        // A single item with a margin larger than the list: must not
        // panic, and settles on a stable value.
        assert_eq!(recompute_ring_viewport(0, 0, 1, 1, 3), 0);
    }

    #[test]
    fn test_recompute_ring_viewport_with_a_window_smaller_than_n() {
        // A window narrower than the total (more modules than fit on
        // screen) should engage rotation earlier than an unwindowed view
        // over the same n/selection would — proving the window parameter
        // actually crops rather than being ignored.
        assert_eq!(recompute_ring_viewport(0, 4, 20, 5, 1), 1);
        assert_eq!(recompute_ring_viewport(0, 4, 20, 20, 1), 0);
    }

    #[test]
    fn test_fade_color_never_drops_below_the_floor() {
        let white = Color::Rgb(255, 255, 255);
        assert_eq!(fade_color(white, 0, 4), white, "no distance, no fade");
        let floor = fade_color(white, 4, 4);
        assert_eq!(
            floor,
            Color::Rgb(102, 102, 102),
            "at max_distance, brightness should sit exactly at FADE_FLOOR"
        );
        // Even far beyond max_distance, it must never dim further.
        assert_eq!(
            fade_color(white, 40, 4),
            floor,
            "fading should never go past the floor, however far distance grows"
        );
    }

    #[test]
    fn test_fade_gradient_is_strictly_monotonic_with_distance() {
        let white = Color::Rgb(255, 255, 255);
        let brightness = |c: Color| match c {
            Color::Rgb(r, _, _) => r,
            _ => panic!("expected Rgb"),
        };
        let samples: Vec<u8> = (0..=6)
            .map(|d| brightness(fade_color(white, d, 6)))
            .collect();
        for pair in samples.windows(2) {
            assert!(
                pair[1] < pair[0],
                "each step further from the cursor should be strictly dimmer than the last (got {samples:?})"
            );
        }
    }
}
