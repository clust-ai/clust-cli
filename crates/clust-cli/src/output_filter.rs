/// Output filter chain for terminal multiplexing.
///
/// All PTY output must pass through a `FilterChain` before reaching stdout.
/// New filters implement the `OutputFilter` trait and are added to the chain.
/// A filter that processes terminal output bytes.
pub trait OutputFilter: Send {
    /// Process a chunk of output data. Returns bytes safe to write to the terminal.
    fn filter(&mut self, data: &[u8]) -> Vec<u8>;

    /// Flush any buffered data. Called when the session ends.
    fn flush(&mut self) -> Vec<u8>;
}

/// A chain of output filters applied in sequence.
#[derive(Default)]
pub struct FilterChain {
    filters: Vec<Box<dyn OutputFilter>>,
}

impl FilterChain {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, filter: Box<dyn OutputFilter>) {
        self.filters.push(filter);
    }

    pub fn filter(&mut self, data: &[u8]) -> Vec<u8> {
        let mut output = data.to_vec();
        for filter in &mut self.filters {
            output = filter.filter(&output);
        }
        output
    }

    pub fn flush(&mut self) -> Vec<u8> {
        let mut all_output = Vec::new();
        for i in 0..self.filters.len() {
            let flushed = self.filters[i].flush();
            if flushed.is_empty() {
                continue;
            }
            // Pass flushed data through remaining filters
            let mut data = flushed;
            for filter in &mut self.filters[i + 1..] {
                data = filter.filter(&data);
            }
            all_output.extend(data);
        }
        all_output
    }
}

// ── Escape Sequence Assembler ──────────────────────────────────────

/// Maximum pending buffer size before safety-flush (8 KB).
const MAX_PENDING: usize = 8192;

/// Parser state for the ANSI escape sequence state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    /// Normal text — not inside any escape sequence.
    Ground,
    /// Received ESC (0x1B), waiting for the next byte.
    Escape,
    /// ESC followed by an intermediate byte (space, #, (, ), *, +)
    /// or SS2/SS3 (N, O). Waiting for one final byte.
    EscapeIntermediate,
    /// Inside a CSI sequence (ESC [). Reading parameter bytes.
    CsiParam,
    /// CSI intermediate bytes (0x20–0x2F) after parameters.
    CsiIntermediate,
    /// Inside an OSC string (ESC ]). Terminated by BEL or ST.
    OscString,
    /// Inside OSC, received ESC — checking for ST (ESC \).
    OscStringEsc,
    /// Inside a string command: DCS (ESC P), APC (ESC _),
    /// PM (ESC ^), or SOS (ESC X). Terminated by ST (ESC \).
    StringCommand,
    /// Inside string command, received ESC — checking for ST.
    StringCommandEsc,
}

/// Ensures ANSI escape sequences are never split across writes.
///
/// Buffers any partial escape sequence at the end of a chunk and
/// prepends it to the next chunk, guaranteeing that only complete
/// sequences (or ground-state text) are written to the terminal.
#[derive(Default)]
pub struct EscapeSequenceAssembler {
    pending: Vec<u8>,
}

