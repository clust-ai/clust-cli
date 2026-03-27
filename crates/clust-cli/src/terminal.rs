use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crossterm::{
    terminal::{self, disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use tokio::io::AsyncReadExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

use clust_ipc::{CliMessage, PoolMessage};

use crate::output_filter::{EscapeSequenceAssembler, FilterChain};
use crate::scroll_break::{ScrollBreak, ScrollMode};
use crate::scrollback::ScrollbackBuffer;
use crate::theme;

/// Ctrl+Q raw byte (DC1/XON) — used as the detach key.
const CTRL_Q: u8 = 0x11;

/// Number of lines scrolled per mouse wheel click.
const SCROLL_STEP: usize = 3;

/// Commands sent from the input task to the output task for scrollback.
enum ScrollCommand {
    /// Redraw the viewport at the current scroll offset.
    Redraw,
    /// Exit scrollback mode: clear viewport so the agent can redraw.
    ExitScrollback,
}

/// An active terminal session attached to an agent in the pool.
pub struct AttachedSession {
    agent_id: String,
    agent_binary: String,
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
}

impl AttachedSession {
    pub fn new(
        agent_id: String,
        agent_binary: String,
        reader: OwnedReadHalf,
        writer: OwnedWriteHalf,
    ) -> Self {
        Self {
            agent_id,
            agent_binary,
            reader,
            writer,
        }
    }

    /// Run the attached session, taking over the terminal.
    ///
    /// Returns when the user detaches (Ctrl+Q) or the agent exits.
    pub async fn run(self) -> io::Result<()> {
        io::stdout().execute(EnterAlternateScreen)?;
        enable_raw_mode()?;

        // Enable mouse button tracking (1000h) + SGR encoding (1006h) so
        // scroll wheel events arrive as parseable mouse escape sequences
        // instead of being converted to arrow keys in alternate screen mode.
        // We deliberately omit 1003h (all-motion) to avoid flooding stdin.
        enable_mouse_tracking();

        // Install panic hook to restore terminal on crash
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            disable_mouse_tracking();
            let _ = disable_raw_mode();
            let _ = io::stdout().execute(LeaveAlternateScreen);
            prev_hook(info);
        }));

        let result = self.run_inner().await;

        // Restore terminal
        disable_mouse_tracking();
        disable_raw_mode()?;
        io::stdout().execute(LeaveAlternateScreen)?;

        result
    }

    async fn run_inner(self) -> io::Result<()> {
        let (cols, rows) = terminal::size()?;

        // Set scroll region to exclude bottom row (status bar)
        set_scroll_region(rows - 1);
        draw_status_bar(&self.agent_id, &self.agent_binary, rows);

        // Send initial resize so the agent knows the available size
        let mut writer = self.writer;
        clust_ipc::send_message_write(
            &mut writer,
            &CliMessage::ResizeAgent {
                id: self.agent_id.clone(),
                cols,
                rows: rows - 1,
            },
        )
        .await?;

        let agent_id = self.agent_id;
        let agent_binary = self.agent_binary;
        let mut reader = self.reader;

        // Shared state for scrollback
        let scroll_offset = Arc::new(AtomicUsize::new(0)); // 0 = live mode
        let scrollback = Arc::new(Mutex::new(ScrollbackBuffer::new()));
        let (scroll_cmd_tx, mut scroll_cmd_rx) = mpsc::channel::<ScrollCommand>(16);

        // Task 1: Read PoolMessages (output/exit) and render to terminal.
        // Output passes through the filter chain to prevent split escape sequences.
        // All output is stored in the scrollback buffer for scroll-back viewing.
        let scroll_offset_out = Arc::clone(&scroll_offset);
        let scrollback_out = Arc::clone(&scrollback);
        let agent_id_for_bar = agent_id.clone();
        let agent_binary_for_bar = agent_binary.clone();
        let output_task = tokio::spawn(async move {
            let mut filter_chain = FilterChain::new();
            filter_chain.push(Box::new(EscapeSequenceAssembler::new()));

            let end = loop {
                tokio::select! {
                    msg = clust_ipc::recv_message_read::<PoolMessage>(&mut reader) => {
                        match msg {
                            Ok(PoolMessage::AgentOutput { data, .. }) => {
                                let filtered = filter_chain.filter(&data);
                                if !filtered.is_empty() {
                                    // Always store in scrollback buffer
                                    {
                                        scrollback_out.lock().unwrap().push(&filtered);
                                    }

                                    // Only write to stdout if in live mode
                                    if scroll_offset_out.load(Ordering::Relaxed) == 0 {
                                        let mut stdout = io::stdout().lock();
                                        let _ = stdout.write_all(&filtered);
                                        let _ = stdout.flush();
                                    }
                                }
                            }
                            Ok(PoolMessage::AgentExited { exit_code, .. }) => {
                                break SessionEnd::AgentExited(exit_code);
                            }
                            Ok(PoolMessage::PoolShutdown) => {
                                break SessionEnd::PoolShutdown;
                            }
                            Err(_) => {
                                break SessionEnd::ConnectionLost;
                            }
                            _ => {}
                        }
                    }
                    cmd = scroll_cmd_rx.recv() => {
                        match cmd {
                            Some(ScrollCommand::Redraw) => {
                                let offset = scroll_offset_out.load(Ordering::Relaxed);
                                let (_, total_rows) = terminal::size().unwrap_or((80, 24));
                                let viewport_height = total_rows.saturating_sub(1).max(1) as usize;
                                render_scrollback(
                                    &scrollback_out,
                                    offset,
                                    viewport_height,
                                    total_rows,
                                );
                            }
                            Some(ScrollCommand::ExitScrollback) => {
                                let (_, total_rows) = terminal::size().unwrap_or((80, 24));
                                let vp_rows = total_rows.saturating_sub(1).max(1);
                                exit_scrollback_mode(vp_rows);
                                draw_status_bar(
                                    &agent_id_for_bar,
                                    &agent_binary_for_bar,
                                    total_rows,
                                );
                            }
                            None => {
                                break SessionEnd::ConnectionLost;
                            }
                        }
                    }
                }
            };

            // Flush any remaining buffered bytes
            let remaining = filter_chain.flush();
            if !remaining.is_empty() {
                scrollback_out.lock().unwrap().push(&remaining);
                if scroll_offset_out.load(Ordering::Relaxed) == 0 {
                    let mut stdout = io::stdout().lock();
                    let _ = stdout.write_all(&remaining);
                    let _ = stdout.flush();
                }
            }

            end
        });

        // Task 2: Read raw stdin bytes and forward to pool.
        // Mouse scroll events are intercepted for scrollback navigation.
        // All other input passes through to the agent.
        let scroll_offset_in = Arc::clone(&scroll_offset);
        let scrollback_in = Arc::clone(&scrollback);
        let agent_id_for_input = agent_id.clone();
        let agent_binary_for_input = agent_binary.clone();
        let input_task = tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            let mut sigwinch = match tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::window_change(),
            ) {
                Ok(s) => s,
                Err(_) => return SessionEnd::ConnectionLost,
            };

            let mut buf = [0u8; 4096];
            let mut scroll_break = ScrollBreak::new(ScrollMode::Intercept);

            loop {
                tokio::select! {
                    result = stdin.read(&mut buf) => {
                        match result {
                            Ok(0) | Err(_) => return SessionEnd::ConnectionLost,
                            Ok(n) => {
                                let data = &buf[..n];

                                // Ctrl+Q = detach
                                if let Some(pos) = data.iter().position(|&b| b == CTRL_Q) {
                                    // Exit scrollback if active
                                    if scroll_offset_in.load(Ordering::Relaxed) > 0 {
                                        scroll_offset_in.store(0, Ordering::Relaxed);
                                        let _ = scroll_cmd_tx.send(ScrollCommand::ExitScrollback).await;
                                    }
                                    // Forward any bytes before Ctrl+Q
                                    if pos > 0 {
                                        let result = scroll_break.filter_intercept(&data[..pos]);
                                        if !result.bytes.is_empty() {
                                            let _ = clust_ipc::send_message_write(
                                                &mut writer,
                                                &CliMessage::AgentInput {
                                                    id: agent_id_for_input.clone(),
                                                    data: result.bytes,
                                                },
                                            ).await;
                                        }
                                    }
                                    let _ = clust_ipc::send_message_write(
                                        &mut writer,
                                        &CliMessage::DetachAgent {
                                            id: agent_id_for_input.clone(),
                                        },
                                    ).await;
                                    return SessionEnd::Detached;
                                }

                                // Filter input — intercept scroll events
                                let result = scroll_break.filter_intercept(data);

                                // Handle scroll events for scrollback navigation
                                if result.scroll_up > 0 || result.scroll_down > 0 {
                                    let (_, total_rows) = terminal::size().unwrap_or((80, 24));
                                    let viewport_height = total_rows.saturating_sub(1).max(1) as usize;
                                    let max_offset = {
                                        let sb = scrollback_in.lock().unwrap();
                                        sb.total_lines().saturating_sub(viewport_height)
                                    };

                                    let current = scroll_offset_in.load(Ordering::Relaxed);
                                    let mut new_offset = current;
                                    new_offset = new_offset
                                        .saturating_add(result.scroll_up as usize * SCROLL_STEP)
                                        .min(max_offset);
                                    new_offset = new_offset
                                        .saturating_sub(result.scroll_down as usize * SCROLL_STEP);

                                    if new_offset != current {
                                        scroll_offset_in.store(new_offset, Ordering::Relaxed);
                                        if new_offset == 0 {
                                            // Scrolled back to live — exit scrollback
                                            let _ = scroll_cmd_tx.send(ScrollCommand::ExitScrollback).await;
                                            // Trigger agent redraw
                                            let (cols, rows) = terminal::size().unwrap_or((80, 24));
                                            let _ = clust_ipc::send_message_write(
                                                &mut writer,
                                                &CliMessage::ResizeAgent {
                                                    id: agent_id_for_input.clone(),
                                                    cols,
                                                    rows: rows.saturating_sub(1).max(1),
                                                },
                                            ).await;
                                        } else {
                                            let _ = scroll_cmd_tx.send(ScrollCommand::Redraw).await;
                                        }
                                    }
                                }

                                // Handle non-scroll input
                                if !result.bytes.is_empty() {
                                    // Exit scrollback mode on any keypress
                                    if scroll_offset_in.load(Ordering::Relaxed) > 0 {
                                        scroll_offset_in.store(0, Ordering::Relaxed);
                                        let _ = scroll_cmd_tx.send(ScrollCommand::ExitScrollback).await;
                                        // Trigger agent redraw
                                        let (cols, rows) = terminal::size().unwrap_or((80, 24));
                                        let _ = clust_ipc::send_message_write(
                                            &mut writer,
                                            &CliMessage::ResizeAgent {
                                                id: agent_id_for_input.clone(),
                                                cols,
                                                rows: rows.saturating_sub(1).max(1),
                                            },
                                        ).await;
                                    }

                                    let _ = clust_ipc::send_message_write(
                                        &mut writer,
                                        &CliMessage::AgentInput {
                                            id: agent_id_for_input.clone(),
                                            data: result.bytes,
                                        },
                                    ).await;
                                }
                            }
                        }
                    }
                    _ = sigwinch.recv() => {
                        // Exit scrollback on resize
                        if scroll_offset_in.load(Ordering::Relaxed) > 0 {
                            scroll_offset_in.store(0, Ordering::Relaxed);
                            let _ = scroll_cmd_tx.send(ScrollCommand::ExitScrollback).await;
                        }

                        let (cols, rows) = terminal::size().unwrap_or((80, 24));
                        set_scroll_region(rows.saturating_sub(1).max(1));
                        draw_status_bar(
                            &agent_id_for_input,
                            &agent_binary_for_input,
                            rows,
                        );
                        let _ = clust_ipc::send_message_write(
                            &mut writer,
                            &CliMessage::ResizeAgent {
                                id: agent_id_for_input.clone(),
                                cols,
                                rows: rows.saturating_sub(1).max(1),
                            },
                        ).await;
                    }
                }
            }
        });

        // Wait for either task to finish
        tokio::select! {
            _ = output_task => {}
            _ = input_task => {}
        }

        Ok(())
    }
}

