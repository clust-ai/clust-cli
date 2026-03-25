use std::io::{self, Write};

use crossterm::{
    event::{self, DisableFocusChange, EnableFocusChange, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal::{self, disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use clust_ipc::{CliMessage, PoolMessage};

use crate::theme;

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
        io::stdout().execute(EnableFocusChange)?;

        // Install panic hook to restore terminal on crash
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = io::stdout().execute(DisableFocusChange);
            let _ = disable_raw_mode();
            let _ = io::stdout().execute(LeaveAlternateScreen);
            prev_hook(info);
        }));

        let result = self.run_inner().await;

        // Restore terminal
        let _ = io::stdout().execute(DisableFocusChange);
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

        // Task 1: Read PoolMessages (output/exit) and render to terminal
        let agent_id_for_output = agent_id.clone();
        let agent_binary_for_output = agent_binary.clone();
        let output_task = tokio::spawn(async move {
            loop {
                match clust_ipc::recv_message_read::<PoolMessage>(&mut reader).await {
                    Ok(PoolMessage::AgentOutput { data, .. }) => {
                        let mut stdout = io::stdout().lock();
                        let _ = stdout.write_all(&data);
                        let _ = stdout.flush();
                        // Redraw status bar in case agent output overflowed
                        let (_, rows) = terminal::size().unwrap_or((80, 24));
                        draw_status_bar(
                            &agent_id_for_output,
                            &agent_binary_for_output,
                            rows,
                        );
                    }
                    Ok(PoolMessage::AgentExited { exit_code, .. }) => {
                        return SessionEnd::AgentExited(exit_code);
                    }
                    Ok(PoolMessage::PoolShutdown) => {
                        return SessionEnd::PoolShutdown;
                    }
                    Err(_) => {
                        return SessionEnd::ConnectionLost;
                    }
                    _ => {}
                }
            }
        });

        // Task 2: Read terminal input and forward to pool
        let agent_id_for_input = agent_id.clone();
        let agent_binary_for_input = agent_binary.clone();
        let input_task = tokio::spawn(async move {
            loop {
                let evt = tokio::task::spawn_blocking(|| {
                    if event::poll(std::time::Duration::from_millis(50)).ok()? {
                        event::read().ok()
                    } else {
                        None
                    }
                })
                .await
                .ok()
                .flatten();

                if let Some(evt) = evt {
                    match evt {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            // Let the terminal emulator handle Cmd/Super keys (e.g., Cmd+C for copy)
                            if key.modifiers.contains(KeyModifiers::SUPER) {
                                continue;
                            }
                            // Ctrl+Q = detach
                            if key.code == KeyCode::Char('q')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                let _ = clust_ipc::send_message_write(
                                    &mut writer,
                                    &CliMessage::DetachAgent {
                                        id: agent_id_for_input.clone(),
                                    },
                                )
                                .await;
                                return SessionEnd::Detached;
                            }
                            // Convert key event to raw bytes and send to agent
                            if let Some(bytes) = key_to_bytes(&key) {
                                let _ = clust_ipc::send_message_write(
                                    &mut writer,
                                    &CliMessage::AgentInput {
                                        id: agent_id_for_input.clone(),
                                        data: bytes,
                                    },
                                )
                                .await;
                            }
                        }
                        Event::Paste(text) => {
                            let data = paste_to_bytes(&text);
                            let _ = clust_ipc::send_message_write(
                                &mut writer,
                                &CliMessage::AgentInput {
                                    id: agent_id_for_input.clone(),
                                    data,
                                },
                            )
                            .await;
                        }
                        Event::Resize(cols, rows) => {
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
                            )
                            .await;
                        }
                        Event::FocusGained => {
                            // Re-send our terminal size so the pool resizes the
                            // PTY to match this client (handles multi-terminal
                            // attach with different sizes).
                            let (cols, rows) = terminal::size().unwrap_or((80, 24));
                            let _ = clust_ipc::send_message_write(
                                &mut writer,
                                &CliMessage::ResizeAgent {
                                    id: agent_id_for_input.clone(),
                                    cols,
                                    rows: rows.saturating_sub(1).max(1),
                                },
                            )
                            .await;
                        }
                        _ => {}
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
    // Background color (bgRaised from theme: #292b30)
    let _ = write!(stdout, "\x1b[48;2;41;43;48m");
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
        RESET_FG = "\x1b[39m",
    );
    // Reset background
    let _ = write!(stdout, "\x1b[49m");
    // Restore cursor position
    let _ = write!(stdout, "\x1b8");
    let _ = stdout.flush();
}

/// Convert pasted text into the raw bytes to send to the PTY,
/// wrapped in bracketed paste markers.
fn paste_to_bytes(text: &str) -> Vec<u8> {
    let mut data = Vec::with_capacity(12 + text.len());
    data.extend_from_slice(b"\x1b[200~");
    data.extend_from_slice(text.as_bytes());
    data.extend_from_slice(b"\x1b[201~");
    data
}

/// Convert a crossterm KeyEvent into the raw bytes that a PTY application expects.
fn key_to_bytes(key: &crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                // Ctrl+A = 0x01, Ctrl+B = 0x02, ..., Ctrl+Z = 0x1A
                let byte = (c as u8).to_ascii_lowercase().wrapping_sub(b'a').wrapping_add(1);
                if byte >= 1 && byte <= 26 {
                    Some(vec![byte])
                } else {
                    None
                }
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                Some(s.as_bytes().to_vec())
            }
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::F(n) => f_key_bytes(n),
        _ => None,
    }
}

