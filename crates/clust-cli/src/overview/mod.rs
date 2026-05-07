pub mod gitdiff;
pub mod input;
pub mod term_complete;

use std::collections::{HashMap, HashSet};

use ratatui::{
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

use clust_ipc::{AgentInfo, BranchInfo, CliMessage, HubMessage, RepoInfo};

use crate::{
    ipc, syntax, terminal_emulator::TerminalEmulator, theme, ui::ClickMap,
};

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
    /// Sent once, immediately after the hub acknowledges `StartTerminal`. Lets
    /// the owning panel learn its id before any output arrives so click rects
    /// and id-routed output never race.
    Started {
        id: String,
    },
    Output {
        id: String,
        data: Vec<u8>,
    },
    Exited {
        id: String,
    },
    ConnectionLost {
        id: String,
    },
    /// The connection task could not start (e.g., hub rejected the request or
    /// the socket was unreachable). Surfaced to the UI as a transient status
    /// line. The `id` is empty when the failure occurred before the hub
    /// acknowledged the terminal.
    SpawnFailed {
        message: String,
    },
}

/// A terminal shell panel in focus mode.
pub struct TerminalPanel {
    pub id: String,
    pub vterm: TerminalEmulator,
    pub command_tx: mpsc::Sender<PanelCommand>,
    pub exited: bool,
    pub scroll_offset: usize,
    /// Per-panel event channel. Owned by the panel so output keeps flowing into
    /// the local `vterm` even while the panel is stashed in the overview's
    /// `agent_terminals` cache (i.e., focus mode closed).
    event_rx: mpsc::Receiver<TerminalOutputEvent>,
    task_handle: JoinHandle<()>,
    /// Local mirror of what the user has typed since the last command boundary,
    /// used to drive Tab completion. The shell still owns its own input state
    /// over the PTY; this is just a best-effort echo of recent keystrokes.
    pub input_buffer: term_complete::InputBuffer,
}

impl TerminalPanel {
    /// Drain pending output events into the panel's vterm. Should be called
    /// every main-loop tick whether or not focus mode is currently displaying
    /// this panel.
    ///
    /// Returns a `SpawnFailed` message string if the connection task reported
    /// a spawn failure during this drain, so the caller can surface it on the
    /// status line.
    pub fn drain_events(&mut self) -> Option<String> {
        let mut spawn_failure: Option<String> = None;
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                TerminalOutputEvent::Started { id } => {
                    if self.id.is_empty() {
                        self.id = id;
                    }
                }
                TerminalOutputEvent::Output { id, data } => {
                    if self.id.is_empty() || self.id == id {
                        if self.id.is_empty() {
                            self.id = id;
                        }
                        self.vterm.process(&data);
                    }
                }
                TerminalOutputEvent::Exited { id } | TerminalOutputEvent::ConnectionLost { id } => {
                    if self.id.is_empty() || self.id == id {
                        self.exited = true;
                    }
                }
                TerminalOutputEvent::SpawnFailed { message } => {
                    self.exited = true;
                    spawn_failure = Some(message);
                }
            }
        }
        spawn_failure
    }
}

/// Cached focus-mode terminals for one agent. Stashed on `OverviewState` while
/// focus mode is closed so shell sessions and their scrollback survive across
/// re-entries. `current_idx` remembers which terminal was active.
pub struct AgentTerminalCache {
    pub panels: Vec<TerminalPanel>,
    pub current_idx: usize,
}

impl AgentTerminalCache {
    pub fn new() -> Self {
        Self {
            panels: Vec::new(),
            current_idx: 0,
        }
    }

    /// Drain events for every cached panel so backgrounded shells keep
    /// accumulating scrollback while focus mode is closed or while the user is
    /// looking at a sibling terminal. Returns the most recent spawn-failure
    /// message, if any, so the UI can surface it.
    pub fn drain_events(&mut self) -> Option<String> {
        let mut last_failure: Option<String> = None;
        for panel in &mut self.panels {
            if let Some(msg) = panel.drain_events() {
                last_failure = Some(msg);
            }
        }
        last_failure
    }
}

impl Default for AgentTerminalCache {
    fn default() -> Self {
        Self::new()
    }
}