#[allow(dead_code)]
enum SessionEnd {
    Detached,
    AgentExited(i32),
    PoolShutdown,
    ConnectionLost,
}

// ── Mouse tracking ──────────────────────────────────────────────────

/// Enable mouse button tracking + SGR encoding so scroll wheel events
/// arrive as mouse escape sequences instead of arrow keys.
fn enable_mouse_tracking() {
    let mut stdout = io::stdout().lock();
    // 1000h = button press/release (includes scroll wheel)
    // 1006h = SGR extended coordinate encoding
    let _ = write!(stdout, "\x1b[?1000h\x1b[?1006h");
    let _ = stdout.flush();
}

/// Disable mouse button tracking + SGR encoding.
fn disable_mouse_tracking() {
    let mut stdout = io::stdout().lock();
    let _ = write!(stdout, "\x1b[?1006l\x1b[?1000l");
    let _ = stdout.flush();
}

// ── Scroll region & status bar ──────────────────────────────────────

/// Set the terminal scroll region to rows 1..bottom_row (1-indexed).
/// Agent output is confined to this region; the status bar lives below it.
fn set_scroll_region(bottom_row: u16) {
    let mut stdout = io::stdout().lock();
    // DECSTBM: set scrolling region
    let _ = write!(stdout, "\x1b[1;{bottom_row}r");
    // Move cursor to top of scroll region
    let _ = write!(stdout, "\x1b[1;1H");
    let _ = stdout.flush();
}

