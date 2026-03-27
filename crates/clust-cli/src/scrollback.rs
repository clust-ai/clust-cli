//! Scrollback buffer for the attached terminal session.
//!
//! Stores agent output as lines in a ring buffer so the user can scroll
//! back through history while attached to an agent. Lines are sanitized
//! to remove cursor-positioning and screen-manipulation escape sequences,
//! keeping only SGR (color/style) codes that are safe to replay.

use std::collections::VecDeque;

/// Default maximum number of lines retained.
const DEFAULT_CAPACITY: usize = 10_000;

/// A ring buffer of terminal output lines for scrollback viewing.
pub struct ScrollbackBuffer {
    /// Complete lines (sanitized: only SGR codes preserved, no trailing newline).
    lines: VecDeque<String>,
    /// Maximum number of lines to store.
    capacity: usize,
    /// Current incomplete line being accumulated.
    partial: String,
}

impl ScrollbackBuffer {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(capacity.min(1024)),
            capacity,
            partial: String::new(),
        }
    }

    /// Append raw output bytes to the buffer.
    ///
    /// Splits on newlines, handles bare `\r` (carriage return), strips
    /// non-SGR ANSI escape sequences, and stores sanitized lines.
    /// Partial lines are buffered until a newline arrives.
    pub fn push(&mut self, data: &[u8]) {
        let text = String::from_utf8_lossy(data);
        let mut chars = text.as_ref();

        while let Some(pos) = chars.find('\n') {
            let line_content = &chars[..pos];
            self.partial.push_str(line_content);

            // Strip trailing \r (handle \r\n), then handle any remaining
            // bare \r as overwrite (e.g., progress bars).
            if self.partial.ends_with('\r') {
                self.partial.pop();
            }
            apply_carriage_returns(&mut self.partial);

            let finished = sanitize_for_scrollback(&self.partial);
            self.partial.clear();

            self.lines.push_back(finished);
            if self.lines.len() > self.capacity {
                self.lines.pop_front();
            }

            chars = &chars[pos + 1..];
        }

        // Remaining text (no newline) goes into partial.
        // Handle bare \r as overwrite (e.g., progress bar updates).
        if !chars.is_empty() {
            self.partial.push_str(chars);
            apply_carriage_returns(&mut self.partial);
        }
    }

    /// Total number of complete lines stored.
    pub fn total_lines(&self) -> usize {
        self.lines.len()
    }

    /// Get lines for rendering the scrollback viewport.
    ///
    /// `offset` is lines from the bottom (0 = most recent line at bottom).
    /// Returns up to `count` lines, ordered top-to-bottom.
    pub fn visible_lines(&self, offset: usize, count: usize) -> Vec<&str> {
        let total = self.lines.len();
        if total == 0 || count == 0 {
            return Vec::new();
        }

        // Clamp offset so we never scroll past the top
        let clamped_offset = offset.min(total.saturating_sub(count.min(total)));

        // Bottom of the visible window (exclusive)
        let bottom = total - clamped_offset;
        // Top of the visible window (inclusive)
        let top = bottom.saturating_sub(count);

        self.lines
            .range(top..bottom)
            .map(|s| s.as_str())
            .collect()
    }
}

// ── Carriage return handling ─────────────────────────────────────────

/// Handle bare `\r` (carriage return without following `\n`).
/// Keeps only the content after the last `\r`, simulating terminal
/// overwrite behavior (e.g., progress bars).
fn apply_carriage_returns(s: &mut String) {
    if let Some(pos) = s.rfind('\r') {
        let after = s[pos + 1..].to_string();
        *s = after;
    }
}

// ── ANSI sanitization ───────────────────────────────────────────────