/// A single agent panel in the overview.
pub struct AgentPanel {
    pub id: String,
    pub agent_binary: String,
    pub branch_name: Option<String>,
    pub repo_path: Option<String>,
    pub is_worktree: bool,
    /// RFC 3339 timestamp of when the hub spawned this agent. Used as the
    /// tertiary sort key in the overview so newly-spawned agents are appended
    /// at the end of their (repo, batch) group.
    pub started_at: String,
    pub vterm: TerminalEmulator,
    pub command_tx: mpsc::Sender<PanelCommand>,
    pub exited: bool,
    /// Whether the worktree cleanup dialog has already been shown for this panel.
    pub worktree_cleanup_shown: bool,
    /// Vertical scroll offset for scrollback (0 = live, >0 = scrolled back).
    pub panel_scroll_offset: usize,
    pub(crate) task_handle: JoinHandle<()>,
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
    /// Cached focus-mode terminal panels, keyed by agent_id. Stashed here while
    /// focus mode is closed so shell sessions and their scrollback survive
    /// across re-entries for the same agent. Each agent may own multiple
    /// terminals; `current_idx` tracks which one was active.
    pub agent_terminals: HashMap<String, AgentTerminalCache>,
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
            agent_terminals: HashMap::new(),
        }
    }

    /// Take the cached focus-mode terminals for an agent. Returns an empty
    /// cache if none exist yet.
    pub fn take_agent_terminals(&mut self, agent_id: &str) -> AgentTerminalCache {
        self.agent_terminals.remove(agent_id).unwrap_or_default()
    }

    /// Stash focus-mode terminals for an agent so they can be reused next time
    /// focus mode is opened for that agent. An empty cache is dropped instead
    /// of stored.
    pub fn store_agent_terminals(&mut self, agent_id: String, cache: AgentTerminalCache) {
        if !cache.panels.is_empty() {
            self.agent_terminals.insert(agent_id, cache);
        }
    }

    /// Drain pending output events for every cached focus-mode terminal so
    /// scrollback keeps accumulating while focus mode is closed.
    pub fn drain_cached_terminal_events(&mut self) {
        for cache in self.agent_terminals.values_mut() {
            cache.drain_events();
        }
    }

    /// Synchronize the panel set with the current agent list (lifecycle only).
    ///
    /// Adds panels for new agents, removes/aborts panels for agents that
    /// disappeared, prunes stale `agent_terminals` cache entries, and
    /// updates per-panel metadata. Does **not** touch panel dimensions —
    /// callers that own a layout call [`resize_panels_to`] separately.
    pub fn sync_agent_set(&mut self, agents: &[AgentInfo]) {
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

        // Prune cached focus-mode terminal panels whose agent is gone. A
        // cache entry can outlive its agent's overview panel if the user
        // closed focus mode just before the agent disappeared. Each cache
        // may hold multiple panels — abort every background task.
        self.agent_terminals.retain(|id, cache| {
            let alive = agents.iter().any(|a| a.id == *id);
            if !alive {
                for panel in &cache.panels {
                    panel.task_handle.abort();
                }
            }
            alive
        });

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

        self.clamp_focus();
        self.initialized = true;
    }

    /// Recalculate the overview-grid panel dimensions from the content area
    /// and push the result to every panel (vterm resize + SIGWINCH).
    pub fn resize_panels_to(&mut self, content_area: Rect) {
        self.recalculate_panel_size(self.panels.len(), content_area);

        // Send the resize command before resizing the local vterm so that
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
    }

    /// Synchronize panels with the current agent list **and** resize them
    /// for the overview grid. Convenience wrapper used by callers that own
    /// the overview layout (i.e. the Overview tab).
    pub fn sync_agents(&mut self, agents: &[AgentInfo], content_area: Rect) {
        self.sync_agent_set(agents);
        self.resize_panels_to(content_area);
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
                AgentOutputEvent::Exited { id, .. } | AgentOutputEvent::ConnectionLost { id } => {
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
                self.focus = OverviewFocus::Terminal(self.panels.len().saturating_sub(1));
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

    /// Compute panel indices sorted by repo group order, then creation time
    /// (newly-spawned agents appear at the end of their group). Panels whose
    /// repo is collapsed are excluded from the result.
    pub fn compute_sorted_indices(&mut self, repos: &[RepoInfo]) {
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

            order_a
                .cmp(&order_b)
                .then_with(|| pa.started_at.cmp(&pb.started_at))
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
            started_at: agent.started_at.clone(),
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

pub(crate) async fn agent_connection_task(
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
                .send(AgentOutputEvent::ConnectionLost { id: agent_id })
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
            .send(AgentOutputEvent::ConnectionLost { id: agent_id })
            .await;
        return;
    }

    // Read response
    match clust_ipc::recv_message_read::<HubMessage>(&mut reader).await {
        Ok(HubMessage::AgentAttached { .. }) => {}
        _ => {
            let _ = event_tx
                .send(AgentOutputEvent::ConnectionLost { id: agent_id })
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
                    .send(AgentOutputEvent::ConnectionLost { id: agent_id })
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
                    #[allow(clippy::collapsible_match)]
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
    agent_id: Option<String>,
    event_tx: mpsc::Sender<TerminalOutputEvent>,
    mut command_rx: mpsc::Receiver<PanelCommand>,
) {
    let stream = match ipc::try_connect().await {
        Ok(s) => s,
        Err(e) => {
            let _ = event_tx
                .send(TerminalOutputEvent::SpawnFailed {
                    message: format!("hub unreachable ({})", e),
                })
                .await;
            return;
        }
    };

    let (mut reader, mut writer) = stream.into_split();

    // Send StartTerminal
    if let Err(e) = clust_ipc::send_message_write(
        &mut writer,
        &CliMessage::StartTerminal {
            working_dir,
            cols,
            rows,
            agent_id,
        },
    )
    .await
    {
        let _ = event_tx
            .send(TerminalOutputEvent::SpawnFailed {
                message: format!("send StartTerminal failed ({})", e),
            })
            .await;
        return;
    }

    // Read response
    let terminal_id = match clust_ipc::recv_message_read::<HubMessage>(&mut reader).await {
        Ok(HubMessage::TerminalStarted { id }) => id,
        Ok(other) => {
            let _ = event_tx
                .send(TerminalOutputEvent::SpawnFailed {
                    message: format!("hub rejected start (got {:?})", other),
                })
                .await;
            return;
        }
        Err(e) => {
            let _ = event_tx
                .send(TerminalOutputEvent::SpawnFailed {
                    message: format!("hub closed before reply ({})", e),
                })
                .await;
            return;
        }
    };

    // Inform the panel of its id immediately so click rects, switches and
    // id-routed output don't race against the first output frame.
    let _ = event_tx
        .send(TerminalOutputEvent::Started {
            id: terminal_id.clone(),
        })
        .await;

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
                    .send(TerminalOutputEvent::ConnectionLost { id: terminal_id })
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
                    #[allow(clippy::collapsible_match)]
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
) {
    // Split into filter bar (1 row) + panels area
    let [options_area, panels_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);

    // 1. Compute sorted+filtered panel indices (populates state.sorted_indices)
    state.compute_sorted_indices(repos);

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
        let panel_color = panel
            .repo_path
            .as_ref()
            .and_then(|rp| repo_colors.get(rp.as_str()))
            .map(|cn| theme::repo_color(cn));
        if let Some(content_area) = render_agent_panel(
            frame,
            panel_areas[i],
            panel,
            is_focused,
            false,
            panel_color,
        ) {
            click_map
                .overview_content_areas
                .push((content_area, global_idx));
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
        let name_span = Span::styled("Other ", Style::default().fg(text_color).bg(chip_bg));

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
            Style::default().fg(theme::R_TEXT_TERTIARY).bg(bar_bg),
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

    // Collect all agents with their global indices, sorted by repo order
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

        order_a
            .cmp(&order_b)
            .then_with(|| a.branch_name.cmp(&b.branch_name))
            .then_with(|| a.id.cmp(&b.id))
    });

    for &(global_idx, panel) in &all_agents {
        let branch = panel.branch_name.as_deref().unwrap_or(&panel.id);
        let is_visible = visible_indices.contains(&global_idx);

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
            Style::default().fg(theme::R_TEXT_PRIMARY).bg(repo_color)
        } else if is_repo_collapsed {
            Style::default().fg(theme::R_TEXT_DISABLED).bg(bar_bg)
        } else {
            Style::default().fg(repo_color).bg(bar_bg)
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

pub(crate) fn render_agent_panel(
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
    let [header_area, content_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);

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
        Span::styled(&panel.id, Style::default().fg(id_color).bg(header_bg)),
        Span::styled(
            " · ",
            Style::default().fg(theme::R_TEXT_TERTIARY).bg(header_bg),
        ),
        Span::styled(
            &panel.agent_binary,
            Style::default().fg(theme::R_TEXT_SECONDARY).bg(header_bg),
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
            Style::default().fg(theme::R_TEXT_TERTIARY).bg(header_bg),
        ));
        header_spans.push(Span::styled(
            repo_display,
            Style::default().fg(repo_fg).bg(header_bg),
        ));
        if let Some(ref branch) = panel.branch_name {
            header_spans.push(Span::styled(
                format!("/{branch}"),
                Style::default().fg(theme::R_TEXT_TERTIARY).bg(header_bg),
            ));
        }
        header_spans.push(Span::styled(" ", Style::default().bg(header_bg)));
    } else if let Some(ref branch) = panel.branch_name {
        header_spans.push(Span::styled(
            "· ",
            Style::default().fg(theme::R_TEXT_TERTIARY).bg(header_bg),
        ));
        header_spans.push(Span::styled(
            branch.as_str(),
            Style::default().fg(theme::R_TEXT_TERTIARY).bg(header_bg),
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

    frame.render_widget(Paragraph::new(Line::from(header_spans)), header_area);

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
            return self
                .branches
                .iter()
                .enumerate()
                .map(|(i, _)| (i, 0))
                .collect();
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
        results.sort_by_key(|b| std::cmp::Reverse(b.1));
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
                if let Some(ch) = self.input[self.cursor_pos..].chars().next() {
                    self.cursor_pos += ch.len_utf8();
                }
                false
            }
            _ => false,
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        // `cursor_pos` is a byte offset into `self.input` (consistent with
        // `String::insert`, which also takes a byte index). Iterating over
        // `chars()` yields a `char` per Unicode scalar value, and advancing the
        // cursor by `c.len_utf8()` after `String::insert(byte_idx, c)` keeps
        // the offset on a valid char boundary. Multi-codepoint grapheme
        // clusters (e.g., emoji + variation selector) are inserted scalar by
        // scalar, which is acceptable here.
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
    pub diff_cursor: usize,
    pub diff_sel_anchor: Option<usize>,
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
    pub compare_cursor: usize,
    pub compare_sel_anchor: Option<usize>,
    pub compare_diff_error: Option<String>,
    compare_diff_rx: mpsc::Receiver<gitdiff::DiffEvent>,
    compare_diff_tx: mpsc::Sender<gitdiff::DiffEvent>,
    compare_diff_stop_tx: Option<watch::Sender<bool>>,
    compare_diff_task: Option<JoinHandle<()>>,
    // PR detection state
    pub pr_info: Option<gitdiff::PrInfo>,
    pr_detection_rx: mpsc::Receiver<Option<gitdiff::PrInfo>>,
    pr_detection_tx: mpsc::Sender<Option<gitdiff::PrInfo>>,
    pr_detection_task: Option<JoinHandle<()>>,
    // Terminal tab state — multiple shells per agent
    pub terminal_panels: Vec<TerminalPanel>,
    pub current_terminal_idx: usize,
    /// Sub-mode within the Terminal tab. `false` = Navigate (keys are TUI
    /// commands), `true` = Type (keys forwarded to the active PTY). Default is
    /// Navigate so the user is never accidentally typing into a shell they
    /// didn't expect to be in.
    pub terminal_input_focused: bool,
    terminal_cols: u16,
    terminal_rows: u16,
    /// Transient one-line message displayed at the bottom of the terminal
    /// content area. Set when a terminal connection task reports a spawn
    /// failure; cleared when the user dismisses or successfully spawns a new
    /// terminal.
    pub terminal_status_message: Option<String>,
    /// Active tab-completion popup, if any. Tied to the active terminal — only
    /// one popup is open at a time across all terminals.
    pub completion: Option<term_complete::CompletionState>,
}

impl FocusModeState {
    pub fn new() -> Self {
        let (output_tx, output_rx) = mpsc::channel(512);
        let (diff_tx, diff_rx) = mpsc::channel(16);
        let (compare_diff_tx, compare_diff_rx) = mpsc::channel(16);
        let (pr_detection_tx, pr_detection_rx) = mpsc::channel(1);
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
            diff_cursor: 0,
            diff_sel_anchor: None,
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
            compare_cursor: 0,
            compare_sel_anchor: None,
            compare_diff_error: None,
            compare_diff_rx,
            compare_diff_tx,
            compare_diff_stop_tx: None,
            compare_diff_task: None,
            pr_info: None,
            pr_detection_rx,
            pr_detection_tx,
            pr_detection_task: None,
            terminal_panels: Vec::new(),
            current_terminal_idx: 0,
            terminal_input_focused: false,
            terminal_cols: 80,
            terminal_rows: 24,
            terminal_status_message: None,
            completion: None,
        }
    }

    /// Open an agent in focus mode, replacing any existing panel.
    ///
    /// `existing_terminals`, if non-empty, are cached `TerminalPanel`s from a
    /// previous focus-mode session for the same agent. When non-empty, the
    /// shell sessions and their scrollback are reused instead of spawning a
    /// new terminal.
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
        existing_terminals: AgentTerminalCache,
    ) {
        self.close_panel();

        self.panel_cols = cols;
        self.panel_rows = rows;
        self.focus_side = FocusSide::Right;
        self.left_tab = LeftPanelTab::Changes;
        self.diff = None;
        self.diff_scroll = 0;
        self.diff_cursor = 0;
        self.diff_sel_anchor = None;
        self.diff_error = None;
        self.working_dir = Some(working_dir.to_string());
        self.repo_path = repo_path.map(|s| s.to_string());
        self.branch_name = branch_name.map(|s| s.to_string());
        self.compare_picker = BranchPicker::new();
        self.compare_diff = None;
        self.compare_diff_scroll = 0;
        self.compare_cursor = 0;
        self.compare_sel_anchor = None;
        self.compare_diff_error = None;
        self.pr_info = None;

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
            // Focus mode has a single panel and never sorts it, so the
            // creation timestamp is irrelevant here.
            started_at: String::new(),
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
            let diff_handle = gitdiff::spawn_diff_task(working_dir.to_string(), diff_tx, stop_rx);
            self.diff_stop_tx = Some(stop_tx);
            self.diff_task = Some(diff_handle);

            // Spawn one-shot PR detection task
            if branch_name.is_some() {
                let pr_tx = self.pr_detection_tx.clone();
                let pr_handle = gitdiff::spawn_pr_detection_task(working_dir.to_string(), pr_tx);
                self.pr_detection_task = Some(pr_handle);
            }
        }

        // Start (or restore) terminal sessions. Each terminal is linked to
        // this agent so its shell is killed alongside the agent (prevents
        // orphaned dev servers, etc.). If cached panels were handed in, reuse
        // their existing shell sessions and scrollback rather than spawning
        // anew.
        // Default sub-mode is Navigate so the user is never accidentally
        // typing into a shell they didn't expect to be in.
        self.terminal_input_focused = false;
        self.completion = None;
        if existing_terminals.panels.is_empty() {
            self.terminal_panels = Vec::new();
            self.current_terminal_idx = 0;
            self.spawn_new_terminal(working_dir, Some(agent_id.to_string()));
        } else {
            self.install_existing_terminals(existing_terminals);
        }
    }

    /// Install previously-cached terminal panels and refit each to the
    /// current focus-mode dimensions. The background `terminal_connection_task`
    /// for each is already running, so no IPC is needed beyond an optional
    /// resize.
    fn install_existing_terminals(&mut self, mut cache: AgentTerminalCache) {
        // Defensive: drop any half-initialized current panels.
        self.close_all_terminals();

        let cols = self.terminal_cols;
        let rows = self.terminal_rows;
        for panel in &mut cache.panels {
            let needs_resize =
                panel.vterm.cols() != cols as usize || panel.vterm.rows() != rows as usize;
            if needs_resize
                && panel
                    .command_tx
                    .try_send(PanelCommand::Resize { cols, rows })
                    .is_ok()
            {
                panel.vterm.resize(cols as usize, rows as usize);
            }
        }
        let panels_len = cache.panels.len();
        self.terminal_panels = cache.panels;
        self.current_terminal_idx = cache.current_idx.min(panels_len.saturating_sub(1));
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
                AgentOutputEvent::Exited { id, .. } | AgentOutputEvent::ConnectionLost { id } => {
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
                    // Clamp scroll and cursor to new diff length
                    let max = parsed.lines.len().saturating_sub(1);
                    self.diff_scroll = self.diff_scroll.min(max);
                    self.diff_cursor = self.diff_cursor.min(max);
                    self.diff_sel_anchor = None;
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

    /// Shut down the current panel and clean up. Tears down the terminal too.
    pub fn shutdown(&mut self) {
        self.close_panel();
    }

    /// Step out of focus mode while keeping the terminal panels alive.
    /// Returns `(agent_id, AgentTerminalCache)` so the caller can stash the
    /// terminals for reuse next time focus mode opens for the same agent. The
    /// agent panel (a focus-mode-only second connection) and all auxiliary
    /// tasks are torn down as usual.
    pub fn detach(&mut self) -> Option<(String, AgentTerminalCache)> {
        let agent_id = self.panel.as_ref().map(|p| p.id.clone())?;
        self.close_panel_keep_terminal();
        let panels = std::mem::take(&mut self.terminal_panels);
        let current_idx = self.current_terminal_idx;
        self.current_terminal_idx = 0;
        self.terminal_input_focused = false;
        self.completion = None;
        if panels.is_empty() {
            return None;
        }
        Some((
            agent_id,
            AgentTerminalCache {
                panels,
                current_idx,
            },
        ))
    }

    pub fn is_active(&self) -> bool {
        self.panel.is_some()
    }

    fn close_panel(&mut self) {
        self.close_panel_keep_terminal();
        // Stop all terminals
        self.close_all_terminals();
    }

    /// Same as `close_panel` except the terminal panel is left untouched. Used
    /// by `detach` so the caller can take the terminal panel for caching.
    fn close_panel_keep_terminal(&mut self) {
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
        self.diff_cursor = 0;
        self.diff_sel_anchor = None;
        self.diff_error = None;
        self.working_dir = None;
        self.repo_path = None;
        // Stop compare diff task
        self.stop_compare_diff();
        // Stop PR detection task
        if let Some(handle) = self.pr_detection_task.take() {
            handle.abort();
        }
        self.pr_info = None;
        // Clear any transient terminal status banner.
        self.terminal_status_message = None;
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

    // Terminal session management — supports multiple terminals per agent

    /// Spawn a new terminal session, append it to `terminal_panels`, and make
    /// it the active terminal. The new terminal inherits the agent's lifetime
    /// (hub kills it when the agent exits) by passing `agent_id`.
    fn spawn_new_terminal(&mut self, working_dir: &str, agent_id: Option<String>) {
        let cols = self.terminal_cols;
        let rows = self.terminal_rows;

        let (event_tx, event_rx) = mpsc::channel::<TerminalOutputEvent>(512);
        let (command_tx, command_rx) = mpsc::channel::<PanelCommand>(64);
        let wd = working_dir.to_string();

        let handle = tokio::task::spawn(async move {
            terminal_connection_task(wd, cols, rows, agent_id, event_tx, command_rx).await;
        });

        self.terminal_panels.push(TerminalPanel {
            id: String::new(),
            vterm: TerminalEmulator::new(cols as usize, rows as usize),
            command_tx,
            exited: false,
            scroll_offset: 0,
            event_rx,
            task_handle: handle,
            input_buffer: term_complete::InputBuffer::new(),
        });
        self.current_terminal_idx = self.terminal_panels.len() - 1;
        // Clear any stale spawn-failure banner — the user just asked for a
        // fresh attempt. If this attempt also fails, drain_terminal_events
        // will repopulate the message.
        self.terminal_status_message = None;
    }

    /// Add a new terminal session for the currently-open agent. No-op if no
    /// agent is open (call from focus mode only).
    pub fn add_terminal(&mut self) {
        let Some(working_dir) = self.working_dir.clone() else {
            return;
        };
        let agent_id = self.panel.as_ref().map(|p| p.id.clone());
        self.spawn_new_terminal(&working_dir, agent_id);
    }

    /// Close the currently-active terminal. The hub kills its PTY and the
    /// background task is aborted. The active index is clamped to the new
    /// length.
    pub fn close_current_terminal(&mut self) {
        if self.current_terminal_idx >= self.terminal_panels.len() {
            return;
        }
        let panel = self.terminal_panels.remove(self.current_terminal_idx);
        let _ = panel.command_tx.try_send(PanelCommand::Detach);
        panel.task_handle.abort();
        self.completion = None;
        if self.terminal_panels.is_empty() {
            self.current_terminal_idx = 0;
        } else if self.current_terminal_idx >= self.terminal_panels.len() {
            self.current_terminal_idx = self.terminal_panels.len() - 1;
        }
    }

    /// Switch to the next terminal in the list (wraps around).
    pub fn next_terminal(&mut self) {
        if self.terminal_panels.len() <= 1 {
            return;
        }
        self.current_terminal_idx = (self.current_terminal_idx + 1) % self.terminal_panels.len();
        self.completion = None;
    }

    /// Switch to the previous terminal in the list (wraps around).
    pub fn prev_terminal(&mut self) {
        if self.terminal_panels.len() <= 1 {
            return;
        }
        self.current_terminal_idx = if self.current_terminal_idx == 0 {
            self.terminal_panels.len() - 1
        } else {
            self.current_terminal_idx - 1
        };
        self.completion = None;
    }

    /// Switch to the terminal at the given index, if valid.
    pub fn select_terminal(&mut self, idx: usize) {
        if idx < self.terminal_panels.len() {
            self.current_terminal_idx = idx;
            self.completion = None;
        }
    }

    /// Borrow the active terminal panel, if any.
    pub fn current_terminal(&self) -> Option<&TerminalPanel> {
        self.terminal_panels.get(self.current_terminal_idx)
    }

    /// Mutably borrow the active terminal panel, if any.
    pub fn current_terminal_mut(&mut self) -> Option<&mut TerminalPanel> {
        self.terminal_panels.get_mut(self.current_terminal_idx)
    }

    /// Tear down every terminal in the list. Used when the focus-mode panel
    /// is fully closed (not on detach — detach hands ownership out instead).
    fn close_all_terminals(&mut self) {
        for panel in self.terminal_panels.drain(..) {
            let _ = panel.command_tx.try_send(PanelCommand::Detach);
            panel.task_handle.abort();
        }
        self.current_terminal_idx = 0;
        self.terminal_input_focused = false;
        self.completion = None;
    }

    /// Drain terminal output events into the vterm of every live panel so
    /// backgrounded shells keep accumulating scrollback. Cached panels in
    /// `OverviewState::agent_terminals` drain their own events via
    /// `OverviewState::drain_cached_terminal_events`.
    ///
    /// Any spawn-failure message reported by a connection task is surfaced
    /// onto `terminal_status_message` so the UI can show a one-line notice.
    pub fn drain_terminal_events(&mut self) {
        for panel in &mut self.terminal_panels {
            if let Some(msg) = panel.drain_events() {
                self.terminal_status_message = Some(format!("Failed to spawn terminal: {}", msg));
            }
        }
    }

    /// Send input bytes to the active terminal.
    pub fn send_terminal_input(&self, data: Vec<u8>) {
        if let Some(panel) = self.current_terminal() {
            let _ = panel.command_tx.try_send(PanelCommand::Input(data));
        }
    }

    /// Single entry point for keystrokes that arrive while the Terminal tab is
    /// focused in Type sub-mode. Owns three concerns:
    ///   1. Driving the completion popup when one is open.
    ///   2. Intercepting plain Tab to start a new completion.
    ///   3. Tracking the local input buffer used by completion, then
    ///      forwarding the key to the PTY (the existing behaviour).
    pub fn handle_terminal_type_key(&mut self, key: &KeyEvent) {
        // 1. Popup is open — give it first crack at the key.
        if self.completion.is_some() {
            match key.code {
                KeyCode::Up => {
                    if let Some(c) = self.completion.as_mut() {
                        c.move_up();
                    }
                    return;
                }
                KeyCode::Down => {
                    if let Some(c) = self.completion.as_mut() {
                        c.move_down();
                    }
                    return;
                }
                KeyCode::Tab | KeyCode::Enter => {
                    self.accept_completion();
                    return;
                }
                KeyCode::Esc => {
                    self.completion = None;
                    return;
                }
                _ => {
                    // Any other key dismisses the popup and falls through to
                    // normal handling — so the shell sees the keystroke and
                    // the user can re-press Tab once they've typed more.
                    self.completion = None;
                }
            }
        }

        // 2. Plain Tab — try TUI-level completion. Modifier+Tab variants are
        //    forwarded as-is so existing shell bindings (e.g. shift-tab as
        //    BackTab → \x1b[Z) keep working.
        let is_plain_tab = matches!(key.code, KeyCode::Tab)
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        if is_plain_tab && self.try_start_completion() {
            return;
        }

        // 3. Track the buffer (best-effort) and forward bytes to the PTY.
        self.update_input_buffer(key);
        if key.code == KeyCode::Esc {
            self.send_terminal_input(vec![0x1b]);
        } else if let Some(bytes) = input::key_event_to_bytes(key) {
            self.send_terminal_input(bytes);
        }
    }

    /// Update the local input buffer in response to a keystroke. Reset on
    /// command boundaries (Enter, Ctrl+C, Ctrl+U, Ctrl+G); append on printable
    /// characters; pop on Backspace. Other keys (arrows, F-keys, Ctrl+letter
    /// other than the resets) are deliberately ignored — tracking them
    /// faithfully would require shadowing the shell's full line editor.
    fn update_input_buffer(&mut self, key: &KeyEvent) {
        let Some(panel) = self.terminal_panels.get_mut(self.current_terminal_idx) else {
            return;
        };
        if matches!(key.code, KeyCode::Enter) {
            panel.input_buffer.clear();
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char(c) = key.code {
                let lower = c.to_ascii_lowercase();
                if matches!(lower, 'c' | 'u' | 'g') {
                    panel.input_buffer.clear();
                }
            }
            return;
        }
        match key.code {
            KeyCode::Char(c) => panel.input_buffer.push_char(c),
            KeyCode::Backspace => panel.input_buffer.pop_char(),
            _ => {}
        }
    }

    /// Compute completions for the active terminal's buffer. Returns `true`
    /// when the keystroke was consumed (single-match inserted inline, or
    /// multi-match popup opened).
    fn try_start_completion(&mut self) -> bool {
        let Some(panel) = self.terminal_panels.get(self.current_terminal_idx) else {
            return false;
        };
        if panel.input_buffer.is_empty() {
            return false;
        }
        let buffer = panel.input_buffer.as_str().to_string();
        let working_dir = self.working_dir.clone().unwrap_or_else(|| ".".to_string());
        let Some(result) = term_complete::compute_completions(&buffer, &working_dir) else {
            return false;
        };

        if result.items.len() == 1 {
            let item = result.items[0].clone();
            self.insert_completion(&result.prefix, &item);
            return true;
        }

        self.completion = Some(term_complete::CompletionState {
            prefix: result.prefix,
            items: result.items,
            selected: 0,
            scroll: 0,
        });
        true
    }

    fn accept_completion(&mut self) {
        let Some(state) = self.completion.take() else {
            return;
        };
        let Some(item) = state.items.get(state.selected).cloned() else {
            return;
        };
        self.insert_completion(&state.prefix, &item);
    }

    /// Send the bytes the shell needs to grow `prefix` into `item.display`
    /// (plus a trailing `/` for directories or space for files/commands), and
    /// mirror those bytes into the local input buffer so the next Tab can
    /// continue from the new state.
    fn insert_completion(&mut self, prefix: &str, item: &term_complete::CompletionItem) {
        if !item.display.starts_with(prefix) {
            // Defensive — `compute_completions` always returns items whose
            // display starts with the prefix, but if that ever drifts we
            // refuse to send junk to the shell.
            return;
        }
        let suffix = &item.display[prefix.len()..];
        let trailing: char = match item.kind {
            term_complete::CompletionKind::Directory => '/',
            term_complete::CompletionKind::File | term_complete::CompletionKind::Command => ' ',
        };

        if let Some(panel) = self.terminal_panels.get_mut(self.current_terminal_idx) {
            panel.input_buffer.push_str(suffix);
            panel.input_buffer.push_char(trailing);
        }

        let mut bytes: Vec<u8> = suffix.as_bytes().to_vec();
        bytes.push(trailing as u8);
        self.send_terminal_input(bytes);
    }

    /// Handle terminal panel resize. Resizes every panel so the user sees
    /// consistent geometry whichever terminal they switch to.
    pub fn handle_terminal_resize(&mut self, cols: u16, rows: u16) {
        if cols == self.terminal_cols && rows == self.terminal_rows {
            return;
        }
        self.terminal_cols = cols;
        self.terminal_rows = rows;
        for panel in &mut self.terminal_panels {
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
                    self.compare_cursor = self.compare_cursor.min(max);
                    self.compare_sel_anchor = None;
                    self.compare_diff = Some(parsed);
                    self.compare_diff_error = None;
                }
                gitdiff::DiffEvent::Error(msg) => {
                    self.compare_diff_error = Some(msg);
                }
            }
        }
    }

    /// Drain PR detection result and auto-open compare tab if a PR is found.
    pub fn drain_pr_events(&mut self) {
        let Ok(result) = self.pr_detection_rx.try_recv() else {
            return;
        };

        let Some(pr) = result else {
            return;
        };

        self.pr_info = Some(pr.clone());

        // Resolve the base branch name for git diff
        let resolved = self.resolve_branch_for_compare(&pr.base_branch);

        // Auto-configure the compare picker
        self.compare_picker.selected_branch = Some(resolved);
        self.compare_picker.mode = BranchPickerMode::Selected;

        // Only auto-switch to Compare if user hasn't navigated away from the default tab
        if self.left_tab == LeftPanelTab::Changes {
            self.left_tab = LeftPanelTab::Compare;
        }

        self.start_compare_diff();
    }

    /// Resolve a short branch name (e.g. "main") to the best available git ref.
    fn resolve_branch_for_compare(&self, short_name: &str) -> String {
        // Check if the short name exists as a local branch
        let has_local = self
            .compare_picker
            .branches
            .iter()
            .any(|b| !b.is_remote && b.name == short_name);
        if has_local {
            return short_name.to_string();
        }

        // Check for origin/<name> in the branch list
        let remote_name = format!("origin/{short_name}");
        let has_remote = self
            .compare_picker
            .branches
            .iter()
            .any(|b| b.name == remote_name);
        if has_remote {
            return remote_name;
        }

        // Fallback: let git resolve the ref
        short_name.to_string()
    }

    /// Update the branch picker's branch list from the current repos data.
    pub fn update_compare_branches(&mut self, repos: &[RepoInfo]) {
        let branches = self
            .repo_path
            .as_ref()
            .and_then(|rp| repos.iter().find(|r| r.path == *rp))
            .map(|r| {
                let mut all = r.local_branches.clone();
                all.extend(r.remote_branches.clone());
                all
            })
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

    // -- Diff cursor / selection (Changes tab) --

    /// Move diff cursor up, auto-scrolling the viewport if needed.
    pub fn diff_cursor_up(&mut self, viewport_height: usize) {
        self.diff_cursor = self.diff_cursor.saturating_sub(1);
        self.diff_ensure_cursor_visible(viewport_height);
    }

    /// Move diff cursor down, auto-scrolling the viewport if needed.
    pub fn diff_cursor_down(&mut self, viewport_height: usize) {
        if let Some(diff) = &self.diff {
            let max = diff.lines.len().saturating_sub(1);
            self.diff_cursor = (self.diff_cursor + 1).min(max);
        }
        self.diff_ensure_cursor_visible(viewport_height);
    }

    fn diff_ensure_cursor_visible(&mut self, viewport_height: usize) {
        if viewport_height == 0 {
            return;
        }
        if self.diff_cursor < self.diff_scroll {
            self.diff_scroll = self.diff_cursor;
        } else if self.diff_cursor >= self.diff_scroll + viewport_height {
            self.diff_scroll = self.diff_cursor.saturating_sub(viewport_height - 1);
        }
    }

    /// Toggle selection anchor at the current cursor position.
    pub fn diff_toggle_anchor(&mut self) {
        if self.diff_sel_anchor.is_some() {
            self.diff_sel_anchor = None;
        } else {
            self.diff_sel_anchor = Some(self.diff_cursor);
        }
    }

    pub fn diff_cancel_selection(&mut self) {
        self.diff_sel_anchor = None;
    }

    /// Build text from the current diff selection and send it to the agent terminal.
    pub fn diff_send_selection(&self) {
        let diff = match &self.diff {
            Some(d) if !d.lines.is_empty() => d,
            _ => return,
        };
        let text = build_selection_text(diff, self.diff_cursor, self.diff_sel_anchor);
        if !text.is_empty() {
            self.send_input(text.into_bytes());
        }
    }

    #[allow(dead_code)]
    pub fn diff_has_selection(&self) -> bool {
        self.diff_sel_anchor.is_some()
    }

    // -- Compare cursor / selection (Compare tab) --

    /// Move compare cursor up, auto-scrolling the viewport if needed.
    pub fn compare_cursor_up(&mut self, viewport_height: usize) {
        self.compare_cursor = self.compare_cursor.saturating_sub(1);
        self.compare_ensure_cursor_visible(viewport_height);
    }

    /// Move compare cursor down, auto-scrolling the viewport if needed.
    pub fn compare_cursor_down(&mut self, viewport_height: usize) {
        if let Some(diff) = &self.compare_diff {
            let max = diff.lines.len().saturating_sub(1);
            self.compare_cursor = (self.compare_cursor + 1).min(max);
        }
        self.compare_ensure_cursor_visible(viewport_height);
    }

    fn compare_ensure_cursor_visible(&mut self, viewport_height: usize) {
        if viewport_height == 0 {
            return;
        }
        if self.compare_cursor < self.compare_diff_scroll {
            self.compare_diff_scroll = self.compare_cursor;
        } else if self.compare_cursor >= self.compare_diff_scroll + viewport_height {
            self.compare_diff_scroll = self.compare_cursor.saturating_sub(viewport_height - 1);
        }
    }

    pub fn compare_toggle_anchor(&mut self) {
        if self.compare_sel_anchor.is_some() {
            self.compare_sel_anchor = None;
        } else {
            self.compare_sel_anchor = Some(self.compare_cursor);
        }
    }

    pub fn compare_cancel_selection(&mut self) {
        self.compare_sel_anchor = None;
    }

    pub fn compare_send_selection(&self) {
        let diff = match &self.compare_diff {
            Some(d) if !d.lines.is_empty() => d,
            _ => return,
        };
        let text = build_selection_text(diff, self.compare_cursor, self.compare_sel_anchor);
        if !text.is_empty() {
            self.send_input(text.into_bytes());
        }
    }

    pub fn compare_has_selection(&self) -> bool {
        self.compare_sel_anchor.is_some()
    }
}

/// Build text payload from a diff selection range (or single cursor line).
fn build_selection_text(
    diff: &gitdiff::ParsedDiff,
    cursor: usize,
    anchor: Option<usize>,
) -> String {
    let max_idx = diff.lines.len().saturating_sub(1);
    let (lo, hi) = match anchor {
        Some(a) => (a.min(cursor).min(max_idx), a.max(cursor).min(max_idx)),
        None => (cursor.min(max_idx), cursor.min(max_idx)),
    };

    let mut out = String::new();
    let mut last_file_idx: Option<usize> = None;

    for line in &diff.lines[lo..=hi] {
        match line.kind {
            gitdiff::DiffLineKind::Add
            | gitdiff::DiffLineKind::Delete
            | gitdiff::DiffLineKind::Context => {
                if last_file_idx != Some(line.file_idx) {
                    if let Some(name) = diff.file_names.get(line.file_idx) {
                        out.push_str("# file: ");
                        out.push_str(name);
                        out.push('\n');
                    }
                    last_file_idx = Some(line.file_idx);
                }
                let prefix = match line.kind {
                    gitdiff::DiffLineKind::Add => '+',
                    gitdiff::DiffLineKind::Delete => '-',
                    _ => ' ',
                };
                out.push(prefix);
                out.push_str(&line.content);
                out.push('\n');
            }
            _ => {} // skip structural lines
        }
    }
    out
}

/// Render the focus mode view: 60% left panel with tabs, 40% agent panel right.
pub fn render_focus_mode(
    frame: &mut Frame,
    area: Rect,
    state: &mut FocusModeState,
    click_map: &mut ClickMap,
    repo_colors: &HashMap<String, String>,
) {
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(area);

    click_map.focus_left_area = left_area;
    click_map.focus_right_area = right_area;

    let panel_color = state
        .repo_path
        .as_ref()
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
            if let Some(content_area) = render_agent_panel(
                frame,
                right_area,
                panel,
                right_focused,
                true,
                panel_color,
            ) {
                click_map.focus_right_content_area = content_area;
            }
        }
        None => render_empty_state(frame, right_area),
    }
}

fn render_left_panel(
    frame: &mut Frame,
    area: Rect,
    state: &mut FocusModeState,
    click_map: &mut ClickMap,
    repo_color: Option<Color>,
) {
    if area.height < 2 {
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::R_BG_BASE)),
            area,
        );
        return;
    }

    let [tab_area, content_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);

    // Render tab bar
    let left_focused = state.focus_side == FocusSide::Left;
    let terminal_sub_mode = if state.terminal_input_focused {
        Some(TerminalSubMode::Type)
    } else {
        Some(TerminalSubMode::Navigate)
    };
    render_left_tab_bar(
        frame,
        tab_area,
        state.left_tab,
        left_focused,
        click_map,
        state.pr_info.as_ref(),
        terminal_sub_mode,
    );

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
            if left_focused {
                Some(state.diff_cursor)
            } else {
                None
            },
            state.diff_sel_anchor,
        ),
        LeftPanelTab::Compare => render_compare_tab(frame, content_area, state, repo_color),
        LeftPanelTab::Terminal => {
            render_terminal_tab(frame, content_area, state, click_map);
        }
    }
}