/// Draw the status bar on the bottom row of the terminal.
fn draw_status_bar(agent_id: &str, agent_binary: &str, total_rows: u16) {
    let mut stdout = io::stdout().lock();
    // Save cursor position
    let _ = write!(stdout, "\x1b7");
    // Move to the last row
    let _ = write!(stdout, "\x1b[{total_rows};1H");
    // Background color
    let _ = write!(stdout, "{}", theme::BG_RAISED);
    // Clear the line
    let _ = write!(stdout, "\x1b[2K");
    // Render status bar content
    let _ = write!(
        stdout,
        " {ACCENT}clust{RESET_FG}  {TEXT_PRIMARY}{agent_id}{RESET_FG} \
         {TEXT_TERTIARY}│{RESET_FG} \
         {TEXT_SECONDARY}{agent_binary}{RESET_FG} \
         {TEXT_TERTIARY}│{RESET_FG} \
         {TEXT_TERTIARY}Ctrl+Q detach{RESET_FG}",
        ACCENT = theme::ACCENT,
        TEXT_PRIMARY = theme::TEXT_PRIMARY,
        TEXT_SECONDARY = theme::TEXT_SECONDARY,
        TEXT_TERTIARY = theme::TEXT_TERTIARY,
        RESET_FG = theme::RESET_FG,
    );
    // Reset background
    let _ = write!(stdout, "{}", theme::RESET_BG);
    // Restore cursor position
    let _ = write!(stdout, "\x1b8");
    let _ = stdout.flush();
}

