use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossterm::{
    terminal::{self, disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use tokio::io::AsyncReadExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

use clust_ipc::{CliMessage, HubMessage};

use crate::output_filter::{EscapeSequenceAssembler, FilterChain};
use crate::terminal_emulator::TerminalEmulator;
use crate::scroll_break::{ScrollBreak, ScrollMode};
use crate::theme;
use crate::version;

/// Ctrl+Q raw byte (DC1/XON) — used as the detach key.
const CTRL_Q: u8 = 0x11;

/// Number of lines scrolled per mouse wheel click.
const SCROLL_STEP: usize = 3;

/// PageUp escape sequence.
const PAGE_UP: &[u8] = b"\x1b[5~";

/// PageDown escape sequence.
const PAGE_DOWN: &[u8] = b"\x1b[6~";

/// F2 escape sequences (SS3 variant and CSI variant).
const F2_SS3: &[u8] = b"\x1bOQ";
const F2_CSI: &[u8] = b"\x1b[12~";

/// Scrollback navigation state shared between input and output tasks.
struct ScrollState {
    /// Current scroll offset (0 = live mode, >0 = scrolled back).
    offset: usize,
    /// Total lines in the buffer when scrollback mode was entered.
    /// Used as the anchor so max_offset doesn't grow while scrolled back.
    anchored_total: usize,
}

/// Commands sent from the input task to the output task for scrollback.
enum ScrollCommand {
    /// Redraw the viewport at the current scroll offset.
    Redraw,
    /// Exit scrollback mode: clear viewport so the agent can redraw.
    ExitScrollback,
}

/// An active terminal session attached to an agent in the hub.
pub struct AttachedSession {
    agent_id: String,
    agent_binary: String,
    repo_path: Option<String>,
    branch_name: Option<String>,
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
}

impl AttachedSession {
    pub fn new(
        agent_id: String,
        agent_binary: String,
        repo_path: Option<String>,
        branch_name: Option<String>,
        reader: OwnedReadHalf,
        writer: OwnedWriteHalf,
    ) -> Self {
        Self {
            agent_id,
            agent_binary,
            repo_path,
            branch_name,
            reader,
            writer,
        }
    }

    /// Run the attached session, taking over the terminal.
    ///
    /// Returns when the user detaches (Ctrl+Q) or the agent exits.
    pub async fn run(self) -> io::Result<SessionEnd> {
        // Check for updates in background
        let update_notice: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let notice_clone = update_notice.clone();
        std::thread::spawn(move || {
            if let Some(msg) = version::check_update() {
                *notice_clone.lock().unwrap() = Some(msg);
            }
        });

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
        println!();

        if let Some(ref msg) = *update_notice.lock().unwrap() {
            println!("\n  {}{msg}{}\n", theme::WARNING, theme::RESET);
        }

        result
    }

    async fn run_inner(self) -> io::Result<SessionEnd> {
        let (cols, rows) = terminal::size()?;

        // Set scroll region to exclude bottom row (status bar)
        set_scroll_region(rows - 1);
        draw_status_bar(
            &self.agent_id,
            &self.agent_binary,
            self.repo_path.as_deref(),
            self.branch_name.as_deref(),
            rows,
            true,
        );

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
        let repo_path = self.repo_path;
        let branch_name = self.branch_name;
        let mut reader = self.reader;

        // Shared state for scrollback — a shadow TerminalEmulator processes all
        // output (including replay) so its VTE scrollback captures properly
        // rendered lines instead of raw newline-split text.
        let scroll_state = Arc::new(Mutex::new(ScrollState {
            offset: 0,
            anchored_total: 0,
        }));
        let scrollback = Arc::new(Mutex::new(
            TerminalEmulator::with_scrollback_capacity(
                cols as usize,
                (rows - 1) as usize,
                5000,
            ),
        ));
        let (scroll_cmd_tx, mut scroll_cmd_rx) = mpsc::channel::<ScrollCommand>(16);
        let mouse_tracking = Arc::new(AtomicBool::new(true));

        // Task 1: Read HubMessages (output/exit) and render to terminal.
        // Output passes through the filter chain to prevent split escape sequences.
        // All output is stored in the scrollback buffer for scroll-back viewing.
        let scroll_state_out = Arc::clone(&scroll_state);
        let scrollback_out = Arc::clone(&scrollback);
        let mouse_tracking_out = Arc::clone(&mouse_tracking);
        let agent_id_for_bar = agent_id.clone();
        let agent_binary_for_bar = agent_binary.clone();
        let repo_path_for_bar = repo_path.clone();
        let branch_name_for_bar = branch_name.clone();
        let output_task = tokio::spawn(async move {
            let mut filter_chain = FilterChain::new();
            filter_chain.push(Box::new(EscapeSequenceAssembler::new()));

            // During replay, output is stored in scrollback but not written to
            // stdout. This prevents a flash of historical content before the
            // agent's SIGWINCH redraw provides a clean screen.
            let mut replaying = true;

            let end = loop {
                tokio::select! {
                    msg = clust_ipc::recv_message_read::<HubMessage>(&mut reader) => {
                        match msg {
                            Ok(HubMessage::AgentOutput { data, .. }) => {
                                let filtered = filter_chain.filter(&data);
                                if !filtered.is_empty() {
                                    // Always feed through the shadow VT
                                    {
                                        let mut vt = scrollback_out.lock().unwrap();
                                        let sb_before = vt.scrollback_len();
                                        vt.process(&filtered);
                                        let new_lines = vt.scrollback_len().saturating_sub(sb_before);
                                        drop(vt);

                                        let mut state = scroll_state_out.lock().unwrap();
                                        if state.offset > 0 && new_lines > 0 {
                                            // Adjust offset to keep viewport stable
                                            state.offset += new_lines;
                                            state.anchored_total += new_lines;
                                            // Cap at max scrollback
                                            let max = scrollback_out.lock().unwrap().scrollback_len();  // lock is mut via MutexGuard DerefMut
                                            state.offset = state.offset.min(max);
                                        }
                                    }

                                    // Only write to stdout after replay is complete
                                    let should_write = !replaying
                                        && scroll_state_out.lock().unwrap().offset == 0;

                                    if should_write {
                                        let (_, total_rows) = terminal::size().unwrap_or((80, 24));
                                        let vp = total_rows.saturating_sub(1).max(1);
                                        let mut stdout = io::stdout().lock();
                                        let _ = stdout.write_all(&filtered);
                                        // Re-apply scroll region and status bar in case
                                        // agent output contained sequences that reset them.
                                        let _ = write!(stdout, "\x1b7");
                                        let _ = write!(stdout, "\x1b[1;{vp}r");
                                        write_status_bar_content(
                                            &mut stdout,
                                            &agent_id_for_bar,
                                            &agent_binary_for_bar,
                                            repo_path_for_bar.as_deref(),
                                            branch_name_for_bar.as_deref(),
                                            total_rows,
                                            mouse_tracking_out.load(Ordering::Relaxed),
                                        );
                                        let _ = write!(stdout, "\x1b8");
                                        let _ = stdout.flush();
                                    }
                                }
                            }
                            Ok(HubMessage::AgentReplayComplete { .. }) => {
                                replaying = false;
                            }
                            Ok(HubMessage::AgentExited { exit_code, .. }) => {
                                break SessionEnd::AgentExited(exit_code);
                            }
                            Ok(HubMessage::HubShutdown) => {
                                break SessionEnd::HubShutdown;
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
                                let offset = scroll_state_out.lock().unwrap().offset;
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
                                    repo_path_for_bar.as_deref(),
                                    branch_name_for_bar.as_deref(),
                                    total_rows,
                                    mouse_tracking_out.load(Ordering::Relaxed),
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
                scrollback_out.lock().unwrap().process(&remaining);
                if scroll_state_out.lock().unwrap().offset == 0 {
                    let mut stdout = io::stdout().lock();
                    let _ = stdout.write_all(&remaining);
                    let _ = stdout.flush();
                }
            }

            end
        });

        // Task 2: Read raw stdin bytes and forward to hub.
        // In live mode, mouse scroll-up enters scrollback; other input forwarded to agent.
        // PageUp also enters scrollback; in scrollback mode, mouse scroll and
        // PageUp/PageDown navigate the buffer. Any other keypress exits scrollback.
        let scroll_state_in = Arc::clone(&scroll_state);
        let scrollback_in = Arc::clone(&scrollback);
        let mouse_tracking_in = Arc::clone(&mouse_tracking);
        let agent_id_for_input = agent_id.clone();
        let agent_binary_for_input = agent_binary.clone();
        let repo_path_for_input = repo_path.clone();
        let branch_name_for_input = branch_name.clone();
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
                                    let was_scrolled = {
                                        let mut state = scroll_state_in.lock().unwrap();
                                        let was = state.offset > 0;
                                        state.offset = 0;
                                        was
                                    };
                                    if was_scrolled {
                                        let _ = scroll_cmd_tx.send(ScrollCommand::ExitScrollback).await;
                                    }
                                    // Forward any bytes before Ctrl+Q
                                    if pos > 0 {
                                        let _ = clust_ipc::send_message_write(
                                            &mut writer,
                                            &CliMessage::AgentInput {
                                                id: agent_id_for_input.clone(),
                                                data: data[..pos].to_vec(),
                                            },
                                        ).await;
                                    }
                                    let _ = clust_ipc::send_message_write(
                                        &mut writer,
                                        &CliMessage::DetachAgent {
                                            id: agent_id_for_input.clone(),
                                        },
                                    ).await;
                                    return SessionEnd::Detached;
                                }

                                // F2 = toggle mouse tracking
                                let f2_pos = find_sequence(data, F2_SS3)
                                    .or_else(|| find_sequence(data, F2_CSI));
                                if let Some(pos) = f2_pos {
                                    let was_enabled = mouse_tracking_in.fetch_xor(true, Ordering::Relaxed);
                                    if was_enabled {
                                        disable_mouse_tracking();
                                    } else {
                                        enable_mouse_tracking();
                                    }
                                    let (_, total_rows) = terminal::size().unwrap_or((80, 24));
                                    draw_status_bar(
                                        &agent_id_for_input,
                                        &agent_binary_for_input,
                                        repo_path_for_input.as_deref(),
                                        branch_name_for_input.as_deref(),
                                        total_rows,
                                        !was_enabled,
                                    );
                                    // Forward bytes before/after F2 to agent
                                    let seq_len = if find_sequence(data, F2_SS3) == Some(pos) {
                                        F2_SS3.len()
                                    } else {
                                        F2_CSI.len()
                                    };
                                    let mut remaining = Vec::new();
                                    if pos > 0 {
                                        remaining.extend_from_slice(&data[..pos]);
                                    }
                                    let after = pos + seq_len;
                                    if after < data.len() {
                                        remaining.extend_from_slice(&data[after..]);
                                    }
                                    if !remaining.is_empty() {
                                        let _ = clust_ipc::send_message_write(
                                            &mut writer,
                                            &CliMessage::AgentInput {
                                                id: agent_id_for_input.clone(),
                                                data: remaining,
                                            },
                                        ).await;
                                    }
                                    continue;
                                }

                                let in_scrollback = scroll_state_in.lock().unwrap().offset > 0;

                                if !in_scrollback {
                                    // ── Live mode ─────────────────────────────
                                    // Check for PageUp to enter scrollback
                                    if let Some(pos) = find_sequence(data, PAGE_UP) {
                                        let (_, total_rows) = terminal::size().unwrap_or((80, 24));
                                        let viewport_height = total_rows.saturating_sub(1).max(1) as usize;
                                        let (sb_len, total_lines) = {
                                            let mut vt = scrollback_in.lock().unwrap();
                                            let sb = vt.scrollback_len();
                                            (sb, sb + vt.rows())
                                        };

                                        if sb_len > 0 {
                                            // Enter scrollback mode
                                            {
                                                let mut state = scroll_state_in.lock().unwrap();
                                                state.offset = viewport_height;
                                                state.anchored_total = total_lines;
                                            }

                                            // Forward bytes before PageUp
                                            if pos > 0 {
                                                let _ = clust_ipc::send_message_write(
                                                    &mut writer,
                                                    &CliMessage::AgentInput {
                                                        id: agent_id_for_input.clone(),
                                                        data: data[..pos].to_vec(),
                                                    },
                                                ).await;
                                            }
                                            // Forward bytes after PageUp
                                            let after = pos + PAGE_UP.len();
                                            if after < data.len() {
                                                let _ = clust_ipc::send_message_write(
                                                    &mut writer,
                                                    &CliMessage::AgentInput {
                                                        id: agent_id_for_input.clone(),
                                                        data: data[after..].to_vec(),
                                                    },
                                                ).await;
                                            }

                                            let _ = scroll_cmd_tx.send(ScrollCommand::Redraw).await;
                                        } else {
                                            // Not enough content — forward PageUp to agent
                                            let _ = clust_ipc::send_message_write(
                                                &mut writer,
                                                &CliMessage::AgentInput {
                                                    id: agent_id_for_input.clone(),
                                                    data: data.to_vec(),
                                                },
                                            ).await;
                                        }
                                    } else if !mouse_tracking_in.load(Ordering::Relaxed) {
                                        // Mouse tracking off — forward all bytes
                                        // directly (no scroll interception).
                                        let _ = clust_ipc::send_message_write(
                                            &mut writer,
                                            &CliMessage::AgentInput {
                                                id: agent_id_for_input.clone(),
                                                data: data.to_vec(),
                                            },
                                        ).await;
                                    } else {
                                        // Intercept scroll-up to enter scrollback;
                                        // non-scroll bytes forwarded to agent.
                                        let result = scroll_break.filter_scroll_only(data);

                                        if result.scroll_up > 0 {
                                            let (sb_len, total_lines) = {
                                                let mut vt = scrollback_in.lock().unwrap();
                                                let sb = vt.scrollback_len();
                                                (sb, sb + vt.rows())
                                            };

                                            if sb_len > 0 {
                                                {
                                                    let mut state = scroll_state_in.lock().unwrap();
                                                    state.offset = (result.scroll_up as usize * SCROLL_STEP).min(sb_len);
                                                    state.anchored_total = total_lines;
                                                }
                                                let _ = scroll_cmd_tx.send(ScrollCommand::Redraw).await;
                                            }
                                        }

                                        // Forward non-scroll bytes to agent
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
                                } else {
                                    // ── Scrollback mode ──────────────────────
                                    let (_, total_rows) = terminal::size().unwrap_or((80, 24));
                                    let viewport_height = total_rows.saturating_sub(1).max(1) as usize;

                                    // PageUp: scroll up by a page
                                    if find_sequence(data, PAGE_UP).is_some() {
                                        {
                                            let mut state = scroll_state_in.lock().unwrap();
                                            let max = {
                                                let mut vt = scrollback_in.lock().unwrap();
                                                vt.scrollback_len()
                                            };
                                            state.offset = state.offset.saturating_add(viewport_height).min(max);
                                        }
                                        let _ = scroll_cmd_tx.send(ScrollCommand::Redraw).await;
                                        continue;
                                    }

                                    // PageDown: scroll down by a page
                                    if find_sequence(data, PAGE_DOWN).is_some() {
                                        let reached_live = {
                                            let mut state = scroll_state_in.lock().unwrap();
                                            state.offset = state.offset.saturating_sub(viewport_height);
                                            state.offset == 0
                                        };
                                        if reached_live {
                                            let _ = scroll_cmd_tx.send(ScrollCommand::ExitScrollback).await;
                                            trigger_agent_redraw(
                                                &mut writer,
                                                &agent_id_for_input,
                                            ).await;
                                        } else {
                                            let _ = scroll_cmd_tx.send(ScrollCommand::Redraw).await;
                                        }
                                        continue;
                                    }

                                    // Filter for mouse scroll events
                                    let result = scroll_break.filter_intercept(data);

                                    if result.scroll_up > 0 || result.scroll_down > 0 {
                                        let (changed, reached_live) = {
                                            let mut state = scroll_state_in.lock().unwrap();
                                            let max = {
                                                let mut vt = scrollback_in.lock().unwrap();
                                                vt.scrollback_len()
                                            };
                                            let prev = state.offset;
                                            state.offset = state.offset
                                                .saturating_add(result.scroll_up as usize * SCROLL_STEP)
                                                .min(max);
                                            state.offset = state.offset
                                                .saturating_sub(result.scroll_down as usize * SCROLL_STEP);
                                            (state.offset != prev, state.offset == 0)
                                        };
                                        if changed {
                                            if reached_live {
                                                let _ = scroll_cmd_tx.send(ScrollCommand::ExitScrollback).await;
                                                trigger_agent_redraw(
                                                    &mut writer,
                                                    &agent_id_for_input,
                                                ).await;
                                            } else {
                                                let _ = scroll_cmd_tx.send(ScrollCommand::Redraw).await;
                                            }
                                        }
                                    }

                                    // Any non-scroll keypress exits scrollback
                                    if !result.bytes.is_empty() {
                                        {
                                            scroll_state_in.lock().unwrap().offset = 0;
                                        }
                                        let _ = scroll_cmd_tx.send(ScrollCommand::ExitScrollback).await;
                                        trigger_agent_redraw(
                                            &mut writer,
                                            &agent_id_for_input,
                                        ).await;
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
                    }
                    _ = sigwinch.recv() => {
                        // Exit scrollback on resize
                        let was_scrolled = {
                            let mut state = scroll_state_in.lock().unwrap();
                            let was = state.offset > 0;
                            state.offset = 0;
                            was
                        };
                        if was_scrolled {
                            let _ = scroll_cmd_tx.send(ScrollCommand::ExitScrollback).await;
                        }

                        let (cols, rows) = terminal::size().unwrap_or((80, 24));
                        // Resize shadow VT to match new dimensions (preserves scrollback)
                        scrollback_in.lock().unwrap().resize(
                            cols as usize,
                            rows.saturating_sub(1).max(1) as usize,
                        );
                        set_scroll_region(rows.saturating_sub(1).max(1));
                        draw_status_bar(
                            &agent_id_for_input,
                            &agent_binary_for_input,
                            repo_path_for_input.as_deref(),
                            branch_name_for_input.as_deref(),
                            rows,
                            mouse_tracking_in.load(Ordering::Relaxed),
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
        let end = tokio::select! {
            result = output_task => {
                result.unwrap_or(SessionEnd::ConnectionLost)
            }
            result = input_task => {
                result.unwrap_or(SessionEnd::ConnectionLost)
            }
        };

        Ok(end)
    }
}

#[allow(dead_code)]
pub enum SessionEnd {
    Detached,
    AgentExited(i32),
    HubShutdown,
    ConnectionLost,
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Find a byte sequence in a data buffer.
fn find_sequence(data: &[u8], seq: &[u8]) -> Option<usize> {
    data.windows(seq.len()).position(|w| w == seq)
}

/// Send a ResizeAgent message to trigger an agent redraw after exiting scrollback.
async fn trigger_agent_redraw(writer: &mut OwnedWriteHalf, agent_id: &str) {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let _ = clust_ipc::send_message_write(
        writer,
        &CliMessage::ResizeAgent {
            id: agent_id.to_string(),
            cols,
            rows: rows.saturating_sub(1).max(1),
        },
    )
    .await;
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

/// Write status bar content at the given row. Does not save/restore cursor.
fn write_status_bar_content(
    w: &mut impl Write,
    agent_id: &str,
    agent_binary: &str,
    repo_path: Option<&str>,
    branch_name: Option<&str>,
    total_rows: u16,
    mouse_tracking: bool,
) {
    let _ = write!(w, "\x1b[{total_rows};1H");
    let _ = write!(w, "{}", theme::BG_RAISED);
    let _ = write!(w, "\x1b[2K");
    let _ = write!(
        w,
        " {ACCENT}clust{RESET_FG}  {TEXT_PRIMARY}{agent_id}{RESET_FG} \
         {TEXT_TERTIARY}│{RESET_FG} \
         {TEXT_SECONDARY}{agent_binary}{RESET_FG}",
        ACCENT = theme::ACCENT,
        TEXT_PRIMARY = theme::TEXT_PRIMARY,
        TEXT_SECONDARY = theme::TEXT_SECONDARY,
        TEXT_TERTIARY = theme::TEXT_TERTIARY,
        RESET_FG = theme::RESET_FG,
    );
    if let Some(rp) = repo_path {
        let repo_display = crate::format::format_repo_display(rp);
        let _ = write!(
            w,
            " {TEXT_TERTIARY}│{RESET_FG} {TEXT_SECONDARY}{repo_display}",
            TEXT_TERTIARY = theme::TEXT_TERTIARY,
            TEXT_SECONDARY = theme::TEXT_SECONDARY,
            RESET_FG = theme::RESET_FG,
        );
        if let Some(branch) = branch_name {
            let _ = write!(
                w,
                "{TEXT_TERTIARY}/{TEXT_SECONDARY}{branch}",
                TEXT_TERTIARY = theme::TEXT_TERTIARY,
                TEXT_SECONDARY = theme::TEXT_SECONDARY,
            );
        }
        let _ = write!(w, "{RESET_FG}", RESET_FG = theme::RESET_FG);
    }
    if !mouse_tracking {
        let _ = write!(
            w,
            " {TEXT_TERTIARY}│{RESET_FG} {WARNING}\x1b[1mMOUSE OFF \u{00b7} F2\x1b[22m{RESET_FG}",
            TEXT_TERTIARY = theme::TEXT_TERTIARY,
            WARNING = theme::WARNING,
            RESET_FG = theme::RESET_FG,
        );
    }
    let _ = write!(
        w,
        " {TEXT_TERTIARY}│{RESET_FG} {TEXT_TERTIARY}Ctrl+Q detach{RESET_FG}",
        TEXT_TERTIARY = theme::TEXT_TERTIARY,
        RESET_FG = theme::RESET_FG,
    );
    let _ = write!(w, "{}", theme::RESET_BG);
}

/// Draw the status bar on the bottom row of the terminal.
fn draw_status_bar(
    agent_id: &str,
    agent_binary: &str,
    repo_path: Option<&str>,
    branch_name: Option<&str>,
    total_rows: u16,
    mouse_tracking: bool,
) {
    let mut stdout = io::stdout().lock();
    let _ = write!(stdout, "\x1b7");
    write_status_bar_content(&mut stdout, agent_id, agent_binary, repo_path, branch_name, total_rows, mouse_tracking);
    let _ = write!(stdout, "\x1b8");
    let _ = stdout.flush();
}

// ── Scrollback rendering ────────────────────────────────────────────

/// Render the scrollback buffer view into the scroll region.
fn render_scrollback(
    scrollback: &Arc<Mutex<TerminalEmulator>>,
    offset: usize,
    viewport_height: usize,
    total_rows: u16,
) {
    let mut vt = scrollback.lock().unwrap();
    let lines = vt.to_ansi_lines_scrolled(offset);
    let total_lines = vt.scrollback_len() + vt.rows();
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
         {TEXT_TERTIARY}PgUp/PgDn scroll, any key to exit{RESET_FG}",
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
