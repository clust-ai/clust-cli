pub mod gitdiff;
pub mod input;

use std::collections::{HashMap, HashSet};

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crossterm::event::{KeyCode, KeyEvent};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

use clust_ipc::{AgentInfo, BranchInfo, CliMessage, HubMessage, RepoInfo};

use crate::{ipc, syntax, tasks::BatchAgentInfo, terminal_emulator::TerminalEmulator, theme, ui::ClickMap};

/// Minimum width in columns for a single agent panel.
const MIN_PANEL_WIDTH: u16 = 60;

/// Maximum number of panels that fit side-by-side at MIN_PANEL_WIDTH.
fn max_panels_for_width(available_width: u16) -> usize {
    (available_width / MIN_PANEL_WIDTH).max(1) as usize
}

/// Width of each panel when `count` panels share the available width evenly.
/// A single panel still only gets half the screen (never full width).
fn panel_width_for_count(available_width: u16, count: usize) -> u16 {
    let slots = (count as u16).max(2); // at least 2 slots so 1 panel = half width
    (available_width / slots).max(MIN_PANEL_WIDTH)
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

/// Events received from terminal connection tasks.
pub enum TerminalOutputEvent {
    Output { id: String, data: Vec<u8> },
    Exited { id: String },
    ConnectionLost { id: String },
}

/// A terminal shell panel in focus mode.
pub struct TerminalPanel {
    pub id: String,
    pub vterm: TerminalEmulator,
    pub command_tx: mpsc::Sender<PanelCommand>,
    pub exited: bool,
    pub scroll_offset: usize,
    task_handle: JoinHandle<()>,
}

/// A single agent panel in the overview.
pub struct AgentPanel {
    pub id: String,
    pub agent_binary: String,
    pub branch_name: Option<String>,
    pub repo_path: Option<String>,
    pub is_worktree: bool,
    pub vterm: TerminalEmulator,
    pub command_tx: mpsc::Sender<PanelCommand>,
    pub exited: bool,
    /// Whether the worktree cleanup dialog has already been shown for this panel.
    pub worktree_cleanup_shown: bool,
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
    /// Repo paths that are currently collapsed in the filter bar.
    /// Collapsed repos hide their agents from both the overview panels and the bar indicators.
    pub collapsed_repos: HashSet<String>,
    /// Cursor position within the repo groups (when OptionsBar is focused).
    pub filter_cursor: usize,
    /// Sorted+filtered panel indices, recomputed each frame.
    /// Used by both rendering and keyboard navigation.
    pub sorted_indices: Vec<usize>,
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
            collapsed_repos: HashSet::new(),
            filter_cursor: 0,
            sorted_indices: Vec::new(),
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

    /// Number of panels that fit in the given width (uses sorted/filtered list).
    pub fn visible_panel_count(&self, width: u16) -> usize {
        if self.sorted_indices.is_empty() {
            return 0;
        }
        let remaining = self.sorted_indices.len().saturating_sub(self.scroll_offset);
        let max_fit = max_panels_for_width(width);
        remaining.min(max_fit)
    }

    /// Move focus to the previous agent terminal (sorted order).
    pub fn focus_prev(&mut self) {
        if let OverviewFocus::Terminal(idx) = self.focus {
            if let Some(pos) = self.sorted_indices.iter().position(|&i| i == idx) {
                let new_pos = if pos > 0 {
                    pos - 1
                } else {
                    self.sorted_indices.len() - 1
                };
                let new_idx = self.sorted_indices[new_pos];
                self.focus = OverviewFocus::Terminal(new_idx);
                self.last_terminal_idx = new_idx;
                self.ensure_visible_sorted(new_idx);
            }
        }
    }

    /// Move focus to the next agent terminal (sorted order).
    pub fn focus_next(&mut self) {
        if let OverviewFocus::Terminal(idx) = self.focus {
            if let Some(pos) = self.sorted_indices.iter().position(|&i| i == idx) {
                let new_pos = if pos + 1 < self.sorted_indices.len() {
                    pos + 1
                } else {
                    0
                };
                let new_idx = self.sorted_indices[new_pos];
                self.focus = OverviewFocus::Terminal(new_idx);
                self.last_terminal_idx = new_idx;
                self.ensure_visible_sorted(new_idx);
            }
        }
    }

    /// Select and focus a specific agent by its ID.
    pub fn select_agent_by_id(&mut self, agent_id: &str) {
        if let Some(idx) = self.panels.iter().position(|p| p.id == agent_id) {
            self.focus = OverviewFocus::Terminal(idx);
            self.last_terminal_idx = idx;
            self.ensure_visible_sorted(idx);
        }
    }

    /// Enter terminal focus from options bar.
    pub fn enter_terminal(&mut self) {
        if !self.sorted_indices.is_empty() {
            // Try to return to the last focused terminal if it's still visible
            if self.sorted_indices.contains(&self.last_terminal_idx) {
                self.focus = OverviewFocus::Terminal(self.last_terminal_idx);
                self.ensure_visible_sorted(self.last_terminal_idx);
            } else {
                // Fall back to the first visible panel in sorted order
                let idx = self.sorted_indices[0];
                self.focus = OverviewFocus::Terminal(idx);
                self.last_terminal_idx = idx;
                self.ensure_visible_sorted(idx);
            }
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

    /// Scroll viewport left (within sorted/filtered list).
    pub fn scroll_left(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset -= 1;
        }
    }

    /// Scroll viewport right (within sorted/filtered list).
    pub fn scroll_right(&mut self, visible_width: u16) {
        let visible = self.visible_panel_count(visible_width);
        if self.scroll_offset + visible < self.sorted_indices.len() {
            self.scroll_offset += 1;
        }
    }

    // -- Private helpers --

    /// Ensure a global panel index is visible on screen by adjusting scroll_offset.
    /// Works on position within sorted_indices.
    pub fn ensure_visible_sorted(&mut self, global_idx: usize) {
        if let Some(pos) = self.sorted_indices.iter().position(|&i| i == global_idx) {
            if pos < self.scroll_offset {
                self.scroll_offset = pos;
            }
            if self.viewport_width > 0 {
                let fully_visible = max_panels_for_width(self.viewport_width);
                if pos >= self.scroll_offset + fully_visible {
                    self.scroll_offset = pos + 1 - fully_visible;
                }
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
        // Clamp scroll to sorted_indices bounds (may be empty if not yet computed)
        if !self.sorted_indices.is_empty() {
            self.scroll_offset = self
                .scroll_offset
                .min(self.sorted_indices.len().saturating_sub(1));
        }
    }

    /// Compute panel indices sorted by repo group order, then branch, then ID.
    /// Batch agents are grouped together by batch, ordered by task index.
    /// Panels whose repo is collapsed are excluded from the result.
    pub fn compute_sorted_indices(
        &mut self,
        repos: &[RepoInfo],
        batch_map: &HashMap<String, BatchAgentInfo>,
    ) {
        let repo_order: HashMap<&str, usize> = repos
            .iter()
            .enumerate()
            .map(|(i, r)| (r.path.as_str(), i))
            .collect();

        let mut indices: Vec<usize> = self
            .panels
            .iter()
            .enumerate()
            .filter(|(_, p)| match &p.repo_path {
                Some(rp) => !self.collapsed_repos.contains(rp),
                None => !self.collapsed_repos.contains(""),
            })
            .map(|(i, _)| i)
            .collect();

        indices.sort_by(|&a, &b| {
            let pa = &self.panels[a];
            let pb = &self.panels[b];
            let order_a = pa
                .repo_path
                .as_deref()
                .and_then(|rp| repo_order.get(rp).copied())
                .unwrap_or(usize::MAX);
            let order_b = pb
                .repo_path
                .as_deref()
                .and_then(|rp| repo_order.get(rp).copied())
                .unwrap_or(usize::MAX);

            let batch_a = batch_map.get(&pa.id);
            let batch_b = batch_map.get(&pb.id);

            // Non-batch agents (0) sort before batch agents (1) within same repo.
            // Batch agents are grouped by batch_id, then ordered by task_index.
            let group_a = batch_a
                .map(|b| (1usize, b.batch_id, b.task_index))
                .unwrap_or((0, 0, 0));
            let group_b = batch_b
                .map(|b| (1usize, b.batch_id, b.task_index))
                .unwrap_or((0, 0, 0));

            order_a
                .cmp(&order_b)
                .then_with(|| group_a.cmp(&group_b))
                .then_with(|| pa.branch_name.cmp(&pb.branch_name))
                .then_with(|| pa.id.cmp(&pb.id))
        });

        self.sorted_indices = indices;
    }

    fn recalculate_panel_size(&mut self, agent_count: usize, content_area: Rect) {
        if agent_count == 0 {
            return;
        }
        self.viewport_width = content_area.width;
        // Content area already excludes the tab bar and status bar.
        // We subtract 3 rows for the grouped filter bar (border + content + border).
        let available_height = content_area.height.saturating_sub(3);
        // Each panel has: top border (1) + header (1) + terminal content + bottom border (1)
        self.panel_rows = available_height.saturating_sub(3).max(1);

        // Derive panel width from the number of panels that will be visible.
        // VTE terminal gets the inner width: total minus 2 border columns.
        let remaining = agent_count.saturating_sub(self.scroll_offset).max(1);
        let max_fit = max_panels_for_width(content_area.width);
        let visible = remaining.min(max_fit);
        let pw = panel_width_for_count(content_area.width, visible);
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
            is_worktree: agent.is_worktree,
            vterm: TerminalEmulator::new(cols as usize, rows as usize),
            command_tx,
            exited: false,
            worktree_cleanup_shown: false,
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
// Terminal connection task
// ---------------------------------------------------------------------------

async fn terminal_connection_task(
    working_dir: String,
    cols: u16,
    rows: u16,
    event_tx: mpsc::Sender<TerminalOutputEvent>,
    mut command_rx: mpsc::Receiver<PanelCommand>,
) {
    let stream = match ipc::try_connect().await {
        Ok(s) => s,
        Err(_) => {
            let _ = event_tx
                .send(TerminalOutputEvent::ConnectionLost {
                    id: String::new(),
                })
                .await;
            return;
        }
    };

    let (mut reader, mut writer) = stream.into_split();

    // Send StartTerminal
    if clust_ipc::send_message_write(
        &mut writer,
        &CliMessage::StartTerminal {
            working_dir,
            cols,
            rows,
        },
    )
    .await
    .is_err()
    {
        let _ = event_tx
            .send(TerminalOutputEvent::ConnectionLost {
                id: String::new(),
            })
            .await;
        return;
    }

    // Read response
    let terminal_id = match clust_ipc::recv_message_read::<HubMessage>(&mut reader).await {
        Ok(HubMessage::TerminalStarted { id }) => id,
        _ => {
            let _ = event_tx
                .send(TerminalOutputEvent::ConnectionLost {
                    id: String::new(),
                })
                .await;
            return;
        }
    };

    // Consume replay data
    loop {
        match clust_ipc::recv_message_read::<HubMessage>(&mut reader).await {
            Ok(HubMessage::TerminalOutput { data, .. }) => {
                let _ = event_tx
                    .send(TerminalOutputEvent::Output {
                        id: terminal_id.clone(),
                        data,
                    })
                    .await;
            }
            Ok(HubMessage::TerminalReplayComplete { .. }) => break,
            Ok(HubMessage::TerminalExited { .. }) => {
                let _ = event_tx
                    .send(TerminalOutputEvent::Exited {
                        id: terminal_id.clone(),
                    })
                    .await;
                return;
            }
            _ => {
                let _ = event_tx
                    .send(TerminalOutputEvent::ConnectionLost {
                        id: terminal_id,
                    })
                    .await;
                return;
            }
        }
    }

    // Send initial resize
    let _ = clust_ipc::send_message_write(
        &mut writer,
        &CliMessage::ResizeTerminal {
            id: terminal_id.clone(),
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
                    Ok(HubMessage::TerminalOutput { data, .. }) => {
                        if event_tx
                            .send(TerminalOutputEvent::Output {
                                id: terminal_id.clone(),
                                data,
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(HubMessage::TerminalExited { .. }) => {
                        let _ = event_tx
                            .send(TerminalOutputEvent::Exited {
                                id: terminal_id.clone(),
                            })
                            .await;
                        return;
                    }
                    Ok(HubMessage::HubShutdown) | Err(_) => {
                        let _ = event_tx
                            .send(TerminalOutputEvent::ConnectionLost {
                                id: terminal_id.clone(),
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
                            &CliMessage::TerminalInput {
                                id: terminal_id.clone(),
                                data,
                            },
                        )
                        .await;
                    }
                    Some(PanelCommand::Resize { cols, rows }) => {
                        let _ = clust_ipc::send_message_write(
                            &mut writer,
                            &CliMessage::ResizeTerminal {
                                id: terminal_id.clone(),
                                cols,
                                rows,
                            },
                        )
                        .await;
                    }
                    Some(PanelCommand::Detach) | None => {
                        let _ = clust_ipc::send_message_write(
                            &mut writer,
                            &CliMessage::DetachTerminal {
                                id: terminal_id.clone(),
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

pub fn render_overview(
    frame: &mut Frame,
    area: Rect,
    state: &mut OverviewState,
    click_map: &mut ClickMap,
    repo_colors: &HashMap<String, String>,
    repos: &[RepoInfo],
    batch_map: &HashMap<String, BatchAgentInfo>,
) {
    // Split into filter bar (1 row) + panels area
    let [options_area, panels_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);

    // 1. Compute sorted+filtered panel indices (populates state.sorted_indices)
    state.compute_sorted_indices(repos, batch_map);

    // 2. Compute scroll range within sorted indices
    let sorted_len = state.sorted_indices.len();
    let visible_count = state.visible_panel_count(panels_area.width);

    let (scroll, end) = if sorted_len == 0 || visible_count == 0 {
        (0, 0)
    } else {
        let scroll = state.scroll_offset.min(sorted_len.saturating_sub(1));
        let end = (scroll + visible_count).min(sorted_len);
        (scroll, end)
    };

    // 3. Build set of global panel indices that are currently visible on screen
    let visible_indices: HashSet<usize> = if end > scroll {
        state.sorted_indices[scroll..end].iter().copied().collect()
    } else {
        HashSet::new()
    };

    // 4. Render the grouped filter bar
    let bar_focused = matches!(state.focus, OverviewFocus::OptionsBar);
    render_options_bar(
        frame,
        options_area,
        bar_focused,
        repos,
        &state.panels,
        &state.sorted_indices,
        &state.collapsed_repos,
        &visible_indices,
        state.filter_cursor,
        click_map,
        batch_map,
    );

    // 5. Render panels
    if sorted_len == 0 || visible_count == 0 {
        render_empty_state(frame, panels_area);
        return;
    }

    let actual_visible = end - scroll;

    // Distribute available width evenly; at least 2 slots so 1 panel = half screen
    let slots = (actual_visible as u32).max(2);
    let constraints: Vec<Constraint> = (0..actual_visible)
        .map(|_| Constraint::Ratio(1, slots))
        .collect();
    let panel_areas = Layout::horizontal(constraints).split(panels_area);

    let focus = state.focus;
    for (i, &global_idx) in state.sorted_indices[scroll..end].iter().enumerate() {
        let panel = &mut state.panels[global_idx];
        let is_focused = matches!(focus, OverviewFocus::Terminal(idx) if idx == global_idx);
        click_map.overview_panels.push((panel_areas[i], global_idx));
        let panel_color = panel.repo_path.as_ref()
            .and_then(|rp| repo_colors.get(rp.as_str()))
            .map(|cn| theme::repo_color(cn));
        let batch_info = batch_map.get(&panel.id);
        if let Some(content_area) = render_agent_panel(frame, panel_areas[i], panel, is_focused, false, panel_color, batch_info) {
            click_map.overview_content_areas.push((content_area, global_idx));
        }
    }

    // Scroll indicators
    if scroll > 0 {
        let left_count = scroll;
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

    if end < sorted_len {
        let right_count = sorted_len - end;
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

#[allow(clippy::too_many_arguments)]
fn render_options_bar(
    frame: &mut Frame,
    area: Rect, // 1 row
    focused: bool,
    repos: &[RepoInfo],
    panels: &[AgentPanel],
    _sorted_indices: &[usize],
    collapsed_repos: &HashSet<String>,
    visible_indices: &HashSet<usize>,
    filter_cursor: usize,
    click_map: &mut ClickMap,
    batch_map: &HashMap<String, BatchAgentInfo>,
) {
    let bar_bg = if focused {
        theme::R_BG_OVERLAY
    } else {
        theme::R_BG_RAISED
    };

    // Fill the 1-row area with background
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ".repeat(area.width as usize),
            Style::default().bg(bar_bg),
        ))),
        area,
    );

    let has_other = panels.iter().any(|p| p.repo_path.is_none());
    let group_count = repos.len() + if has_other { 1 } else { 0 };

    if group_count == 0 {
        return;
    }

    let mut spans: Vec<Span> = Vec::new();
    let mut x = area.x;

    // ── LEFT SECTION: Repo chips ──

    for (i, repo) in repos.iter().enumerate() {
        let is_collapsed = collapsed_repos.contains(&repo.path);
        let is_cursor = focused && i == filter_cursor;

        let color = repo
            .color
            .as_ref()
            .map(|c| theme::repo_color(c))
            .unwrap_or(theme::R_ACCENT);

        let chip_bg = if is_cursor {
            theme::R_BG_ACTIVE
        } else {
            bar_bg
        };

        let (dot_color, text_color) = if is_collapsed {
            (theme::dim_color(color), theme::R_TEXT_DISABLED)
        } else {
            (color, theme::R_TEXT_PRIMARY)
        };

        let dot_span = Span::styled(" \u{25cf} ", Style::default().fg(dot_color).bg(chip_bg));
        let name_span = Span::styled(
            format!("{} ", repo.name),
            Style::default().fg(text_color).bg(chip_bg),
        );

        let chip_width = 3 + repo.name.len() as u16 + 1; // " ● " + name + " "

        // Register click target for repo chip
        if x + chip_width <= area.x + area.width {
            let chip_rect = Rect {
                x,
                y: area.y,
                width: chip_width,
                height: 1,
            };
            click_map
                .overview_repo_buttons
                .push((chip_rect, repo.path.clone()));
        }

        spans.push(dot_span);
        spans.push(name_span);
        x += chip_width;
    }

    // "Other" chip for agents with no repo
    if has_other {
        let is_collapsed = collapsed_repos.contains("");
        let is_cursor = focused && repos.len() == filter_cursor;
        let color = theme::R_ACCENT;

        let chip_bg = if is_cursor {
            theme::R_BG_ACTIVE
        } else {
            bar_bg
        };

        let (dot_color, text_color) = if is_collapsed {
            (theme::dim_color(color), theme::R_TEXT_DISABLED)
        } else {
            (color, theme::R_TEXT_PRIMARY)
        };

        let dot_span = Span::styled(" \u{25cf} ", Style::default().fg(dot_color).bg(chip_bg));
        let name_span = Span::styled(
            "Other ",
            Style::default().fg(text_color).bg(chip_bg),
        );

        let chip_width: u16 = 3 + 6; // " ● " + "Other "
        if x + chip_width <= area.x + area.width {
            let chip_rect = Rect {
                x,
                y: area.y,
                width: chip_width,
                height: 1,
            };
            click_map
                .overview_repo_buttons
                .push((chip_rect, String::new()));
        }

        spans.push(dot_span);
        spans.push(name_span);
        x += chip_width;
    }

    // ── SEPARATOR ──

    if !panels.is_empty() {
        spans.push(Span::styled(
            " \u{2502} ",
            Style::default()
                .fg(theme::R_TEXT_TERTIARY)
                .bg(bar_bg),
        ));
        x += 3;
    }

    // ── RIGHT SECTION: Branch indicators (all agents, sorted by repo order) ──

    // Build repo order map for sorting
    let repo_order: HashMap<&str, usize> = repos
        .iter()
        .enumerate()
        .map(|(i, r)| (r.path.as_str(), i))
        .collect();

    // Collect all agents with their global indices, sorted by repo order then batch group
    let mut all_agents: Vec<(usize, &AgentPanel)> = panels.iter().enumerate().collect();
    all_agents.sort_by(|&(_, a), &(_, b)| {
        let order_a = a
            .repo_path
            .as_deref()
            .and_then(|rp| repo_order.get(rp).copied())
            .unwrap_or(usize::MAX);
        let order_b = b
            .repo_path
            .as_deref()
            .and_then(|rp| repo_order.get(rp).copied())
            .unwrap_or(usize::MAX);

        let ba = batch_map.get(&a.id);
        let bb = batch_map.get(&b.id);
        let group_a = ba
            .map(|bi| (1usize, bi.batch_id, bi.task_index))
            .unwrap_or((0, 0, 0));
        let group_b = bb
            .map(|bi| (1usize, bi.batch_id, bi.task_index))
            .unwrap_or((0, 0, 0));

        order_a
            .cmp(&order_b)
            .then_with(|| group_a.cmp(&group_b))
            .then_with(|| a.branch_name.cmp(&b.branch_name))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut last_batch_id: Option<usize> = None;

    for &(global_idx, panel) in &all_agents {
        let branch = panel.branch_name.as_deref().unwrap_or(&panel.id);
        let is_visible = visible_indices.contains(&global_idx);
        let cur_batch = batch_map.get(&panel.id);

        // Insert batch label when entering a new batch group
        let cur_bid = cur_batch.map(|b| b.batch_id);
        if cur_bid != last_batch_id {
            if let Some(bi) = cur_batch {
                let label = format!(" {}:", bi.batch_title);
                let label_width = label.len() as u16;
                spans.push(Span::styled(
                    label,
                    Style::default()
                        .fg(theme::R_INFO)
                        .bg(bar_bg)
                        .add_modifier(Modifier::BOLD),
                ));
                x += label_width;
            }
        }
        last_batch_id = cur_bid;

        let repo_color = panel
            .repo_path
            .as_ref()
            .and_then(|rp| {
                repos
                    .iter()
                    .find(|r| r.path == *rp)
                    .and_then(|r| r.color.as_ref())
                    .map(|c| theme::repo_color(c))
            })
            .unwrap_or(theme::R_ACCENT);

        let is_repo_collapsed = match &panel.repo_path {
            Some(rp) => collapsed_repos.contains(rp),
            None => collapsed_repos.contains(""),
        };

        let style = if is_visible {
            // Inverse video: repo color background, primary text
            Style::default()
                .fg(theme::R_TEXT_PRIMARY)
                .bg(repo_color)
        } else if is_repo_collapsed {
            Style::default()
                .fg(theme::R_TEXT_DISABLED)
                .bg(bar_bg)
        } else {
            Style::default()
                .fg(repo_color)
                .bg(bar_bg)
        };

        let branch_text = format!(" {branch} ");
        let branch_width = branch_text.len() as u16;

        // Register click target for agent indicator
        if x + branch_width <= area.x + area.width {
            let agent_rect = Rect {
                x,
                y: area.y,
                width: branch_width,
                height: 1,
            };
            click_map
                .overview_agent_indicators
                .push((agent_rect, global_idx));
        }

        spans.push(Span::styled(branch_text, style));
        x += branch_width;
    }

    // Fill remaining width with background
    let content_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let remaining = (area.width as usize).saturating_sub(content_len);
    if remaining > 0 {
        spans.push(Span::styled(
            " ".repeat(remaining),
            Style::default().bg(bar_bg),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_agent_panel(
    frame: &mut Frame,
    area: Rect,
    panel: &mut AgentPanel,
    focused: bool,
    in_focus_mode: bool,
    repo_color: Option<Color>,
    batch_info: Option<&BatchAgentInfo>,
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

    // Show batch title in top border for batch agents
    if let Some(bi) = batch_info {
        block = block.title_top(Line::from(vec![
            Span::styled(
                format!(" {} ", bi.batch_title),
                Style::default()
                    .fg(theme::R_INFO)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{}/{} ", bi.task_index + 1, bi.task_count),
                Style::default().fg(theme::R_TEXT_TERTIARY),
            ),
        ]));
    }

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

    // Terminal content (skip for batch agents — header is sufficient)
    if batch_info.is_none() {
        let lines = if panel.panel_scroll_offset > 0 {
            panel
                .vterm
                .to_ratatui_lines_scrolled(panel.panel_scroll_offset)
        } else {
            panel.vterm.to_ratatui_lines()
        };
        let paragraph = Paragraph::new(lines).style(Style::default().bg(theme::R_BG_BASE));
        frame.render_widget(paragraph, content_area);
    }
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
    Compare,
    Terminal,
}

impl LeftPanelTab {
    pub fn next(self) -> Self {
        match self {
            Self::Changes => Self::Compare,
            Self::Compare => Self::Terminal,
            Self::Terminal => Self::Changes,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Changes => Self::Terminal,
            Self::Compare => Self::Changes,
            Self::Terminal => Self::Compare,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Changes => "Changes",
            Self::Compare => "Compare",
            Self::Terminal => "Terminal",
        }
    }

    fn all() -> &'static [LeftPanelTab] {
        &[Self::Changes, Self::Compare, Self::Terminal]
    }
}

// ---------------------------------------------------------------------------
// Branch picker for the Compare tab
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BranchPickerMode {
    /// User is typing in the search field; branch list is visible.
    Searching,
    /// A branch has been selected; diff is shown below the label.
    Selected,
}

pub struct BranchPicker {
    pub mode: BranchPickerMode,
    pub input: String,
    pub cursor_pos: usize,
    pub selected_idx: usize,
    pub selected_branch: Option<String>,
    pub branches: Vec<BranchInfo>,
    matcher: SkimMatcherV2,
}

impl BranchPicker {
    pub fn new() -> Self {
        Self {
            mode: BranchPickerMode::Selected,
            input: String::new(),
            cursor_pos: 0,
            selected_idx: 0,
            selected_branch: None,
            branches: Vec::new(),
            matcher: SkimMatcherV2::default(),
        }
    }

    /// Update the branch list. Filters out the agent's own branch.
    pub fn update_branches(&mut self, branches: Vec<BranchInfo>, agent_branch: Option<&str>) {
        self.branches = branches
            .into_iter()
            .filter(|b| agent_branch.is_none_or(|ab| b.name != ab))
            .collect();
        // Clamp selection
        let count = self.filtered_branches().len();
        if count > 0 && self.selected_idx >= count {
            self.selected_idx = count - 1;
        }
    }

    /// Returns filtered branches as (original_index, score) sorted by score descending.
    pub fn filtered_branches(&self) -> Vec<(usize, i64)> {
        if self.input.is_empty() {
            return self.branches.iter().enumerate().map(|(i, _)| (i, 0)).collect();
        }
        let mut results: Vec<(usize, i64)> = self
            .branches
            .iter()
            .enumerate()
            .filter_map(|(i, branch)| {
                self.matcher
                    .fuzzy_match(&branch.name, &self.input)
                    .map(|score| (i, score))
            })
            .collect();
        results.sort_by(|a, b| b.1.cmp(&a.1));
        results
    }

    /// Enter searching mode, resetting the input.
    pub fn enter_search(&mut self) {
        self.mode = BranchPickerMode::Searching;
        self.input.clear();
        self.cursor_pos = 0;
        self.selected_idx = 0;
    }

    /// Handle a key event while in Searching mode.
    /// Returns `true` if a branch was selected (caller should start diff task).
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Enter => {
                let filtered = self.filtered_branches();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    self.selected_branch = Some(self.branches[idx].name.clone());
                    self.mode = BranchPickerMode::Selected;
                    return true;
                }
                false
            }
            KeyCode::Esc => {
                self.mode = BranchPickerMode::Selected;
                false
            }
            KeyCode::Up => {
                self.selected_idx = self.selected_idx.saturating_sub(1);
                false
            }
            KeyCode::Down => {
                let count = self.filtered_branches().len();
                if count > 0 && self.selected_idx < count - 1 {
                    self.selected_idx += 1;
                }
                false
            }
            KeyCode::Char(c) => {
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                self.selected_idx = 0;
                false
            }
            KeyCode::Backspace => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.remove(self.cursor_pos);
                    self.selected_idx = 0;
                }
                false
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                false
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos +=
                        self.input[self.cursor_pos..].chars().next().unwrap().len_utf8();
                }
                false
            }
            _ => false,
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        for c in text.chars() {
            if c == '\n' || c == '\r' {
                continue;
            }
            self.input.insert(self.cursor_pos, c);
            self.cursor_pos += c.len_utf8();
        }
        self.selected_idx = 0;
    }

    /// Compute scroll offset to keep the selected item centered.
    pub fn compute_scroll(&self, total: usize, visible: usize) -> usize {
        if total <= visible {
            return 0;
        }
        let half = visible / 2;
        if self.selected_idx <= half {
            0
        } else if self.selected_idx + half >= total {
            total.saturating_sub(visible)
        } else {
            self.selected_idx - half
        }
    }
}

/// Metadata about the batch a focus-mode agent belongs to.
#[derive(Clone, Debug)]
pub struct BatchOrigin {
    pub batch_title: String,
    pub batch_idx: usize,
    pub task_idx: usize,
}

/// State for the single-agent focus mode view.
pub struct FocusModeState {
    pub panel: Option<AgentPanel>,
    output_rx: mpsc::Receiver<AgentOutputEvent>,
    output_tx: mpsc::Sender<AgentOutputEvent>,
    panel_cols: u16,
    panel_rows: u16,
    /// Set when the agent was opened from a batch task.
    pub batch_origin: Option<BatchOrigin>,
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
    pub branch_name: Option<String>,
    // Branch compare tab state
    pub compare_picker: BranchPicker,
    pub compare_diff: Option<gitdiff::ParsedDiff>,
    pub compare_diff_scroll: usize,
    pub compare_diff_error: Option<String>,
    compare_diff_rx: mpsc::Receiver<gitdiff::DiffEvent>,
    compare_diff_tx: mpsc::Sender<gitdiff::DiffEvent>,
    compare_diff_stop_tx: Option<watch::Sender<bool>>,
    compare_diff_task: Option<JoinHandle<()>>,
    // Terminal tab state
    pub terminal_panel: Option<TerminalPanel>,
    terminal_output_rx: mpsc::Receiver<TerminalOutputEvent>,
    terminal_output_tx: mpsc::Sender<TerminalOutputEvent>,
    terminal_cols: u16,
    terminal_rows: u16,
}

impl FocusModeState {
    pub fn new() -> Self {
        let (output_tx, output_rx) = mpsc::channel(512);
        let (diff_tx, diff_rx) = mpsc::channel(16);
        let (compare_diff_tx, compare_diff_rx) = mpsc::channel(16);
        let (terminal_output_tx, terminal_output_rx) = mpsc::channel(512);
        Self {
            panel: None,
            output_rx,
            output_tx,
            panel_cols: 80,
            panel_rows: 24,
            batch_origin: None,
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
            branch_name: None,
            compare_picker: BranchPicker::new(),
            compare_diff: None,
            compare_diff_scroll: 0,
            compare_diff_error: None,
            compare_diff_rx,
            compare_diff_tx,
            compare_diff_stop_tx: None,
            compare_diff_task: None,
            terminal_panel: None,
            terminal_output_rx,
            terminal_output_tx,
            terminal_cols: 80,
            terminal_rows: 24,
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
        is_worktree: bool,
    ) {
        self.close_panel();

        self.panel_cols = cols;
        self.panel_rows = rows;
        self.batch_origin = None;
        self.focus_side = FocusSide::Right;
        self.left_tab = LeftPanelTab::Changes;
        self.diff = None;
        self.diff_scroll = 0;
        self.diff_error = None;
        self.working_dir = Some(working_dir.to_string());
        self.repo_path = repo_path.map(|s| s.to_string());
        self.branch_name = branch_name.map(|s| s.to_string());
        self.compare_picker = BranchPicker::new();
        self.compare_diff = None;
        self.compare_diff_scroll = 0;
        self.compare_diff_error = None;

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
            is_worktree,
            vterm: TerminalEmulator::new(cols as usize, rows as usize),
            command_tx,
            exited: false,
            worktree_cleanup_shown: false,
            panel_scroll_offset: 0,
            task_handle: handle,
        });

        // Spawn diff refresh task (only if inside a repository)
        if repo_path.is_some() {
            let (stop_tx, stop_rx) = watch::channel(false);
            let diff_tx = self.diff_tx.clone();
            let diff_handle =
                gitdiff::spawn_diff_task(working_dir.to_string(), diff_tx, stop_rx);
            self.diff_stop_tx = Some(stop_tx);
            self.diff_task = Some(diff_handle);
        }

        // Start terminal session
        self.open_terminal(working_dir, self.terminal_cols, self.terminal_rows);
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
        // Stop compare diff task
        self.stop_compare_diff();
        // Stop terminal
        self.close_terminal();
    }

    /// Stop the branch compare diff background task.
    fn stop_compare_diff(&mut self) {
        if let Some(stop_tx) = self.compare_diff_stop_tx.take() {
            let _ = stop_tx.send(true);
        }
        if let Some(handle) = self.compare_diff_task.take() {
            handle.abort();
        }
        self.compare_diff = None;
        self.compare_diff_scroll = 0;
        self.compare_diff_error = None;
    }

    // Terminal session management

    fn open_terminal(&mut self, working_dir: &str, cols: u16, rows: u16) {
        self.close_terminal();
        self.terminal_cols = cols;
        self.terminal_rows = rows;

        let event_tx = self.terminal_output_tx.clone();
        let (command_tx, command_rx) = mpsc::channel::<PanelCommand>(64);
        let wd = working_dir.to_string();

        let handle = tokio::task::spawn(async move {
            terminal_connection_task(wd, cols, rows, event_tx, command_rx).await;
        });

        self.terminal_panel = Some(TerminalPanel {
            id: String::new(),
            vterm: TerminalEmulator::new(cols as usize, rows as usize),
            command_tx,
            exited: false,
            scroll_offset: 0,
            task_handle: handle,
        });
    }

    fn close_terminal(&mut self) {
        if let Some(panel) = self.terminal_panel.take() {
            let _ = panel.command_tx.try_send(PanelCommand::Detach);
            panel.task_handle.abort();
        }
    }

    /// Drain terminal output events into the vterm.
    pub fn drain_terminal_events(&mut self) {
        while let Ok(event) = self.terminal_output_rx.try_recv() {
            match event {
                TerminalOutputEvent::Output { id, data } => {
                    if let Some(panel) = self.terminal_panel.as_mut() {
                        if panel.id.is_empty() || panel.id == id {
                            if panel.id.is_empty() {
                                panel.id = id;
                            }
                            panel.vterm.process(&data);
                        }
                    }
                }
                TerminalOutputEvent::Exited { id }
                | TerminalOutputEvent::ConnectionLost { id } => {
                    if let Some(panel) = self.terminal_panel.as_mut() {
                        if panel.id.is_empty() || panel.id == id {
                            panel.exited = true;
                        }
                    }
                }
            }
        }
    }

    /// Send input bytes to the terminal.
    pub fn send_terminal_input(&self, data: Vec<u8>) {
        if let Some(panel) = &self.terminal_panel {
            let _ = panel.command_tx.try_send(PanelCommand::Input(data));
        }
    }

    /// Handle terminal panel resize.
    pub fn handle_terminal_resize(&mut self, cols: u16, rows: u16) {
        if cols == self.terminal_cols && rows == self.terminal_rows {
            return;
        }
        self.terminal_cols = cols;
        self.terminal_rows = rows;
        if let Some(panel) = &mut self.terminal_panel {
            if panel
                .command_tx
                .try_send(PanelCommand::Resize { cols, rows })
                .is_ok()
            {
                panel.vterm.resize(cols as usize, rows as usize);
            }
        }
    }

    /// Start the branch compare diff background task for the selected branch.
    pub fn start_compare_diff(&mut self) {
        self.stop_compare_diff();

        let (Some(working_dir), Some(agent_branch), Some(compare_branch)) = (
            self.working_dir.as_ref(),
            self.branch_name.as_ref(),
            self.compare_picker.selected_branch.as_ref(),
        ) else {
            return;
        };

        let (stop_tx, stop_rx) = watch::channel(false);
        let tx = self.compare_diff_tx.clone();
        let handle = gitdiff::spawn_branch_diff_task(
            working_dir.clone(),
            compare_branch.clone(),
            agent_branch.clone(),
            tx,
            stop_rx,
        );
        self.compare_diff_stop_tx = Some(stop_tx);
        self.compare_diff_task = Some(handle);
    }

    /// Drain branch compare diff events from the background task.
    pub fn drain_compare_diff_events(&mut self) {
        while let Ok(event) = self.compare_diff_rx.try_recv() {
            match event {
                gitdiff::DiffEvent::Updated(parsed) => {
                    let max = parsed.lines.len().saturating_sub(1);
                    self.compare_diff_scroll = self.compare_diff_scroll.min(max);
                    self.compare_diff = Some(parsed);
                    self.compare_diff_error = None;
                }
                gitdiff::DiffEvent::Error(msg) => {
                    self.compare_diff_error = Some(msg);
                }
            }
        }
    }

    /// Update the branch picker's branch list from the current repos data.
    pub fn update_compare_branches(&mut self, repos: &[RepoInfo]) {
        let branches = self
            .repo_path
            .as_ref()
            .and_then(|rp| repos.iter().find(|r| r.path == *rp))
            .map(|r| r.local_branches.clone())
            .unwrap_or_default();
        self.compare_picker
            .update_branches(branches, self.branch_name.as_deref());
    }

    /// Scroll compare diff view up by one line.
    pub fn compare_scroll_up(&mut self) {
        self.compare_diff_scroll = self.compare_diff_scroll.saturating_sub(1);
    }

    /// Scroll compare diff view down by one line.
    pub fn compare_scroll_down(&mut self) {
        if let Some(diff) = &self.compare_diff {
            let max = diff.lines.len().saturating_sub(1);
            if self.compare_diff_scroll < max {
                self.compare_diff_scroll += 1;
            }
        }
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

    // Left side: repo-aware rendering
    if state.repo_path.is_some() {
        render_left_panel(frame, left_area, state, click_map, panel_color);
    } else {
        render_no_repo_left_panel(frame, left_area);
    }

    // Right side: agent panel or empty state
    let right_focused = state.focus_side == FocusSide::Right;
    match &mut state.panel {
        Some(panel) => {
            if let Some(content_area) = render_agent_panel(frame, right_area, panel, right_focused, true, panel_color, None) {
                click_map.focus_right_content_area = content_area;
            }
        }
        None => render_empty_state(frame, right_area),
    }
}

fn render_left_panel(frame: &mut Frame, area: Rect, state: &mut FocusModeState, click_map: &mut ClickMap, repo_color: Option<Color>) {
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
        LeftPanelTab::Changes => render_diff_viewer(
            frame,
            content_area,
            state.diff.as_ref(),
            state.diff_scroll,
            state.diff_error.as_deref(),
            "No uncommitted changes",
            repo_color,
        ),
        LeftPanelTab::Compare => render_compare_tab(frame, content_area, state, repo_color),
        LeftPanelTab::Terminal => {
            render_terminal_tab(frame, content_area, state);
        }
    }
}

fn render_terminal_tab(frame: &mut Frame, area: Rect, state: &mut FocusModeState) {
    match &mut state.terminal_panel {
        Some(panel) if !panel.exited => {
            let lines = if panel.scroll_offset > 0 {
                panel.vterm.to_ratatui_lines_scrolled(panel.scroll_offset)
            } else {
                panel.vterm.to_ratatui_lines()
            };
            let paragraph = Paragraph::new(lines)
                .style(Style::default().bg(theme::R_BG_BASE));
            frame.render_widget(paragraph, area);
        }
        Some(_) => {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "Terminal session ended",
                    Style::default().fg(theme::R_TEXT_TERTIARY),
                )))
                .alignment(ratatui::layout::Alignment::Center)
                .style(Style::default().bg(theme::R_BG_BASE)),
                area,
            );
        }
        None => {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "Starting terminal...",
                    Style::default().fg(theme::R_TEXT_TERTIARY),
                )))
                .alignment(ratatui::layout::Alignment::Center)
                .style(Style::default().bg(theme::R_BG_BASE)),
                area,
            );
        }
    }
}

fn render_no_repo_left_panel(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Agent not running inside repository",
            Style::default().fg(theme::R_TEXT_TERTIARY),
        )))
        .alignment(ratatui::layout::Alignment::Center)
        .style(Style::default().bg(theme::R_BG_BASE)),
        area,
    );
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

fn render_diff_viewer(
    frame: &mut Frame,
    area: Rect,
    diff: Option<&gitdiff::ParsedDiff>,
    scroll: usize,
    error: Option<&str>,
    empty_message: &str,
    repo_color: Option<Color>,
) {
    let bg_style = Style::default().bg(theme::R_BG_BASE);

    // Error state
    if let Some(err) = error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                err.to_string(),
                Style::default().fg(theme::R_ERROR),
            )))
            .wrap(Wrap { trim: false })
            .style(bg_style),
            area,
        );
        return;
    }

    // No diff data yet
    let diff = match diff {
        Some(d) if !d.lines.is_empty() => d,
        Some(_) => {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    empty_message,
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
    let start = scroll;
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

        // Syntax-highlight code lines (Add/Delete/Context); others keep plain styling
        let content_spans = match diff_line.kind {
            gitdiff::DiffLineKind::Add
            | gitdiff::DiffLineKind::Delete
            | gitdiff::DiffLineKind::Context => {
                let file_name = diff.file_names.get(diff_line.file_idx).map(|s| s.as_str());
                let file_syntax = file_name.and_then(syntax::syntax_for_file);
                match file_syntax {
                    Some(syn) => {
                        let spans = syntax::highlight_line(
                            &display_content, syn, line_bg, content_fg,
                        );
                        if spans.is_empty() {
                            vec![Span::styled(display_content, content_style)]
                        } else {
                            spans
                        }
                    }
                    None => vec![Span::styled(display_content, content_style)],
                }
            }
            _ => vec![Span::styled(display_content, content_style)],
        };

        let mut spans = vec![
            Span::styled(gutter_text, gutter_style),
            Span::styled(separator, sep_style),
        ];
        spans.extend(content_spans);

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

// ---------------------------------------------------------------------------
// Compare tab rendering
// ---------------------------------------------------------------------------

fn render_compare_tab(
    frame: &mut Frame,
    area: Rect,
    state: &FocusModeState,
    repo_color: Option<Color>,
) {
    match state.compare_picker.mode {
        BranchPickerMode::Searching => {
            let [hint_area, input_area, _gap_area, list_area] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .areas(area);

            render_picker_hint(frame, hint_area);
            render_picker_input(frame, input_area, &state.compare_picker);
            // gap renders as default bg
            frame.render_widget(
                Paragraph::new("").style(Style::default().bg(theme::R_BG_BASE)),
                _gap_area,
            );
            render_picker_branch_list(frame, list_area, &state.compare_picker);
        }
        BranchPickerMode::Selected => {
            let [label_area, diff_area] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .areas(area);

            render_compare_label(frame, label_area, &state.compare_picker);

            if state.compare_picker.selected_branch.is_some() {
                render_diff_viewer(
                    frame,
                    diff_area,
                    state.compare_diff.as_ref(),
                    state.compare_diff_scroll,
                    state.compare_diff_error.as_deref(),
                    "No differences",
                    repo_color,
                );
            } else {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "Press Enter to select a branch to compare",
                        Style::default().fg(theme::R_TEXT_TERTIARY),
                    )))
                    .alignment(ratatui::layout::Alignment::Center)
                    .style(Style::default().bg(theme::R_BG_BASE)),
                    diff_area,
                );
            }
        }
    }
}

