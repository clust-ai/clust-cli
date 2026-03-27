# Terminal Multiplexing

How clust forwards terminal I/O between child processes and the user's terminal.

## Architecture

```
Child process ─► PTY ─► Pool (broadcast) ─► IPC ─► CLI ─► FilterChain ─► stdout
                                                    CLI ◄── raw stdin (bytes)
```

## Output Filter Chain

All PTY output passes through a `FilterChain` before reaching stdout. The chain is defined in `crates/clust-cli/src/output_filter.rs`.

### Rules

- **Never write raw PTY output directly to stdout.** Always go through the filter chain.
- **Never inject escape sequences between output chunks.** Status bar redraws, cursor saves, or any other escape sequences must not be interleaved with agent output. The DECSTBM scroll region protects the status bar row; it does not need per-chunk redraws.
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

**Safety valve:** If the pending buffer exceeds 8 KB, flush everything and reset to Ground.

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
- **Intercept Ctrl+Q and mouse scroll events.** Ctrl+Q (byte 0x11) is the detach key. Mouse scroll events are intercepted for scrollback navigation (see below). Everything else passes through to the child PTY.
- **Use SIGWINCH for resize detection** (`tokio::signal::unix::SignalKind::window_change()`), not crossterm events.
- **Mouse button tracking is enabled by clust.** The attached session enables `?1000h` (button press/release) and `?1006h` (SGR encoding) so that scroll wheel events arrive as parseable mouse escape sequences instead of being converted to arrow keys by the terminal emulator in alternate screen mode. This is the one exception to the "don't enable terminal modes on behalf of the child" principle — it's required for scrollback to work. Only button tracking is enabled; `?1003h` (all-motion) is deliberately omitted to avoid flooding stdin with motion events.

## Scrollback

The attached session maintains a scrollback buffer (`scrollback.rs`) that stores agent output as lines. Mouse scroll events are intercepted by `ScrollBreak` in `Intercept` mode and used to navigate the buffer.

- **Scroll up**: enters scrollback mode, renders historical output from the buffer. The status bar shows "SCROLLBACK" with the current offset.
- **Scroll down**: moves toward live output. When offset reaches 0, exits scrollback mode.
- **Any keypress** while in scrollback: exits scrollback mode, triggers agent redraw via `ResizeAgent`, and forwards the keypress.
- **Terminal resize** while in scrollback: exits scrollback mode.
- Output arriving while in scrollback mode is buffered but not rendered until the user returns to live mode.

## Status Bar

- Drawn on the bottom row, outside the DECSTBM scroll region.
- Redrawn only on: initial attach, terminal resize (SIGWINCH), scrollback mode enter/exit.
- Never redrawn inside the output processing loop.
