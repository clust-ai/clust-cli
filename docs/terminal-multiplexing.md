# Terminal Multiplexing

How clust forwards terminal I/O between child processes and the user's terminal.

## Architecture

```
Child process ─► PTY ─► Hub (broadcast) ─► IPC ─► CLI ─► FilterChain ─► stdout
                                                    CLI ◄── raw stdin (bytes)
```

## Output Filter Chain

All PTY output passes through a `FilterChain` before reaching stdout. The chain is defined in `crates/clust-cli/src/output_filter.rs`.

### Rules

- **Never write raw PTY output directly to stdout.** Always go through the filter chain.
- **Re-apply the scroll region and status bar after every output write.** Agent output may contain escape sequences (e.g., `\x1b[r`) that reset the DECSTBM scroll region, causing the status bar to disappear. After writing each chunk, the CLI saves the cursor, re-applies the scroll region, redraws the status bar content, and restores the cursor. This uses the `write_status_bar_content` helper to avoid duplicating rendering logic.
- **New filters implement the `OutputFilter` trait** and are added to the chain via `FilterChain::push()`.

### OutputFilter Trait

```rust
pub trait OutputFilter: Send {
    fn filter(&mut self, data: &[u8]) -> Vec<u8>;
    fn flush(&mut self) -> Vec<u8>;
}
```

- `filter()` processes a chunk of bytes and returns the safe-to-write portion. May buffer partial data internally.
- `flush()` returns any buffered data. Called when the session ends.

### EscapeSequenceAssembler

The primary filter. Prevents ANSI escape sequences from being split across writes to the terminal.

**State machine states:**
- `Ground` — normal text
- `Escape` — after ESC (0x1B)
- `EscapeIntermediate` — ESC + intermediate byte or SS2/SS3, waiting for final byte
- `CsiParam` — CSI parameters (ESC [ ...)
- `CsiIntermediate` — CSI intermediate bytes after parameters
- `OscString` / `OscStringEsc` — OSC string (ESC ] ... BEL/ST)
- `StringCommand` / `StringCommandEsc` — DCS/APC/PM/SOS (ESC P/_ /^/X ... ST)

**Buffer management:** Tracks a `safe_end` position (last byte where state was Ground). At chunk end, outputs bytes up to `safe_end` and buffers the rest. On the next chunk, pending bytes are prepended and re-parsed from Ground state.

**Safety valve:** If the pending buffer exceeds `MAX_PENDING`, the assembler degrades gracefully instead of flushing the whole buffer (which would paint partial-escape garbage to the terminal). It scans backwards for the last `0x1b`, emits everything up to that byte, and re-buffers only the trailing partial sequence. If the buffer contains no `0x1b` at all, the full contents are emitted.

### Adding a New Filter

1. Create a struct implementing `OutputFilter` in `output_filter.rs`
2. Add it to the chain in `terminal.rs` `run_inner()`:
   ```rust
   filter_chain.push(Box::new(MyNewFilter::new()));
   ```
3. Filters run in order — place byte-level assemblers first, semantic filters after.

## Input Forwarding

Input uses **raw stdin byte forwarding** (not crossterm event conversion). This is the same approach used by tmux and screen.

### Rules

- **Forward raw bytes directly.** Do not convert between event representations. Raw forwarding preserves mouse events, terminal-specific protocols (kitty keyboard, sixel), alt+key, and all escape sequences without loss.
- **Intercept Ctrl+Q, F2, PageUp, and mouse scroll in live mode.** Ctrl+Q (byte 0x11) is the detach key. F2 (`\x1bOQ` SS3 variant or `\x1b[12~` CSI variant) toggles mouse tracking on/off. PageUp (`\x1b[5~`) enters scrollback mode one page up. Mouse scroll-up also enters scrollback mode (by `SCROLL_STEP` lines per tick). Non-scroll mouse events (clicks, releases) and all keyboard input other than the above are forwarded to the child PTY unchanged. The `filter_scroll_only()` method on `ScrollBreak` handles this selective interception.
- **Use SIGWINCH for resize detection** (`tokio::signal::unix::SignalKind::window_change()`), not crossterm events.
- **Mouse button tracking is enabled by clust.** The attached session enables `?1000h` (button press/release) and `?1006h` (SGR encoding) so that scroll wheel events arrive as parseable mouse escape sequences instead of being converted to arrow keys by the terminal emulator in alternate screen mode. Non-scroll mouse events (clicks, releases) are forwarded to the agent in live mode; scroll events are intercepted for scrollback navigation. Only button tracking is enabled; `?1003h` (all-motion) is deliberately omitted to avoid flooding stdin with motion events.
- **F2 toggles mouse tracking.** Pressing F2 (detected via multiple variants in the raw byte stream, including `\x1bOQ` SS3 and `\x1b[12~` CSI) toggles an `AtomicBool` shared between input and output tasks. When mouse tracking is disabled, the terminal's mouse tracking escape sequences are turned off (allowing native text selection), mouse scroll interception is bypassed (all bytes are forwarded directly to the agent), and the status bar shows a `MOUSE OFF . F2` indicator. When re-enabled, mouse tracking escape sequences are re-sent and normal scroll interception resumes. The F2 key bytes are consumed and not forwarded to the agent; any surrounding bytes in the same read are forwarded normally.
- **PageUp/PageDown handling preserves surrounding bytes.** When a chunk of stdin contains a PageUp/PageDown sequence alongside other bytes (e.g., from a paste burst), the handler segments the input around the page event and forwards the leading and trailing segments unchanged, only consuming the page-key bytes. This prevents accidental loss of typed input when scrollback is entered mid-burst.

