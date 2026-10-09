use serde::{Deserialize, Serialize};

/// A theme as written in config.toml (`[themes.<name>]`): raw color
/// strings, no UI dependency. moku-core's `MokuTheme::from_colors` parses
/// them into ratatui colors.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct ThemeColors {
    pub base_fg: String,
    pub base_bg: String,
    pub border: String,
    pub selection_fg: String,
    pub selection_bg: String,
    pub info: String,
    pub warning: String,
    pub error: String,
    pub success: String,
}

impl Default for ThemeColors {
    fn default() -> Self {
        Self {
            base_fg: "White".to_string(),
            base_bg: "Reset".to_string(),
            border: "DarkGray".to_string(),
            selection_fg: "Yellow".to_string(),
            selection_bg: "Reset".to_string(),
            // Semantic accent colors below are picked from the Okabe-Ito
            // colorblind-safe palette rather than plain ANSI names — the
            // classic success=Green/error=Red pair is the single most
            // confusable pairing under red-green color vision deficiency.
            // Backgrounds stay "Reset" (transparent); only these four hues
            // changed. Still fully overridable per-user via `[themes.*]`
            // in config.toml.
            info: "#0072B2".to_string(),
            warning: "#E69F00".to_string(),
            error: "#D55E00".to_string(),
            success: "#009E73".to_string(),
        }
    }
}