impl EscapeSequenceAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    fn step(state: ParseState, byte: u8) -> ParseState {
        match state {
            ParseState::Ground => {
                if byte == 0x1b {
                    ParseState::Escape
                } else {
                    ParseState::Ground
                }
            }
            ParseState::Escape => match byte {
                b'[' => ParseState::CsiParam,
                b']' => ParseState::OscString,
                b'P' => ParseState::StringCommand, // DCS
                b'_' => ParseState::StringCommand, // APC
                b'^' => ParseState::StringCommand, // PM
                b'X' => ParseState::StringCommand, // SOS
                // Intermediate bytes that need one more final byte
                b' ' | b'#' | b'(' | b')' | b'*' | b'+' => ParseState::EscapeIntermediate,
                // SS2/SS3: ESC N / ESC O followed by one character
                b'N' | b'O' => ParseState::EscapeIntermediate,
                // Final bytes for two-character sequences (ESC + byte)
                0x30..=0x7e => ParseState::Ground,
                // Unknown or invalid — treat as complete
                _ => ParseState::Ground,
            },
            ParseState::EscapeIntermediate => {
                // After ESC + intermediate/SS2/SS3, next byte completes the sequence
                ParseState::Ground
            }
            ParseState::CsiParam => match byte {
                // Parameter bytes: 0–9 ; < = > ?
                0x30..=0x3f => ParseState::CsiParam,
                // Intermediate bytes
                0x20..=0x2f => ParseState::CsiIntermediate,
                // Final byte — sequence complete
                0x40..=0x7e => ParseState::Ground,
                // Invalid — abort sequence
                _ => ParseState::Ground,
            },
            ParseState::CsiIntermediate => match byte {
                // More intermediate bytes
                0x20..=0x2f => ParseState::CsiIntermediate,
                // Final byte — sequence complete
                0x40..=0x7e => ParseState::Ground,
                // Invalid — abort
                _ => ParseState::Ground,
            },
            ParseState::OscString => match byte {
                // BEL terminates OSC
                0x07 => ParseState::Ground,
                // ESC might be start of ST (ESC \)
                0x1b => ParseState::OscStringEsc,
                // Everything else is OSC content
                _ => ParseState::OscString,
            },
            ParseState::OscStringEsc => {
                if byte == b'\\' {
                    // ST = ESC \ — OSC complete
                    ParseState::Ground
                } else {
                    // Not ST — back to OSC content
                    ParseState::OscString
                }
            }
            ParseState::StringCommand => match byte {
                // ESC might be start of ST
                0x1b => ParseState::StringCommandEsc,
                // Everything else is string content
                _ => ParseState::StringCommand,
            },
            ParseState::StringCommandEsc => {
                if byte == b'\\' {
                    // ST = ESC \ — string command complete
                    ParseState::Ground
                } else {
                    // Not ST — back to string content
                    ParseState::StringCommand
                }
            }
        }
    }
}

impl OutputFilter for EscapeSequenceAssembler {
    fn filter(&mut self, data: &[u8]) -> Vec<u8> {
        // Combine pending bytes with new data
        let mut combined = std::mem::take(&mut self.pending);
        combined.extend_from_slice(data);

        // Safety valve: if buffer is too large, flush everything and reset
        if combined.len() > MAX_PENDING {
            return combined;
        }

        // Parse from Ground — pending bytes are always the start of an
        // incomplete sequence and get re-parsed from scratch.
        let mut state = ParseState::Ground;
        let mut safe_end: usize = 0;

        for (i, &byte) in combined.iter().enumerate() {
            state = Self::step(state, byte);
            if state == ParseState::Ground {
                safe_end = i + 1;
            }
        }

        if state == ParseState::Ground {
            // All bytes are safe — no pending sequence
            combined
        } else {
            // Buffer the partial sequence, output everything before it
            self.pending = combined[safe_end..].to_vec();
            combined[..safe_end].to_vec()
        }
    }

    fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── FilterChain tests ──────────────────────────────────────────

    #[test]
    fn empty_chain_passes_through() {
        let mut chain = FilterChain::new();
        assert_eq!(chain.filter(b"hello"), b"hello");
    }

    #[test]
    fn chain_flush_empty() {
        let mut chain = FilterChain::new();
        assert!(chain.flush().is_empty());
    }

    #[test]
    fn chain_with_assembler() {
        let mut chain = FilterChain::new();
        chain.push(Box::new(EscapeSequenceAssembler::new()));

        let out1 = chain.filter(b"text\x1b[38;2;255;");
        assert_eq!(out1, b"text");

        let out2 = chain.filter(b"255;255mworld");
        assert_eq!(out2, b"\x1b[38;2;255;255;255mworld");
    }

    #[test]
    fn chain_flush_with_assembler() {
        let mut chain = FilterChain::new();
        chain.push(Box::new(EscapeSequenceAssembler::new()));

        let _ = chain.filter(b"\x1b[31");
        let flushed = chain.flush();
        assert_eq!(flushed, b"\x1b[31");
    }

    // ── EscapeSequenceAssembler tests ──────────────────────────────

    #[test]
    fn plain_text_passes_through() {
        let mut asm = EscapeSequenceAssembler::new();
        assert_eq!(asm.filter(b"hello world"), b"hello world");
        assert!(asm.pending.is_empty());
    }