## Scrollback

The attached session maintains scrollback using a **shadow `TerminalEmulator`** — the same `vt100`-backed terminal emulator used by overview mode panels. All output (including hub replay buffer data) is fed through this shadow terminal, which properly captures lines as they scroll off the screen. This approach correctly handles TUI agents that use cursor positioning (`\x1b[row;colH`) instead of newlines, which a line-oriented buffer would fail to render. The shadow terminal has a scrollback capacity of 5,000 lines.

Both PageUp and mouse scroll-up can enter scrollback mode from live mode. In live mode, `ScrollBreak::filter_scroll_only()` intercepts only scroll events while passing all other input through. In scrollback mode, `ScrollBreak::filter_intercept()` strips all mouse events.

`ScrollBreak::should_allow_scroll` returns `false` for `ScrollMode::Intercept` (rather than panicking via `unreachable!()`), so `filter()` may safely be called in intercept mode and will simply drop every scroll byte. The intercept-mode safety valve also drops mouse bytes instead of leaking them, which would otherwise reach the agent and cause stray cursor jumps.

When entering scrollback mode, the current `total_lines` is recorded as an anchor. The scroll offset is bounded by this anchor so the ceiling doesn't rise as new output arrives. New lines that arrive while scrolled back automatically adjust the offset to keep the viewport stable.

- **PageUp** (live mode): enters scrollback mode one page up, if the buffer has enough content. The status bar shows "SCROLLBACK" with the current offset.
- **Mouse scroll up** (live mode): enters scrollback mode by `SCROLL_STEP` lines (finer granularity than PageUp), if the buffer has enough content.
- **PageUp** (scrollback mode): scrolls up by one page.
- **PageDown** (scrollback mode): scrolls down by one page. When offset reaches 0, exits scrollback mode.
- **Mouse scroll up/down** (scrollback mode): navigates by `SCROLL_STEP` lines. Scrolling down to offset 0 exits scrollback mode.
- **Any other keypress** while in scrollback: exits scrollback mode, triggers agent redraw via `ResizeAgent`, and forwards the keypress.
- **Terminal resize** while in scrollback: exits scrollback mode. The shadow terminal is resized via `resize()` which preserves scrollback history.
- **Exiting scrollback** renders the shadow VT's live content (offset 0) to the viewport using the same `to_ansi_lines_scrolled()` pattern, writing each line with cursor positioning and clearing any leftover rows below the content. This avoids clearing the viewport, which would cause a visible blank flash until the agent redraws via SIGWINCH.
- Output arriving while in scrollback mode is stored in the shadow VT and the scroll offset is adjusted, but not rendered to stdout until the user returns to live mode.
- Scrollback rendering uses `to_ansi_lines_scrolled()` which converts the shadow terminal's cell grid to strings with embedded ANSI SGR escape codes for direct stdout output.

## Status Bar

- Drawn on the bottom row, outside the DECSTBM scroll region.
- Shows: `clust` branding, agent ID, agent binary name, repo/branch context (when the agent is running in a git repository), mouse tracking indicator (when mouse tracking is off, displays `MOUSE OFF . F2` in the warning color), and the `Ctrl+Q detach` hint. The repo name is displayed as the basename of the repo path (e.g., `my-project/feature-branch`).
- Redrawn on: initial attach, terminal resize (SIGWINCH), scrollback mode enter/exit, and after every agent output write (to guard against agent escape sequences that reset the scroll region).
- The `draw_status_bar` function handles standalone redraws (saves/restores cursor, flushes). The `write_status_bar_content` helper writes only the bar content to a provided writer, used by both `draw_status_bar` and the output processing loop to avoid duplicating rendering logic.