/// Visual sub-mode within the Terminal tab — used by the tab bar to show
/// `· type` / `· nav` when the Terminal tab is active.
#[derive(Clone, Copy, Debug, PartialEq)]
enum TerminalSubMode {
    Navigate,
    Type,
}

fn render_terminal_tab(
    frame: &mut Frame,
    area: Rect,
    state: &mut FocusModeState,
    click_map: &mut ClickMap,
) {
    // Reset click regions for this frame.
    click_map.focus_terminal_labels.clear();
    click_map.focus_terminal_new_button = Rect::default();
    click_map.focus_terminal_content_area = Rect::default();

    let panel_focused = state.focus_side == FocusSide::Left;
    let in_type_mode = state.terminal_input_focused;

    // Split into label strip (1 row) + content + optional status row.
    if area.height < 1 {
        return;
    }
    let has_status = state.terminal_status_message.is_some() && area.height >= 3;
    let (label_area, content_area, status_area) = if has_status {
        let [label_area, content_area, status_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);
        (label_area, content_area, Some(status_area))
    } else {
        let [label_area, content_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        (label_area, content_area, None)
    };

    render_terminal_label_strip(
        frame,
        label_area,
        &state.terminal_panels,
        state.current_terminal_idx,
        panel_focused,
        click_map,
    );

    // Paint the status line first (if any), so subsequent early returns from
    // the content-area branches don't skip it.
    if let (Some(area), Some(msg)) = (status_area, state.terminal_status_message.as_deref()) {
        let status_line = Paragraph::new(Line::from(Span::styled(
            format!(" {} ", msg),
            Style::default().fg(theme::R_ERROR).bg(theme::R_BG_SURFACE),
        )))
        .style(Style::default().bg(theme::R_BG_SURFACE));
        frame.render_widget(status_line, area);
    }

    click_map.focus_terminal_content_area = content_area;

    let panels_len = state.terminal_panels.len();
    let current_idx = state.current_terminal_idx;
    if panels_len == 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "No terminals — press n (or click [+]) to start one",
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )))
            .alignment(ratatui::layout::Alignment::Center)
            .style(Style::default().bg(theme::R_BG_BASE)),
            content_area,
        );
        return;
    }

    let panel = &mut state.terminal_panels[current_idx];
    if panel.exited {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Terminal session ended — press x to close, n to start a new one",
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )))
            .alignment(ratatui::layout::Alignment::Center)
            .style(Style::default().bg(theme::R_BG_BASE)),
            content_area,
        );
        return;
    }

    let lines = if panel.scroll_offset > 0 {
        panel.vterm.to_ratatui_lines_scrolled(panel.scroll_offset)
    } else {
        panel.vterm.to_ratatui_lines()
    };
    let paragraph = Paragraph::new(lines).style(Style::default().bg(theme::R_BG_BASE));
    frame.render_widget(paragraph, content_area);

    // Show the hardware cursor only when the terminal is the active input
    // target — i.e., focus_side == Left AND sub-mode == Type. In Navigate
    // mode the cursor is hidden so the user has a clear visual cue that
    // typing won't reach the shell.
    let cursor_pos = if panel_focused
        && in_type_mode
        && panel.scroll_offset == 0
        && !panel.vterm.hide_cursor()
    {
        let (cursor_row, cursor_col) = panel.vterm.cursor_position();
        let x = content_area.x + cursor_col;
        let y = content_area.y + cursor_row;
        if x < content_area.x + content_area.width && y < content_area.y + content_area.height {
            frame.set_cursor_position(Position { x, y });
            Some((x, y))
        } else {
            None
        }
    } else {
        None
    };

    // Render the tab-completion popup, if any, anchored near the shell cursor.
    if let (Some(completion), Some((cx, cy))) = (state.completion.as_ref(), cursor_pos) {
        render_completion_popup(frame, content_area, cx, cy, completion);
    }
}