fn f_key_bytes(n: u8) -> Option<Vec<u8>> {
    let seq = match n {
        1 => "\x1bOP",
        2 => "\x1bOQ",
        3 => "\x1bOR",
        4 => "\x1bOS",
        5 => "\x1b[15~",
        6 => "\x1b[17~",
        7 => "\x1b[18~",
        8 => "\x1b[19~",
        9 => "\x1b[20~",
        10 => "\x1b[21~",
        11 => "\x1b[23~",
        12 => "\x1b[24~",
        _ => return None,
    };
    Some(seq.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventState};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn printable_char() {
        assert_eq!(key_to_bytes(&press(KeyCode::Char('a'))), Some(vec![b'a']));
        assert_eq!(key_to_bytes(&press(KeyCode::Char('Z'))), Some(vec![b'Z']));
        assert_eq!(key_to_bytes(&press(KeyCode::Char('5'))), Some(vec![b'5']));
    }

    #[test]
    fn unicode_char() {
        let bytes = key_to_bytes(&press(KeyCode::Char('é'))).unwrap();
        assert_eq!(bytes, "é".as_bytes());
    }

    #[test]
    fn ctrl_letters() {
        assert_eq!(key_to_bytes(&ctrl('a')), Some(vec![0x01])); // Ctrl+A
        assert_eq!(key_to_bytes(&ctrl('c')), Some(vec![0x03])); // Ctrl+C
        assert_eq!(key_to_bytes(&ctrl('d')), Some(vec![0x04])); // Ctrl+D
        assert_eq!(key_to_bytes(&ctrl('z')), Some(vec![0x1a])); // Ctrl+Z
    }

    #[test]
    fn enter_key() {
        assert_eq!(key_to_bytes(&press(KeyCode::Enter)), Some(vec![b'\r']));
    }

    #[test]
    fn backspace_key() {
        assert_eq!(key_to_bytes(&press(KeyCode::Backspace)), Some(vec![0x7f]));
    }

    #[test]
    fn tab_key() {
        assert_eq!(key_to_bytes(&press(KeyCode::Tab)), Some(vec![b'\t']));
    }

    #[test]
    fn escape_key() {
        assert_eq!(key_to_bytes(&press(KeyCode::Esc)), Some(vec![0x1b]));
    }

    #[test]
    fn arrow_keys() {
        assert_eq!(
            key_to_bytes(&press(KeyCode::Up)),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_to_bytes(&press(KeyCode::Down)),
            Some(b"\x1b[B".to_vec())
        );
        assert_eq!(
            key_to_bytes(&press(KeyCode::Right)),
            Some(b"\x1b[C".to_vec())
        );
        assert_eq!(
            key_to_bytes(&press(KeyCode::Left)),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn home_end_delete() {
        assert_eq!(
            key_to_bytes(&press(KeyCode::Home)),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            key_to_bytes(&press(KeyCode::End)),
            Some(b"\x1b[F".to_vec())
        );
        assert_eq!(
            key_to_bytes(&press(KeyCode::Delete)),
            Some(b"\x1b[3~".to_vec())
        );
    }

    #[test]
    fn page_up_down() {
        assert_eq!(
            key_to_bytes(&press(KeyCode::PageUp)),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            key_to_bytes(&press(KeyCode::PageDown)),
            Some(b"\x1b[6~".to_vec())
        );
    }

    #[test]
    fn f_keys() {
        assert_eq!(
            key_to_bytes(&press(KeyCode::F(1))),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            key_to_bytes(&press(KeyCode::F(12))),
            Some(b"\x1b[24~".to_vec())
        );
    }

    #[test]
    fn unrecognized_key_returns_none() {
        assert_eq!(key_to_bytes(&press(KeyCode::CapsLock)), None);
    }

    fn super_key(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::SUPER,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn super_modifier_not_filtered_by_key_to_bytes() {
        // SUPER filtering happens in the event loop, not in key_to_bytes.
        assert_eq!(key_to_bytes(&super_key('c')), Some(vec![b'c']));
        assert_eq!(key_to_bytes(&super_key('v')), Some(vec![b'v']));
    }

    #[test]
    fn paste_to_bytes_simple() {
        assert_eq!(paste_to_bytes("hello"), b"\x1b[200~hello\x1b[201~");
    }

    #[test]
    fn paste_to_bytes_empty() {
        assert_eq!(paste_to_bytes(""), b"\x1b[200~\x1b[201~");
    }

    #[test]
    fn paste_to_bytes_with_newlines() {
        assert_eq!(
            paste_to_bytes("line1\nline2\n"),
            b"\x1b[200~line1\nline2\n\x1b[201~"
        );
    }

    #[test]
    fn paste_to_bytes_unicode() {
        let result = paste_to_bytes("caf\u{00e9}");
        let mut expected = Vec::new();
        expected.extend_from_slice(b"\x1b[200~");
        expected.extend_from_slice("caf\u{00e9}".as_bytes());
        expected.extend_from_slice(b"\x1b[201~");
        assert_eq!(result, expected);
    }
}
