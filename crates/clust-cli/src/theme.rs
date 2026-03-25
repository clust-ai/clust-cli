#![allow(dead_code)]
/// ANSI true-color (24-bit) escape helpers for the Graphite theme.
///
/// See docs/theme.md for the full palette.

// Accent
pub const ACCENT: &str = "\x1b[38;2;94;154;191m";      // #5e9abf
pub const ACCENT_BRIGHT: &str = "\x1b[38;2;114;174;208m"; // #72aed0

// Text
pub const TEXT_PRIMARY: &str = "\x1b[38;2;220;221;224m";   // #dcdde0
pub const TEXT_SECONDARY: &str = "\x1b[38;2;160;162;168m"; // #a0a2a8
pub const TEXT_TERTIARY: &str = "\x1b[38;2;108;110;116m";  // #6c6e74

// Semantic
pub const SUCCESS: &str = "\x1b[38;2;91;184;114m";  // #5bb872
pub const ERROR: &str = "\x1b[38;2;204;80;72m";     // #cc5048
pub const WARNING: &str = "\x1b[38;2;212;173;58m";  // #d4ad3a
pub const INFO: &str = "\x1b[38;2;88;152;196m";     // #5898c4

pub const RESET: &str = "\x1b[0m";