/// Draw the tab-completion popup as a small bordered list near the shell
/// cursor. Placed above the cursor when there's room above; otherwise below.
fn render_completion_popup(
    frame: &mut Frame,
    area: Rect,
    cursor_x: u16,
    cursor_y: u16,
    state: &term_complete::CompletionState,
) {
    if state.items.is_empty() {
        return;
    }

    let max_display_width: u16 = state
        .items
        .iter()
        .map(|i| i.display.chars().count() as u16)
        .max()
        .unwrap_or(0);
    // +2 for the side borders, +2 for left/right padding.
    let desired_width = max_display_width.saturating_add(4).max(10);
    let popup_width = desired_width.min(area.width.max(1));

    let visible_rows = state
        .items
        .len()
        .min(term_complete::POPUP_VISIBLE_ROWS) as u16;
    // +2 for top/bottom borders.
    let popup_height = visible_rows.saturating_add(2).min(area.height.max(1));

    // Prefer placing above the cursor row so the user can still see what they
    // typed. Fall back to below if the cursor is too close to the top of the
    // panel.
    let above_room = cursor_y.saturating_sub(area.y);
    let popup_y = if above_room >= popup_height {
        cursor_y.saturating_sub(popup_height)
    } else {
        let candidate = cursor_y.saturating_add(1);
        let max_y = area.y + area.height.saturating_sub(popup_height);
        candidate.min(max_y)
    };

    // Anchor to the cursor column, but never let the popup spill off the right
    // edge of the panel.
    let max_x = area.x + area.width.saturating_sub(popup_width);
    let popup_x = cursor_x.min(max_x).max(area.x);

    let popup_rect = Rect {
        x: popup_x,
        y: popup_y,
        width: popup_width,
        height: popup_height,
    };

    // Build the visible slice with the selected row highlighted.
    let visible_items: Vec<Line> = state
        .items
        .iter()
        .enumerate()
        .skip(state.scroll)
        .take(term_complete::POPUP_VISIBLE_ROWS)
        .map(|(abs_idx, item)| {
            let style = if abs_idx == state.selected {
                Style::default()
                    .bg(theme::R_SELECTION_BG)
                    .fg(theme::R_TEXT_PRIMARY)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .bg(theme::R_BG_SURFACE)
                    .fg(theme::R_TEXT_SECONDARY)
            };
            Line::from(Span::styled(format!(" {} ", item.display), style))
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .style(
            Style::default()
                .bg(theme::R_BG_SURFACE)
                .fg(theme::R_TEXT_TERTIARY),
        );
    let para = Paragraph::new(visible_items)
        .block(block)
        .style(Style::default().bg(theme::R_BG_SURFACE));

    // Clear the area first so we don't see terminal contents bleeding through.
    frame.render_widget(ratatui::widgets::Clear, popup_rect);
    frame.render_widget(para, popup_rect);
}

/// Render the per-terminal label strip: `[1] [2*] [3]    [+]`. The active
/// terminal is shown with the strong overlay background; idle terminals use
/// the surface background. The `[+]` affordance always appears at the right.
fn render_terminal_label_strip(
    frame: &mut Frame,
    area: Rect,
    panels: &[TerminalPanel],
    current_idx: usize,
    panel_focused: bool,
    click_map: &mut ClickMap,
) {
    let mut spans: Vec<Span> = Vec::new();
    let mut cursor_x = area.x;

    // Reserve room for the right-aligned [+] button so labels never crowd it.
    let plus_label = " + ";
    let plus_width = plus_label.chars().count() as u16;
    let area_end = area.x.saturating_add(area.width);
    // Stop registering label rects once the next one would overflow into the
    // [+] reservation. We always leave room for the [+] (3 cols) plus a single
    // ellipsis cell, so widths below 4 truncate everything.
    let labels_budget_end = area_end.saturating_sub(plus_width);

    let mut truncated = false;

    for (idx, panel) in panels.iter().enumerate() {
        let is_active = idx == current_idx;
        let label = if panel.exited {
            format!(" {}× ", idx + 1)
        } else if is_active {
            format!(" {}* ", idx + 1)
        } else {
            format!(" {} ", idx + 1)
        };
        let (fg, bg) = if is_active && panel_focused {
            (theme::R_TEXT_PRIMARY, theme::R_BG_OVERLAY)
        } else if is_active {
            (theme::R_TEXT_SECONDARY, theme::R_BG_RAISED)
        } else {
            (theme::R_TEXT_TERTIARY, theme::R_BG_SURFACE)
        };
        let style = if is_active {
            Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(fg).bg(bg)
        };
        let label_width = label.chars().count() as u16;

        // If this label would extend past the reserved budget, stop adding
        // labels and show a one-cell ellipsis instead. Click rects beyond the
        // visible region must not be registered or clicks land on hidden cells.
        if cursor_x.saturating_add(label_width) > labels_budget_end {
            truncated = true;
            break;
        }

        // Render the visual label even when the panel id is empty, but skip
        // the click rect so the user can't switch into a panel that hasn't
        // received its hub-assigned id yet.
        if !panel.id.is_empty() {
            click_map.focus_terminal_labels.push((
                Rect {
                    x: cursor_x,
                    y: area.y,
                    width: label_width,
                    height: 1,
                },
                idx,
            ));
        }
        spans.push(Span::styled(label, style));
        cursor_x += label_width;

        // separator between labels (small gap), only if both the separator
        // and at least one more cell of label can still fit.
        if idx + 1 < panels.len() && cursor_x < labels_budget_end {
            spans.push(Span::styled(" ", Style::default().bg(theme::R_BG_SURFACE)));
            cursor_x += 1;
        }
    }

    if truncated && cursor_x < labels_budget_end {
        spans.push(Span::styled(
            "…",
            Style::default()
                .fg(theme::R_TEXT_TERTIARY)
                .bg(theme::R_BG_SURFACE),
        ));
        cursor_x += 1;
    }

    // Spacer up to the right-aligned [+] button. Only emit the [+] (and its
    // click rect) if it fits inside `area.width`.
    let plus_fits = cursor_x.saturating_add(plus_width) <= area_end;
    if plus_fits {
        let gap = area_end - cursor_x - plus_width;
        if gap > 0 {
            spans.push(Span::styled(
                " ".repeat(gap as usize),
                Style::default().bg(theme::R_BG_SURFACE),
            ));
            cursor_x += gap;
        }
        let plus_style = Style::default()
            .fg(theme::R_TEXT_SECONDARY)
            .bg(theme::R_BG_RAISED)
            .add_modifier(Modifier::BOLD);
        spans.push(Span::styled(plus_label, plus_style));
        click_map.focus_terminal_new_button = Rect {
            x: cursor_x,
            y: area.y,
            width: plus_width,
            height: 1,
        };
    } else {
        // No room for [+] — leave the click rect empty so misclicks don't
        // land on a hidden button.
        click_map.focus_terminal_new_button = Rect::default();
    }

    let para = Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::R_BG_SURFACE));
    frame.render_widget(para, area);
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
    pr_info: Option<&gitdiff::PrInfo>,
    terminal_sub_mode: Option<TerminalSubMode>,
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
            Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(fg).bg(bg)
        };

        let label = format!(" {} ", tab.label());

        // Add PR indicator after the Compare tab label
        if let Some(pr) = pr_info.filter(|_| *tab == LeftPanelTab::Compare) {
            let pr_label = format!("PR #{} ", pr.number);

            let label_width = label.chars().count() as u16;
            let pr_width = pr_label.chars().count() as u16;
            let total_width = label_width + pr_width;

            click_map.focus_left_tabs.push((
                Rect {
                    x: cursor_x,
                    y: area.y,
                    width: total_width,
                    height: 1,
                },
                *tab,
            ));

            spans.push(Span::styled(label, style));
            let pr_style = if is_active {
                Style::default()
                    .fg(theme::R_INFO)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::R_INFO).bg(bg)
            };
            spans.push(Span::styled(pr_label, pr_style));
            cursor_x += total_width;
        } else if *tab == LeftPanelTab::Terminal && is_active {
            // When the Terminal tab is active, append a sub-mode hint so the
            // user can see at a glance whether typing reaches the shell.
            let sub_label = match terminal_sub_mode {
                Some(TerminalSubMode::Type) => " · type ",
                _ => " · nav ",
            };
            let label_width = label.chars().count() as u16;
            let sub_width = sub_label.chars().count() as u16;
            let total_width = label_width + sub_width;
            click_map.focus_left_tabs.push((
                Rect {
                    x: cursor_x,
                    y: area.y,
                    width: total_width,
                    height: 1,
                },
                *tab,
            ));
            spans.push(Span::styled(label, style));
            let sub_fg = match terminal_sub_mode {
                Some(TerminalSubMode::Type) => theme::R_INFO,
                _ => theme::R_TEXT_TERTIARY,
            };
            let sub_style = Style::default()
                .fg(sub_fg)
                .bg(bg)
                .add_modifier(Modifier::BOLD);
            spans.push(Span::styled(sub_label, sub_style));
            cursor_x += total_width;
        } else {
            let label_width = label.chars().count() as u16;
            click_map.focus_left_tabs.push((
                Rect {
                    x: cursor_x,
                    y: area.y,
                    width: label_width,
                    height: 1,
                },
                *tab,
            ));
            cursor_x += label_width;
            spans.push(Span::styled(label, style));
        }
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

