use ratatui::style::Color;

use crate::config::{MokuConfig, ThemeColors};

#[derive(Debug, Clone, PartialEq)]
pub struct MokuTheme {
    pub base_fg: Color,
    pub base_bg: Color,
    pub border: Color,
    pub selection_fg: Color,
    pub selection_bg: Color,
    pub info: Color,
    pub warning: Color,
    pub error: Color,
    pub success: Color,
}

impl MokuTheme {
    /// The theme named by `[general].theme`, or the built-in default if no
    /// `[themes.<name>]` table matches it.
    pub fn from_config(config: &MokuConfig) -> Self {
        match config.themes.get(&config.general.theme) {
            Some(colors) => Self::from_colors(colors),
            None => Self::default(),
        }
    }

    pub fn from_colors(colors: &ThemeColors) -> Self {
        Self {
            base_fg: parse_color(&colors.base_fg),
            base_bg: parse_color(&colors.base_bg),
            border: parse_color(&colors.border),
            selection_fg: parse_color(&colors.selection_fg),
            selection_bg: parse_color(&colors.selection_bg),
            info: parse_color(&colors.info),
            warning: parse_color(&colors.warning),
            error: parse_color(&colors.error),
            success: parse_color(&colors.success),
        }
    }
}

impl Default for MokuTheme {
    fn default() -> Self {
        Self::from_colors(&ThemeColors::default())
    }
}

/// Parses a color string into a Ratatui Color. `pub` so other crates (e.g.
/// a module with its own config-driven color palette beyond `MokuTheme`'s
/// fixed semantic fields) can reuse the same hex/named-color parsing
/// instead of duplicating it.
pub fn parse_color(s: &str) -> Color {
    let s = s.trim();

    // 1. Hex Code Handling (#RRGGBB or #RGB). ASCII only: the byte-index
    // slicing below would panic mid-codepoint on e.g. "#é12" (4 bytes, 3
    // chars), and no valid hex color contains a non-ASCII byte anyway.
    if s.starts_with('#') && s.is_ascii() {
        match s.len() {
            7 => {
                // #RRGGBB
                if let (Ok(r), Ok(g), Ok(b)) = (
                    u8::from_str_radix(&s[1..3], 16),
                    u8::from_str_radix(&s[3..5], 16),
                    u8::from_str_radix(&s[5..7], 16),
                ) {
                    return Color::Rgb(r, g, b);
                }
            }
            4 => {
                // #RGB (shorthand)
                if let (Ok(r), Ok(g), Ok(b)) = (
                    u8::from_str_radix(&s[1..2].repeat(2), 16),
                    u8::from_str_radix(&s[2..3].repeat(2), 16),
                    u8::from_str_radix(&s[3..4].repeat(2), 16),
                ) {
                    return Color::Rgb(r, g, b);
                }
            }
            _ => {}
        }
    }

    // 2. Named Colors (Extended for Ratatui)
    match s.to_lowercase().as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "reset" | "transparent" => Color::Reset,
        _ => Color::White, // Fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_color_shorthand_hex() {
        assert_eq!(parse_color("#F00"), Color::Rgb(255, 0, 0));
        assert_eq!(parse_color("#0f0"), Color::Rgb(0, 255, 0));
    }

    #[test]
    fn test_parse_extended_names() {
        assert_eq!(parse_color("LightBlue"), Color::LightBlue);
        assert_eq!(parse_color("lightred"), Color::LightRed);
    }

    #[test]
    fn test_parse_trim_and_case() {
        assert_eq!(parse_color("  Reset  "), Color::Reset);
        assert_eq!(parse_color("MAGENTA"), Color::Magenta);
    }

    #[test]
    fn test_invalid_hex_fallback() {
        // Fallback if hex has invalid length
        assert_eq!(parse_color("#12345"), Color::White);
    }

    #[test]
    fn test_theme_fallback_chain() {
        // Step 1: Completely invalid HEX (Cannot be parsed)
        let color_hex_bad = parse_color("#Z12345");
        assert_eq!(
            color_hex_bad,
            Color::White,
            "Invalid HEX -> Should fallback to White"
        );

        // Step 2: Invalid color name (Undefined)
        let color_name_bad = parse_color("neon-glowing-green");
        assert_eq!(
            color_name_bad,
            Color::White,
            "Unknown name -> Should fallback to White"
        );

        // Step 3: Empty or whitespace-only string
        let color_empty = parse_color("   ");
        assert_eq!(
            color_empty,
            Color::White,
            "Empty value -> Should fallback to White"
        );
    }

    #[test]
    fn test_parse_color_non_ascii_hex_does_not_panic() {
        // 4 bytes (a hex-shorthand length) but 3 chars, so byte slicing
        // would split the 2-byte 'é'.
        assert_eq!(parse_color("#é12"), parse_color("definitely-not-a-color"));
        assert_eq!(parse_color("#aé"), parse_color("definitely-not-a-color"));
        assert_eq!(parse_color("#ff0000"), Color::Rgb(255, 0, 0));
    }
}