    #[test]
    fn complete_sgr_passes_through() {
        let mut asm = EscapeSequenceAssembler::new();
        let seq = b"\x1b[38;2;255;255;255m";
        assert_eq!(asm.filter(seq), seq.to_vec());
        assert!(asm.pending.is_empty());
    }

    #[test]
    fn complete_cursor_position_passes_through() {
        let mut asm = EscapeSequenceAssembler::new();
        let seq = b"\x1b[84;1H";
        assert_eq!(asm.filter(seq), seq.to_vec());
    }

    #[test]
    fn split_sgr_across_two_chunks() {
        let mut asm = EscapeSequenceAssembler::new();

        let out1 = asm.filter(b"hello\x1b[38;2;255;255;25");
        assert_eq!(out1, b"hello");
        assert_eq!(asm.pending, b"\x1b[38;2;255;255;25");

        let out2 = asm.filter(b"5mworld");
        assert_eq!(out2, b"\x1b[38;2;255;255;255mworld");
        assert!(asm.pending.is_empty());
    }

    #[test]
    fn split_at_esc_byte() {
        let mut asm = EscapeSequenceAssembler::new();

        let out1 = asm.filter(b"text\x1b");
        assert_eq!(out1, b"text");
        assert_eq!(asm.pending, b"\x1b");

        let out2 = asm.filter(b"[1;2H");
        assert_eq!(out2, b"\x1b[1;2H");
        assert!(asm.pending.is_empty());
    }

    #[test]
    fn split_after_csi_bracket() {
        let mut asm = EscapeSequenceAssembler::new();

        let out1 = asm.filter(b"abc\x1b[");
        assert_eq!(out1, b"abc");

        let out2 = asm.filter(b"2Kdef");
        assert_eq!(out2, b"\x1b[2Kdef");
    }

    #[test]
    fn multiple_complete_sequences() {
        let mut asm = EscapeSequenceAssembler::new();
        let input = b"\x1b[1m\x1b[31mhello\x1b[0m";
        assert_eq!(asm.filter(input), input.to_vec());
    }

    #[test]
    fn osc_terminated_by_bel() {
        let mut asm = EscapeSequenceAssembler::new();
        let seq = b"\x1b]0;my title\x07rest";
        assert_eq!(asm.filter(seq), seq.to_vec());
    }

    #[test]
    fn osc_terminated_by_st() {
        let mut asm = EscapeSequenceAssembler::new();
        let seq = b"\x1b]0;title\x1b\\rest";
        assert_eq!(asm.filter(seq), seq.to_vec());
    }

    #[test]
    fn osc_split_across_chunks() {
        let mut asm = EscapeSequenceAssembler::new();

        let out1 = asm.filter(b"pre\x1b]0;my ti");
        assert_eq!(out1, b"pre");

        let out2 = asm.filter(b"tle\x07post");
        assert_eq!(out2, b"\x1b]0;my title\x07post");
    }

    #[test]
    fn dcs_string_command() {
        let mut asm = EscapeSequenceAssembler::new();
        let seq = b"\x1bPcontent\x1b\\after";
        assert_eq!(asm.filter(seq), seq.to_vec());
    }

    #[test]
    fn apc_string_command() {
        let mut asm = EscapeSequenceAssembler::new();
        let seq = b"\x1b_Gcontent\x1b\\after";
        assert_eq!(asm.filter(seq), seq.to_vec());
    }

    #[test]
    fn two_char_escape_sequences() {
        let mut asm = EscapeSequenceAssembler::new();
        assert_eq!(asm.filter(b"\x1b7text\x1b8"), b"\x1b7text\x1b8");
    }

    #[test]
    fn escape_intermediate_sequence() {
        let mut asm = EscapeSequenceAssembler::new();
        // ESC # 8 = DEC screen alignment test
        assert_eq!(asm.filter(b"\x1b#8"), b"\x1b#8");
    }

    #[test]
    fn escape_intermediate_split() {
        let mut asm = EscapeSequenceAssembler::new();

        let out1 = asm.filter(b"a\x1b#");
        assert_eq!(out1, b"a");

        let out2 = asm.filter(b"8b");
        assert_eq!(out2, b"\x1b#8b");
    }