fn render_compare_label(frame: &mut Frame, area: Rect, picker: &BranchPicker) {
    let (label, hint) = match &picker.selected_branch {
        Some(branch) => (
            format!(" Comparing: {branch}"),
            " [Enter] change ",
        ),
        None => (
            " No branch selected".to_string(),
            " [Enter] select ",
        ),
    };

    let hint_width = hint.chars().count();
    let label_width = (area.width as usize).saturating_sub(hint_width);

    // Truncate label if needed
    let display_label: String = if label.chars().count() > label_width {
        label.chars().take(label_width.saturating_sub(1)).collect::<String>() + "…"
    } else {
        let pad = label_width.saturating_sub(label.chars().count());
        format!("{}{}", label, " ".repeat(pad))
    };

    let line = Line::from(vec![
        Span::styled(
            display_label,
            Style::default()
                .fg(theme::R_TEXT_SECONDARY)
                .bg(theme::R_BG_SURFACE),
        ),
        Span::styled(
            hint,
            Style::default()
                .fg(theme::R_TEXT_TERTIARY)
                .bg(theme::R_BG_SURFACE),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_picker_hint(frame: &mut Frame, area: Rect) {
    let line = Line::from(Span::styled(
        " Select a branch to compare against",
        Style::default()
            .fg(theme::R_TEXT_SECONDARY)
            .bg(theme::R_BG_BASE),
    ));
    let remaining = (area.width as usize).saturating_sub(35);
    let mut spans = vec![line.spans[0].clone()];
    if remaining > 0 {
        spans.push(Span::styled(
            " ".repeat(remaining),
            Style::default().bg(theme::R_BG_BASE),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_picker_input(frame: &mut Frame, area: Rect, picker: &BranchPicker) {
    let before_cursor = &picker.input[..picker.cursor_pos];
    let (cursor_char, after_cursor) = if picker.cursor_pos < picker.input.len() {
        let ch_len = picker.input[picker.cursor_pos..]
            .chars()
            .next()
            .unwrap()
            .len_utf8();
        (
            &picker.input[picker.cursor_pos..picker.cursor_pos + ch_len],
            &picker.input[picker.cursor_pos + ch_len..],
        )
    } else {
        (" ", "")
    };

    let line = Line::from(vec![
        Span::styled(
            " > ",
            Style::default()
                .fg(theme::R_ACCENT_BRIGHT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(before_cursor, Style::default().fg(theme::R_TEXT_PRIMARY)),
        Span::styled(
            cursor_char,
            Style::default()
                .fg(theme::R_BG_BASE)
                .bg(theme::R_TEXT_PRIMARY),
        ),
        Span::styled(after_cursor, Style::default().fg(theme::R_TEXT_PRIMARY)),
    ]);

    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme::R_BG_INPUT)),
        area,
    );
}

fn render_picker_branch_list(frame: &mut Frame, area: Rect, picker: &BranchPicker) {
    let filtered = picker.filtered_branches();
    let max_visible = area.height as usize;
    let scroll = picker.compute_scroll(filtered.len(), max_visible);

    let lines: Vec<Line> = filtered
        .iter()
        .skip(scroll)
        .take(max_visible)
        .enumerate()
        .map(|(vis_idx, &(orig_idx, _))| {
            let branch = &picker.branches[orig_idx];
            let is_selected = vis_idx + scroll == picker.selected_idx;

            // Build spans: indicator + name + badges
            let mut spans = if is_selected {
                vec![
                    Span::styled(
                        "  > ",
                        Style::default()
                            .fg(theme::R_ACCENT_BRIGHT)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        branch.name.as_str(),
                        Style::default()
                            .fg(theme::R_TEXT_PRIMARY)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]
            } else {
                vec![
                    Span::styled("    ", Style::default()),
                    Span::styled(
                        branch.name.as_str(),
                        Style::default().fg(theme::R_TEXT_SECONDARY),
                    ),
                ]
            };

            // Badges (same as CreateAgentModal)
            if branch.is_head {
                spans.push(Span::styled(
                    " HEAD",
                    Style::default().fg(theme::R_SUCCESS),
                ));
            }
            if branch.is_worktree {
                spans.push(Span::styled(
                    " [worktree]",
                    Style::default().fg(theme::R_INFO),
                ));
            }
            if branch.active_agent_count > 0 {
                spans.push(Span::styled(
                    format!(
                        " ({} agent{})",
                        branch.active_agent_count,
                        if branch.active_agent_count == 1 { "" } else { "s" }
                    ),
                    Style::default().fg(theme::R_WARNING),
                ));
            }

            Line::from(spans)
        })
        .collect();

    // Fill remaining with empty lines
    let mut all_lines = lines;
    for _ in all_lines.len()..max_visible {
        all_lines.push(Line::from(Span::styled(
            " ".repeat(area.width as usize),
            Style::default().bg(theme::R_BG_BASE),
        )));
    }

    frame.render_widget(
        Paragraph::new(all_lines).style(Style::default().bg(theme::R_BG_BASE)),
        area,
    );
}
