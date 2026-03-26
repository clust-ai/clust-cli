use std::io::{self, Write};

use crossterm::{
    terminal::{self, disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use tokio::io::AsyncReadExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use clust_ipc::{CliMessage, PoolMessage};

use crate::output_filter::{EscapeSequenceAssembler, FilterChain};
use crate::scroll_break::{ScrollBreak, ScrollMode};
use crate::theme;

/// Ctrl+Q raw byte (DC1/XON) — used as the detach key.
const CTRL_Q: u8 = 0x11;

/// Maximum scroll events forwarded per second in the attached session.
const SCROLL_MAX_PER_SEC: u32 = 5;

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

        // Install panic hook to restore terminal on crash
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = io::stdout().execute(LeaveAlternateScreen);
            prev_hook(info);
        }));

        let result = self.run_inner().await;

        // Restore terminal
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

        // Task 1: Read PoolMessages (output/exit) and render to terminal.
        // Output passes through the filter chain to prevent split escape sequences.
        let output_task = tokio::spawn(async move {
            let mut filter_chain = FilterChain::new();
            filter_chain.push(Box::new(EscapeSequenceAssembler::new()));

            let end = loop {
                match clust_ipc::recv_message_read::<PoolMessage>(&mut reader).await {
                    Ok(PoolMessage::AgentOutput { data, .. }) => {
                        let filtered = filter_chain.filter(&data);
                        if !filtered.is_empty() {
                            let mut stdout = io::stdout().lock();
                            let _ = stdout.write_all(&filtered);
                            let _ = stdout.flush();
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
            };

            // Flush any remaining buffered bytes
            let remaining = filter_chain.flush();
            if !remaining.is_empty() {
                let mut stdout = io::stdout().lock();
                let _ = stdout.write_all(&remaining);
                let _ = stdout.flush();
            }

            end
        });

        // Task 2: Read raw stdin bytes and forward to pool.
        // Input bytes pass through the scroll break filter (rate-limiting mouse
        // scroll events) before forwarding. All other bytes — keyboard input,
        // terminal protocols, alt+key — pass through unchanged.
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
            let mut scroll_break = ScrollBreak::new(ScrollMode::RateLimited {
                max_per_sec: SCROLL_MAX_PER_SEC,
            });

            loop {
                tokio::select! {
                    result = stdin.read(&mut buf) => {
                        match result {
                            Ok(0) | Err(_) => return SessionEnd::ConnectionLost,
                            Ok(n) => {
                                let data = &buf[..n];

                                // Ctrl+Q = detach
                                if let Some(pos) = data.iter().position(|&b| b == CTRL_Q) {
                                    // Forward any bytes before Ctrl+Q
                                    if pos > 0 {
                                        let filtered = scroll_break.filter(&data[..pos]);
                                        if !filtered.is_empty() {
                                            let _ = clust_ipc::send_message_write(
                                                &mut writer,
                                                &CliMessage::AgentInput {
                                                    id: agent_id_for_input.clone(),
                                                    data: filtered,
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

                                // Filter scroll events, then forward
                                let filtered = scroll_break.filter(data);
                                if !filtered.is_empty() {
                                    let _ = clust_ipc::send_message_write(
                                        &mut writer,
                                        &CliMessage::AgentInput {
                                            id: agent_id_for_input.clone(),
                                            data: filtered,
                                        },
                                    ).await;
                                }
                            }
                        }
                    }
                    _ = sigwinch.recv() => {
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

enum SessionEnd {
    Detached,
    AgentExited(i32),
    PoolShutdown,
    ConnectionLost,
}

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
