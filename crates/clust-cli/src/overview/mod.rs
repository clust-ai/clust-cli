pub mod input;
pub mod screen;

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use clust_ipc::{AgentInfo, CliMessage, PoolMessage};

use crate::{ipc, theme};

use self::screen::VirtualTerminal;

/// Minimum width in columns for a single agent panel.
const MIN_PANEL_WIDTH: u16 = 40;

/// Calculate the total panel width (including borders) for a given available width.
/// Targets 2.5 panels across the screen so the user sees 2 full + half of a third.
fn panel_total_width(available_width: u16) -> u16 {
    (available_width * 2 / 5).max(MIN_PANEL_WIDTH)
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Focus state within the overview tab.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OverviewFocus {
    OptionsBar,
    Terminal(usize), // index into panels
}

/// Commands sent from the UI thread to a background connection task.
pub enum PanelCommand {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Detach,
}

/// Events received from background connection tasks.
pub enum AgentOutputEvent {
    Output { id: String, data: Vec<u8> },
    Exited { id: String, _exit_code: i32 },
    ConnectionLost { id: String },
}

/// A single agent panel in the overview.
pub struct AgentPanel {
    pub id: String,
    pub agent_binary: String,
    pub vterm: VirtualTerminal,
    pub command_tx: mpsc::Sender<PanelCommand>,
    pub exited: bool,
    task_handle: JoinHandle<()>,
}

/// Top-level overview state.
pub struct OverviewState {
    pub panels: Vec<AgentPanel>,
    pub focus: OverviewFocus,
    /// Index of the last focused terminal (for Shift+Down to return to).
    pub last_terminal_idx: usize,
    pub scroll_offset: usize,
    output_rx: mpsc::Receiver<AgentOutputEvent>,
    output_tx: mpsc::Sender<AgentOutputEvent>,
    panel_cols: u16,
    panel_rows: u16,
    pub initialized: bool,
}

impl OverviewState {
    pub fn new() -> Self {
        let (output_tx, output_rx) = mpsc::channel(512);
        Self {
            panels: Vec::new(),
            focus: OverviewFocus::OptionsBar,
            last_terminal_idx: 0,
            scroll_offset: 0,
            output_rx,
            output_tx,
            panel_cols: 80,
            panel_rows: 24,
            initialized: false,
        }
    }

    /// Synchronize panels with the current agent list.
    pub fn sync_agents(&mut self, agents: &[AgentInfo], content_area: Rect) {
        // Calculate panel dimensions based on agent count and available space
        self.recalculate_panel_size(agents.len(), content_area);

        // Remove panels for agents no longer running
        let mut i = 0;
        while i < self.panels.len() {
            if !agents.iter().any(|a| a.id == self.panels[i].id) {
                let panel = self.panels.remove(i);
                panel.task_handle.abort();
            } else {
                i += 1;
            }
        }

        // Add panels for new agents
        for agent in agents {
            if !self.panels.iter().any(|p| p.id == agent.id) {
                self.spawn_agent_connection(agent);
            }
        }

        // Resize existing panels if dimensions changed
        for panel in &mut self.panels {
            let (pw, ph) = (self.panel_cols as usize, self.panel_rows as usize);
            if panel.vterm.screen.cols != pw || panel.vterm.screen.rows != ph {
                panel.vterm.resize(pw, ph);
                let _ = panel
                    .command_tx
                    .try_send(PanelCommand::Resize {
                        cols: self.panel_cols,
                        rows: self.panel_rows,
                    });
            }
        }

        self.clamp_focus();
        self.initialized = true;
    }

    /// Drain all pending output events from background tasks.
    pub fn drain_output_events(&mut self) {
        while let Ok(event) = self.output_rx.try_recv() {
            match event {
                AgentOutputEvent::Output { id, data } => {
                    if let Some(panel) = self.panels.iter_mut().find(|p| p.id == id) {
                        panel.vterm.process(&data);
                    }
                }
                AgentOutputEvent::Exited { id, .. }
                | AgentOutputEvent::ConnectionLost { id } => {
                    if let Some(panel) = self.panels.iter_mut().find(|p| p.id == id) {
                        panel.exited = true;
                    }
                }
            }
        }
    }

    /// Handle terminal resize.
    pub fn handle_resize(&mut self, agent_count: usize, content_area: Rect) {
        self.recalculate_panel_size(agent_count, content_area);
        for panel in &mut self.panels {
            panel
                .vterm
                .resize(self.panel_cols as usize, self.panel_rows as usize);
            let _ = panel
                .command_tx
                .try_send(PanelCommand::Resize {
                    cols: self.panel_cols,
                    rows: self.panel_rows,
                });
        }
    }

    /// Send detach to all connections and abort tasks.
    pub fn shutdown(&mut self) {
        for panel in self.panels.drain(..) {
            let _ = panel.command_tx.try_send(PanelCommand::Detach);
            panel.task_handle.abort();
        }
    }

    /// Number of panels that fit in the given width.
    pub fn visible_panel_count(&self, width: u16) -> usize {
        if self.panels.is_empty() {
            return 0;
        }
        let pw = panel_total_width(width);
        // Ceiling division so the partially-visible panel is included.
        let max_fit = ((width + pw - 1) / pw).max(1) as usize;
        max_fit.min(self.panels.len() - self.scroll_offset)
    }

    /// Move focus to the previous agent terminal.
    pub fn focus_prev(&mut self) {
        if let OverviewFocus::Terminal(idx) = self.focus {
            if idx > 0 {
                self.focus = OverviewFocus::Terminal(idx - 1);
                self.last_terminal_idx = idx - 1;
                self.ensure_visible(idx - 1);
            }
        }
    }

    /// Move focus to the next agent terminal.
    pub fn focus_next(&mut self) {
        if let OverviewFocus::Terminal(idx) = self.focus {
            if idx + 1 < self.panels.len() {
                self.focus = OverviewFocus::Terminal(idx + 1);
                self.last_terminal_idx = idx + 1;
                self.ensure_visible(idx + 1);
            }
        }
    }

    /// Enter terminal focus from options bar.
    pub fn enter_terminal(&mut self) {
        if !self.panels.is_empty() {
            let idx = self.last_terminal_idx.min(self.panels.len() - 1);
            self.focus = OverviewFocus::Terminal(idx);
            self.ensure_visible(idx);
        }
    }

    /// Return to options bar from terminal.
    pub fn exit_terminal(&mut self) {
        if let OverviewFocus::Terminal(idx) = self.focus {
            self.last_terminal_idx = idx;
        }
        self.focus = OverviewFocus::OptionsBar;
    }

    /// Send input bytes to the focused agent.
    pub fn send_input(&self, data: Vec<u8>) {
        if let OverviewFocus::Terminal(idx) = self.focus {
            if let Some(panel) = self.panels.get(idx) {
                let _ = panel.command_tx.try_send(PanelCommand::Input(data));
            }
        }
    }

    /// Scroll viewport left.
    pub fn scroll_left(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset -= 1;
        }
    }

    /// Scroll viewport right.
    pub fn scroll_right(&mut self, visible_width: u16) {
        let visible = self.visible_panel_count(visible_width);
        if self.scroll_offset + visible < self.panels.len() {
            self.scroll_offset += 1;
        }
    }

    // -- Private helpers --

    fn ensure_visible(&mut self, idx: usize) {
        if idx < self.scroll_offset {
            self.scroll_offset = idx;
        }
        // We don't know the width here, so we can't fully clamp the right side.
        // The render function will handle clamping.
    }

    fn clamp_focus(&mut self) {
        if self.panels.is_empty() {
            self.focus = OverviewFocus::OptionsBar;
            return;
        }
        if let OverviewFocus::Terminal(idx) = self.focus {
            if idx >= self.panels.len() {
                self.focus =
                    OverviewFocus::Terminal(self.panels.len().saturating_sub(1));
            }
        }
        self.last_terminal_idx = self
            .last_terminal_idx
            .min(self.panels.len().saturating_sub(1));
        self.scroll_offset = self
            .scroll_offset
            .min(self.panels.len().saturating_sub(1));
    }

    fn recalculate_panel_size(&mut self, agent_count: usize, content_area: Rect) {
        if agent_count == 0 {
            return;
        }
        // Content area already excludes the tab bar and status bar.
        // We subtract 1 row for the options bar.
        let available_height = content_area.height.saturating_sub(1);
        // Each panel has: top border (1) + header (1) + terminal content + bottom border (1)
        self.panel_rows = available_height.saturating_sub(3).max(1);

        // Panel width targets 2.5 panels across the screen.
        // VTE terminal gets the inner width: total minus 2 border columns.
        let pw = panel_total_width(content_area.width);
        self.panel_cols = pw.saturating_sub(2).max(1);
    }

    fn spawn_agent_connection(&mut self, agent: &AgentInfo) {
        let id = agent.id.clone();
        let binary = agent.agent_binary.clone();
        let cols = self.panel_cols;
        let rows = self.panel_rows;
        let event_tx = self.output_tx.clone();
        let (command_tx, command_rx) = mpsc::channel::<PanelCommand>(64);

        let agent_id = id.clone();
        let handle = tokio::task::spawn(async move {
            agent_connection_task(agent_id, cols, rows, event_tx, command_rx).await;
        });

        self.panels.push(AgentPanel {
            id,
            agent_binary: binary,
            vterm: VirtualTerminal::new(cols as usize, rows as usize),
            command_tx,
            exited: false,
            task_handle: handle,
        });
    }
}

