use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Default scrollback capacity for overview panels.
const DEFAULT_SCROLLBACK: usize = 2000;

/// Terminal emulator backed by the `vt100` crate.
///
/// Wraps `vt100::Parser` and provides conversion to ratatui types for TUI
/// rendering, plus ANSI string output for the non-TUI attached terminal's
/// scrollback display.
pub struct TerminalEmulator {
    parser: vt100::Parser,
    cols: usize,
    rows: usize,
}

impl TerminalEmulator {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self::with_scrollback_capacity(cols, rows, DEFAULT_SCROLLBACK)
    }

    pub fn with_scrollback_capacity(cols: usize, rows: usize, scrollback_capacity: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            // vt100::Parser::new takes (rows, cols, scrollback_len)
            parser: vt100::Parser::new(rows as u16, cols as u16, scrollback_capacity),
            cols,
            rows,
        }
    }

    /// Feed raw bytes from the PTY into the terminal emulator.
    pub fn process(&mut self, data: &[u8]) {
        self.parser.process(data);
    }

    /// Resize the terminal. Preserves scrollback.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        // In vt100 0.15, set_size is on Parser directly
        self.parser.set_size(rows as u16, cols as u16);
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Returns the current cursor position as `(row, col)`.
    pub fn cursor_position(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    /// Returns `true` if the application has hidden the cursor.
    pub fn hide_cursor(&self) -> bool {
        self.parser.screen().hide_cursor()
    }

    /// Number of scrollback lines available above the visible screen.
    ///
    /// Uses the `set_scrollback(MAX)` probe since vt100 exposes the current
    /// offset, not the total count.
    pub fn scrollback_len(&mut self) -> usize {
        let prev = self.parser.screen().scrollback();
        // Use a very large value that vt100 will clamp to the actual max.
        // Avoid usize::MAX to prevent potential overflow in vt100 internals.
        self.parser.set_scrollback(u32::MAX as usize);
        let total = self.parser.screen().scrollback();
        self.parser.set_scrollback(prev);
        total
    }

    /// Render the current visible screen as ratatui `Line`s.
    pub fn to_ratatui_lines(&self) -> Vec<Line<'static>> {
        let screen = self.parser.screen();
        let rows = self.rows as u16;
        let cols = self.cols as u16;
        let mut lines = Vec::with_capacity(self.rows);

        for row in 0..rows {
            lines.push(row_to_line(screen, row, cols));
        }
        lines
    }

    /// Render lines from scrollback + screen at the given offset.
    /// `offset` is measured in lines from the bottom (0 = live screen).
    pub fn to_ratatui_lines_scrolled(&mut self, offset: usize) -> Vec<Line<'static>> {
        if offset == 0 {
            return self.to_ratatui_lines();
        }

        let prev_offset = self.parser.screen().scrollback();
        self.parser.set_scrollback(offset);
        let actual_offset = self.parser.screen().scrollback();

        let rows = self.rows as u16;
        let cols = self.cols as u16;
        let screen = self.parser.screen();
        let mut lines = Vec::with_capacity(self.rows);
        for row in 0..rows {
            lines.push(row_to_line(screen, row, cols));
        }

        self.parser.set_scrollback(prev_offset.min(actual_offset));
        lines
    }

    /// Find a URL at the given terminal row and column.
    /// Returns the full URL string if one is found at that position.
    pub fn url_at_position(&self, row: u16, col: u16) -> Option<String> {
        let screen = self.parser.screen();
        let (text, col_to_byte) = row_text_from_screen(screen, row, self.cols as u16);
        let byte_offset = col_to_byte.get(col as usize).copied()?;
        find_url_at_offset(&text, byte_offset)
    }

    /// Find a URL at the given terminal row and column, accounting for
    /// scrollback offset (same pattern as `to_ratatui_lines_scrolled`).
    pub fn url_at_position_scrolled(
        &mut self,
        row: u16,
        col: u16,
        scroll_offset: usize,
    ) -> Option<String> {
        if scroll_offset == 0 {
            return self.url_at_position(row, col);
        }
        let prev = self.parser.screen().scrollback();
        self.parser.set_scrollback(scroll_offset);
        let result = self.url_at_position(row, col);
        self.parser.set_scrollback(prev);
        result
    }

    /// Render lines from scrollback + screen as ANSI strings.
    /// Used by the non-TUI attached terminal for scrollback display.
    pub fn to_ansi_lines_scrolled(&mut self, offset: usize) -> Vec<String> {
        let prev_offset = self.parser.screen().scrollback();
        self.parser.set_scrollback(offset);

        let screen = self.parser.screen();
        let ansi_rows: Vec<String> = screen
            .rows_formatted(0, self.cols as u16)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .collect();

        self.parser.set_scrollback(prev_offset);
        ansi_rows
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

/// Convert a single screen row to a ratatui `Line`.
fn row_to_line(screen: &vt100::Screen, row: u16, cols: u16) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut cur_style = Style::default();
    let mut first = true;

    for col in 0..cols {
        let Some(cell) = screen.cell(row, col) else {
            continue;
        };

        // Skip the second half of wide characters
        if cell.is_wide_continuation() {
            continue;
        }

        let style = cell_to_style(cell);

        if first {
            cur_style = style;
            first = false;
        }

        if style != cur_style {
            if !text.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut text), cur_style));
            }
            cur_style = style;
        }

        let contents = cell.contents();
        if contents.is_empty() {
            text.push(' ');
        } else {
            text.push_str(&contents);
        }
    }

    if !text.is_empty() {
        spans.push(Span::styled(text, cur_style));
    }

    Line::from(spans)
}

