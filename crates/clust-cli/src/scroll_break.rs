//! Input filter that detects mouse scroll escape sequences and optionally
//! rate-limits them. Non-scroll bytes always pass through immediately.
//!
//! Supports both SGR extended mouse mode (`\x1b[<button;x;yM`) and legacy
//! mouse mode (`\x1b[M` + 3 raw bytes). Each `AttachedSession` owns its
//! own `ScrollBreak`, so scroll speed can be configured per terminal.

use std::time::{Duration, Instant};

use crate::output_filter::OutputFilter;

/// Maximum pending buffer size before safety-flush (8 KB).
const MAX_PENDING: usize = 8192;

// ── Configuration ────────────────────────────────────────────────────

/// Scroll throttle mode.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ScrollMode {
    /// No throttling — all scroll events pass through.
    Full,
    /// Rate-limited — at most `max_per_sec` scroll events forwarded per second.
    RateLimited { max_per_sec: u32 },
}

// ── Parser state ─────────────────────────────────────────────────────

/// State machine for detecting mouse escape sequences in raw input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Normal bytes — not inside any escape sequence.
    Ground,
    /// Received ESC (0x1B), waiting for the next byte.
    Escape,
    /// Inside a CSI sequence (ESC [). Accumulating parameter bytes.
    Csi,
    /// Inside a legacy mouse sequence (ESC [ M). Expecting `remaining` raw bytes.
    LegacyMouse { remaining: u8 },
}

// ── ScrollBreak ──────────────────────────────────────────────────────

/// Input filter that throttles mouse scroll events in raw terminal byte streams.
pub struct ScrollBreak {
    mode: ScrollMode,
    /// Minimum time between forwarded scroll events.
    min_interval: Duration,
    /// Timestamp of the last scroll event that was forwarded.
    last_scroll: Option<Instant>,
    /// Buffer for partial escape sequences that span read chunk boundaries.
    pending: Vec<u8>,
}

impl ScrollBreak {
    pub fn new(mode: ScrollMode) -> Self {
        let min_interval = match &mode {
            ScrollMode::Full => Duration::ZERO,
            ScrollMode::RateLimited { max_per_sec } => {
                if *max_per_sec == 0 {
                    Duration::MAX
                } else {
                    Duration::from_secs(1) / *max_per_sec
                }
            }
        };
        Self {
            mode,
            min_interval,
            last_scroll: None,
            pending: Vec::new(),
        }
    }

    /// Filter a chunk of raw input bytes. Returns bytes to forward to the agent.
    pub fn filter(&mut self, data: &[u8]) -> Vec<u8> {
        self.filter_at(data, Instant::now())
    }