// ---------------------------------------------------------------------------
// Background connection task
// ---------------------------------------------------------------------------

async fn agent_connection_task(
    agent_id: String,
    cols: u16,
    rows: u16,
    event_tx: mpsc::Sender<AgentOutputEvent>,
    mut command_rx: mpsc::Receiver<PanelCommand>,
) {
    let stream = match ipc::try_connect().await {
        Ok(s) => s,
        Err(_) => {
            let _ = event_tx
                .send(AgentOutputEvent::ConnectionLost {
                    id: agent_id,
                })
                .await;
            return;
        }
    };

    let (mut reader, mut writer) = stream.into_split();

    // Send AttachAgent
    if clust_ipc::send_message_write(
        &mut writer,
        &CliMessage::AttachAgent {
            id: agent_id.clone(),
        },
    )
    .await
    .is_err()
    {
        let _ = event_tx
            .send(AgentOutputEvent::ConnectionLost {
                id: agent_id,
            })
            .await;
        return;
    }

    // Read response
    match clust_ipc::recv_message_read::<PoolMessage>(&mut reader).await {
        Ok(PoolMessage::AgentAttached { .. }) => {}
        _ => {
            let _ = event_tx
                .send(AgentOutputEvent::ConnectionLost {
                    id: agent_id,
                })
                .await;
            return;
        }
    }

    // Send initial resize
    let _ = clust_ipc::send_message_write(
        &mut writer,
        &CliMessage::ResizeAgent {
            id: agent_id.clone(),
            cols,
            rows,
        },
    )
    .await;

    // Main loop: read output from pool + forward commands from UI
    loop {
        tokio::select! {
            msg = clust_ipc::recv_message_read::<PoolMessage>(&mut reader) => {
                match msg {
                    Ok(PoolMessage::AgentOutput { data, .. }) => {
                        if event_tx
                            .send(AgentOutputEvent::Output {
                                id: agent_id.clone(),
                                data,
                            })
                            .await
                            .is_err()
                        {
                            return; // UI gone
                        }
                    }
                    Ok(PoolMessage::AgentExited { exit_code, .. }) => {
                        let _ = event_tx
                            .send(AgentOutputEvent::Exited {
                                id: agent_id.clone(),
                                _exit_code: exit_code,
                            })
                            .await;
                        return;
                    }
                    Ok(PoolMessage::PoolShutdown) => {
                        let _ = event_tx
                            .send(AgentOutputEvent::ConnectionLost {
                                id: agent_id.clone(),
                            })
                            .await;
                        return;
                    }
                    Err(_) => {
                        let _ = event_tx
                            .send(AgentOutputEvent::ConnectionLost {
                                id: agent_id.clone(),
                            })
                            .await;
                        return;
                    }
                    _ => {}
                }
            }
            cmd = command_rx.recv() => {
                match cmd {
                    Some(PanelCommand::Input(data)) => {
                        let _ = clust_ipc::send_message_write(
                            &mut writer,
                            &CliMessage::AgentInput {
                                id: agent_id.clone(),
                                data,
                            },
                        )
                        .await;
                    }
                    Some(PanelCommand::Resize { cols, rows }) => {
                        let _ = clust_ipc::send_message_write(
                            &mut writer,
                            &CliMessage::ResizeAgent {
                                id: agent_id.clone(),
                                cols,
                                rows,
                            },
                        )
                        .await;
                    }
                    Some(PanelCommand::Detach) | None => {
                        let _ = clust_ipc::send_message_write(
                            &mut writer,
                            &CliMessage::DetachAgent {
                                id: agent_id.clone(),
                            },
                        )
                        .await;
                        return;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn render_overview(frame: &mut Frame, area: Rect, state: &OverviewState) {
    // Split into options bar (1 row) + panels area
    let [options_area, panels_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);

    render_options_bar(
        frame,
        options_area,
        matches!(state.focus, OverviewFocus::OptionsBar),
    );

    if state.panels.is_empty() {
        render_empty_state(frame, panels_area);
        return;
    }

    let visible_count = state.visible_panel_count(panels_area.width);
    if visible_count == 0 {
        render_empty_state(frame, panels_area);
        return;
    }

    let end = (state.scroll_offset + visible_count).min(state.panels.len());
    let visible_panels = &state.panels[state.scroll_offset..end];
    let actual_visible = visible_panels.len();

    // Fixed-width columns so 2.5 panels fit on screen
    let pw = panel_total_width(panels_area.width);
    let constraints: Vec<Constraint> = (0..actual_visible)
        .map(|_| Constraint::Length(pw))
        .collect();
    let panel_areas = Layout::horizontal(constraints).split(panels_area);

    for (i, (panel, &panel_area)) in visible_panels.iter().zip(panel_areas.iter()).enumerate() {
        let global_idx = state.scroll_offset + i;
        let is_focused = matches!(state.focus, OverviewFocus::Terminal(idx) if idx == global_idx);
        render_agent_panel(frame, panel_area, panel, is_focused);
    }

    // Scroll indicators
    if state.scroll_offset > 0 {
        let left_count = state.scroll_offset;
        let indicator = Span::styled(
            format!(" ◀ {left_count} "),
            Style::default()
                .fg(theme::R_TEXT_SECONDARY)
                .bg(theme::R_BG_OVERLAY),
        );
        let indicator_area = Rect {
            x: panels_area.x,
            y: panels_area.y,
            width: indicator.content.len() as u16,
            height: 1,
        };
        frame.render_widget(Paragraph::new(Line::from(indicator)), indicator_area);
    }

    if end < state.panels.len() {
        let right_count = state.panels.len() - end;
        let text = format!(" {right_count} ▶ ");
        let text_len = text.chars().count() as u16;
        let indicator = Span::styled(
            text,
            Style::default()
                .fg(theme::R_TEXT_SECONDARY)
                .bg(theme::R_BG_OVERLAY),
        );
        let x = panels_area.x + panels_area.width - text_len;
        let indicator_area = Rect {
            x,
            y: panels_area.y,
            width: text_len,
            height: 1,
        };
        frame.render_widget(Paragraph::new(Line::from(indicator)), indicator_area);
    }
}

fn render_options_bar(frame: &mut Frame, area: Rect, focused: bool) {
    let bg = if focused {
        theme::R_BG_OVERLAY
    } else {
        theme::R_BG_RAISED
    };
    // Empty options bar for now — future filter buttons go here
    let fill = " ".repeat(area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            fill,
            Style::default().bg(bg),
        ))),
        area,
    );
}

fn render_agent_panel(frame: &mut Frame, area: Rect, panel: &AgentPanel, focused: bool) {
    if area.height < 3 {
        return;
    }

    // Border color indicates focus
    let border_color = if focused {
        theme::R_ACCENT_BRIGHT
    } else {
        theme::R_TEXT_TERTIARY
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(theme::R_BG_BASE));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 1 {
        return;
    }

    // Split inner area into header (1 row) + terminal content
    let [header_area, content_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);

    // Header
    let header_bg = if focused {
        theme::R_BG_OVERLAY
    } else {
        theme::R_BG_RAISED
    };
    let id_color = if focused {
        theme::R_ACCENT_BRIGHT
    } else {
        theme::R_ACCENT
    };

    let mut header_spans = vec![
        Span::styled(" ", Style::default().bg(header_bg)),
        Span::styled(
            &panel.id,
            Style::default().fg(id_color).bg(header_bg),
        ),
        Span::styled(
            " · ",
            Style::default()
                .fg(theme::R_TEXT_TERTIARY)
                .bg(header_bg),
        ),
        Span::styled(
            &panel.agent_binary,
            Style::default()
                .fg(theme::R_TEXT_SECONDARY)
                .bg(header_bg),
        ),
        Span::styled(" ", Style::default().bg(header_bg)),
    ];

    if panel.exited {
        header_spans.push(Span::styled(
            "[exited]",
            Style::default().fg(theme::R_ERROR).bg(header_bg),
        ));
        header_spans.push(Span::styled(" ", Style::default().bg(header_bg)));
    } else {
        header_spans.push(Span::styled(
            "●",
            Style::default().fg(theme::R_SUCCESS).bg(header_bg),
        ));
        header_spans.push(Span::styled(" ", Style::default().bg(header_bg)));
    }

    // Fill remaining header width
    let content_width: usize = header_spans.iter().map(|s| s.content.chars().count()).sum();
    let remaining = (header_area.width as usize).saturating_sub(content_width);
    if remaining > 0 {
        header_spans.push(Span::styled(
            " ".repeat(remaining),
            Style::default().bg(header_bg),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(header_spans)),
        header_area,
    );

    // Terminal content
    let lines = panel.vterm.screen.to_ratatui_lines();
    let paragraph = Paragraph::new(lines).style(Style::default().bg(theme::R_BG_BASE));
    frame.render_widget(paragraph, content_area);
}

fn render_empty_state(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "No agents running",
            Style::default().fg(theme::R_TEXT_TERTIARY),
        )))
        .alignment(ratatui::layout::Alignment::Center)
        .style(Style::default().bg(theme::R_BG_BASE)),
        area,
    );
}