/// Map a `vt100::Cell` to a `ratatui::Style`.
fn cell_to_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();

    if let Some(fg) = vt100_color_to_ratatui(cell.fgcolor()) {
        style = style.fg(fg);
    }
    if let Some(bg) = vt100_color_to_ratatui(cell.bgcolor()) {
        style = style.bg(bg);
    }

    let mut modifiers = Modifier::empty();
    if cell.bold() {
        modifiers |= Modifier::BOLD;
    }
    if cell.italic() {
        modifiers |= Modifier::ITALIC;
    }
    if cell.underline() {
        modifiers |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        modifiers |= Modifier::REVERSED;
    }
    if !modifiers.is_empty() {
        style = style.add_modifier(modifiers);
    }

    style
}

/// Map a `vt100::Color` to a `ratatui::style::Color`.
fn vt100_color_to_ratatui(color: vt100::Color) -> Option<Color> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(n) => Some(Color::Indexed(n)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

// ---------------------------------------------------------------------------
// URL detection helpers
// ---------------------------------------------------------------------------

/// Extract the plain text for a screen row with a column-to-byte-offset mapping.
///
/// Returns `(text, col_to_byte)` where `col_to_byte[screen_col]` gives the byte
/// offset into `text` for that column. Wide-continuation columns map to the same
/// offset as the wide character's first column.
fn row_text_from_screen(screen: &vt100::Screen, row: u16, cols: u16) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut col_to_byte: Vec<usize> = Vec::with_capacity(cols as usize);

    for col in 0..cols {
        let byte_offset = text.len();
        let Some(cell) = screen.cell(row, col) else {
            col_to_byte.push(byte_offset);
            text.push(' ');
            continue;
        };
        if cell.is_wide_continuation() {
            // Map to the same byte as the wide char's first column.
            let prev = if col_to_byte.is_empty() { 0 } else { col_to_byte[col_to_byte.len() - 1] };
            col_to_byte.push(prev);
            continue;
        }
        col_to_byte.push(byte_offset);
        let contents = cell.contents();
        if contents.is_empty() {
            text.push(' ');
        } else {
            text.push_str(&contents);
        }
    }
    (text, col_to_byte)
}

/// Find a URL at the given byte offset within a line of text.
fn find_url_at_offset(text: &str, byte_offset: usize) -> Option<String> {
    for scheme in ["https://", "http://"] {
        let mut search_start = 0;
        while let Some(rel_pos) = text[search_start..].find(scheme) {
            let url_start = search_start + rel_pos;
            let url_end = find_url_end(text, url_start);
            if byte_offset >= url_start && byte_offset < url_end {
                return Some(text[url_start..url_end].to_string());
            }
            search_start = url_end;
        }
    }
    None
}

