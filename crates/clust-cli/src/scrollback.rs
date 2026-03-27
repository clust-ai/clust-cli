//! Scrollback buffer for the attached terminal session.
//!
//! Stores agent output as lines in a ring buffer so the user can scroll
//! back through history while attached to an agent.

use std::collections::VecDeque;

/// Default maximum number of lines retained.
const DEFAULT_CAPACITY: usize = 10_000;

/// A ring buffer of terminal output lines for scrollback viewing.
pub struct ScrollbackBuffer {
    /// Complete lines (ANSI codes preserved, no trailing newline).
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
    /// Splits on newlines, strips trailing `\r`, preserves ANSI escape codes.
    /// Partial lines are buffered until a newline arrives.
    pub fn push(&mut self, data: &[u8]) {
        let text = String::from_utf8_lossy(data);
        let mut chars = text.as_ref();

        while let Some(pos) = chars.find('\n') {
            let line_content = &chars[..pos];
            self.partial.push_str(line_content);

            // Strip trailing \r (handle \r\n)
            let finished = if self.partial.ends_with('\r') {
                let mut s = std::mem::take(&mut self.partial);
                s.pop();
                s
            } else {
                std::mem::take(&mut self.partial)
            };

            self.lines.push_back(finished);
            if self.lines.len() > self.capacity {
                self.lines.pop_front();
            }

            chars = &chars[pos + 1..];
        }

        // Remaining text (no newline) goes into partial
        if !chars.is_empty() {
            self.partial.push_str(chars);
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
    fn ansi_codes_preserved() {
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
}