/// State machine for ANSI escape sequence parsing during sanitization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SanitizeState {
    /// Normal text.
    Ground,
    /// Received ESC (0x1B).
    Escape,
    /// Inside CSI sequence (ESC [), accumulating parameter/intermediate bytes.
    CsiAccum,
    /// ESC followed by intermediate byte or SS2/SS3, waiting for final byte.
    EscapeIntermediate,
    /// Inside OSC string (ESC ]).
    OscString,
    /// OSC string, received ESC — checking for ST (ESC \).
    OscStringEsc,
    /// Inside DCS/APC/PM/SOS string.
    StringCommand,
    /// String command, received ESC — checking for ST.
    StringCommandEsc,
}

/// Strip non-SGR ANSI escape sequences from a line for scrollback display.
///
/// Keeps CSI sequences ending in `m` (SGR: colors, bold, italic, etc.)
/// and all normal text. Strips cursor positioning, line clearing, scroll
/// regions, OSC strings, DCS, and other terminal manipulation sequences.
fn sanitize_for_scrollback(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut state = SanitizeState::Ground;
    // Start index of the current escape sequence being accumulated
    let mut seq_start: usize = 0;

    for (i, &byte) in bytes.iter().enumerate() {
        match state {
            SanitizeState::Ground => {
                if byte == 0x1b {
                    state = SanitizeState::Escape;
                    seq_start = i;
                } else if byte == 0x07 || byte == 0x08 {
                    // Strip bell and backspace
                } else {
                    output.push(byte);
                }
            }
            SanitizeState::Escape => match byte {
                b'[' => state = SanitizeState::CsiAccum,
                b']' => state = SanitizeState::OscString,
                b'P' | b'_' | b'^' | b'X' => state = SanitizeState::StringCommand,
                b' ' | b'#' | b'(' | b')' | b'*' | b'+' | b'N' | b'O' => {
                    state = SanitizeState::EscapeIntermediate;
                }
                // Two-character escape (ESC 7, ESC 8, ESC D, ESC M, etc.) — strip
                0x30..=0x7e => state = SanitizeState::Ground,
                // Unknown — treat as complete, strip
                _ => state = SanitizeState::Ground,
            },
            SanitizeState::EscapeIntermediate => {
                // After ESC + intermediate/SS2/SS3, next byte completes — strip all
                state = SanitizeState::Ground;
            }
            SanitizeState::CsiAccum => match byte {
                // Final byte — sequence complete
                0x40..=0x7e => {
                    if byte == b'm' {
                        // SGR — keep the entire sequence
                        output.extend_from_slice(&bytes[seq_start..=i]);
                    }
                    // All other CSI sequences are stripped
                    state = SanitizeState::Ground;
                }
                // Parameter bytes (0-9 ; < = > ?) or intermediate bytes (space-/)
                0x20..=0x3f => {}
                // Invalid byte — strip accumulated sequence
                _ => state = SanitizeState::Ground,
            },
            SanitizeState::OscString => match byte {
                0x07 => state = SanitizeState::Ground,
                0x1b => state = SanitizeState::OscStringEsc,
                _ => {}
            },
            SanitizeState::OscStringEsc => {
                state = if byte == b'\\' {
                    SanitizeState::Ground
                } else {
                    SanitizeState::OscString
                };
            }
            SanitizeState::StringCommand => match byte {
                0x1b => state = SanitizeState::StringCommandEsc,
                _ => {}
            },
            SanitizeState::StringCommandEsc => {
                state = if byte == b'\\' {
                    SanitizeState::Ground
                } else {
                    SanitizeState::StringCommand
                };
            }
        }
    }

    // Safety: output contains only bytes copied from valid UTF-8 input
    unsafe { String::from_utf8_unchecked(output) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer() {
        let sb = ScrollbackBuffer::new();
        assert_eq!(sb.total_lines(), 0);
        assert!(sb.visible_lines(0, 10).is_empty());
    }

    #[test]
    fn single_complete_line() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"hello world\n");
        assert_eq!(sb.total_lines(), 1);
        assert_eq!(sb.visible_lines(0, 10), vec!["hello world"]);
    }

    #[test]
    fn multiple_lines_in_one_push() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"line1\nline2\nline3\n");
        assert_eq!(sb.total_lines(), 3);
        assert_eq!(sb.visible_lines(0, 10), vec!["line1", "line2", "line3"]);
    }

    #[test]
    fn partial_line_buffered() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"partial");
        assert_eq!(sb.total_lines(), 0);
        sb.push(b" line\n");
        assert_eq!(sb.total_lines(), 1);
        assert_eq!(sb.visible_lines(0, 10), vec!["partial line"]);
    }

    #[test]
    fn crlf_stripped() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"windows\r\nstyle\r\n");
        assert_eq!(sb.total_lines(), 2);
        assert_eq!(sb.visible_lines(0, 10), vec!["windows", "style"]);
    }

    #[test]
    fn sgr_codes_preserved() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"\x1b[31mred text\x1b[0m\n");
        assert_eq!(sb.total_lines(), 1);
        assert_eq!(sb.visible_lines(0, 10), vec!["\x1b[31mred text\x1b[0m"]);
    }

    #[test]
    fn ring_buffer_eviction() {
        let mut sb = ScrollbackBuffer::with_capacity(3);
        sb.push(b"a\nb\nc\nd\ne\n");
        assert_eq!(sb.total_lines(), 3);
        assert_eq!(sb.visible_lines(0, 10), vec!["c", "d", "e"]);
    }

    #[test]
    fn visible_lines_with_offset() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"1\n2\n3\n4\n5\n");
        // Offset 0, viewport 3 → last 3 lines
        assert_eq!(sb.visible_lines(0, 3), vec!["3", "4", "5"]);
        // Offset 2, viewport 3 → lines 1,2,3
        assert_eq!(sb.visible_lines(2, 3), vec!["1", "2", "3"]);
        // Offset beyond total → returns what's available from the top
        assert_eq!(sb.visible_lines(10, 3), vec!["1", "2", "3"]);
    }

    #[test]
    fn visible_lines_viewport_larger_than_buffer() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"a\nb\n");
        assert_eq!(sb.visible_lines(0, 100), vec!["a", "b"]);
    }

    #[test]
    fn empty_lines_preserved() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"a\n\nb\n");
        assert_eq!(sb.total_lines(), 3);
        assert_eq!(sb.visible_lines(0, 10), vec!["a", "", "b"]);
    }

    #[test]
    fn consecutive_pushes() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"hel");
        sb.push(b"lo\nwo");
        sb.push(b"rld\n");
        assert_eq!(sb.total_lines(), 2);
        assert_eq!(sb.visible_lines(0, 10), vec!["hello", "world"]);
    }

    #[test]
    fn empty_push_is_noop() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"");
        assert_eq!(sb.total_lines(), 0);
    }

    // ── Carriage return handling ──────────────────────────────────────

    #[test]
    fn bare_cr_keeps_last_overwrite() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"first\rsecond\n");
        assert_eq!(sb.total_lines(), 1);
        assert_eq!(sb.visible_lines(0, 10), vec!["second"]);
    }

    #[test]
    fn multiple_cr_keeps_last() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"a\rb\rc\n");
        assert_eq!(sb.visible_lines(0, 10), vec!["c"]);
    }

    #[test]
    fn cr_across_pushes() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"progress: 50%\r");
        sb.push(b"progress: 100%\n");
        assert_eq!(sb.total_lines(), 1);
        assert_eq!(sb.visible_lines(0, 10), vec!["progress: 100%"]);
    }

    #[test]
    fn progress_bar_simulation() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"Downloading 10%\r");
        sb.push(b"Downloading 50%\r");
        sb.push(b"Downloading 100%\n");
        assert_eq!(sb.visible_lines(0, 10), vec!["Downloading 100%"]);
    }

    #[test]
    fn crlf_still_works_with_cr_handling() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"line1\r\nline2\r\n");
        assert_eq!(sb.total_lines(), 2);
        assert_eq!(sb.visible_lines(0, 10), vec!["line1", "line2"]);
    }

    // ── ANSI sanitization ────────────────────────────────────────────

    #[test]
    fn sanitize_strips_cursor_position() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"hello\x1b[5;10Hworld\n");
        assert_eq!(sb.visible_lines(0, 10), vec!["helloworld"]);
    }

    #[test]
    fn sanitize_strips_line_clear() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"text\x1b[2Kmore\n");
        assert_eq!(sb.visible_lines(0, 10), vec!["textmore"]);
    }

    #[test]
    fn sanitize_strips_erase_display() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"before\x1b[2Jafter\n");
        assert_eq!(sb.visible_lines(0, 10), vec!["beforeafter"]);
    }

    #[test]
    fn sanitize_preserves_sgr_color() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"\x1b[38;2;255;0;0mred\x1b[0m\n");
        assert_eq!(
            sb.visible_lines(0, 10),
            vec!["\x1b[38;2;255;0;0mred\x1b[0m"]
        );
    }

    #[test]
    fn sanitize_preserves_sgr_bold_reset() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"\x1b[1mbold\x1b[0m\n");
        assert_eq!(sb.visible_lines(0, 10), vec!["\x1b[1mbold\x1b[0m"]);
    }

    #[test]
    fn sanitize_preserves_bare_sgr_reset() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"\x1b[mtext\n");
        assert_eq!(sb.visible_lines(0, 10), vec!["\x1b[mtext"]);
    }

    #[test]
    fn sanitize_strips_scroll_region() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"\x1b[1;24rtext\n");
        assert_eq!(sb.visible_lines(0, 10), vec!["text"]);
    }

    #[test]
    fn sanitize_strips_cursor_save_restore() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"\x1b7text\x1b8\n");
        assert_eq!(sb.visible_lines(0, 10), vec!["text"]);
    }

    #[test]
    fn sanitize_strips_osc_title() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"\x1b]0;my title\x07visible\n");
        assert_eq!(sb.visible_lines(0, 10), vec!["visible"]);
    }

    #[test]
    fn sanitize_strips_osc_with_st() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"\x1b]0;title\x1b\\visible\n");
        assert_eq!(sb.visible_lines(0, 10), vec!["visible"]);
    }

    #[test]
    fn sanitize_mixed_sgr_and_cursor() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"\x1b[31mred\x1b[5;1H\x1b[2Kstill red\x1b[0m\n");
        assert_eq!(
            sb.visible_lines(0, 10),
            vec!["\x1b[31mredstill red\x1b[0m"]
        );
    }

    #[test]
    fn sanitize_strips_backspace_and_bell() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"ab\x08c\x07d\n");
        assert_eq!(sb.visible_lines(0, 10), vec!["abcd"]);
    }

    #[test]
    fn sanitize_strips_dcs() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"\x1bPcontent\x1b\\visible\n");
        assert_eq!(sb.visible_lines(0, 10), vec!["visible"]);
    }

    #[test]
    fn sanitize_strips_cursor_movement() {
        let mut sb = ScrollbackBuffer::new();
        // CUU, CUD, CUF, CUB
        sb.push(b"a\x1b[Ab\x1b[Bc\x1b[Cd\x1b[D\n");
        assert_eq!(sb.visible_lines(0, 10), vec!["abcd"]);
    }

    #[test]
    fn sanitize_strips_mode_set_reset() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"\x1b[?1000htext\x1b[?1000l\n");
        assert_eq!(sb.visible_lines(0, 10), vec!["text"]);
    }

    #[test]
    fn sanitize_preserves_tab() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"col1\tcol2\n");
        assert_eq!(sb.visible_lines(0, 10), vec!["col1\tcol2"]);
    }
}