/// Split a list of styled spans into rows of at most `max_width` characters.
/// Preserves styles across wrap boundaries by splitting individual spans as needed.
fn wrap_spans(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Vec<Span<'static>>> {
    // When the available width is zero (e.g., the diff body is narrower than
    // the gutter) there is no room to render content. Return a single empty
    // row so callers iterate exactly once and do not enter a non-progressing
    // wrap loop.
    if max_width == 0 {
        return vec![Vec::new()];
    }

    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current_row: Vec<Span<'static>> = Vec::new();
    let mut current_width: usize = 0;

    for span in spans {
        let style = span.style;
        let full: &str = &span.content;
        let mut remaining = full;

        while !remaining.is_empty() {
            let budget = max_width.saturating_sub(current_width);
            let take_chars = remaining.chars().count().min(budget);

            if take_chars == 0 {
                // No budget on the current row — flush it and start fresh on
                // the next pass. Without this guard `byte_end` would resolve
                // to 0 and `remaining` would never advance.
                rows.push(std::mem::take(&mut current_row));
                current_width = 0;
                continue;
            }

            let byte_end = remaining
                .char_indices()
                .nth(take_chars)
                .map(|(i, _)| i)
                .unwrap_or(remaining.len());

            let (chunk, rest) = remaining.split_at(byte_end);
            if !chunk.is_empty() {
                current_row.push(Span::styled(chunk.to_string(), style));
                current_width += take_chars;
            }
            remaining = rest;

            if current_width >= max_width && !remaining.is_empty() {
                rows.push(std::mem::take(&mut current_row));
                current_width = 0;
            }
        }
    }

    rows.push(current_row);
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn render_diff_viewer(
    frame: &mut Frame,
    area: Rect,
    diff: Option<&gitdiff::ParsedDiff>,
    scroll: usize,
    error: Option<&str>,
    empty_message: &str,
    repo_color: Option<Color>,
    cursor: Option<usize>,
    sel_anchor: Option<usize>,
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

    // Split area for hint bar when cursor is active
    let (body_area, hint_area) = if cursor.is_some() && area.height >= 2 {
        let [body, hint] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        (body, Some(hint))
    } else {
        (area, None)
    };

    let visible_height = body_area.height as usize;

    // Gutter width: "  old | new │" = 4+4+1 = 9 chars
    let gutter_width: u16 = 9;
    let content_width = body_area.width.saturating_sub(gutter_width);

    // Compute selection range for highlight checks
    let sel_range = match (cursor, sel_anchor) {
        (Some(c), Some(a)) => Some((a.min(c), a.max(c))),
        _ => None,
    };

    let mut lines = Vec::with_capacity(visible_height);
    let mut diff_idx = scroll;

    while lines.len() < visible_height && diff_idx < diff.lines.len() {
        let diff_line = &diff.lines[diff_idx];
        let abs_idx = diff_idx;
        diff_idx += 1;

        // Determine if this line is the cursor or part of a selection
        let is_cursor = cursor == Some(abs_idx);
        let is_selected = sel_range
            .map(|(lo, hi)| abs_idx >= lo && abs_idx <= hi)
            .unwrap_or(false);

        // Separator: blank line between files
        if diff_line.kind == gitdiff::DiffLineKind::Separator {
            let sep_bg = if is_cursor {
                theme::R_BG_ACTIVE
            } else if is_selected {
                theme::R_SELECTION_BG
            } else {
                theme::R_BG_BASE
            };
            lines.push(Line::from(Span::styled(
                " ".repeat(body_area.width as usize),
                Style::default().bg(sep_bg),
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

        // Override background for cursor/selection
        let row_bg = if is_cursor {
            theme::R_BG_ACTIVE
        } else if is_selected {
            theme::R_SELECTION_BG
        } else {
            line_bg
        };

        let gutter_style = Style::default().fg(theme::R_TEXT_TERTIARY).bg(row_bg);
        let sep_style = Style::default().fg(theme::R_TEXT_DISABLED).bg(row_bg);
        let content_style = if diff_line.kind == gitdiff::DiffLineKind::FileHeader {
            Style::default()
                .fg(content_fg)
                .bg(row_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(content_fg).bg(row_bg)
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
            diff.file_names
                .get(diff_line.file_idx)
                .map(|s| format!(" {s}"))
                .unwrap_or_else(|| diff_line.content.clone())
        } else {
            diff_line.content.clone()
        };

        // Syntax-highlight code lines (Add/Delete/Context); others keep plain styling
        let content_spans = match diff_line.kind {
            gitdiff::DiffLineKind::Add
            | gitdiff::DiffLineKind::Delete
            | gitdiff::DiffLineKind::Context => {
                let file_name = diff.file_names.get(diff_line.file_idx).map(|s| s.as_str());
                let file_syntax = file_name.and_then(syntax::syntax_for_file);
                match file_syntax {
                    Some(syn) => {
                        let spans =
                            syntax::highlight_line(&display_content, syn, row_bg, content_fg);
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

        // Wrap content spans into visual rows that fit within content_width
        let wrapped_rows = wrap_spans(content_spans, content_width as usize);

        for (row_i, row_spans) in wrapped_rows.into_iter().enumerate() {
            if lines.len() >= visible_height {
                break;
            }

            // First row: real gutter with line numbers; continuation rows: blank gutter
            let (gutter_for_row, sep_for_row) = if row_i == 0 {
                (gutter_text.clone(), separator)
            } else {
                ("        ".to_string(), " ")
            };

            let mut spans = vec![
                Span::styled(gutter_for_row, gutter_style),
                Span::styled(sep_for_row, sep_style),
            ];

            let row_content_chars: usize =
                row_spans.iter().map(|s| s.content.chars().count()).sum();
            spans.extend(row_spans);

            let pad = (content_width as usize).saturating_sub(row_content_chars);
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), content_style));
            }

            lines.push(Line::from(spans));
        }
    }

    // Fill remaining visible area with empty lines
    for _ in lines.len()..visible_height {
        lines.push(Line::from(Span::styled(
            " ".repeat(body_area.width as usize),
            bg_style,
        )));
    }

    frame.render_widget(Paragraph::new(lines), body_area);

    // Render hint bar when cursor is active
    if let Some(hint_area) = hint_area {
        let hint_text = if sel_anchor.is_some() {
            " \u{2191}\u{2193} extend  Enter send  Esc cancel"
        } else {
            " v select  Enter send  Esc cancel"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint_text,
                Style::default()
                    .fg(theme::R_TEXT_TERTIARY)
                    .bg(theme::R_BG_SURFACE),
            ))),
            hint_area,
        );
    }
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
            let [label_area, diff_area] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);

            render_compare_label(frame, label_area, &state.compare_picker);

            if state.compare_picker.selected_branch.is_some() {
                let left_focused = state.focus_side == FocusSide::Left;
                render_diff_viewer(
                    frame,
                    diff_area,
                    state.compare_diff.as_ref(),
                    state.compare_diff_scroll,
                    state.compare_diff_error.as_deref(),
                    "No differences",
                    repo_color,
                    if left_focused {
                        Some(state.compare_cursor)
                    } else {
                        None
                    },
                    state.compare_sel_anchor,
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
        Some(branch) => (format!(" Comparing: {branch}"), " [Enter] change "),
        None => (" No branch selected".to_string(), " [Enter] select "),
    };

    let hint_width = hint.chars().count();
    let label_width = (area.width as usize).saturating_sub(hint_width);

    // Truncate label if needed
    let display_label: String = if label.chars().count() > label_width {
        label
            .chars()
            .take(label_width.saturating_sub(1))
            .collect::<String>()
            + "…"
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
            if branch.is_remote {
                spans.push(Span::styled(
                    " [remote]",
                    Style::default().fg(theme::R_TEXT_TERTIARY),
                ));
            }
            if branch.is_head {
                spans.push(Span::styled(" HEAD", Style::default().fg(theme::R_SUCCESS)));
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
                        if branch.active_agent_count == 1 {
                            ""
                        } else {
                            "s"
                        }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn make_key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Repro for the panic at the end of an ASCII input — `KeyCode::Right`
    /// previously called `.unwrap()` on `chars().next()` after only a
    /// byte-position guard, which exploded when `cursor_pos == input.len()`.
    #[test]
    fn branch_picker_right_at_end_does_not_panic() {
        let mut picker = BranchPicker::new();
        picker.enter_search();
        picker.handle_key(make_key(KeyCode::Char('a')));
        picker.handle_key(make_key(KeyCode::Char('b')));
        // cursor_pos == input.len() (== 2). Right should be a no-op.
        let cursor_before = picker.cursor_pos;
        picker.handle_key(make_key(KeyCode::Right));
        assert_eq!(picker.cursor_pos, cursor_before);
        assert_eq!(picker.input, "ab");
    }

    /// Multi-byte chars in the input must not corrupt `cursor_pos`. Walk Left
    /// then Right across an emoji and verify we always land on a char
    /// boundary (i.e., the `String` slicing in subsequent operations is
    /// valid).
    #[test]
    fn branch_picker_left_right_handles_multibyte() {
        let mut picker = BranchPicker::new();
        picker.enter_search();
        // 'a' (1 byte) + 'é' (2 bytes) + 'a' (1 byte) = 4-byte string.
        for c in "aéa".chars() {
            picker.handle_key(make_key(KeyCode::Char(c)));
        }
        assert_eq!(picker.cursor_pos, picker.input.len());
        // Walk left across all chars.
        picker.handle_key(make_key(KeyCode::Left));
        picker.handle_key(make_key(KeyCode::Left));
        picker.handle_key(make_key(KeyCode::Left));
        assert_eq!(picker.cursor_pos, 0);
        // Now walk right across all chars.
        picker.handle_key(make_key(KeyCode::Right));
        picker.handle_key(make_key(KeyCode::Right));
        picker.handle_key(make_key(KeyCode::Right));
        assert_eq!(picker.cursor_pos, picker.input.len());
        // One more right past the end is a no-op (no panic).
        picker.handle_key(make_key(KeyCode::Right));
        assert_eq!(picker.cursor_pos, picker.input.len());
    }

    /// Pasting multi-byte text places the cursor on a valid char boundary
    /// after each scalar value. `String::insert` would panic on a non-boundary
    /// byte index, so the absence of a panic here exercises the invariant.
    #[test]
    fn branch_picker_paste_multibyte_keeps_cursor_valid() {
        let mut picker = BranchPicker::new();
        picker.enter_search();
        picker.handle_paste("aé");
        // Cursor is at byte len after paste.
        assert_eq!(picker.cursor_pos, picker.input.len());
        // Inserting at the cursor must succeed (i.e., cursor is on a boundary).
        picker.input.insert(picker.cursor_pos, 'z');
    }

    /// `wrap_spans` must not loop forever when `max_width == 0` (e.g., when
    /// the diff body is narrower than the gutter width).
    #[test]
    fn wrap_spans_max_width_zero_returns_one_empty_row() {
        let spans = vec![Span::raw("hello world")];
        let rows = wrap_spans(spans, 0);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_empty());
    }

    /// Sanity: ordinary wrapping still produces multiple rows.
    #[test]
    fn wrap_spans_wraps_long_input() {
        let spans = vec![Span::raw("abcdefghij")];
        let rows = wrap_spans(spans, 3);
        let joined: Vec<String> = rows
            .iter()
            .map(|row| row.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert_eq!(joined, vec!["abc", "def", "ghi", "j"]);
    }

    /// Multi-byte spans must wrap on char boundaries (no panic from
    /// `split_at` landing in the middle of a UTF-8 sequence).
    #[test]
    fn wrap_spans_handles_multibyte() {
        let spans = vec![Span::raw("aébécédéf")];
        let rows = wrap_spans(spans, 2);
        // Just verify the joined output equals the input — no chars lost,
        // no panic on multi-byte boundaries.
        let joined: String = rows
            .iter()
            .flat_map(|row| row.iter().map(|s| s.content.to_string()))
            .collect();
        assert_eq!(joined, "aébécédéf");
    }
}
