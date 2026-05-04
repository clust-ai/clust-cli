#![allow(dead_code)]
/// ANSI true-color (24-bit) escape helpers for the Graphite theme.
///
/// See docs/theme.md for the full palette.
// Accent
pub const ACCENT: &str = "\x1b[38;2;94;154;191m"; // #5e9abf
pub const ACCENT_BRIGHT: &str = "\x1b[38;2;114;174;208m"; // #72aed0

// Text
pub const TEXT_PRIMARY: &str = "\x1b[38;2;220;221;224m"; // #dcdde0
pub const TEXT_SECONDARY: &str = "\x1b[38;2;160;162;168m"; // #a0a2a8
pub const TEXT_TERTIARY: &str = "\x1b[38;2;108;110;116m"; // #6c6e74

// Semantic
pub const SUCCESS: &str = "\x1b[38;2;91;184;114m"; // #5bb872
pub const ERROR: &str = "\x1b[38;2;204;80;72m"; // #cc5048
pub const WARNING: &str = "\x1b[38;2;212;173;58m"; // #d4ad3a
pub const WARNING_TEXT: &str = "\x1b[38;2;232;196;88m"; // #e8c458
pub const INFO: &str = "\x1b[38;2;88;152;196m"; // #5898c4

pub const RESET: &str = "\x1b[0m";
pub const RESET_FG: &str = "\x1b[39m";
pub const RESET_BG: &str = "\x1b[49m";

// Backgrounds (ANSI)
pub const BG_RAISED: &str = "\x1b[48;2;41;43;48m"; // #292b30

// Ratatui colors (same Graphite palette)
use ratatui::style::Color;

// Accent
pub const R_ACCENT: Color = Color::Rgb(94, 154, 191); // #5e9abf
pub const R_ACCENT_DIM: Color = Color::Rgb(78, 138, 170); // #4e8aaa
pub const R_ACCENT_BRIGHT: Color = Color::Rgb(114, 174, 208); // #72aed0
pub const R_ACCENT_TEXT: Color = Color::Rgb(126, 184, 216); // #7eb8d8

// Text
pub const R_TEXT_PRIMARY: Color = Color::Rgb(220, 221, 224); // #dcdde0
pub const R_TEXT_SECONDARY: Color = Color::Rgb(160, 162, 168); // #a0a2a8
pub const R_TEXT_TERTIARY: Color = Color::Rgb(108, 110, 116); // #6c6e74
pub const R_TEXT_DISABLED: Color = Color::Rgb(74, 76, 82); // #4a4c52

// Semantic
pub const R_SUCCESS: Color = Color::Rgb(91, 184, 114); // #5bb872
pub const R_WARNING: Color = Color::Rgb(212, 173, 58); // #d4ad3a
pub const R_ERROR: Color = Color::Rgb(204, 80, 72); // #cc5048
pub const R_INFO: Color = Color::Rgb(88, 152, 196); // #5898c4

// Backgrounds
pub const R_BG_BASE: Color = Color::Rgb(27, 29, 32); // #1b1d20
pub const R_BG_SURFACE: Color = Color::Rgb(34, 36, 40); // #222428
pub const R_BG_RAISED: Color = Color::Rgb(41, 43, 48); // #292b30
pub const R_BG_OVERLAY: Color = Color::Rgb(49, 52, 58); // #31343a
pub const R_BG_INPUT: Color = Color::Rgb(57, 60, 66); // #393c42
pub const R_BG_HOVER: Color = Color::Rgb(64, 67, 74); // #40434a
pub const R_BG_ACTIVE: Color = Color::Rgb(72, 75, 82); // #484b52

// Diff backgrounds
pub const R_DIFF_ADD_BG: Color = Color::Rgb(30, 50, 30);
pub const R_DIFF_DEL_BG: Color = Color::Rgb(55, 25, 25);

// Selection highlight — accent blue at ~14% opacity blended onto R_BG_BASE
pub const R_SELECTION_BG: Color = Color::Rgb(40, 49, 58);

// Repository colors — vibrant, high-chroma, perceptually distinct on dark bg
pub const R_REPO_RED: Color = Color::Rgb(240, 96, 96); // #f06060
pub const R_REPO_ORANGE: Color = Color::Rgb(240, 160, 48); // #f0a030
pub const R_REPO_YELLOW: Color = Color::Rgb(224, 208, 64); // #e0d040
pub const R_REPO_LIME: Color = Color::Rgb(128, 224, 80); // #80e050
pub const R_REPO_GREEN: Color = Color::Rgb(64, 216, 144); // #40d890
pub const R_REPO_TEAL: Color = Color::Rgb(64, 208, 208); // #40d0d0
pub const R_REPO_BLUE: Color = Color::Rgb(80, 160, 240); // #50a0f0
pub const R_REPO_PURPLE: Color = Color::Rgb(160, 112, 240); // #a070f0
pub const R_REPO_PINK: Color = Color::Rgb(224, 112, 192); // #e070c0
pub const R_REPO_CORAL: Color = Color::Rgb(240, 128, 112); // #f08070

/// Map a repo color name to a ratatui Color.
pub fn repo_color(name: &str) -> Color {
    match name {
        "red" => R_REPO_RED,
        "orange" => R_REPO_ORANGE,
        "yellow" => R_REPO_YELLOW,
        "lime" => R_REPO_LIME,
        "green" => R_REPO_GREEN,
        "teal" => R_REPO_TEAL,
        "blue" => R_REPO_BLUE,
        "purple" => R_REPO_PURPLE,
        "pink" => R_REPO_PINK,
        "coral" => R_REPO_CORAL,
        _ => R_ACCENT,
    }
}

/// Dim a color to ~60% brightness for unfocused elements.
pub fn dim_color(c: Color) -> Color {
    match c {
        Color::Rgb(r, g, b) => Color::Rgb(
            (r as u16 * 60 / 100) as u8,
            (g as u16 * 60 / 100) as u8,
            (b as u16 * 60 / 100) as u8,
        ),
        other => other,
    }
}

/// All repo color names in palette order.
pub const REPO_COLOR_NAMES: &[&str] = &[
    "red", "orange", "yellow", "lime", "green", "teal", "blue", "purple", "pink", "coral",
];