    #[test]
    fn ss3_f_key_sequence() {
        let mut asm = EscapeSequenceAssembler::new();
        // ESC O P = F1
        assert_eq!(asm.filter(b"\x1bOP"), b"\x1bOP");
    }

    #[test]
    fn ss3_split_across_chunks() {
        let mut asm = EscapeSequenceAssembler::new();

        let out1 = asm.filter(b"text\x1bO");
        assert_eq!(out1, b"text");

        let out2 = asm.filter(b"Pmore");
        assert_eq!(out2, b"\x1bOPmore");
    }

    #[test]
    fn c0_controls_pass_through() {
        let mut asm = EscapeSequenceAssembler::new();
        assert_eq!(asm.filter(b"\x07\x08\t\n\r"), b"\x07\x08\t\n\r");
    }

    #[test]
    fn flush_returns_pending() {
        let mut asm = EscapeSequenceAssembler::new();
        let _ = asm.filter(b"text\x1b[31");
        assert_eq!(asm.pending, b"\x1b[31");

        let flushed = asm.flush();
        assert_eq!(flushed, b"\x1b[31");
        assert!(asm.pending.is_empty());
    }

    #[test]
    fn safety_valve_flushes_on_overflow() {
        let mut asm = EscapeSequenceAssembler::new();
        let mut big = Vec::new();
        big.extend_from_slice(b"\x1b]0;");
        big.extend(vec![b'X'; MAX_PENDING + 100]);
        let output = asm.filter(&big);
        assert_eq!(output.len(), big.len());
        assert!(asm.pending.is_empty());
    }

    #[test]
    fn the_exact_bug_scenario() {
        // Simulates the exact bug from the screenshots:
        // PTY outputs \x1b[38;2;255;255;255m in two chunks
        let mut asm = EscapeSequenceAssembler::new();

        let out1 = asm.filter(b"some text\x1b[38;2;255;255;");
        assert_eq!(out1, b"some text");

        // Without the assembler, "255m" would appear as visible text
        let out2 = asm.filter(b"255m more text");
        assert_eq!(out2, b"\x1b[38;2;255;255;255m more text");
    }

    #[test]
    fn cursor_position_split() {
        // Another bug from screenshots: "84;1H" visible
        let mut asm = EscapeSequenceAssembler::new();

        let out1 = asm.filter(b"line\x1b[");
        assert_eq!(out1, b"line");

        let out2 = asm.filter(b"84;1Hcontent");
        assert_eq!(out2, b"\x1b[84;1Hcontent");
    }

    #[test]
    fn mixed_text_and_sequences() {
        let mut asm = EscapeSequenceAssembler::new();
        let input = b"hello \x1b[1mbold\x1b[0m normal \x1b[38;2;100;200;50mcolor\x1b[0m";
        assert_eq!(asm.filter(input), input.to_vec());
    }

    #[test]
    fn empty_input() {
        let mut asm = EscapeSequenceAssembler::new();
        assert_eq!(asm.filter(b""), b"".to_vec());
    }

    #[test]
    fn flush_empty() {
        let mut asm = EscapeSequenceAssembler::new();
        assert!(asm.flush().is_empty());
    }

    #[test]
    fn three_way_split() {
        let mut asm = EscapeSequenceAssembler::new();

        let out1 = asm.filter(b"text\x1b");
        assert_eq!(out1, b"text");

        let out2 = asm.filter(b"[48;2;");
        assert!(out2.is_empty());

        let out3 = asm.filter(b"10;10;10mmore");
        assert_eq!(out3, b"\x1b[48;2;10;10;10mmore");
    }

    #[test]
    fn consecutive_partial_sequences() {
        let mut asm = EscapeSequenceAssembler::new();

        // First sequence completes, second is partial
        let out1 = asm.filter(b"\x1b[1m\x1b[38;2;");
        assert_eq!(out1, b"\x1b[1m");

        let out2 = asm.filter(b"200;100;50m");
        assert_eq!(out2, b"\x1b[38;2;200;100;50m");
    }
}