// ── Scrollback rendering ────────────────────────────────────────────

/// Render the scrollback buffer view into the scroll region.
fn render_scrollback(
    scrollback: &Arc<Mutex<ScrollbackBuffer>>,
    offset: usize,
    viewport_height: usize,
    total_rows: u16,
) {
    let sb = scrollback.lock().unwrap();
    let lines = sb.visible_lines(offset, viewport_height);
    let total_lines = sb.total_lines();
    let mut stdout = io::stdout().lock();

    // Save cursor, hide during redraw
    let _ = write!(stdout, "\x1b7\x1b[?25l");

    for (i, line) in lines.iter().enumerate() {
        let row = i + 1; // 1-indexed
        let _ = write!(stdout, "\x1b[{row};1H\x1b[0m\x1b[2K{line}");
    }

    // Clear any remaining rows in the viewport
    for i in lines.len()..viewport_height {
        let row = i + 1;
        let _ = write!(stdout, "\x1b[{row};1H\x1b[2K");
    }

    // Scrollback status bar
    let _ = write!(stdout, "\x1b[{total_rows};1H");
    let _ = write!(stdout, "{}", theme::BG_RAISED);
    let _ = write!(stdout, "\x1b[2K");
    let _ = write!(
        stdout,
        " {ACCENT}SCROLLBACK{RESET_FG}  \
         {TEXT_SECONDARY}{offset} lines up{RESET_FG} \
         {TEXT_TERTIARY}│{RESET_FG} \
         {TEXT_SECONDARY}{total_lines} total{RESET_FG} \
         {TEXT_TERTIARY}│{RESET_FG} \
         {TEXT_TERTIARY}press any key to return{RESET_FG}",
        ACCENT = theme::ACCENT,
        TEXT_SECONDARY = theme::TEXT_SECONDARY,
        TEXT_TERTIARY = theme::TEXT_TERTIARY,
        RESET_FG = theme::RESET_FG,
    );
    let _ = write!(stdout, "{}", theme::RESET_BG);

    // Show cursor, restore position
    let _ = write!(stdout, "\x1b[?25h\x1b8");
    let _ = stdout.flush();
}

/// Clear the viewport when exiting scrollback mode.
fn exit_scrollback_mode(viewport_rows: u16) {
    let mut stdout = io::stdout().lock();
    for row in 1..=viewport_rows {
        let _ = write!(stdout, "\x1b[{row};1H\x1b[2K");
    }
    let _ = write!(stdout, "\x1b[1;1H");
    let _ = stdout.flush();
}