/// Find the end byte offset of a URL starting at `start`.
fn find_url_end(text: &str, start: usize) -> usize {
    let rest = &text[start..];
    let mut end = rest.len();
    for (i, ch) in rest.char_indices() {
        if ch.is_whitespace() || ch == '\0' {
            end = i;
            break;
        }
    }
    let url = &rest[..end];
    // Strip trailing punctuation that is usually not part of the URL.
    let trimmed = url.trim_end_matches(['.', ',', ';', ':', '!', '?', '"', '\'', '>', ']']);
    // Only strip trailing ')' when there is no matching '(' inside the URL
    // (preserves Wikipedia-style URLs).
    let trimmed = if !trimmed.contains('(') {
        trimmed.trim_end_matches(')')
    } else {
        trimmed
    };
    start + trimmed.len()
}

/// Open a URL in the system's default browser (fire-and-forget).
pub fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_text(te: &TerminalEmulator) -> Vec<String> {
        let screen = te.parser.screen();
        (0..te.rows as u16)
            .map(|row| {
                (0..te.cols as u16)
                    .map(|col| {
                        screen
                            .cell(row, col)
                            .map(|c| {
                                let s = c.contents();
                                if s.is_empty() { ' ' } else { s.chars().next().unwrap() }
                            })
                            .unwrap_or(' ')
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn test_basic_print() {
        let mut te = TerminalEmulator::new(10, 3);
        te.process(b"Hello");
        let lines = screen_text(&te);
        assert_eq!(&lines[0], "Hello     ");
    }

    #[test]
    fn test_newline() {
        let mut te = TerminalEmulator::new(10, 3);
        te.process(b"A\r\nB");
        let lines = screen_text(&te);
        assert_eq!(&lines[0], "A         ");
        assert_eq!(&lines[1], "B         ");
    }

    #[test]
    fn test_cursor_movement() {
        let mut te = TerminalEmulator::new(10, 5);
        te.process(b"\x1b[3;5HA");
        let lines = screen_text(&te);
        assert_eq!(lines[2].chars().nth(4), Some('A'));
    }

    #[test]
    fn test_erase_display() {
        let mut te = TerminalEmulator::new(5, 3);
        te.process(b"AAAAA\nBBBBB\nCCCCC");
        te.process(b"\x1b[2J");
        let lines = screen_text(&te);
        assert_eq!(&lines[0], "     ");
        assert_eq!(&lines[1], "     ");
        assert_eq!(&lines[2], "     ");
    }

    #[test]
    fn test_line_wrap() {
        let mut te = TerminalEmulator::new(5, 3);
        te.process(b"ABCDEFGH");
        let lines = screen_text(&te);
        assert_eq!(&lines[0], "ABCDE");
        assert_eq!(&lines[1], "FGH  ");
    }

    #[test]
    fn test_sgr_bold() {
        let mut te = TerminalEmulator::new(10, 3);
        te.process(b"\x1b[1mBold\x1b[0m");
        let screen = te.parser.screen();
        let cell = screen.cell(0, 0).unwrap();
        assert!(cell.bold());
        // After reset, should not be bold
        let cell_after = screen.cell(0, 4).unwrap();
        assert!(!cell_after.bold());
    }

    #[test]
    fn test_sgr_true_color() {
        let mut te = TerminalEmulator::new(10, 3);
        te.process(b"\x1b[38;2;255;128;0mX");
        let screen = te.parser.screen();
        let cell = screen.cell(0, 0).unwrap();
        assert_eq!(cell.fgcolor(), vt100::Color::Rgb(255, 128, 0));
    }

    #[test]
    fn test_resize_preserves_scrollback() {
        let mut te = TerminalEmulator::new(5, 3);
        // Fill screen and scroll to create scrollback
        te.process(b"AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD\r\nEEEEE");
        let sb_before = te.scrollback_len();
        assert!(sb_before > 0, "should have scrollback");
        te.resize(10, 5);
        let sb_after = te.scrollback_len();
        // Scrollback should be preserved (may change slightly due to reflow)
        assert!(sb_after > 0, "scrollback should survive resize");
    }

    #[test]
    fn test_alternate_screen_does_not_corrupt_scrollback() {
        let mut te = TerminalEmulator::new(10, 3);
        // Write some content in normal mode to create scrollback
        te.process(b"Line1\r\nLine2\r\nLine3\r\nLine4\r\nLine5");
        let sb_before = te.scrollback_len();
        assert!(sb_before > 0, "should have scrollback before alt screen");

        // Enter alternate screen
        te.process(b"\x1b[?1049h");
        // Clear and draw in alternate screen (like Claude Code does)
        te.process(b"\x1b[2J\x1b[HALTSCREEN");
        te.process(b"\x1b[2J\x1b[HALTSCREEN2");
        te.process(b"\x1b[2J\x1b[HALTSCREEN3");

        // Leave alternate screen — scrollback should be preserved from before
        te.process(b"\x1b[?1049l");

        let sb_after = te.scrollback_len();
        assert_eq!(
            sb_after, sb_before,
            "alternate screen clears must not corrupt main scrollback"
        );
    }

    #[test]
    fn test_to_ratatui_lines() {
        let mut te = TerminalEmulator::new(5, 2);
        te.process(b"Hello");
        let lines = te.to_ratatui_lines();
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_scrollback_navigation() {
        let mut te = TerminalEmulator::new(5, 3);
        // Generate enough output to create scrollback
        for i in 0..20 {
            te.process(format!("L{:03}\r\n", i).as_bytes());
        }
        let sb = te.scrollback_len();
        assert!(sb > 0);

        // Read scrolled-back content with a modest offset
        let lines = te.to_ratatui_lines_scrolled(3);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn test_ansi_lines_scrolled() {
        let mut te = TerminalEmulator::new(10, 3);
        for i in 0..10 {
            te.process(format!("Line{:03}\r\n", i).as_bytes());
        }
        let lines = te.to_ansi_lines_scrolled(0);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn test_resize_noop() {
        let mut te = TerminalEmulator::new(5, 3);
        te.process(b"Hello");
        te.resize(5, 3); // same dimensions — no-op
        let lines = screen_text(&te);
        assert_eq!(&lines[0], "Hello");
    }

    // URL detection tests

    #[test]
    fn test_find_url_at_offset_basic() {
        let text = "visit https://example.com for info";
        assert_eq!(
            find_url_at_offset(text, 6),
            Some("https://example.com".to_string())
        );
        assert_eq!(
            find_url_at_offset(text, 24),
            Some("https://example.com".to_string())
        );
        // Before URL
        assert_eq!(find_url_at_offset(text, 0), None);
        // After URL
        assert_eq!(find_url_at_offset(text, 26), None);
    }

    #[test]
    fn test_find_url_at_offset_trailing_punctuation() {
        let text = "see https://example.com/path.";
        assert_eq!(
            find_url_at_offset(text, 4),
            Some("https://example.com/path".to_string())
        );
    }

    #[test]
    fn test_find_url_at_offset_parens() {
        // Wikipedia-style URL with parens
        let text = "https://en.wikipedia.org/wiki/Rust_(language) done";
        assert_eq!(
            find_url_at_offset(text, 0),
            Some("https://en.wikipedia.org/wiki/Rust_(language)".to_string())
        );
        // Trailing paren without opener should be stripped
        let text2 = "(https://example.com)";
        assert_eq!(
            find_url_at_offset(text2, 1),
            Some("https://example.com".to_string())
        );
    }

    #[test]
    fn test_find_url_at_offset_http() {
        let text = "http://localhost:3000/api";
        assert_eq!(
            find_url_at_offset(text, 0),
            Some("http://localhost:3000/api".to_string())
        );
    }

    #[test]
    fn test_find_url_at_offset_no_url() {
        assert_eq!(find_url_at_offset("no urls here", 5), None);
    }

    #[test]
    fn test_url_at_position_in_terminal() {
        let mut te = TerminalEmulator::new(60, 3);
        te.process(b"Check https://example.com/path for details");
        assert_eq!(
            te.url_at_position(0, 6),
            Some("https://example.com/path".to_string())
        );
        assert_eq!(te.url_at_position(0, 0), None);
        assert_eq!(te.url_at_position(1, 0), None);
    }
}