    /// Filter with an explicit timestamp (for deterministic testing).
    fn filter_at(&mut self, data: &[u8], now: Instant) -> Vec<u8> {
        let mut input = std::mem::take(&mut self.pending);
        input.extend_from_slice(data);

        // Safety valve: if buffer is too large, flush everything
        if input.len() > MAX_PENDING {
            return input;
        }

        let mut output = Vec::with_capacity(input.len());
        let mut state = State::Ground;
        // Start index of the current escape sequence being accumulated
        let mut seq_start: usize = 0;

        for (i, &byte) in input.iter().enumerate() {
            match state {
                State::Ground => {
                    if byte == 0x1b {
                        state = State::Escape;
                        seq_start = i;
                    } else {
                        output.push(byte);
                    }
                }
                State::Escape => {
                    if byte == b'[' {
                        state = State::Csi;
                    } else {
                        // Two-character escape (e.g. ESC 7, alt+key) — forward it
                        output.extend_from_slice(&input[seq_start..=i]);
                        state = State::Ground;
                    }
                }
                State::Csi => {
                    match byte {
                        // Final byte — CSI sequence complete
                        0x40..=0x7e => {
                            let params = &input[seq_start + 2..i]; // bytes between [ and final
                            if byte == b'M' && params.is_empty() {
                                // Legacy mouse introducer: ESC [ M + 3 raw bytes
                                state = State::LegacyMouse { remaining: 3 };
                            } else if (byte == b'M' || byte == b'm')
                                && params.first() == Some(&b'<')
                            {
                                // SGR mouse event — check if scroll
                                let button = parse_sgr_button(&params[1..]);
                                if is_scroll_button(button) {
                                    if self.should_allow_scroll(now) {
                                        output.extend_from_slice(&input[seq_start..=i]);
                                    }
                                    // else: drop the scroll event
                                } else {
                                    output.extend_from_slice(&input[seq_start..=i]);
                                }
                                state = State::Ground;
                            } else {
                                // Non-mouse CSI sequence — forward unchanged
                                output.extend_from_slice(&input[seq_start..=i]);
                                state = State::Ground;
                            }
                        }
                        // Parameter bytes (0-9 ; < = > ?) or intermediate bytes (space-/)
                        0x20..=0x3f => {
                            // Continue accumulating
                        }
                        // Invalid byte — bail out, forward everything
                        _ => {
                            output.extend_from_slice(&input[seq_start..=i]);
                            state = State::Ground;
                        }
                    }
                }
                State::LegacyMouse { remaining } => {
                    let remaining = remaining - 1;
                    if remaining == 0 {
                        // All 3 bytes received. First byte after ESC[M is button+32.
                        let button_byte = input[seq_start + 3]; // first byte after ESC [ M
                        let button = (button_byte as u32).wrapping_sub(32);
                        if is_scroll_button(button) {
                            if self.should_allow_scroll(now) {
                                output.extend_from_slice(&input[seq_start..=i]);
                            }
                        } else {
                            output.extend_from_slice(&input[seq_start..=i]);
                        }
                        state = State::Ground;
                    } else {
                        state = State::LegacyMouse { remaining };
                    }
                }
            }
        }

        // Buffer any incomplete escape sequence
        if state != State::Ground {
            self.pending = input[seq_start..].to_vec();
        }

        output
    }

    /// Decide whether a scroll event should be forwarded based on the rate limit.
    fn should_allow_scroll(&mut self, now: Instant) -> bool {
        match self.mode {
            ScrollMode::Full => true,
            ScrollMode::RateLimited { max_per_sec } => {
                if max_per_sec == 0 {
                    return false;
                }
                match self.last_scroll {
                    None => {
                        self.last_scroll = Some(now);
                        true
                    }
                    Some(last) => {
                        if now.duration_since(last) >= self.min_interval {
                            self.last_scroll = Some(now);
                            true
                        } else {
                            false
                        }
                    }
                }
            }
        }
    }
}

impl OutputFilter for ScrollBreak {
    fn filter(&mut self, data: &[u8]) -> Vec<u8> {
        self.filter_at(data, Instant::now())
    }

    fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

/// Parse the button number from SGR mouse parameter bytes.
/// Input is the bytes after `<` and before the first `;` (or the end).
/// Returns the parsed button number, or u32::MAX on parse failure.
fn parse_sgr_button(params: &[u8]) -> u32 {
    let end = params.iter().position(|&b| b == b';').unwrap_or(params.len());
    let digits = &params[..end];
    if digits.is_empty() {
        return u32::MAX;
    }
    let mut n: u32 = 0;
    for &d in digits {
        if !d.is_ascii_digit() {
            return u32::MAX;
        }
        n = n.saturating_mul(10).saturating_add((d - b'0') as u32);
    }
    n
}

/// Check if a mouse button value represents a scroll event.
/// Scroll events have bit 6 set and bit 7 clear: (button & 0xC0) == 0x40.
fn is_scroll_button(button: u32) -> bool {
    (button & 0xC0) == 0x40
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn full() -> ScrollBreak {
        ScrollBreak::new(ScrollMode::Full)
    }

    fn limited(max_per_sec: u32) -> ScrollBreak {
        ScrollBreak::new(ScrollMode::RateLimited { max_per_sec })
    }

    // ── Helper: build SGR mouse escape sequence ──────────────────────

    fn sgr_mouse(button: u32, x: u32, y: u32, press: bool) -> Vec<u8> {
        let suffix = if press { b'M' } else { b'm' };
        format!("\x1b[<{button};{x};{y}{}", suffix as char).into_bytes()
    }

    fn legacy_mouse(button: u8, x: u8, y: u8) -> Vec<u8> {
        vec![0x1b, b'[', b'M', button + 32, x + 32, y + 32]
    }

    // ── Pass-through tests ───────────────────────────────────────────

    #[test]
    fn plain_text_passes_through() {
        let mut sb = full();
        assert_eq!(sb.filter(b"hello world"), b"hello world");
    }

    #[test]
    fn empty_input_returns_empty() {
        let mut sb = full();
        assert_eq!(sb.filter(b""), b"".to_vec());
    }

    #[test]
    fn non_mouse_csi_passes_through() {
        let mut sb = full();
        // Cursor position
        assert_eq!(sb.filter(b"\x1b[1;2H"), b"\x1b[1;2H");
        // Erase line
        assert_eq!(sb.filter(b"\x1b[2K"), b"\x1b[2K");
        // SGR color
        assert_eq!(
            sb.filter(b"\x1b[38;2;255;0;0m"),
            b"\x1b[38;2;255;0;0m"
        );
    }

    #[test]
    fn two_char_escape_passes_through() {
        let mut sb = full();
        // Save/restore cursor
        assert_eq!(sb.filter(b"\x1b7text\x1b8"), b"\x1b7text\x1b8");
    }

    #[test]
    fn ctrl_bytes_pass_through() {
        let mut sb = full();
        assert_eq!(sb.filter(b"\x07\x08\t\n\r"), b"\x07\x08\t\n\r");
    }

    #[test]
    fn non_scroll_sgr_mouse_passes_through() {
        let mut sb = full();
        // Left click (button 0)
        let click = sgr_mouse(0, 15, 20, true);
        assert_eq!(sb.filter(&click), click);
        // Right click (button 2)
        let rclick = sgr_mouse(2, 15, 20, true);
        assert_eq!(sb.filter(&rclick), rclick);
    }

    #[test]
    fn non_scroll_legacy_mouse_passes_through() {
        let mut sb = full();
        // Left click (button 0)
        let click = legacy_mouse(0, 10, 20);
        assert_eq!(sb.filter(&click), click);
    }

    #[test]
    fn mixed_text_and_non_scroll_sequences() {
        let mut sb = full();
        let mut input = b"hello ".to_vec();
        input.extend_from_slice(b"\x1b[1;2H");
        input.extend_from_slice(b" world");
        input.extend_from_slice(&sgr_mouse(0, 5, 5, true));
        assert_eq!(sb.filter(&input), input);
    }

    // ── Scroll detection (Full mode) ─────────────────────────────────

    #[test]
    fn sgr_scroll_up_passes_in_full_mode() {
        let mut sb = full();
        let scroll = sgr_mouse(64, 10, 20, true);
        assert_eq!(sb.filter(&scroll), scroll);
    }

    #[test]
    fn sgr_scroll_down_passes_in_full_mode() {
        let mut sb = full();
        let scroll = sgr_mouse(65, 10, 20, true);
        assert_eq!(sb.filter(&scroll), scroll);
    }

    #[test]
    fn sgr_scroll_left_passes_in_full_mode() {
        let mut sb = full();
        let scroll = sgr_mouse(66, 10, 20, true);
        assert_eq!(sb.filter(&scroll), scroll);
    }

    #[test]
    fn sgr_scroll_right_passes_in_full_mode() {
        let mut sb = full();
        let scroll = sgr_mouse(67, 10, 20, true);
        assert_eq!(sb.filter(&scroll), scroll);
    }

    #[test]
    fn sgr_scroll_with_shift_passes_in_full_mode() {
        let mut sb = full();
        // Shift+scroll up = 64 + 4 = 68
        let scroll = sgr_mouse(68, 10, 20, true);
        assert_eq!(sb.filter(&scroll), scroll);
    }

    #[test]
    fn sgr_scroll_with_ctrl_passes_in_full_mode() {
        let mut sb = full();
        // Ctrl+scroll down = 65 + 16 = 81
        let scroll = sgr_mouse(81, 10, 20, true);
        assert_eq!(sb.filter(&scroll), scroll);
    }

    #[test]
    fn legacy_scroll_up_passes_in_full_mode() {
        let mut sb = full();
        let scroll = legacy_mouse(64, 10, 20);
        assert_eq!(sb.filter(&scroll), scroll);
    }

    #[test]
    fn legacy_scroll_down_passes_in_full_mode() {
        let mut sb = full();
        let scroll = legacy_mouse(65, 10, 20);
        assert_eq!(sb.filter(&scroll), scroll);
    }

    // ── Rate-limiting tests ──────────────────────────────────────────

    #[test]
    fn first_scroll_always_passes() {
        let mut sb = limited(5);
        let now = t0();
        let scroll = sgr_mouse(64, 10, 20, true);
        assert_eq!(sb.filter_at(&scroll, now), scroll);
    }

    #[test]
    fn scroll_within_interval_is_dropped() {
        let mut sb = limited(5); // min_interval = 200ms
        let now = t0();
        let scroll = sgr_mouse(64, 10, 20, true);
        assert_eq!(sb.filter_at(&scroll, now), scroll);
        // 50ms later — too soon
        let out = sb.filter_at(&scroll, now + Duration::from_millis(50));
        assert!(out.is_empty());
    }

    #[test]
    fn scroll_after_interval_passes() {
        let mut sb = limited(5); // min_interval = 200ms
        let now = t0();
        let scroll = sgr_mouse(64, 10, 20, true);
        assert_eq!(sb.filter_at(&scroll, now), scroll);
        // 250ms later — allowed
        assert_eq!(
            sb.filter_at(&scroll, now + Duration::from_millis(250)),
            scroll
        );
    }

    #[test]
    fn scroll_at_exact_interval_passes() {
        let mut sb = limited(5); // min_interval = 200ms
        let now = t0();
        let scroll = sgr_mouse(65, 10, 20, true);
        assert_eq!(sb.filter_at(&scroll, now), scroll);
        // Exactly 200ms later — passes (>=)
        assert_eq!(
            sb.filter_at(&scroll, now + Duration::from_millis(200)),
            scroll
        );
    }

    #[test]
    fn rapid_burst_only_first_passes() {
        let mut sb = limited(5);
        let now = t0();
        let scroll = sgr_mouse(64, 10, 20, true);
        // First passes
        assert_eq!(sb.filter_at(&scroll, now), scroll);
        // Next 4 at same timestamp are dropped
        for _ in 0..4 {
            assert!(sb.filter_at(&scroll, now).is_empty());
        }
    }

    #[test]
    fn interleaved_scroll_and_text() {
        let mut sb = limited(5);
        let now = t0();

        let mut chunk = b"hello".to_vec();
        chunk.extend_from_slice(&sgr_mouse(64, 10, 20, true));
        chunk.extend_from_slice(b"world");
        chunk.extend_from_slice(&sgr_mouse(65, 10, 21, true));
        chunk.extend_from_slice(b"end");

        let out = sb.filter_at(&chunk, now);

        // First scroll passes, second dropped, all text passes
        let mut expected = b"hello".to_vec();
        expected.extend_from_slice(&sgr_mouse(64, 10, 20, true));
        expected.extend_from_slice(b"world");
        // Second scroll dropped
        expected.extend_from_slice(b"end");

        assert_eq!(out, expected);
    }

    #[test]
    fn non_scroll_mouse_not_rate_limited() {
        let mut sb = limited(5);
        let now = t0();
        let click = sgr_mouse(0, 10, 20, true);
        // Rapid clicks all pass through
        for _ in 0..10 {
            assert_eq!(sb.filter_at(&click, now), click);
        }
    }

    #[test]
    fn rate_limit_zero_blocks_all_scrolls() {
        let mut sb = limited(0);
        let now = t0();
        let scroll = sgr_mouse(64, 10, 20, true);
        // All scrolls blocked, even the first
        assert!(sb.filter_at(&scroll, now).is_empty());
        assert!(
            sb.filter_at(&scroll, now + Duration::from_secs(100))
                .is_empty()
        );
    }

    #[test]
    fn different_scroll_directions_share_rate_limit() {
        let mut sb = limited(5);
        let now = t0();
        let up = sgr_mouse(64, 10, 20, true);
        let down = sgr_mouse(65, 10, 20, true);
        // Scroll up passes
        assert_eq!(sb.filter_at(&up, now), up);
        // Scroll down immediately after — dropped (shared limit)
        assert!(sb.filter_at(&down, now).is_empty());
    }

    // ── Partial escape sequence / buffer tests ───────────────────────

    #[test]
    fn split_sgr_across_chunks() {
        let mut sb = full();
        let out1 = sb.filter(b"text\x1b[<64;10;");
        assert_eq!(out1, b"text");
        let out2 = sb.filter(b"20M more");
        assert_eq!(out2, b"\x1b[<64;10;20M more");
    }

    #[test]
    fn split_at_esc_byte() {
        let mut sb = full();
        let out1 = sb.filter(b"text\x1b");
        assert_eq!(out1, b"text");
        let out2 = sb.filter(b"[<64;10;20M");
        assert_eq!(out2, b"\x1b[<64;10;20M");
    }

    #[test]
    fn split_after_bracket() {
        let mut sb = full();
        let out1 = sb.filter(b"\x1b[");
        assert!(out1.is_empty());
        let out2 = sb.filter(b"<65;10;20Mrest");
        assert_eq!(out2, b"\x1b[<65;10;20Mrest");
    }

    #[test]
    fn split_legacy_mouse() {
        let mut sb = full();
        // ESC [ M button_byte
        let out1 = sb.filter(&[0x1b, b'[', b'M', 96]);
        assert!(out1.is_empty());
        // x_byte y_byte
        let out2 = sb.filter(&[42, 52]);
        assert_eq!(out2, &[0x1b, b'[', b'M', 96, 42, 52]);
    }

    #[test]
    fn three_way_split() {
        let mut sb = full();
        let out1 = sb.filter(b"\x1b");
        assert!(out1.is_empty());
        let out2 = sb.filter(b"[<64;");
        assert!(out2.is_empty());
        let out3 = sb.filter(b"10;20M");
        assert_eq!(out3, b"\x1b[<64;10;20M");
    }

    #[test]
    fn flush_returns_pending() {
        let mut sb = full();
        let _ = sb.filter(b"text\x1b[<64;10;");
        let flushed = sb.flush();
        assert_eq!(flushed, b"\x1b[<64;10;");
    }

    #[test]
    fn flush_empty_when_no_pending() {
        let mut sb = full();
        assert!(sb.flush().is_empty());
    }

    // ── Edge cases ───────────────────────────────────────────────────

    #[test]
    fn invalid_sgr_params_forwarded() {
        let mut sb = full();
        // Non-digit in button position — not a valid mouse event, forward unchanged
        let input = b"\x1b[<abc;10;20M";
        assert_eq!(sb.filter(input), input.to_vec());
    }

    #[test]
    fn sgr_mouse_release_also_throttled() {
        let mut sb = limited(5);
        let now = t0();
        // Scroll press
        let press = sgr_mouse(64, 10, 20, true);
        assert_eq!(sb.filter_at(&press, now), press);
        // Scroll release (lowercase m) — also throttled
        let release = sgr_mouse(64, 10, 20, false);
        assert!(sb.filter_at(&release, now).is_empty());
    }

    #[test]
    fn empty_csi_m_is_legacy_mouse() {
        let mut sb = full();
        // ESC [ M with no params = legacy mouse, not CSI "delete character"
        let input = legacy_mouse(0, 10, 20);
        assert_eq!(sb.filter(&input), input);
    }

    #[test]
    fn very_large_button_number_passes_through() {
        let mut sb = full();
        // Button 9999 — not a scroll event
        let input = sgr_mouse(9999, 10, 20, true);
        assert_eq!(sb.filter(&input), input);
    }

    #[test]
    fn scroll_event_surrounded_by_text() {
        let mut sb = limited(5);
        let now = t0();
        let scroll = sgr_mouse(64, 10, 20, true);
        let mut input = b"hello".to_vec();
        input.extend_from_slice(&scroll);
        input.extend_from_slice(b"world");

        let out = sb.filter_at(&input, now);

        let mut expected = b"hello".to_vec();
        expected.extend_from_slice(&scroll);
        expected.extend_from_slice(b"world");
        assert_eq!(out, expected);
    }

    #[test]
    fn multiple_scroll_events_in_one_chunk() {
        let mut sb = limited(5);
        let now = t0();
        let s1 = sgr_mouse(64, 10, 20, true);
        let s2 = sgr_mouse(64, 10, 21, true);
        let s3 = sgr_mouse(64, 10, 22, true);
        let mut input = Vec::new();
        input.extend_from_slice(&s1);
        input.extend_from_slice(&s2);
        input.extend_from_slice(&s3);

        let out = sb.filter_at(&input, now);
        // Only the first passes
        assert_eq!(out, s1);
    }

    #[test]
    fn throttled_scroll_does_not_corrupt_state() {
        let mut sb = limited(5);
        let now = t0();

        let mut input = Vec::new();
        input.extend_from_slice(&sgr_mouse(64, 10, 20, true));
        input.extend_from_slice(&sgr_mouse(64, 10, 21, true)); // dropped
        input.extend_from_slice(b"after");

        let out = sb.filter_at(&input, now);
        let mut expected = sgr_mouse(64, 10, 20, true);
        expected.extend_from_slice(b"after");
        assert_eq!(out, expected);
    }

    #[test]
    fn safety_valve_flushes_on_overflow() {
        let mut sb = full();
        let mut big = vec![0x1b, b'[', b'<'];
        big.extend(vec![b'1'; MAX_PENDING + 100]);
        let output = sb.filter(&big);
        assert_eq!(output.len(), big.len());
        assert!(sb.pending.is_empty());
    }

    #[test]
    fn csi_with_params_ending_in_m_is_not_scroll() {
        let mut sb = full();
        // CSI 1m = bold SGR, not a mouse event (params don't start with <)
        assert_eq!(sb.filter(b"\x1b[1m"), b"\x1b[1m");
        // CSI 38;2;255;0;0m = color SGR
        assert_eq!(
            sb.filter(b"\x1b[38;2;255;0;0m"),
            b"\x1b[38;2;255;0;0m"
        );
    }

    #[test]
    fn legacy_scroll_throttled() {
        let mut sb = limited(5);
        let now = t0();
        let scroll = legacy_mouse(64, 10, 20);
        assert_eq!(sb.filter_at(&scroll, now), scroll);
        // Same timestamp — dropped
        assert!(sb.filter_at(&scroll, now).is_empty());
    }

    #[test]
    fn sgr_scroll_with_all_modifiers() {
        let mut sb = full();
        // Shift(4) + Meta(8) + Ctrl(16) + scroll up(64) = 92
        let scroll = sgr_mouse(92, 10, 20, true);
        assert_eq!(sb.filter(&scroll), scroll);
        // Verify it's detected as scroll
        assert!(is_scroll_button(92));
    }

    #[test]
    fn button_128_is_not_scroll() {
        // Extended button 8 (bit 7 set) — not a scroll event
        assert!(!is_scroll_button(128));
        assert!(!is_scroll_button(129));
    }
}
