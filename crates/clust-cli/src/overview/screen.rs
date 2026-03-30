use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use vte::{Params, Perform};

// ---------------------------------------------------------------------------
// Cell
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            style: Style::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Screen — the VTE Perform implementor
// ---------------------------------------------------------------------------

pub struct Screen {
    grid: Vec<Vec<Cell>>,
    pub cols: usize,
    pub rows: usize,
    cursor_row: usize,
    cursor_col: usize,
    wrap_pending: bool,
    saved_cursor: (usize, usize),
    current_style: Style,
    scroll_top: usize,
    scroll_bottom: usize, // inclusive
}

impl Screen {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            grid: vec![vec![Cell::default(); cols]; rows],
            cols,
            rows,
            cursor_row: 0,
            cursor_col: 0,
            wrap_pending: false,
            saved_cursor: (0, 0),
            current_style: Style::default(),
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        // Clear grid to prevent stale content artifacts after resize.
        // The agent will send a full redraw after receiving SIGWINCH.
        self.grid.clear();
        self.grid.resize_with(rows, || vec![Cell::default(); cols]);
        self.cols = cols;
        self.rows = rows;
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
    }

    pub fn to_ratatui_lines(&self) -> Vec<Line<'static>> {
        self.grid
            .iter()
            .map(|row| {
                if row.is_empty() {
                    return Line::from("");
                }
                let mut spans: Vec<Span<'static>> = Vec::new();
                let mut text = String::new();
                let mut cur_style = row[0].style;

                for cell in row {
                    if cell.style != cur_style {
                        if !text.is_empty() {
                            spans.push(Span::styled(std::mem::take(&mut text), cur_style));
                        }
                        cur_style = cell.style;
                    }
                    text.push(cell.ch);
                }
                if !text.is_empty() {
                    spans.push(Span::styled(text, cur_style));
                }
                Line::from(spans)
            })
            .collect()
    }

    // -- Private helpers -----------------------------------------------------

    fn scroll_up(&mut self) {
        if self.scroll_top < self.scroll_bottom && self.scroll_bottom < self.rows {
            self.grid.remove(self.scroll_top);
            self.grid
                .insert(self.scroll_bottom, vec![Cell::default(); self.cols]);
        }
    }

    fn scroll_down(&mut self) {
        if self.scroll_top < self.scroll_bottom && self.scroll_bottom < self.rows {
            self.grid.remove(self.scroll_bottom);
            self.grid
                .insert(self.scroll_top, vec![Cell::default(); self.cols]);
        }
    }

    fn linefeed(&mut self) {
        if self.cursor_row == self.scroll_bottom {
            self.scroll_up();
        } else if self.cursor_row + 1 < self.rows {
            self.cursor_row += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.cursor_row == self.scroll_top {
            self.scroll_down();
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
        }
    }

    fn clear_row(&mut self, row: usize) {
        if row < self.rows {
            self.grid[row] = vec![Cell::default(); self.cols];
        }
    }

    fn erase_in_display(&mut self, mode: u16) {
        match mode {
            // From cursor to end of screen
            0 => {
                self.erase_in_line(0);
                for r in (self.cursor_row + 1)..self.rows {
                    self.clear_row(r);
                }
            }
            // From start of screen to cursor
            1 => {
                for r in 0..self.cursor_row {
                    self.clear_row(r);
                }
                self.erase_in_line(1);
            }
            // Entire screen
            2 | 3 => {
                for r in 0..self.rows {
                    self.clear_row(r);
                }
            }
            _ => {}
        }
    }

    fn erase_in_line(&mut self, mode: u16) {
        if self.cursor_row >= self.rows {
            return;
        }
        let row = &mut self.grid[self.cursor_row];
        match mode {
            // From cursor to end of line
            0 => {
                for cell in row.iter_mut().take(self.cols).skip(self.cursor_col) {
                    *cell = Cell::default();
                }
            }
            // From start of line to cursor
            1 => {
                for cell in row.iter_mut().take(self.cursor_col.min(self.cols - 1) + 1) {
                    *cell = Cell::default();
                }
            }
            // Entire line
            2 => {
                for cell in row.iter_mut().take(self.cols) {
                    *cell = Cell::default();
                }
            }
            _ => {}
        }
    }

    fn write_char(&mut self, c: char) {
        if self.wrap_pending {
            self.wrap_pending = false;
            self.cursor_col = 0;
            self.linefeed();
        }
        if self.cursor_row < self.rows && self.cursor_col < self.cols {
            self.grid[self.cursor_row][self.cursor_col] = Cell {
                ch: c,
                style: self.current_style,
            };
            if self.cursor_col + 1 >= self.cols {
                self.wrap_pending = true;
            } else {
                self.cursor_col += 1;
            }
        }
    }

    fn apply_sgr(&mut self, params: &Params) {
        let mut iter = params.iter();
        // If no params, treat as reset
        let first = match iter.next() {
            Some(p) => p,
            None => {
                self.current_style = Style::default();
                return;
            }
        };

        // Process first param and all remaining params
        let mut pending: Option<&[u16]> = Some(first);
        while let Some(param) = pending.take().or_else(|| iter.next()) {
            let code = param[0];
            match code {
                0 => self.current_style = Style::default(),
                1 => self.current_style = self.current_style.add_modifier(Modifier::BOLD),
                2 => self.current_style = self.current_style.add_modifier(Modifier::DIM),
                3 => self.current_style = self.current_style.add_modifier(Modifier::ITALIC),
                4 => self.current_style = self.current_style.add_modifier(Modifier::UNDERLINED),
                7 => self.current_style = self.current_style.add_modifier(Modifier::REVERSED),
                8 => self.current_style = self.current_style.add_modifier(Modifier::HIDDEN),
                9 => {
                    self.current_style = self.current_style.add_modifier(Modifier::CROSSED_OUT)
                }
                22 => {
                    self.current_style = self
                        .current_style
                        .remove_modifier(Modifier::BOLD | Modifier::DIM)
                }
                23 => {
                    self.current_style = self.current_style.remove_modifier(Modifier::ITALIC)
                }
                24 => {
                    self.current_style = self.current_style.remove_modifier(Modifier::UNDERLINED)
                }
                27 => {
                    self.current_style = self.current_style.remove_modifier(Modifier::REVERSED)
                }
                28 => self.current_style = self.current_style.remove_modifier(Modifier::HIDDEN),
                29 => {
                    self.current_style = self.current_style.remove_modifier(Modifier::CROSSED_OUT)
                }
                // Standard foreground colors
                30..=37 => {
                    self.current_style = self.current_style.fg(ansi_color(code - 30));
                }
                38 => {
                    // Extended foreground: 38;5;N or 38;2;R;G;B
                    // Subparams come as part of the same slice or next slices
                    if param.len() >= 3 && param[1] == 5 {
                        self.current_style =
                            self.current_style.fg(color256(param[2]));
                    } else if param.len() >= 5 && param[1] == 2 {
                        self.current_style = self.current_style.fg(Color::Rgb(
                            param[2] as u8,
                            param[3] as u8,
                            param[4] as u8,
                        ));
                    } else {
                        // Try consuming next params (semicolon-separated)
                        self.parse_extended_color(&mut iter, true);
                    }
                }
                39 => self.current_style = self.current_style.fg(Color::Reset),
                // Standard background colors
                40..=47 => {
                    self.current_style = self.current_style.bg(ansi_color(code - 40));
                }
                48 => {
                    if param.len() >= 3 && param[1] == 5 {
                        self.current_style =
                            self.current_style.bg(color256(param[2]));
                    } else if param.len() >= 5 && param[1] == 2 {
                        self.current_style = self.current_style.bg(Color::Rgb(
                            param[2] as u8,
                            param[3] as u8,
                            param[4] as u8,
                        ));
                    } else {
                        self.parse_extended_color(&mut iter, false);
                    }
                }
                49 => self.current_style = self.current_style.bg(Color::Reset),
                // Bright foreground colors
                90..=97 => {
                    self.current_style = self.current_style.fg(ansi_bright_color(code - 90));
                }
                // Bright background colors
                100..=107 => {
                    self.current_style = self.current_style.bg(ansi_bright_color(code - 100));
                }
                _ => {}
            }
        }
    }

    /// Parse extended color from subsequent semicolon-separated params.
    fn parse_extended_color<'a>(
        &mut self,
        iter: &mut impl Iterator<Item = &'a [u16]>,
        foreground: bool,
    ) {
        let Some(mode_param) = iter.next() else {
            return;
        };
        let mode = mode_param[0];
        match mode {
            5 => {
                // 256-color: next param is color index
                if let Some(idx_param) = iter.next() {
                    let color = color256(idx_param[0]);
                    if foreground {
                        self.current_style = self.current_style.fg(color);
                    } else {
                        self.current_style = self.current_style.bg(color);
                    }
                }
            }
            2 => {
                // True-color: next three params are R, G, B
                let r = iter.next().map(|p| p[0] as u8).unwrap_or(0);
                let g = iter.next().map(|p| p[0] as u8).unwrap_or(0);
                let b = iter.next().map(|p| p[0] as u8).unwrap_or(0);
                let color = Color::Rgb(r, g, b);
                if foreground {
                    self.current_style = self.current_style.fg(color);
                } else {
                    self.current_style = self.current_style.bg(color);
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// vte::Perform implementation
// ---------------------------------------------------------------------------

impl Perform for Screen {
    fn print(&mut self, c: char) {
        self.write_char(c);
    }

    fn execute(&mut self, byte: u8) {
        self.wrap_pending = false;
        match byte {
            // Line feed / vertical tab / form feed
            0x0A..=0x0C => self.linefeed(),
            // Carriage return
            0x0D => self.cursor_col = 0,
            // Backspace
            0x08 => {
                self.cursor_col = self.cursor_col.saturating_sub(1);
            }
            // Tab
            0x09 => {
                self.cursor_col = ((self.cursor_col / 8) + 1) * 8;
                if self.cursor_col >= self.cols {
                    self.cursor_col = self.cols.saturating_sub(1);
                }
            }
            // Bell — ignore
            0x07 => {}
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let first = params.iter().next().map(|p| p[0]).unwrap_or(0);
        let second = params.iter().nth(1).map(|p| p[0]).unwrap_or(0);

        // Check for private mode indicator
        let private = intermediates.first() == Some(&b'?');

        // Clear wrap pending on any CSI that moves the cursor
        match action {
            'A' | 'B' | 'C' | 'D' | 'E' | 'F' | 'G' | 'H' | 'f' | 'r' | 's' | 'u' => {
                self.wrap_pending = false;
            }
            _ => {}
        }

        match action {
            // CUU — Cursor Up
            'A' => {
                let n = first.max(1) as usize;
                self.cursor_row = self.cursor_row.saturating_sub(n);
            }
            // CUD — Cursor Down
            'B' => {
                let n = first.max(1) as usize;
                self.cursor_row = (self.cursor_row + n).min(self.rows - 1);
            }
            // CUF — Cursor Forward
            'C' => {
                let n = first.max(1) as usize;
                self.cursor_col = (self.cursor_col + n).min(self.cols - 1);
            }
            // CUB — Cursor Back
            'D' => {
                let n = first.max(1) as usize;
                self.cursor_col = self.cursor_col.saturating_sub(n);
            }
            // CNL — Cursor Next Line
            'E' => {
                let n = first.max(1) as usize;
                self.cursor_row = (self.cursor_row + n).min(self.rows - 1);
                self.cursor_col = 0;
            }
            // CPL — Cursor Previous Line
            'F' => {
                let n = first.max(1) as usize;
                self.cursor_row = self.cursor_row.saturating_sub(n);
                self.cursor_col = 0;
            }
            // CHA — Cursor Horizontal Absolute
            'G' => {
                let col = first.max(1) as usize;
                self.cursor_col = (col - 1).min(self.cols - 1);
            }
            // CUP / HVP — Cursor Position
            'H' | 'f' => {
                let row = first.max(1) as usize;
                let col = second.max(1) as usize;
                self.cursor_row = (row - 1).min(self.rows - 1);
                self.cursor_col = (col - 1).min(self.cols - 1);
            }
            // ED — Erase in Display
            'J' => {
                self.erase_in_display(first);
            }
            // EL — Erase in Line
            'K' => {
                self.erase_in_line(first);
            }
            // IL — Insert Lines
            'L' => {
                let n = first.max(1) as usize;
                if self.cursor_row >= self.scroll_top && self.cursor_row <= self.scroll_bottom {
                    for _ in 0..n {
                        if self.scroll_bottom < self.rows {
                            self.grid.remove(self.scroll_bottom);
                            self.grid
                                .insert(self.cursor_row, vec![Cell::default(); self.cols]);
                        }
                    }
                }
            }
            // DL — Delete Lines
            'M' => {
                let n = first.max(1) as usize;
                if self.cursor_row >= self.scroll_top && self.cursor_row <= self.scroll_bottom {
                    for _ in 0..n {
                        if self.scroll_bottom < self.rows {
                            self.grid.remove(self.cursor_row);
                            self.grid
                                .insert(self.scroll_bottom, vec![Cell::default(); self.cols]);
                        }
                    }
                }
            }
            // DCH — Delete Characters
            'P' => {
                let n = first.max(1) as usize;
                if self.cursor_row < self.rows {
                    let row = &mut self.grid[self.cursor_row];
                    for _ in 0..n.min(self.cols - self.cursor_col) {
                        if self.cursor_col < row.len() {
                            row.remove(self.cursor_col);
                            row.push(Cell::default());
                        }
                    }
                }
            }
            // SU — Scroll Up
            'S' if !private => {
                let n = first.max(1) as usize;
                for _ in 0..n {
                    self.scroll_up();
                }
            }
            // SD — Scroll Down
            'T' if !private => {
                let n = first.max(1) as usize;
                for _ in 0..n {
                    self.scroll_down();
                }
            }
            // ICH — Insert Characters
            '@' => {
                let n = first.max(1) as usize;
                if self.cursor_row < self.rows {
                    let row = &mut self.grid[self.cursor_row];
                    for _ in 0..n.min(self.cols - self.cursor_col) {
                        if row.len() > self.cols.saturating_sub(1) {
                            row.pop();
                        }
                        row.insert(self.cursor_col, Cell::default());
                    }
                    row.truncate(self.cols);
                }
            }
            // SGR — Select Graphic Rendition
            'm' => {
                self.apply_sgr(params);
            }
            // DECSTBM — Set Scrolling Region
            'r' if !private => {
                let top = first.max(1) as usize;
                let bot = if second == 0 {
                    self.rows
                } else {
                    second as usize
                };
                if top < bot && bot <= self.rows {
                    self.scroll_top = top - 1;
                    self.scroll_bottom = bot - 1;
                }
                self.cursor_row = 0;
                self.cursor_col = 0;
            }
            // ECH — Erase Characters
            'X' => {
                let n = first.max(1) as usize;
                if self.cursor_row < self.rows {
                    let row = &mut self.grid[self.cursor_row];
                    let end = (self.cursor_col + n).min(self.cols);
                    for cell in row.iter_mut().take(end).skip(self.cursor_col) {
                        *cell = Cell::default();
                    }
                }
            }
            // Private modes — ignore (cursor visibility, mouse, alt screen, etc.)
            'h' | 'l' => {}
            // Cursor save/restore (CSI s / CSI u)
            's' if !private => {
                self.saved_cursor = (self.cursor_row, self.cursor_col);
            }
            'u' if !private => {
                self.cursor_row = self.saved_cursor.0.min(self.rows - 1);
                self.cursor_col = self.saved_cursor.1.min(self.cols - 1);
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (byte, intermediates) {
            // IND — Index (move cursor down, scroll if at bottom of scroll region)
            (b'D', []) => self.linefeed(),
            // RI — Reverse Index
            (b'M', []) => self.reverse_index(),
            // DECSC — Save Cursor
            (b'7', []) => {
                self.saved_cursor = (self.cursor_row, self.cursor_col);
            }
            // DECRC — Restore Cursor
            (b'8', []) => {
                self.cursor_row = self.saved_cursor.0.min(self.rows - 1);
                self.cursor_col = self.saved_cursor.1.min(self.cols - 1);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// VirtualTerminal — owns Parser + Screen
// ---------------------------------------------------------------------------

pub struct VirtualTerminal {
    parser: vte::Parser,
    pub screen: Screen,
}

impl VirtualTerminal {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            parser: vte::Parser::new(),
            screen: Screen::new(cols, rows),
        }
    }

    pub fn process(&mut self, data: &[u8]) {
        for &byte in data {
            self.parser.advance(&mut self.screen, byte);
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.screen.resize(cols, rows);
    }
}

// ---------------------------------------------------------------------------
// Color helpers
// ---------------------------------------------------------------------------

fn ansi_color(idx: u16) -> Color {
    match idx {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::White,
        _ => Color::Reset,
    }
}

fn ansi_bright_color(idx: u16) -> Color {
    match idx {
        0 => Color::DarkGray,
        1 => Color::LightRed,
        2 => Color::LightGreen,
        3 => Color::LightYellow,
        4 => Color::LightBlue,
        5 => Color::LightMagenta,
        6 => Color::LightCyan,
        7 => Color::Gray,
        _ => Color::Reset,
    }
}

fn color256(idx: u16) -> Color {
    Color::Indexed(idx as u8)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_text(screen: &Screen) -> Vec<String> {
        screen
            .grid
            .iter()
            .map(|row| row.iter().map(|c| c.ch).collect())
            .collect()
    }

    #[test]
    fn test_basic_print() {
        let mut vt = VirtualTerminal::new(10, 3);
        vt.process(b"Hello");
        let lines = screen_text(&vt.screen);
        assert_eq!(&lines[0], "Hello     ");
    }

    #[test]
    fn test_newline() {
        let mut vt = VirtualTerminal::new(10, 3);
        // LF only moves cursor down, CR+LF moves to start of next line
        vt.process(b"A\r\nB");
        let lines = screen_text(&vt.screen);
        assert_eq!(&lines[0], "A         ");
        assert_eq!(&lines[1], "B         ");
    }

    #[test]
    fn test_carriage_return() {
        let mut vt = VirtualTerminal::new(10, 3);
        vt.process(b"Hello\rXY");
        let lines = screen_text(&vt.screen);
        assert_eq!(&lines[0], "XYllo     ");
    }

    #[test]
    fn test_cursor_movement() {
        let mut vt = VirtualTerminal::new(10, 5);
        // Move cursor to row 3, col 5 (1-indexed)
        vt.process(b"\x1b[3;5HA");
        let lines = screen_text(&vt.screen);
        assert_eq!(lines[2].chars().nth(4), Some('A'));
    }

    #[test]
    fn test_erase_display() {
        let mut vt = VirtualTerminal::new(5, 3);
        vt.process(b"AAAAA\nBBBBB\nCCCCC");
        // Erase entire display
        vt.process(b"\x1b[2J");
        let lines = screen_text(&vt.screen);
        assert_eq!(&lines[0], "     ");
        assert_eq!(&lines[1], "     ");
        assert_eq!(&lines[2], "     ");
    }

    #[test]
    fn test_erase_line() {
        let mut vt = VirtualTerminal::new(10, 3);
        vt.process(b"ABCDEFGHIJ");
        // Move to col 5, erase from cursor to end
        vt.process(b"\x1b[1;6H\x1b[0K");
        let lines = screen_text(&vt.screen);
        assert_eq!(&lines[0], "ABCDE     ");
    }

    #[test]
    fn test_scroll_region() {
        let mut vt = VirtualTerminal::new(5, 5);
        // Use CUP to position each line precisely
        vt.process(b"\x1b[1;1HAAAAA\x1b[2;1HBBBBB\x1b[3;1HCCCCC\x1b[4;1HDDDDD\x1b[5;1HEEEEE");
        // Set scroll region to lines 2-4
        vt.process(b"\x1b[2;4r");
        // Move to line 4 and do linefeed (should scroll within region)
        vt.process(b"\x1b[4;1H\n");
        let lines = screen_text(&vt.screen);
        assert_eq!(&lines[0], "AAAAA"); // unchanged (outside region)
        assert_eq!(&lines[1], "CCCCC"); // was line 3
        assert_eq!(&lines[2], "DDDDD"); // was line 4
        assert_eq!(&lines[3], "     "); // new empty line
        assert_eq!(&lines[4], "EEEEE"); // unchanged (outside region)
    }

    #[test]
    fn test_line_wrap() {
        let mut vt = VirtualTerminal::new(5, 3);
        vt.process(b"ABCDEFGH");
        let lines = screen_text(&vt.screen);
        assert_eq!(&lines[0], "ABCDE");
        assert_eq!(&lines[1], "FGH  ");
    }

    #[test]
    fn test_sgr_bold() {
        let mut vt = VirtualTerminal::new(10, 3);
        vt.process(b"\x1b[1mBold\x1b[0m");
        assert!(vt.screen.grid[0][0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        // After reset, should not be bold
        assert!(!vt.screen.grid[0][4]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
    }

    #[test]
    fn test_sgr_true_color() {
        let mut vt = VirtualTerminal::new(10, 3);
        // Set fg to RGB(255, 128, 0) using colon-separated subparams
        vt.process(b"\x1b[38;2;255;128;0mX");
        let style = vt.screen.grid[0][0].style;
        assert_eq!(style.fg, Some(Color::Rgb(255, 128, 0)));
    }

    #[test]
    fn test_backspace() {
        let mut vt = VirtualTerminal::new(10, 3);
        vt.process(b"AB\x08C");
        let lines = screen_text(&vt.screen);
        assert_eq!(&lines[0], "AC        ");
    }

    #[test]
    fn test_resize() {
        let mut vt = VirtualTerminal::new(5, 3);
        vt.process(b"Hello");
        vt.resize(10, 5);
        assert_eq!(vt.screen.cols, 10);
        assert_eq!(vt.screen.rows, 5);
        // Grid is cleared on resize to prevent stale content artifacts
        let lines = screen_text(&vt.screen);
        assert_eq!(&lines[0], "          ");
    }

    #[test]
    fn test_resize_noop_preserves_content() {
        let mut vt = VirtualTerminal::new(5, 3);
        vt.process(b"Hello");
        vt.resize(5, 3); // same dimensions — no-op
        let lines = screen_text(&vt.screen);
        assert_eq!(&lines[0], "Hello");
    }

    #[test]
    fn test_to_ratatui_lines() {
        let mut vt = VirtualTerminal::new(5, 2);
        vt.process(b"Hello");
        let lines = vt.screen.to_ratatui_lines();
        assert_eq!(lines.len(), 2);
    }
}
