pub mod gitdiff;
pub mod input;

use std::collections::HashMap;

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use clust_ipc::{AgentInfo, CliMessage, HubMessage};

use crate::{ipc, terminal_emulator::TerminalEmulator, theme, ui::ClickMap};

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
    pub branch_name: Option<String>,
    pub repo_path: Option<String>,
    pub vterm: TerminalEmulator,
    pub command_tx: mpsc::Sender<PanelCommand>,
    pub exited: bool,
    /// Vertical scroll offset for scrollback (0 = live, >0 = scrolled back).
    pub panel_scroll_offset: usize,
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
    viewport_width: u16,
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
            viewport_width: 0,
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

        // Update metadata that may change during agent lifetime
        for agent in agents {
            if let Some(panel) = self.panels.iter_mut().find(|p| p.id == agent.id) {
                panel.branch_name = agent.branch_name.clone();
                panel.repo_path = agent.repo_path.clone();
            }
        }

        // Resize existing panels if dimensions changed.
        // Send the resize command before clearing the local screen so that
        // if the send fails the grid is not left empty without a SIGWINCH.
        for panel in &mut self.panels {
            let (pw, ph) = (self.panel_cols as usize, self.panel_rows as usize);
            if (panel.vterm.cols() != pw || panel.vterm.rows() != ph)
                && panel
                    .command_tx
                    .try_send(PanelCommand::Resize {
                        cols: self.panel_cols,
                        rows: self.panel_rows,
                    })
                    .is_ok()
            {
                panel.vterm.resize(pw, ph);
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
        let old_cols = self.panel_cols;
        let old_rows = self.panel_rows;
        self.recalculate_panel_size(agent_count, content_area);
        if self.panel_cols == old_cols && self.panel_rows == old_rows {
            return;
        }
        for panel in &mut self.panels {
            if panel
                .command_tx
                .try_send(PanelCommand::Resize {
                    cols: self.panel_cols,
                    rows: self.panel_rows,
                })
                .is_ok()
            {
                panel
                    .vterm
                    .resize(self.panel_cols as usize, self.panel_rows as usize);
            }
        }
    }

    /// Re-send current panel dimensions to the hub for all panels.
    /// Unlike `handle_resize`, this does not skip when dimensions are unchanged,
    /// because the hub's PTY may have been resized by another client.
    pub fn force_resize_all(&self) {
        for panel in &self.panels {
            let _ = panel.command_tx.try_send(PanelCommand::Resize {
                cols: self.panel_cols,
                rows: self.panel_rows,
            });
        }
    }

    /// Re-send current panel dimensions to the hub for the focused panel only.
    pub fn force_resize_focused(&self) {
        if let OverviewFocus::Terminal(idx) = self.focus {
            if let Some(panel) = self.panels.get(idx) {
                let _ = panel.command_tx.try_send(PanelCommand::Resize {
                    cols: self.panel_cols,
                    rows: self.panel_rows,
                });
            }
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
        let max_fit = width.div_ceil(pw).max(1) as usize;
        max_fit.min(self.panels.len() - self.scroll_offset)
    }

    /// Move focus to the previous agent terminal.
    pub fn focus_prev(&mut self) {
        if let OverviewFocus::Terminal(idx) = self.focus {
            let new_idx = if idx > 0 {
                idx - 1
            } else {
                self.panels.len() - 1
            };
            self.focus = OverviewFocus::Terminal(new_idx);
            self.last_terminal_idx = new_idx;
            self.ensure_visible(new_idx);
        }
    }

    /// Move focus to the next agent terminal.
    pub fn focus_next(&mut self) {
        if let OverviewFocus::Terminal(idx) = self.focus {
            let new_idx = if idx + 1 < self.panels.len() {
                idx + 1
            } else {
                0
            };
            self.focus = OverviewFocus::Terminal(new_idx);
            self.last_terminal_idx = new_idx;
            self.ensure_visible(new_idx);
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

    /// Scroll the focused panel's scrollback up by one page.
    pub fn panel_scroll_up(&mut self) {
        if let OverviewFocus::Terminal(idx) = self.focus {
            if let Some(panel) = self.panels.get_mut(idx) {
                let page = panel.vterm.rows();
                let max = panel.vterm.scrollback_len();
                panel.panel_scroll_offset = (panel.panel_scroll_offset + page).min(max);
            }
        }
    }

    /// Scroll the focused panel's scrollback down by one page.
    pub fn panel_scroll_down(&mut self) {
        if let OverviewFocus::Terminal(idx) = self.focus {
            if let Some(panel) = self.panels.get_mut(idx) {
                let page = panel.vterm.rows();
                panel.panel_scroll_offset = panel.panel_scroll_offset.saturating_sub(page);
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
        if self.viewport_width > 0 {
            let pw = panel_total_width(self.viewport_width);
            let fully_visible = (self.viewport_width / pw).max(1) as usize;
            if idx >= self.scroll_offset + fully_visible {
                self.scroll_offset = idx + 1 - fully_visible;
            }
        }
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
        self.viewport_width = content_area.width;
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
            branch_name: agent.branch_name.clone(),
            repo_path: agent.repo_path.clone(),
            vterm: TerminalEmulator::new(cols as usize, rows as usize),
            command_tx,
            exited: false,
            panel_scroll_offset: 0,
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
    match clust_ipc::recv_message_read::<HubMessage>(&mut reader).await {
        Ok(HubMessage::AgentAttached { .. }) => {}
        _ => {
            let _ = event_tx
                .send(AgentOutputEvent::ConnectionLost {
                    id: agent_id,
                })
                .await;
            return;
        }
    }

    // Consume replay data — the hub sends buffered output followed by
    // AgentReplayComplete before live streaming begins.
    loop {
        match clust_ipc::recv_message_read::<HubMessage>(&mut reader).await {
            Ok(HubMessage::AgentOutput { data, .. }) => {
                let _ = event_tx
                    .send(AgentOutputEvent::Output {
                        id: agent_id.clone(),
                        data,
                    })
                    .await;
            }
            Ok(HubMessage::AgentReplayComplete { .. }) => break,
            Ok(HubMessage::AgentExited { exit_code, .. }) => {
                let _ = event_tx
                    .send(AgentOutputEvent::Exited {
                        id: agent_id.clone(),
                        _exit_code: exit_code,
                    })
                    .await;
                return;
            }
            _ => {
                let _ = event_tx
                    .send(AgentOutputEvent::ConnectionLost {
                        id: agent_id,
                    })
                    .await;
                return;
            }
        }
    }

    // Send initial resize — triggers SIGWINCH for a fresh redraw on top of
    // the replayed state.
    let _ = clust_ipc::send_message_write(
        &mut writer,
        &CliMessage::ResizeAgent {
            id: agent_id.clone(),
            cols,
            rows,
        },
    )
    .await;

    // Main loop: read output from hub + forward commands from UI
    loop {
        tokio::select! {
            msg = clust_ipc::recv_message_read::<HubMessage>(&mut reader) => {
                match msg {
                    Ok(HubMessage::AgentOutput { data, .. }) => {
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
                    Ok(HubMessage::AgentExited { exit_code, .. }) => {
                        let _ = event_tx
                            .send(AgentOutputEvent::Exited {
                                id: agent_id.clone(),
                                _exit_code: exit_code,
                            })
                            .await;
                        return;
                    }
                    Ok(HubMessage::HubShutdown) => {
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

pub fn render_overview(frame: &mut Frame, area: Rect, state: &mut OverviewState, click_map: &mut ClickMap, repo_colors: &HashMap<String, String>) {
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
    let actual_visible = end - state.scroll_offset;

    // Fixed-width columns so 2.5 panels fit on screen
    let pw = panel_total_width(panels_area.width);
    let constraints: Vec<Constraint> = (0..actual_visible)
        .map(|_| Constraint::Length(pw))
        .collect();
    let panel_areas = Layout::horizontal(constraints).split(panels_area);

    let scroll_offset = state.scroll_offset;
    let focus = state.focus;
    for (i, panel) in state.panels[scroll_offset..end].iter_mut().enumerate() {
        let global_idx = scroll_offset + i;
        let is_focused = matches!(focus, OverviewFocus::Terminal(idx) if idx == global_idx);
        click_map.overview_panels.push((panel_areas[i], global_idx));
        let panel_color = panel.repo_path.as_ref()
            .and_then(|rp| repo_colors.get(rp.as_str()))
            .map(|cn| theme::repo_color(cn));
        if let Some(content_area) = render_agent_panel(frame, panel_areas[i], panel, is_focused, false, panel_color) {
            click_map.overview_content_areas.push((content_area, global_idx));
        }
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

fn render_agent_panel(
    frame: &mut Frame,
    area: Rect,
    panel: &mut AgentPanel,
    focused: bool,
    in_focus_mode: bool,
    repo_color: Option<Color>,
) -> Option<Rect> {
    if area.height < 3 {
        return None;
    }

    // Border color: use repo color when available, accent as fallback
    let border_color = match (focused, repo_color) {
        (true, Some(c)) => c,
        (false, Some(c)) => theme::dim_color(c),
        (true, None) => theme::R_ACCENT_BRIGHT,
        (false, None) => theme::R_TEXT_TERTIARY,
    };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(theme::R_BG_BASE));

    if focused && !in_focus_mode {
        block = block.title_bottom(
            Line::from(vec![
                Span::styled(
                    " Shift+\u{2193} ",
                    Style::default().fg(theme::R_ACCENT_BRIGHT),
                ),
                Span::styled("focus ", Style::default().fg(theme::R_TEXT_SECONDARY)),
            ])
            .centered(),
        );
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 1 {
        return None;
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

    if let Some(ref rp) = panel.repo_path {
        let repo_display = std::path::Path::new(rp)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| rp.clone());
        let repo_fg = repo_color.unwrap_or(theme::R_ACCENT);
        header_spans.push(Span::styled(
            "· ",
            Style::default()
                .fg(theme::R_TEXT_TERTIARY)
                .bg(header_bg),
        ));
        header_spans.push(Span::styled(
            repo_display,
            Style::default().fg(repo_fg).bg(header_bg),
        ));
        if let Some(ref branch) = panel.branch_name {
            header_spans.push(Span::styled(
                format!("/{branch}"),
                Style::default()
                    .fg(theme::R_TEXT_TERTIARY)
                    .bg(header_bg),
            ));
        }
        header_spans.push(Span::styled(" ", Style::default().bg(header_bg)));
    } else if let Some(ref branch) = panel.branch_name {
        header_spans.push(Span::styled(
            "· ",
            Style::default()
                .fg(theme::R_TEXT_TERTIARY)
                .bg(header_bg),
        ));
        header_spans.push(Span::styled(
            branch.as_str(),
            Style::default()
                .fg(theme::R_TEXT_TERTIARY)
                .bg(header_bg),
        ));
        header_spans.push(Span::styled(" ", Style::default().bg(header_bg)));
    }

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

    // Scroll indicator
    if panel.panel_scroll_offset > 0 {
        let indicator = format!("↑{} ", panel.panel_scroll_offset);
        header_spans.push(Span::styled(
            indicator,
            Style::default().fg(theme::R_WARNING).bg(header_bg),
        ));
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
    let lines = if panel.panel_scroll_offset > 0 {
        panel
            .vterm
            .to_ratatui_lines_scrolled(panel.panel_scroll_offset)
    } else {
        panel.vterm.to_ratatui_lines()
    };
    let paragraph = Paragraph::new(lines).style(Style::default().bg(theme::R_BG_BASE));
    frame.render_widget(paragraph, content_area);
    Some(content_area)
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

// ---------------------------------------------------------------------------
// Focus mode
// ---------------------------------------------------------------------------

/// Which side of the focus view has keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FocusSide {
    Left,
    Right,
}

/// Tabs available in the left panel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LeftPanelTab {
    Changes,
    Panel2,
    Panel3,
}

impl LeftPanelTab {
    pub fn next(self) -> Self {
        match self {
            Self::Changes => Self::Panel2,
            Self::Panel2 => Self::Panel3,
            Self::Panel3 => Self::Changes,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Changes => "Changes",
            Self::Panel2 => "Panel 2",
            Self::Panel3 => "Panel 3",
        }
    }

    fn all() -> &'static [LeftPanelTab] {
        &[Self::Changes, Self::Panel2, Self::Panel3]
    }
}

/// State for the single-agent focus mode view.
pub struct FocusModeState {
    pub panel: Option<AgentPanel>,
    output_rx: mpsc::Receiver<AgentOutputEvent>,
    output_tx: mpsc::Sender<AgentOutputEvent>,
    panel_cols: u16,
    panel_rows: u16,
    // Left panel state
    pub focus_side: FocusSide,
    pub left_tab: LeftPanelTab,
    pub diff: Option<gitdiff::ParsedDiff>,
    pub diff_scroll: usize,
    pub diff_error: Option<String>,
    diff_rx: mpsc::Receiver<gitdiff::DiffEvent>,
    diff_tx: mpsc::Sender<gitdiff::DiffEvent>,
    diff_stop_tx: Option<watch::Sender<bool>>,
    diff_task: Option<JoinHandle<()>>,
    pub working_dir: Option<String>,
    pub repo_path: Option<String>,
}

impl FocusModeState {
    pub fn new() -> Self {
        let (output_tx, output_rx) = mpsc::channel(512);
        let (diff_tx, diff_rx) = mpsc::channel(16);
        Self {
            panel: None,
            output_rx,
            output_tx,
            panel_cols: 80,
            panel_rows: 24,
            focus_side: FocusSide::Right,
            left_tab: LeftPanelTab::Changes,
            diff: None,
            diff_scroll: 0,
            diff_error: None,
            diff_rx,
            diff_tx,
            diff_stop_tx: None,
            diff_task: None,
            working_dir: None,
            repo_path: None,
        }
    }

    /// Open an agent in focus mode, replacing any existing panel.
    #[allow(clippy::too_many_arguments)]
    pub fn open_agent(
        &mut self,
        agent_id: &str,
        agent_binary: &str,
        cols: u16,
        rows: u16,
        working_dir: &str,
        repo_path: Option<&str>,
        branch_name: Option<&str>,
    ) {
        self.close_panel();

        self.panel_cols = cols;
        self.panel_rows = rows;
        self.focus_side = FocusSide::Right;
        self.left_tab = LeftPanelTab::Changes;
        self.diff = None;
        self.diff_scroll = 0;
        self.diff_error = None;
        self.working_dir = Some(working_dir.to_string());
        self.repo_path = repo_path.map(|s| s.to_string());

        let id = agent_id.to_string();
        let binary = agent_binary.to_string();
        let event_tx = self.output_tx.clone();
        let (command_tx, command_rx) = mpsc::channel::<PanelCommand>(64);

        let task_agent_id = id.clone();
        let handle = tokio::task::spawn(async move {
            agent_connection_task(task_agent_id, cols, rows, event_tx, command_rx).await;
        });

        self.panel = Some(AgentPanel {
            id,
            agent_binary: binary,
            branch_name: branch_name.map(|s| s.to_string()),
            repo_path: repo_path.map(|s| s.to_string()),
            vterm: TerminalEmulator::new(cols as usize, rows as usize),
            command_tx,
            exited: false,
            panel_scroll_offset: 0,
            task_handle: handle,
        });

        // Spawn diff refresh task
        let (stop_tx, stop_rx) = watch::channel(false);
        let diff_tx = self.diff_tx.clone();
        let diff_handle =
            gitdiff::spawn_diff_task(working_dir.to_string(), diff_tx, stop_rx);
        self.diff_stop_tx = Some(stop_tx);
        self.diff_task = Some(diff_handle);
    }

    /// Drain all pending output events from the background task.
    pub fn drain_output_events(&mut self) {
        while let Ok(event) = self.output_rx.try_recv() {
            match event {
                AgentOutputEvent::Output { id, data } => {
                    if let Some(panel) = self.panel.as_mut().filter(|p| p.id == id) {
                        panel.vterm.process(&data);
                    }
                }
                AgentOutputEvent::Exited { id, .. }
                | AgentOutputEvent::ConnectionLost { id } => {
                    if let Some(panel) = self.panel.as_mut().filter(|p| p.id == id) {
                        panel.exited = true;
                    }
                }
            }
        }
    }

    /// Drain diff refresh events.
    pub fn drain_diff_events(&mut self) {
        while let Ok(event) = self.diff_rx.try_recv() {
            match event {
                gitdiff::DiffEvent::Updated(parsed) => {
                    // Clamp scroll to new diff length
                    let max = parsed.lines.len().saturating_sub(1);
                    self.diff_scroll = self.diff_scroll.min(max);
                    self.diff = Some(parsed);
                    self.diff_error = None;
                }
                gitdiff::DiffEvent::Error(msg) => {
                    self.diff_error = Some(msg);
                }
            }
        }
    }

    /// Send input bytes to the focused agent.
    pub fn send_input(&self, data: Vec<u8>) {
        if let Some(panel) = &self.panel {
            let _ = panel.command_tx.try_send(PanelCommand::Input(data));
        }
    }

    /// Handle terminal resize.
    pub fn handle_resize(&mut self, cols: u16, rows: u16) {
        if cols == self.panel_cols && rows == self.panel_rows {
            return;
        }
        self.panel_cols = cols;
        self.panel_rows = rows;
        if let Some(panel) = &mut self.panel {
            if panel
                .command_tx
                .try_send(PanelCommand::Resize { cols, rows })
                .is_ok()
            {
                panel.vterm.resize(cols as usize, rows as usize);
            }
        }
    }

    /// Re-send current panel dimensions to the hub unconditionally.
    pub fn force_resize(&self) {
        if let Some(panel) = &self.panel {
            let _ = panel.command_tx.try_send(PanelCommand::Resize {
                cols: self.panel_cols,
                rows: self.panel_rows,
            });
        }
    }

    /// Scroll diff view up by one line.
    pub fn diff_scroll_up(&mut self) {
        self.diff_scroll = self.diff_scroll.saturating_sub(1);
    }

    /// Scroll diff view down by one line.
    pub fn diff_scroll_down(&mut self) {
        if let Some(diff) = &self.diff {
            let max = diff.lines.len().saturating_sub(1);
            if self.diff_scroll < max {
                self.diff_scroll += 1;
            }
        }
    }

    /// Jump to the previous file header in the diff.
    pub fn diff_jump_prev_file(&mut self) {
        if let Some(diff) = &self.diff {
            // Find the last file_start_index that is strictly before current scroll
            let target = diff
                .file_start_indices
                .iter()
                .rev()
                .find(|&&idx| idx < self.diff_scroll);
            if let Some(&idx) = target {
                self.diff_scroll = idx;
            } else {
                self.diff_scroll = 0;
            }
        }
    }

    /// Jump to the next file header in the diff.
    pub fn diff_jump_next_file(&mut self) {
        if let Some(diff) = &self.diff {
            let target = diff
                .file_start_indices
                .iter()
                .find(|&&idx| idx > self.diff_scroll);
            if let Some(&idx) = target {
                self.diff_scroll = idx;
            }
        }
    }

    /// Shut down the current panel and clean up.
    pub fn shutdown(&mut self) {
        self.close_panel();
    }

    pub fn is_active(&self) -> bool {
        self.panel.is_some()
    }

    fn close_panel(&mut self) {
        if let Some(panel) = self.panel.take() {
            let _ = panel.command_tx.try_send(PanelCommand::Detach);
            panel.task_handle.abort();
        }
        // Stop diff task
        if let Some(stop_tx) = self.diff_stop_tx.take() {
            let _ = stop_tx.send(true);
        }
        if let Some(handle) = self.diff_task.take() {
            handle.abort();
        }
        self.diff = None;
        self.diff_scroll = 0;
        self.diff_error = None;
        self.working_dir = None;
        self.repo_path = None;
    }
}

/// Render the focus mode view: 60% left panel with tabs, 40% agent panel right.
pub fn render_focus_mode(frame: &mut Frame, area: Rect, state: &mut FocusModeState, click_map: &mut ClickMap, repo_colors: &HashMap<String, String>) {
    let [left_area, right_area] = Layout::horizontal([
        Constraint::Percentage(60),
        Constraint::Percentage(40),
    ])
    .areas(area);

    click_map.focus_left_area = left_area;
    click_map.focus_right_area = right_area;

    let panel_color = state.repo_path.as_ref()
        .and_then(|rp| repo_colors.get(rp.as_str()))
        .map(|cn| theme::repo_color(cn));

    // Left side: tab bar + content
    render_left_panel(frame, left_area, state, click_map, panel_color);

    // Right side: agent panel or empty state
    let right_focused = state.focus_side == FocusSide::Right;
    match &mut state.panel {
        Some(panel) => {
            if let Some(content_area) = render_agent_panel(frame, right_area, panel, right_focused, true, panel_color) {
                click_map.focus_right_content_area = content_area;
            }
        }
        None => render_empty_state(frame, right_area),
    }
}

fn render_left_panel(frame: &mut Frame, area: Rect, state: &FocusModeState, click_map: &mut ClickMap, repo_color: Option<Color>) {
    if area.height < 2 {
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::R_BG_BASE)),
            area,
        );
        return;
    }

    let [tab_area, content_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);

    // Render tab bar
    let left_focused = state.focus_side == FocusSide::Left;
    render_left_tab_bar(frame, tab_area, state.left_tab, left_focused, click_map);

    // Render active tab content
    match state.left_tab {
        LeftPanelTab::Changes => render_diff_viewer(frame, content_area, state, repo_color),
        _ => {
            // Empty placeholder for Panel 2 / Panel 3
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("{} (coming soon)", state.left_tab.label()),
                    Style::default().fg(theme::R_TEXT_TERTIARY),
                )))
                .alignment(ratatui::layout::Alignment::Center)
                .style(Style::default().bg(theme::R_BG_BASE)),
                content_area,
            );
        }
    }
}

fn render_left_tab_bar(
    frame: &mut Frame,
    area: Rect,
    active: LeftPanelTab,
    panel_focused: bool,
    click_map: &mut ClickMap,
) {
    let mut spans = Vec::new();
    let mut cursor_x = area.x;

    for (i, tab) in LeftPanelTab::all().iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                "│",
                Style::default()
                    .fg(theme::R_TEXT_TERTIARY)
                    .bg(theme::R_BG_SURFACE),
            ));
            cursor_x += 1;
        }

        let is_active = *tab == active;
        let (fg, bg) = if is_active && panel_focused {
            (theme::R_TEXT_PRIMARY, theme::R_BG_OVERLAY)
        } else if is_active {
            (theme::R_TEXT_SECONDARY, theme::R_BG_RAISED)
        } else {
            (theme::R_TEXT_TERTIARY, theme::R_BG_SURFACE)
        };

        let style = if is_active {
            Style::default()
                .fg(fg)
                .bg(bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(fg).bg(bg)
        };

        let label = format!(" {} ", tab.label());
        let label_width = label.chars().count() as u16;
        click_map.focus_left_tabs.push((
            Rect { x: cursor_x, y: area.y, width: label_width, height: 1 },
            *tab,
        ));
        cursor_x += label_width;

        spans.push(Span::styled(label, style));
    }

    // Fill rest with background
    let content_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let remaining = (area.width as usize).saturating_sub(content_width);
    if remaining > 0 {
        spans.push(Span::styled(
            " ".repeat(remaining),
            Style::default().bg(theme::R_BG_SURFACE),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_diff_viewer(frame: &mut Frame, area: Rect, state: &FocusModeState, repo_color: Option<Color>) {
    let bg_style = Style::default().bg(theme::R_BG_BASE);

    // Error state
    if let Some(ref err) = state.diff_error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                err.as_str(),
                Style::default().fg(theme::R_ERROR),
            )))
            .wrap(Wrap { trim: false })
            .style(bg_style),
            area,
        );
        return;
    }

    // No diff data yet
    let diff = match &state.diff {
        Some(d) if !d.lines.is_empty() => d,
        Some(_) => {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "No uncommitted changes",
                    Style::default().fg(theme::R_TEXT_TERTIARY),
                )))
                .alignment(ratatui::layout::Alignment::Center)
                .style(bg_style),
                area,
            );
            return;
        }
        None => {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "Loading diff...",
                    Style::default().fg(theme::R_TEXT_TERTIARY),
                )))
                .alignment(ratatui::layout::Alignment::Center)
                .style(bg_style),
                area,
            );
            return;
        }
    };

    let visible_height = area.height as usize;
    let start = state.diff_scroll;
    let end = (start + visible_height).min(diff.lines.len());

    // Gutter width: "  old | new │" = 4+4+1 = 9 chars
    let gutter_width: u16 = 9;
    let content_width = area.width.saturating_sub(gutter_width);

    let mut lines = Vec::with_capacity(visible_height);

    for diff_line in &diff.lines[start..end] {
        // Separator: blank line between files
        if diff_line.kind == gitdiff::DiffLineKind::Separator {
            lines.push(Line::from(Span::styled(
                " ".repeat(area.width as usize),
                bg_style,
            )));
            continue;
        }

        let accent = repo_color.unwrap_or(theme::R_ACCENT);
        let (line_bg, content_fg) = match diff_line.kind {
            gitdiff::DiffLineKind::FileHeader => (accent, theme::R_BG_BASE),
            gitdiff::DiffLineKind::FileMetadata => (theme::R_BG_SURFACE, theme::R_TEXT_TERTIARY),
            gitdiff::DiffLineKind::HunkHeader => (theme::R_BG_SURFACE, accent),
            gitdiff::DiffLineKind::Add => (theme::R_DIFF_ADD_BG, theme::R_TEXT_PRIMARY),
            gitdiff::DiffLineKind::Delete => (theme::R_DIFF_DEL_BG, theme::R_TEXT_PRIMARY),
            gitdiff::DiffLineKind::Context => (theme::R_BG_BASE, theme::R_TEXT_SECONDARY),
            gitdiff::DiffLineKind::Separator => unreachable!(),
        };

        let gutter_style = Style::default().fg(theme::R_TEXT_TERTIARY).bg(line_bg);
        let sep_style = Style::default().fg(theme::R_TEXT_DISABLED).bg(line_bg);
        let content_style = if diff_line.kind == gitdiff::DiffLineKind::FileHeader {
            Style::default().fg(content_fg).bg(line_bg).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(content_fg).bg(line_bg)
        };

        // Build gutter: " old  new│"
        let old_str = match diff_line.old_lineno {
            Some(n) => format!("{:>4}", n),
            None => "    ".to_string(),
        };
        let new_str = match diff_line.new_lineno {
            Some(n) => format!("{:>4}", n),
            None => "    ".to_string(),
        };

        // For file headers and hunk headers, show no line numbers
        let (gutter_text, separator) = match diff_line.kind {
            gitdiff::DiffLineKind::FileHeader
            | gitdiff::DiffLineKind::FileMetadata
            | gitdiff::DiffLineKind::HunkHeader => ("        ".to_string(), " "),
            _ => (format!("{}{}", old_str, new_str), "│"),
        };

        // File headers: show clean file name instead of raw "diff --git ..." line
        let display_content = if diff_line.kind == gitdiff::DiffLineKind::FileHeader {
            diff.file_names.get(diff_line.file_idx)
                .map(|s| format!(" {s}"))
                .unwrap_or_else(|| diff_line.content.clone())
        } else {
            diff_line.content.clone()
        };
        let content_chars = display_content.chars().count();
        let pad = (content_width as usize).saturating_sub(content_chars);

        let mut spans = vec![
            Span::styled(gutter_text, gutter_style),
            Span::styled(separator, sep_style),
            Span::styled(display_content, content_style),
        ];

        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), content_style));
        }

        lines.push(Line::from(spans));
    }

    // Fill remaining visible area with empty lines
    for _ in end.saturating_sub(start)..visible_height {
        lines.push(Line::from(Span::styled(
            " ".repeat(area.width as usize),
            bg_style,
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}
