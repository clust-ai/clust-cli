use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph},
    Frame, Terminal,
};

use clust_ipc::{AgentInfo, CliMessage, PoolMessage, RepoInfo};

use crate::{
    format::{format_attached, format_started},
    ipc,
    overview::{self, OverviewFocus, OverviewState},
    theme, version,
};

const LOGO_LINES: &[&str] = &[
    "██████╗ ██╗     ██╗   ██╗███████╗████████╗",
    "██╔═══╝ ██║     ██║   ██║██╔════╝╚══██╔══╝",
    "██║     ██║     ██║   ██║███████╗   ██║   ",
    "██║     ██║     ██║   ██║╚════██║   ██║   ",
    "╚██████╗███████╗╚██████╔╝███████║   ██║   ",
    " ╚═════╝╚══════╝ ╚═════╝ ╚══════╝   ╚═╝   ",
];

const AGENT_FETCH_INTERVAL: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Tree selection state
// ---------------------------------------------------------------------------

/// Which level of the repo tree the user is navigating.
#[derive(Clone, Copy, Debug, PartialEq)]
enum TreeLevel {
    Repo,     // Level 0: selecting between repositories
    Category, // Level 1: selecting Local/Remote within a repo
    Branch,   // Level 2: selecting a branch within a category
}

/// Which panel currently has keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq)]
enum FocusPanel {
    Left,
    Right,
}

/// Active tab in the top-level tab bar.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ActiveTab {
    Repositories,
    Overview,
    FocusMode,
}

impl ActiveTab {
    fn next(self) -> Self {
        match self {
            Self::Repositories => Self::Overview,
            Self::Overview => Self::FocusMode,
            Self::FocusMode => Self::Repositories,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Repositories => Self::FocusMode,
            Self::Overview => Self::Repositories,
            Self::FocusMode => Self::Overview,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Repositories => "Repositories",
            Self::Overview => "Overview",
            Self::FocusMode => "Focus",
        }
    }
}

// ---------------------------------------------------------------------------
// Agent view mode
// ---------------------------------------------------------------------------

/// Controls how agents are grouped in the right panel.
#[derive(Clone, Copy, Debug, PartialEq)]
enum AgentViewMode {
    /// Group agents by their pool name (default).
    ByPool,
    /// Group agents by their git repository path.
    ByRepo,
}

// ---------------------------------------------------------------------------
// Agent selection state (right panel)
// ---------------------------------------------------------------------------

/// Returns sorted, deduplicated group names from an agent list based on view mode.
fn group_names(agents: &[AgentInfo], mode: AgentViewMode) -> Vec<String> {
    let mut names: Vec<String> = agents
        .iter()
        .map(|a| match mode {
            AgentViewMode::ByPool => a.pool.clone(),
            AgentViewMode::ByRepo => a
                .repo_path
                .clone()
                .unwrap_or_else(|| "No repository".to_string()),
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Returns the group key for an agent based on view mode.
fn agent_group_key(agent: &AgentInfo, mode: AgentViewMode) -> String {
    match mode {
        AgentViewMode::ByPool => agent.pool.clone(),
        AgentViewMode::ByRepo => agent
            .repo_path
            .clone()
            .unwrap_or_else(|| "No repository".to_string()),
    }
}

/// Tracks the user's cursor position within the agent panel (group + agent).
#[derive(Default)]
struct AgentSelection {
    group_idx: usize,
    agent_idx: usize,
}

impl AgentSelection {
    /// Returns the number of agents in the currently selected group.
    fn agent_count(&self, agents: &[AgentInfo], mode: AgentViewMode) -> usize {
        let names = group_names(agents, mode);
        names
            .get(self.group_idx)
            .map(|group| {
                agents
                    .iter()
                    .filter(|a| agent_group_key(a, mode) == *group)
                    .count()
            })
            .unwrap_or(0)
    }

    /// Adjust indices to stay within bounds after data refresh.
    fn clamp(&mut self, agents: &[AgentInfo], mode: AgentViewMode) {
        let names = group_names(agents, mode);
        if names.is_empty() {
            self.group_idx = 0;
            self.agent_idx = 0;
            return;
        }
        self.group_idx = self.group_idx.min(names.len() - 1);
        let ac = self.agent_count(agents, mode);
        if ac > 0 {
            self.agent_idx = self.agent_idx.min(ac - 1);
        } else {
            self.agent_idx = 0;
        }
    }

    fn move_up(&mut self, agents: &[AgentInfo], mode: AgentViewMode) {
        if group_names(agents, mode).is_empty() {
            return;
        }
        self.agent_idx = self.agent_idx.saturating_sub(1);
    }

    fn move_down(&mut self, agents: &[AgentInfo], mode: AgentViewMode) {
        let ac = self.agent_count(agents, mode);
        if ac > 0 {
            self.agent_idx = (self.agent_idx + 1).min(ac - 1);
        }
    }

    fn prev_group(&mut self, agents: &[AgentInfo], mode: AgentViewMode) {
        if group_names(agents, mode).is_empty() {
            return;
        }
        if self.group_idx > 0 {
            self.group_idx -= 1;
            self.agent_idx = 0;
        }
    }

    fn next_group(&mut self, agents: &[AgentInfo], mode: AgentViewMode) {
        let names = group_names(agents, mode);
        if names.is_empty() {
            return;
        }
        if self.group_idx + 1 < names.len() {
            self.group_idx += 1;
            self.agent_idx = 0;
        }
    }
}

// ---------------------------------------------------------------------------
// Tree selection state (left panel)
// ---------------------------------------------------------------------------

/// Tracks the user's cursor position within the three-level repo tree.
struct TreeSelection {
    level: TreeLevel,
    repo_idx: usize,
    category_idx: usize, // 0 = Local, 1 = Remote
    branch_idx: usize,
    /// Collapsed state per repo index.
    repo_collapsed: HashMap<usize, bool>,
    /// Collapsed state per (repo_idx, category_idx).
    category_collapsed: HashMap<(usize, usize), bool>,
}

impl Default for TreeSelection {
    fn default() -> Self {
        Self {
            level: TreeLevel::Repo,
            repo_idx: 0,
            category_idx: 0,
            branch_idx: 0,
            repo_collapsed: HashMap::new(),
            category_collapsed: HashMap::new(),
        }
    }
}

impl TreeSelection {
    /// Returns valid category indices (0=local, 1=remote) for the selected repo.
    fn visible_categories(&self, repos: &[RepoInfo]) -> Vec<usize> {
        let Some(repo) = repos.get(self.repo_idx) else {
            return vec![];
        };
        let mut cats = Vec::new();
        if !repo.local_branches.is_empty() {
            cats.push(0);
        }
        if !repo.remote_branches.is_empty() {
            cats.push(1);
        }
        cats
    }

    /// Returns the number of branches in the currently selected category.
    fn branch_count(&self, repos: &[RepoInfo]) -> usize {
        let Some(repo) = repos.get(self.repo_idx) else {
            return 0;
        };
        match self.category_idx {
            0 => repo.local_branches.len(),
            1 => repo.remote_branches.len(),
            _ => 0,
        }
    }

    /// Adjust indices to stay within bounds after data refresh.
    fn clamp(&mut self, repos: &[RepoInfo]) {
        if repos.is_empty() {
            *self = Self::default();
            return;
        }
        self.repo_idx = self.repo_idx.min(repos.len() - 1);

        let cats = self.visible_categories(repos);
        if cats.is_empty() {
            self.level = TreeLevel::Repo;
            return;
        }
        if self.level != TreeLevel::Repo && !cats.contains(&self.category_idx) {
            self.category_idx = cats[0];
            if self.level == TreeLevel::Branch {
                self.level = TreeLevel::Category;
            }
        }

        let bc = self.branch_count(repos);
        if bc == 0 && self.level == TreeLevel::Branch {
            self.level = TreeLevel::Category;
        } else if bc > 0 {
            self.branch_idx = self.branch_idx.min(bc - 1);
        }
    }

    fn move_up(&mut self, repos: &[RepoInfo]) {
        if repos.is_empty() {
            return;
        }
        match self.level {
            TreeLevel::Repo => {
                self.repo_idx = self.repo_idx.saturating_sub(1);
            }
            TreeLevel::Category => {
                let cats = self.visible_categories(repos);
                if let Some(pos) = cats.iter().position(|&c| c == self.category_idx) {
                    if pos > 0 {
                        self.category_idx = cats[pos - 1];
                        self.branch_idx = 0;
                    }
                }
            }
            TreeLevel::Branch => {
                self.branch_idx = self.branch_idx.saturating_sub(1);
            }
        }
    }

    fn move_down(&mut self, repos: &[RepoInfo]) {
        if repos.is_empty() {
            return;
        }
        match self.level {
            TreeLevel::Repo => {
                self.repo_idx = (self.repo_idx + 1).min(repos.len() - 1);
            }
            TreeLevel::Category => {
                let cats = self.visible_categories(repos);
                if let Some(pos) = cats.iter().position(|&c| c == self.category_idx) {
                    if pos + 1 < cats.len() {
                        self.category_idx = cats[pos + 1];
                        self.branch_idx = 0;
                    }
                }
            }
            TreeLevel::Branch => {
                let bc = self.branch_count(repos);
                if bc > 0 {
                    self.branch_idx = (self.branch_idx + 1).min(bc - 1);
                }
            }
        }
    }

    /// Right arrow: descend one level deeper, or expand if collapsed.
    fn descend(&mut self, repos: &[RepoInfo]) {
        if repos.is_empty() {
            return;
        }
        match self.level {
            TreeLevel::Repo => {
                // Only descend if expanded; collapsed repos require Enter to open
                if !self.is_repo_collapsed(self.repo_idx) {
                    let cats = self.visible_categories(repos);
                    if !cats.is_empty() {
                        self.level = TreeLevel::Category;
                        self.category_idx = cats[0];
                        self.branch_idx = 0;
                    }
                }
            }
            TreeLevel::Category => {
                // Only descend if expanded; collapsed categories require Enter to open
                if !self.is_category_collapsed(self.repo_idx, self.category_idx)
                    && self.branch_count(repos) > 0
                {
                    self.level = TreeLevel::Branch;
                    self.branch_idx = 0;
                }
            }
            TreeLevel::Branch => {} // already deepest
        }
    }

    /// Left arrow: navigate up one level (never collapses — use Enter to toggle).
    fn ascend(&mut self) {
        match self.level {
            TreeLevel::Repo => {} // already at top
            TreeLevel::Category => self.level = TreeLevel::Repo,
            TreeLevel::Branch => self.level = TreeLevel::Category,
        }
    }

    /// Toggle collapse state at the current level.
    fn toggle_collapse(&mut self) {
        match self.level {
            TreeLevel::Repo => {
                let entry = self.repo_collapsed.entry(self.repo_idx).or_insert(false);
                *entry = !*entry;
            }
            TreeLevel::Category => {
                let key = (self.repo_idx, self.category_idx);
                let entry = self.category_collapsed.entry(key).or_insert(false);
                *entry = !*entry;
            }
            TreeLevel::Branch => {} // leaf nodes, no collapse
        }
    }

    fn is_repo_collapsed(&self, repo_idx: usize) -> bool {
        *self.repo_collapsed.get(&repo_idx).unwrap_or(&false)
    }

    fn is_category_collapsed(&self, repo_idx: usize, cat_idx: usize) -> bool {
        // Remote branches (cat_idx 1) are collapsed by default
        let default = cat_idx == 1;
        *self
            .category_collapsed
            .get(&(repo_idx, cat_idx))
            .unwrap_or(&default)
    }
}

pub fn run(pool_name: &str) -> io::Result<()> {
    io::stdout().execute(EnterAlternateScreen)?;
    enable_raw_mode()?;

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
        hook(info);
    }));

    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let pool_running = block_on_async(async { ipc::connect_to_pool().await.is_ok() });

    let update_notice: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let notice_clone = update_notice.clone();
    std::thread::spawn(move || {
        if let Some(msg) = version::check_update() {
            *notice_clone.lock().unwrap() = Some(msg);
        }
    });

    let mut agents: Vec<AgentInfo> = Vec::new();
    let mut repos: Vec<RepoInfo> = Vec::new();
    let mut selection = TreeSelection::default();
    let mut focus = FocusPanel::Left;
    let mut agent_selection = AgentSelection::default();
    let mut agent_view_mode = AgentViewMode::ByPool;
    let mut active_tab = ActiveTab::Repositories;
    let mut last_agent_fetch = Instant::now() - Duration::from_secs(10);
    let mut last_repo_fetch = Instant::now() - Duration::from_secs(10);

    let mut pool_stopped = false;
    let mut pool_count: usize = 1;
    let mut show_help = false;
    let mut overview_state = OverviewState::new();
    let mut last_content_area = Rect::default();

    loop {
        // Drain overview output events (non-blocking, runs regardless of tab)
        overview_state.drain_output_events();

        // Periodically fetch agent list and repo state from pool
        let mut agents_refreshed = false;
        if pool_running && last_agent_fetch.elapsed() >= AGENT_FETCH_INTERVAL {
            agents = fetch_agents();
            agent_selection.clamp(&agents, agent_view_mode);
            last_agent_fetch = Instant::now();
            agents_refreshed = true;
        }
        if pool_running && last_repo_fetch.elapsed() >= AGENT_FETCH_INTERVAL {
            repos = fetch_repos();
            selection.clamp(&repos);
            last_repo_fetch = Instant::now();
        }

        // Sync overview agent connections when agents are refreshed
        if agents_refreshed && active_tab == ActiveTab::Overview {
            overview_state.sync_agents(&agents, last_content_area);
        }

        let pool_status = pool_running;
        let notice = update_notice.lock().unwrap().clone();
        let cur_focus = focus;
        let cur_tab = active_tab;
        let overview_focus = overview_state.focus;
        let show_help_now = show_help;

        terminal.draw(|frame| {
            let area = frame.area();

            // Top-level: tab bar + content area + status bar
            let [tab_bar_area, content_area, status_area] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .areas(area);

            last_content_area = content_area;

            render_tab_bar(frame, tab_bar_area, cur_tab);

            match cur_tab {
                ActiveTab::Repositories => {
                    // Content: left (40%) + divider (1 col) + right (60%)
                    let [left_area, divider_area, right_area] = Layout::horizontal([
                        Constraint::Percentage(40),
                        Constraint::Length(1),
                        Constraint::Percentage(60),
                    ])
                    .areas(content_area);

                    render_left_panel(
                        frame,
                        left_area,
                        &repos,
                        &selection,
                        cur_focus == FocusPanel::Left,
                    );
                    render_divider(frame, divider_area);
                    render_right_panel(
                        frame,
                        right_area,
                        &agents,
                        &agent_selection,
                        cur_focus == FocusPanel::Right,
                        agent_view_mode,
                    );
                }
                ActiveTab::Overview => {
                    overview::render_overview(frame, content_area, &overview_state);
                }
                ActiveTab::FocusMode => {
                    render_placeholder(frame, content_area, "Focus mode - coming soon");
                }
            }

            render_status_bar(
                frame,
                status_area,
                pool_status,
                &notice,
                pool_name,
                cur_tab,
                overview_focus,
            );

            if show_help_now {
                render_help_overlay(frame, content_area, cur_tab);
            }
        })?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // When overview terminal is focused, intercept all keys
                    // except Shift+arrows — everything else goes to the agent.
                    if active_tab == ActiveTab::Overview
                        && matches!(overview_state.focus, OverviewFocus::Terminal(_))
                    {
                        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                        if shift {
                            match key.code {
                                KeyCode::Up => overview_state.exit_terminal(),
                                KeyCode::Left => overview_state.focus_prev(),
                                KeyCode::Right => overview_state.focus_next(),
                                _ => {
                                    // Shift+other key — forward to agent
                                    if let Some(bytes) =
                                        overview::input::key_event_to_bytes(&key)
                                    {
                                        overview_state.send_input(bytes);
                                    }
                                }
                            }
                        } else if let Some(bytes) =
                            overview::input::key_event_to_bytes(&key)
                        {
                            overview_state.send_input(bytes);
                        }
                    } else {
                        // Normal key handling (options bar, other tabs)
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            KeyCode::Char('c')
                                if key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                break
                            }
                            KeyCode::Char('Q') => {
                                let mut names: Vec<&str> =
                                    agents.iter().map(|a| a.pool.as_str()).collect();
                                names.sort();
                                names.dedup();
                                pool_count = names.len().max(1);
                                block_on_async(async {
                                    if let Ok(mut stream) = ipc::try_connect().await {
                                        let _ = ipc::send_stop(&mut stream).await;
                                    }
                                });
                                pool_stopped = true;
                                break;
                            }
                            // Tab switching
                            KeyCode::Tab => {
                                let prev_tab = active_tab;
                                active_tab = active_tab.next();
                                // Initialize overview on first switch
                                if active_tab == ActiveTab::Overview
                                    && !overview_state.initialized
                                {
                                    overview_state
                                        .sync_agents(&agents, last_content_area);
                                }
                                _ = prev_tab;
                            }
                            KeyCode::BackTab => {
                                let prev_tab = active_tab;
                                active_tab = active_tab.prev();
                                if active_tab == ActiveTab::Overview
                                    && !overview_state.initialized
                                {
                                    overview_state
                                        .sync_agents(&agents, last_content_area);
                                }
                                _ = prev_tab;
                            }
                            KeyCode::Char('?') => {
                                show_help = !show_help;
                            }
                            // Overview OptionsBar navigation
                            _ if active_tab == ActiveTab::Overview => {
                                let shift =
                                    key.modifiers.contains(KeyModifiers::SHIFT);
                                match key.code {
                                    KeyCode::Down if shift => {
                                        overview_state.enter_terminal();
                                    }
                                    KeyCode::Left if shift => {
                                        overview_state
                                            .scroll_left();
                                    }
                                    KeyCode::Right if shift => {
                                        overview_state
                                            .scroll_right(last_content_area.width);
                                    }
                                    _ => {}
                                }
                            }
                            // Repositories tab navigation
                            _ if active_tab == ActiveTab::Repositories => {
                                match key.code {
                                    KeyCode::Left
                                        if key
                                            .modifiers
                                            .contains(KeyModifiers::SHIFT) =>
                                    {
                                        focus = FocusPanel::Left;
                                    }
                                    KeyCode::Right
                                        if key
                                            .modifiers
                                            .contains(KeyModifiers::SHIFT) =>
                                    {
                                        focus = FocusPanel::Right;
                                    }
                                    KeyCode::Enter => {
                                        if focus == FocusPanel::Left {
                                            selection.toggle_collapse();
                                        }
                                    }
                                    KeyCode::Char('v')
                                        if focus == FocusPanel::Right =>
                                    {
                                        agent_view_mode = match agent_view_mode {
                                            AgentViewMode::ByPool => {
                                                AgentViewMode::ByRepo
                                            }
                                            AgentViewMode::ByRepo => {
                                                AgentViewMode::ByPool
                                            }
                                        };
                                        agent_selection = AgentSelection::default();
                                    }
                                    KeyCode::Up => match focus {
                                        FocusPanel::Left => {
                                            selection.move_up(&repos)
                                        }
                                        FocusPanel::Right => {
                                            agent_selection
                                                .move_up(&agents, agent_view_mode)
                                        }
                                    },
                                    KeyCode::Down => match focus {
                                        FocusPanel::Left => {
                                            selection.move_down(&repos)
                                        }
                                        FocusPanel::Right => {
                                            agent_selection
                                                .move_down(&agents, agent_view_mode)
                                        }
                                    },
                                    KeyCode::Right => match focus {
                                        FocusPanel::Left => {
                                            selection.descend(&repos)
                                        }
                                        FocusPanel::Right => {
                                            agent_selection
                                                .next_group(&agents, agent_view_mode)
                                        }
                                    },
                                    KeyCode::Left => match focus {
                                        FocusPanel::Left => selection.ascend(),
                                        FocusPanel::Right => {
                                            agent_selection
                                                .prev_group(&agents, agent_view_mode)
                                        }
                                    },
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                        // Dismiss help overlay on any non-? keypress
                        if key.code != KeyCode::Char('?') {
                            show_help = false;
                        }
                    }
                }
                Event::Resize(cols, rows) => {
                    // Compute content area from the new dimensions directly
                    // to avoid using stale last_content_area.
                    if active_tab == ActiveTab::Overview {
                        let new_content_area = Rect {
                            x: 0,
                            y: 1, // tab bar
                            width: cols,
                            height: rows.saturating_sub(2), // tab bar + status bar
                        };
                        overview_state
                            .handle_resize(agents.len(), new_content_area);
                    }
                }
                _ => {}
            }
        }
    }

    // Clean up overview connections before exiting
    overview_state.shutdown();

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    println!();

    if pool_stopped {
        let label = if pool_count > 1 { "pools" } else { "pool" };
        println!(
            "\n  {}{label} stopped{}\n",
            theme::TEXT_SECONDARY,
            theme::RESET
        );
    }

    if let Some(ref msg) = *update_notice.lock().unwrap() {
        println!("  {}{msg}{}\n", theme::WARNING, theme::RESET);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering functions
// ---------------------------------------------------------------------------

fn render_tab_bar(frame: &mut Frame, area: Rect, active_tab: ActiveTab) {
    let tabs = [
        ActiveTab::Repositories,
        ActiveTab::Overview,
        ActiveTab::FocusMode,
    ];
    let mut spans = Vec::new();

    spans.push(Span::styled(" ", Style::default().bg(theme::R_BG_RAISED)));

    for (i, tab) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                " │ ",
                Style::default()
                    .fg(theme::R_TEXT_TERTIARY)
                    .bg(theme::R_BG_RAISED),
            ));
        }

        let (fg, bg) = if *tab == active_tab {
            (theme::R_ACCENT_BRIGHT, theme::R_BG_OVERLAY)
        } else {
            (theme::R_TEXT_SECONDARY, theme::R_BG_RAISED)
        };

        spans.push(Span::styled(
            format!(" {} ", tab.label()),
            Style::default().fg(fg).bg(bg),
        ));
    }

    // Tab switching hint
    spans.push(Span::styled(
        "  Tab/Shift+Tab",
        Style::default()
            .fg(theme::R_TEXT_TERTIARY)
            .bg(theme::R_BG_RAISED),
    ));

    // Fill remaining width with background
    let content_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let remaining = (area.width as usize).saturating_sub(content_width);
    if remaining > 0 {
        spans.push(Span::styled(
            " ".repeat(remaining),
            Style::default().bg(theme::R_BG_RAISED),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_placeholder(frame: &mut Frame, area: Rect, message: &str) {
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::R_BG_BASE)),
        area,
    );

    let text = Paragraph::new(Line::from(Span::styled(
        message.to_string(),
        Style::default().fg(theme::R_TEXT_TERTIARY),
    )))
    .alignment(Alignment::Center);

    let [centered] = Layout::vertical([Constraint::Length(1)])
        .flex(Flex::Center)
        .areas(area);

    frame.render_widget(text, centered);
}

fn render_divider(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::R_BG_RAISED)),
        area,
    );
}

fn render_left_panel(
    frame: &mut Frame,
    area: Rect,
    repos: &[RepoInfo],
    selection: &TreeSelection,
    focused: bool,
) {
    let block = Block::default()
        .style(Style::default().bg(theme::R_BG_SURFACE))
        .padding(Padding::new(2, 2, 1, 0));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if repos.is_empty() {
        let text = Paragraph::new(Line::from(Span::styled(
            "No repositories found",
            Style::default().fg(theme::R_TEXT_TERTIARY),
        )))
        .alignment(Alignment::Center);

        let [centered] = Layout::vertical([Constraint::Length(1)])
            .flex(Flex::Center)
            .areas(inner);

        frame.render_widget(text, centered);
    } else {
        let mut lines = vec![];
        lines.extend(build_repo_tree_lines(repos, selection, inner.width));
        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    // Focus indicator in top-right corner
    let indicator_color = if focused {
        theme::R_ACCENT_BRIGHT
    } else {
        theme::R_TEXT_TERTIARY
    };
    let indicator = Paragraph::new(Span::styled(
        "●",
        Style::default().fg(indicator_color).bg(theme::R_BG_SURFACE),
    ))
    .alignment(Alignment::Right);
    let indicator_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1,
    };
    frame.render_widget(indicator, indicator_area);
}

fn build_repo_tree_lines(
    repos: &[RepoInfo],
    selection: &TreeSelection,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    for (repo_idx, repo) in repos.iter().enumerate() {
        let is_this_repo = repo_idx == selection.repo_idx;
        let repo_selected = is_this_repo && selection.level == TreeLevel::Repo;
        let repo_open = is_this_repo && selection.level != TreeLevel::Repo;
        let repo_collapsed = selection.is_repo_collapsed(repo_idx);

        // Repo name header with collapse chevron
        let chevron = if repo_collapsed { "▸" } else { "▾" };
        let (name_color, bg) = if repo_selected {
            (theme::R_ACCENT_BRIGHT, Some(theme::R_BG_HOVER))
        } else if repo_open {
            (theme::R_ACCENT_DIM, None)
        } else {
            (theme::R_ACCENT, None)
        };

        let text = format!(" {chevron} {}", repo.name);
        let mut style = Style::default().fg(name_color).add_modifier(Modifier::BOLD);
        if let Some(bg_color) = bg {
            style = style.bg(bg_color);
        }
        lines.push(pad_line(vec![Span::styled(text, style)], width, bg));

        // Skip children if repo is collapsed
        if repo_collapsed {
            continue;
        }

        let has_local = !repo.local_branches.is_empty();
        let has_remote = !repo.remote_branches.is_empty();
        let local_cat_collapsed = selection.is_category_collapsed(repo_idx, 0);
        let remote_cat_collapsed = selection.is_category_collapsed(repo_idx, 1);

        // Local Branches section
        if has_local {
            let cat_selected = is_this_repo
                && selection.level == TreeLevel::Category
                && selection.category_idx == 0;
            let cat_open =
                is_this_repo && selection.level == TreeLevel::Branch && selection.category_idx == 0;

            let connector = if has_remote { "├──" } else { "└──" };
            let cat_chevron = if local_cat_collapsed { "▸" } else { "▾" };
            let (cat_fg, cat_bg) = if cat_selected {
                (theme::R_TEXT_PRIMARY, Some(theme::R_BG_HOVER))
            } else if cat_open {
                (theme::R_TEXT_TERTIARY, None)
            } else {
                (theme::R_TEXT_SECONDARY, None)
            };

            let cat_text = format!("   {connector} {cat_chevron} Local Branches");
            let mut cat_style = Style::default().fg(cat_fg);
            if let Some(bg_color) = cat_bg {
                cat_style = cat_style.bg(bg_color);
            }
            lines.push(pad_line(
                vec![Span::styled(cat_text, cat_style)],
                width,
                cat_bg,
            ));

            if !local_cat_collapsed {
                let continuation = if has_remote { "│" } else { " " };
                for (i, branch) in repo.local_branches.iter().enumerate() {
                    let is_last = i == repo.local_branches.len() - 1;
                    let branch_connector = if is_last { "└──" } else { "├──" };
                    let branch_selected = is_this_repo
                        && selection.level == TreeLevel::Branch
                        && selection.category_idx == 0
                        && i == selection.branch_idx;
                    lines.push(format_branch_line(
                        branch,
                        continuation,
                        branch_connector,
                        branch_selected,
                        width,
                    ));
                }
            }
        }

        // Remote Branches section
        if has_remote {
            let cat_selected = is_this_repo
                && selection.level == TreeLevel::Category
                && selection.category_idx == 1;
            let cat_open =
                is_this_repo && selection.level == TreeLevel::Branch && selection.category_idx == 1;

            let cat_chevron = if remote_cat_collapsed { "▸" } else { "▾" };
            let (cat_fg, cat_bg) = if cat_selected {
                (theme::R_TEXT_PRIMARY, Some(theme::R_BG_HOVER))
            } else if cat_open {
                (theme::R_TEXT_TERTIARY, None)
            } else {
                (theme::R_TEXT_SECONDARY, None)
            };

            let cat_text = format!("   └── {cat_chevron} Remote Branches");
            let mut cat_style = Style::default().fg(cat_fg);
            if let Some(bg_color) = cat_bg {
                cat_style = cat_style.bg(bg_color);
            }
            lines.push(pad_line(
                vec![Span::styled(cat_text, cat_style)],
                width,
                cat_bg,
            ));

            if !remote_cat_collapsed {
                for (i, branch) in repo.remote_branches.iter().enumerate() {
                    let is_last = i == repo.remote_branches.len() - 1;
                    let branch_connector = if is_last { "└──" } else { "├──" };
                    let branch_selected = is_this_repo
                        && selection.level == TreeLevel::Branch
                        && selection.category_idx == 1
                        && i == selection.branch_idx;
                    lines.push(format_branch_line(
                        branch,
                        " ",
                        branch_connector,
                        branch_selected,
                        width,
                    ));
                }
            }
        }

    }

    lines
}

fn format_branch_line(
    branch: &clust_ipc::BranchInfo,
    continuation: &str,
    connector: &str,
    is_selected: bool,
    width: u16,
) -> Line<'static> {
    let mut spans = Vec::new();
    let bg = if is_selected {
        Some(theme::R_BG_HOVER)
    } else {
        None
    };

    let indicator = if is_selected { "▸ " } else { "  " };

    // Tree structure prefix
    let mut prefix_style = Style::default().fg(theme::R_TEXT_TERTIARY);
    if let Some(bg_color) = bg {
        prefix_style = prefix_style.bg(bg_color);
    }
    spans.push(Span::styled(
        format!("   {continuation}  {connector} {indicator}"),
        prefix_style,
    ));

    // Active agent indicator
    if branch.active_agent_count > 0 {
        let mut dot_style = Style::default().fg(theme::R_SUCCESS);
        if let Some(bg_color) = bg {
            dot_style = dot_style.bg(bg_color);
        }
        spans.push(Span::styled(
            format!("● {} ", branch.active_agent_count),
            dot_style,
        ));
    }

    let name_color = if is_selected || branch.is_head {
        theme::R_ACCENT_BRIGHT
    } else {
        theme::R_TEXT_PRIMARY
    };
    let mut name_style = Style::default().fg(name_color).add_modifier(Modifier::BOLD);
    if let Some(bg_color) = bg {
        name_style = name_style.bg(bg_color);
    }
    spans.push(Span::styled(branch.name.clone(), name_style));

    // Worktree indicator
    if branch.is_worktree {
        let mut wt_style = Style::default().fg(theme::R_TEXT_SECONDARY);
        if let Some(bg_color) = bg {
            wt_style = wt_style.bg(bg_color);
        }
        spans.push(Span::styled(" ⎇".to_string(), wt_style));
    }

    pad_line(spans, width, bg)
}

/// Pad a line's spans to fill `width`, applying background color to the padding.
fn pad_line(spans: Vec<Span<'static>>, width: u16, bg: Option<Color>) -> Line<'static> {
    let content_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let remaining = (width as usize).saturating_sub(content_width);
    let mut all_spans = spans;
    if remaining > 0 {
        let mut pad_style = Style::default();
        if let Some(bg_color) = bg {
            pad_style = pad_style.bg(bg_color);
        }
        all_spans.push(Span::styled(" ".repeat(remaining), pad_style));
    }
    Line::from(all_spans)
}

fn render_right_panel(
    frame: &mut Frame,
    area: Rect,
    agents: &[AgentInfo],
    agent_sel: &AgentSelection,
    focused: bool,
    mode: AgentViewMode,
) {
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::R_BG_BASE)),
        area,
    );

    if agents.is_empty() {
        render_logo(frame, area);
        // Focus indicator even when empty
        let indicator_color = if focused {
            theme::R_ACCENT_BRIGHT
        } else {
            theme::R_TEXT_TERTIARY
        };
        let indicator = Paragraph::new(Span::styled(
            "●",
            Style::default().fg(indicator_color).bg(theme::R_BG_BASE),
        ))
        .alignment(Alignment::Right);
        let indicator_area = Rect {
            x: area.x + 1,
            y: area.y,
            width: area.width.saturating_sub(2),
            height: 1,
        };
        frame.render_widget(indicator, indicator_area);
    } else {
        render_agent_list(frame, area, agents, agent_sel, focused, mode);
    }
}

fn render_logo(frame: &mut Frame, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(""));

    // Logo lines with accent colors
    for (i, text) in LOGO_LINES.iter().enumerate() {
        let color = if i == 2 || i == 3 {
            theme::R_ACCENT_BRIGHT
        } else {
            theme::R_ACCENT
        };
        let padded = format!("  {:<44}", text);
        lines.push(Line::from(Span::styled(padded, Style::default().fg(color))));
    }

    lines.push(Line::from(""));

    // Gradient bar
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("░░", Style::default().fg(theme::R_TEXT_TERTIARY)),
        Span::styled("▒▒", Style::default().fg(theme::R_TEXT_SECONDARY)),
        Span::styled(
            "▓▓██████████████████████████████",
            Style::default().fg(theme::R_TEXT_PRIMARY),
        ),
        Span::styled("▓▓", Style::default().fg(theme::R_TEXT_SECONDARY)),
        Span::styled("▒▒░░", Style::default().fg(theme::R_TEXT_TERTIARY)),
        Span::raw("  "),
    ]));

    let block_height = lines.len() as u16;
    let block_width = 46u16;

    let [vert_area] = Layout::vertical([Constraint::Length(block_height)])
        .flex(Flex::Center)
        .areas(area);

    let [horz_area] = Layout::horizontal([Constraint::Length(block_width)])
        .flex(Flex::Center)
        .areas(vert_area);

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, horz_area);
}

fn render_agent_list(
    frame: &mut Frame,
    area: Rect,
    agents: &[AgentInfo],
    agent_sel: &AgentSelection,
    focused: bool,
    mode: AgentViewMode,
) {
    let block = Block::default().padding(Padding::new(2, 2, 0, 0));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Mode label + spacer
    let mode_label = match mode {
        AgentViewMode::ByPool => "by pool",
        AgentViewMode::ByRepo => "by repo",
    };
    let mode_line = Paragraph::new(Line::from(Span::styled(
        mode_label,
        Style::default().fg(theme::R_TEXT_TERTIARY),
    )));

    // Focus indicator in top-right corner (overlaid on mode label line)
    let indicator_color = if focused {
        theme::R_ACCENT_BRIGHT
    } else {
        theme::R_TEXT_TERTIARY
    };
    let indicator = Paragraph::new(Span::styled("●", Style::default().fg(indicator_color)))
        .alignment(Alignment::Right);

    match mode {
        AgentViewMode::ByPool => render_agent_list_by_pool(
            frame, inner, agents, agent_sel, focused, &mode_line, &indicator,
        ),
        AgentViewMode::ByRepo => render_agent_list_by_repo(
            frame, inner, agents, agent_sel, focused, &mode_line, &indicator,
        ),
    }
}

fn render_agent_list_by_pool(
    frame: &mut Frame,
    inner: Rect,
    agents: &[AgentInfo],
    agent_sel: &AgentSelection,
    focused: bool,
    mode_line: &Paragraph<'_>,
    indicator: &Paragraph<'_>,
) {
    let mut sorted: Vec<&AgentInfo> = agents.iter().collect();
    sorted.sort_by(|a, b| a.pool.cmp(&b.pool).then(a.started_at.cmp(&b.started_at)));

    let mut pnames: Vec<&str> = sorted.iter().map(|a| a.pool.as_str()).collect();
    pnames.dedup();

    // Build layout: mode label + spacer + pool headers + agent cards + gaps
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(1), // mode label
        Constraint::Length(1), // spacer
    ];
    for (pidx, pool_name) in pnames.iter().enumerate() {
        if pidx > 0 {
            constraints.push(Constraint::Length(1)); // gap before pool header
        }
        constraints.push(Constraint::Length(1)); // pool header
        constraints.push(Constraint::Length(1)); // spacer after header
        let count = sorted.iter().filter(|a| a.pool == *pool_name).count();
        for i in 0..count {
            constraints.push(Constraint::Length(4)); // agent card
            if i < count - 1 {
                constraints.push(Constraint::Length(1)); // gap between cards
            }
        }
    }
    constraints.push(Constraint::Min(0));

    let areas = Layout::vertical(constraints).split(inner);

    frame.render_widget(mode_line.clone(), areas[0]);
    frame.render_widget(
        indicator.clone(),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        },
    );

    let mut area_idx = 2;
    for (pidx, pool_name) in pnames.iter().enumerate() {
        if pidx > 0 {
            area_idx += 1; // skip gap before pool header
        }
        let pool_header = Paragraph::new(Line::from(vec![Span::styled(
            format!(" {pool_name}"),
            Style::default().fg(theme::R_ACCENT),
        )]));
        frame.render_widget(pool_header, areas[area_idx]);
        area_idx += 1;
        area_idx += 1; // skip spacer after header

        let agents_in_pool: Vec<(usize, &&AgentInfo)> = sorted
            .iter()
            .filter(|a| a.pool == *pool_name)
            .enumerate()
            .collect();
        let agent_count = agents_in_pool.len();
        for (aidx, agent) in agents_in_pool {
            let is_selected = focused && pidx == agent_sel.group_idx && aidx == agent_sel.agent_idx;
            render_agent_card(frame, areas[area_idx], agent, is_selected);
            area_idx += 1;
            if aidx < agent_count - 1 {
                area_idx += 1; // skip gap between cards
            }
        }
    }
}

fn render_agent_list_by_repo(
    frame: &mut Frame,
    inner: Rect,
    agents: &[AgentInfo],
    agent_sel: &AgentSelection,
    focused: bool,
    mode_line: &Paragraph<'_>,
    indicator: &Paragraph<'_>,
) {
    let mut sorted: Vec<&AgentInfo> = agents.iter().collect();
    sorted.sort_by(|a, b| {
        let ak = agent_group_key(a, AgentViewMode::ByRepo);
        let bk = agent_group_key(b, AgentViewMode::ByRepo);
        ak.cmp(&bk)
            .then(a.branch_name.cmp(&b.branch_name))
            .then(a.started_at.cmp(&b.started_at))
    });

    let gnames = group_names(agents, AgentViewMode::ByRepo);

    // Build layout: mode label + spacer + repo/branch headers + agent cards + gaps
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(1), // mode label
        Constraint::Length(1), // spacer
    ];
    for (ridx, repo) in gnames.iter().enumerate() {
        if ridx > 0 {
            constraints.push(Constraint::Length(1)); // gap before repo header
        }
        constraints.push(Constraint::Length(1)); // repo header
        let mut branches: Vec<&str> = sorted
            .iter()
            .filter(|a| agent_group_key(a, AgentViewMode::ByRepo) == *repo)
            .map(|a| a.branch_name.as_deref().unwrap_or("no branch"))
            .collect();
        branches.dedup();
        for branch in &branches {
            constraints.push(Constraint::Length(1)); // branch sub-header
            let count = sorted
                .iter()
                .filter(|a| {
                    agent_group_key(a, AgentViewMode::ByRepo) == *repo
                        && a.branch_name.as_deref().unwrap_or("no branch") == *branch
                })
                .count();
            for i in 0..count {
                constraints.push(Constraint::Length(4)); // agent card
                if i < count - 1 {
                    constraints.push(Constraint::Length(1)); // gap between cards
                }
            }
        }
    }
    constraints.push(Constraint::Min(0));

    let areas = Layout::vertical(constraints).split(inner);

    frame.render_widget(mode_line.clone(), areas[0]);
    frame.render_widget(
        indicator.clone(),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        },
    );

    let mut area_idx = 2;
    let mut flat_agent_idx = 0;
    for (gidx, repo) in gnames.iter().enumerate() {
        if gidx > 0 {
            area_idx += 1; // skip gap before repo header
        }
        // Repo header — show just the repo name (last path component)
        let repo_display = std::path::Path::new(repo)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| repo.clone());
        let repo_header = Paragraph::new(Line::from(vec![Span::styled(
            format!(" {repo_display}"),
            Style::default().fg(theme::R_ACCENT),
        )]));
        frame.render_widget(repo_header, areas[area_idx]);
        area_idx += 1;

        // Branch sub-groups
        let repo_agents: Vec<&AgentInfo> = sorted
            .iter()
            .filter(|a| agent_group_key(a, AgentViewMode::ByRepo) == *repo)
            .copied()
            .collect();
        let mut branches: Vec<&str> = repo_agents
            .iter()
            .map(|a| a.branch_name.as_deref().unwrap_or("no branch"))
            .collect();
        branches.dedup();

        for branch in &branches {
            // Branch sub-header
            let branch_header = Paragraph::new(Line::from(vec![Span::styled(
                format!("   {branch}"),
                Style::default().fg(theme::R_TEXT_SECONDARY),
            )]));
            frame.render_widget(branch_header, areas[area_idx]);
            area_idx += 1;

            let branch_agents: Vec<&&AgentInfo> = repo_agents
                .iter()
                .filter(|a| a.branch_name.as_deref().unwrap_or("no branch") == *branch)
                .collect();
            let branch_agent_count = branch_agents.len();
            for (bidx, agent) in branch_agents.into_iter().enumerate() {
                let is_selected =
                    focused && gidx == agent_sel.group_idx && flat_agent_idx == agent_sel.agent_idx;
                render_agent_card(frame, areas[area_idx], agent, is_selected);
                area_idx += 1;
                flat_agent_idx += 1;
                if bidx < branch_agent_count - 1 {
                    area_idx += 1; // skip gap between cards
                }
            }
        }
        flat_agent_idx = 0; // reset for next repo group
    }
}

fn render_agent_card(frame: &mut Frame, area: Rect, agent: &AgentInfo, is_selected: bool) {
    let bg = if is_selected {
        theme::R_BG_HOVER
    } else {
        theme::R_BG_SURFACE
    };
    let block = Block::default()
        .style(Style::default().bg(bg))
        .padding(Padding::new(1, 1, 0, 0));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let started = format_started(&agent.started_at);
    let attached = format_attached(agent.attached_clients);

    let lines = vec![
        Line::from(Span::styled(
            &agent.id,
            Style::default().fg(theme::R_ACCENT),
        )),
        Line::from({
            let mut spans = vec![
                Span::styled(
                    agent.agent_binary.clone(),
                    Style::default().fg(theme::R_TEXT_PRIMARY),
                ),
                Span::raw("  "),
                Span::styled("● running", Style::default().fg(theme::R_SUCCESS)),
            ];
            if let Some(ref branch) = agent.branch_name {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("\u{e0a0} {branch}"),
                    Style::default().fg(theme::R_TEXT_SECONDARY),
                ));
            }
            spans
        }),
        Line::from(vec![
            Span::styled(
                format!("started {started}"),
                Style::default().fg(theme::R_TEXT_SECONDARY),
            ),
            Span::raw("    "),
            Span::styled(
                format!("attached: {attached}"),
                Style::default().fg(theme::R_TEXT_SECONDARY),
            ),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    pool_running: bool,
    update_notice: &Option<String>,
    pool_name: &str,
    active_tab: ActiveTab,
    overview_focus: OverviewFocus,
) {
    let bg = Style::default().bg(theme::R_BG_RAISED);

    // Build left spans
    let (dot_color, status_label) = if pool_running {
        (theme::R_SUCCESS, "connected")
    } else {
        (theme::R_TEXT_TERTIARY, "disconnected")
    };

    let mut left_spans = vec![
        Span::styled(" ●", Style::default().fg(dot_color).bg(theme::R_BG_RAISED)),
        Span::styled(
            format!(" {status_label}"),
            Style::default()
                .fg(theme::R_TEXT_SECONDARY)
                .bg(theme::R_BG_RAISED),
        ),
    ];

    if pool_name != clust_ipc::DEFAULT_POOL {
        left_spans.push(Span::styled("  ", Style::default().bg(theme::R_BG_RAISED)));
        left_spans.push(Span::styled(
            pool_name.to_string(),
            Style::default().fg(theme::R_ACCENT).bg(theme::R_BG_RAISED),
        ));
    }

    let hint_text = if active_tab == ActiveTab::Overview {
        match overview_focus {
            OverviewFocus::Terminal(_) => {
                "Shift+\u{2191} options  Shift+\u{2190}/\u{2192} switch agent"
            }
            OverviewFocus::OptionsBar => {
                "Shift+\u{2193} enter terminal  Shift+\u{2190}/\u{2192} scroll  q quit  Hold ? for keys"
            }
        }
    } else {
        "q quit  Q stop+quit  Hold ? for keys"
    };

    left_spans.extend([
        Span::styled("  ", Style::default().bg(theme::R_BG_RAISED)),
        Span::styled(
            hint_text,
            Style::default()
                .fg(theme::R_TEXT_TERTIARY)
                .bg(theme::R_BG_RAISED),
        ),
    ]);

    if let Some(ref msg) = *update_notice {
        left_spans.push(Span::styled("  ", Style::default().bg(theme::R_BG_RAISED)));
        left_spans.push(Span::styled(
            msg.clone(),
            Style::default().fg(theme::R_WARNING).bg(theme::R_BG_RAISED),
        ));
    }

    let left_line = Line::from(left_spans);

    // Right side: version
    let version_text = format!("v{} ", env!("CARGO_PKG_VERSION"));
    let version_width = version_text.len() as u16;
    let right_line = Line::from(Span::styled(
        version_text,
        Style::default()
            .fg(theme::R_TEXT_TERTIARY)
            .bg(theme::R_BG_RAISED),
    ));

    let [left_area, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(version_width)]).areas(area);

    frame.render_widget(
        Paragraph::new(left_line).block(Block::default().style(bg)),
        left_area,
    );
    frame.render_widget(
        Paragraph::new(right_line)
            .alignment(Alignment::Right)
            .block(Block::default().style(bg)),
        right_area,
    );
}

fn render_help_overlay(frame: &mut Frame, area: Rect, active_tab: ActiveTab) {
    let mut bindings: Vec<(&str, &str)> = vec![
        ("q / Esc", "Quit"),
        ("Q", "Quit and stop pool"),
        ("Ctrl+C", "Quit"),
        ("Tab", "Next tab"),
        ("Shift+Tab", "Previous tab"),
    ];

    if active_tab == ActiveTab::Repositories {
        bindings.extend([
            ("↑ / ↓", "Navigate items"),
            ("← / →", "Collapse / expand"),
            ("Shift+←/→", "Switch panel"),
            ("Enter", "Toggle collapse"),
            ("v", "Toggle agent grouping"),
        ]);
    }

    let modal_width: u16 = 38;
    let modal_height: u16 = bindings.len() as u16 + 2; // +2 for border

    let [horz_area] = Layout::horizontal([Constraint::Length(modal_width)])
        .flex(Flex::Center)
        .areas(area);

    let modal_rect = Rect {
        x: horz_area.x,
        y: area.y + area.height.saturating_sub(modal_height),
        width: modal_width,
        height: modal_height.min(area.height),
    };

    frame.render_widget(Clear, modal_rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::R_TEXT_TERTIARY))
        .style(Style::default().bg(theme::R_BG_OVERLAY));

    let inner = block.inner(modal_rect);
    frame.render_widget(block, modal_rect);

    let lines: Vec<Line> = bindings
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(
                    format!(" {:<14}", key),
                    Style::default().fg(theme::R_ACCENT),
                ),
                Span::styled(
                    desc.to_string(),
                    Style::default().fg(theme::R_TEXT_PRIMARY),
                ),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fetch_agents() -> Vec<AgentInfo> {
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return vec![];
        };
        if clust_ipc::send_message(&mut stream, &CliMessage::ListAgents { pool: None })
            .await
            .is_err()
        {
            return vec![];
        }
        match clust_ipc::recv_message::<PoolMessage>(&mut stream).await {
            Ok(PoolMessage::AgentList { agents }) => agents,
            _ => vec![],
        }
    })
}

fn fetch_repos() -> Vec<RepoInfo> {
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return vec![];
        };
        if clust_ipc::send_message(&mut stream, &CliMessage::ListRepos)
            .await
            .is_err()
        {
            return vec![];
        }
        match clust_ipc::recv_message::<PoolMessage>(&mut stream).await {
            Ok(PoolMessage::RepoList { repos }) => repos,
            _ => vec![],
        }
    })
}

/// Run an async future from the synchronous UI loop.
/// Requires the multi-thread tokio scheduler (`#[tokio::main]`).
fn block_on_async<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Repository tree rendering tests ──────────────────────────

    fn make_branch(
        name: &str,
        is_head: bool,
        agent_count: usize,
        is_worktree: bool,
    ) -> clust_ipc::BranchInfo {
        clust_ipc::BranchInfo {
            name: name.to_string(),
            is_head,
            active_agent_count: agent_count,
            is_worktree,
        }
    }

    fn make_repo(
        name: &str,
        local: Vec<clust_ipc::BranchInfo>,
        remote: Vec<clust_ipc::BranchInfo>,
    ) -> clust_ipc::RepoInfo {
        clust_ipc::RepoInfo {
            path: format!("/repos/{name}"),
            name: name.to_string(),
            local_branches: local,
            remote_branches: remote,
        }
    }

    #[test]
    fn tree_empty_repos_produces_no_lines() {
        let sel = TreeSelection::default();
        let lines = build_repo_tree_lines(&[], &sel, 80);
        assert!(lines.is_empty());
    }

    #[test]
    fn tree_single_repo_with_local_branches() {
        let repo = make_repo(
            "myrepo",
            vec![
                make_branch("main", true, 0, false),
                make_branch("feature", false, 0, false),
            ],
            vec![],
        );
        let sel = TreeSelection::default();
        let lines = build_repo_tree_lines(&[repo], &sel, 80);

        // Should have: repo name + "Local Branches" header + 2 branch lines
        assert_eq!(lines.len(), 4);

        // First line is repo name
        let first = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(first.contains("myrepo"));

        // Second line is section header
        let second = lines[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(second.contains("Local Branches"));
    }

    #[test]
    fn tree_repo_with_local_and_remote() {
        let repo = make_repo(
            "myrepo",
            vec![make_branch("main", true, 0, false)],
            vec![make_branch("origin/main", false, 0, false)],
        );
        let sel = TreeSelection::default();
        let lines = build_repo_tree_lines(&[repo], &sel, 80);

        // repo name + local header + 1 local branch + remote header (collapsed by default)
        assert_eq!(lines.len(), 4);

        let texts: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert!(texts[0].contains("myrepo"));
        assert!(texts[1].contains("Local Branches"));
        assert!(texts[2].contains("main"));
        assert!(texts[3].contains("Remote Branches"));
    }

    #[test]
    fn tree_multiple_repos_separated_by_blank_line() {
        let repos = vec![
            make_repo("alpha", vec![make_branch("main", true, 0, false)], vec![]),
            make_repo("beta", vec![make_branch("main", true, 0, false)], vec![]),
        ];
        let sel = TreeSelection::default();
        let lines = build_repo_tree_lines(&repos, &sel, 80);

        // alpha: name + header + branch = 3
        // beta: name + header + branch = 3
        assert_eq!(lines.len(), 6);
    }

    #[test]
    fn format_branch_line_shows_agent_indicator() {
        let branch = make_branch("main", false, 1, false);
        let line = format_branch_line(&branch, "│", "├─", false, 80);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("●"), "should have active agent indicator");
        assert!(text.contains("main"));
    }

    #[test]
    fn format_branch_line_no_agent_indicator() {
        let branch = make_branch("main", false, 0, false);
        let line = format_branch_line(&branch, "│", "├─", false, 80);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("●"), "should not have agent indicator");
    }

    #[test]
    fn format_branch_line_shows_worktree_indicator() {
        let branch = make_branch("feature", false, 0, true);
        let line = format_branch_line(&branch, " ", "└─", false, 80);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("⎇"), "should have worktree indicator");
    }

    #[test]
    fn format_branch_line_no_worktree_indicator() {
        let branch = make_branch("feature", false, 0, false);
        let line = format_branch_line(&branch, " ", "└─", false, 80);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("⎇"), "should not have worktree indicator");
    }

    #[test]
    fn format_branch_line_head_and_agent_and_worktree() {
        let branch = make_branch("main", true, 1, true);
        let line = format_branch_line(&branch, "│", "├─", false, 80);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("●"), "agent indicator");
        assert!(text.contains("main"), "branch name");
        assert!(text.contains("⎇"), "worktree indicator");
    }

    // ── Tree selection state tests ──────────────────────────────

    fn sample_repos() -> Vec<RepoInfo> {
        vec![
            make_repo(
                "alpha",
                vec![
                    make_branch("main", true, 0, false),
                    make_branch("dev", false, 0, false),
                ],
                vec![make_branch("origin/main", false, 0, false)],
            ),
            make_repo("beta", vec![make_branch("main", true, 0, false)], vec![]),
        ]
    }

    #[test]
    fn selection_default_is_repo_level() {
        let sel = TreeSelection::default();
        assert_eq!(sel.level, TreeLevel::Repo);
        assert_eq!(sel.repo_idx, 0);
    }

    #[test]
    fn selection_move_down_repos() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.move_down(&repos);
        assert_eq!(sel.repo_idx, 1);
        // Cannot go past last
        sel.move_down(&repos);
        assert_eq!(sel.repo_idx, 1);
    }

    #[test]
    fn selection_move_up_repos() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        // Already at 0, stays at 0
        sel.move_up(&repos);
        assert_eq!(sel.repo_idx, 0);
    }

    #[test]
    fn selection_descend_to_category() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos);
        assert_eq!(sel.level, TreeLevel::Category);
        assert_eq!(sel.category_idx, 0); // first valid = local
    }

    #[test]
    fn selection_descend_to_branch() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos); // -> Category
        sel.descend(&repos); // -> Branch
        assert_eq!(sel.level, TreeLevel::Branch);
        assert_eq!(sel.branch_idx, 0);
    }

    #[test]
    fn selection_ascend_round_trip() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos); // -> Category
        sel.descend(&repos); // -> Branch
        sel.ascend(); // -> Category
        assert_eq!(sel.level, TreeLevel::Category);
        sel.ascend(); // -> Repo (ascend always goes up, never collapses)
        assert_eq!(sel.level, TreeLevel::Repo);
        sel.ascend(); // no-op, already at top
        assert_eq!(sel.level, TreeLevel::Repo);
    }

    #[test]
    fn selection_category_up_down() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        // alpha has both local (0) and remote (1)
        sel.descend(&repos); // -> Category, idx 0
        assert_eq!(sel.category_idx, 0);
        sel.move_down(&repos);
        assert_eq!(sel.category_idx, 1);
        // Can't go past remote
        sel.move_down(&repos);
        assert_eq!(sel.category_idx, 1);
        sel.move_up(&repos);
        assert_eq!(sel.category_idx, 0);
    }

    #[test]
    fn selection_branch_up_down() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos); // -> Category (local)
        sel.descend(&repos); // -> Branch (0)
        assert_eq!(sel.branch_idx, 0);
        sel.move_down(&repos); // alpha has 2 local branches
        assert_eq!(sel.branch_idx, 1);
        sel.move_down(&repos); // saturates
        assert_eq!(sel.branch_idx, 1);
        sel.move_up(&repos);
        assert_eq!(sel.branch_idx, 0);
    }

    #[test]
    fn selection_descend_noop_on_empty_repos() {
        let mut sel = TreeSelection::default();
        sel.descend(&[]);
        assert_eq!(sel.level, TreeLevel::Repo);
    }

    #[test]
    fn selection_descend_skips_empty_category() {
        // beta has only local branches
        let repos = sample_repos();
        let mut sel = TreeSelection {
            repo_idx: 1,
            ..TreeSelection::default()
        };
        sel.descend(&repos); // -> Category
        assert_eq!(sel.category_idx, 0); // only local exists
                                         // Move down should be no-op (no remote)
        sel.move_down(&repos);
        assert_eq!(sel.category_idx, 0);
    }

    #[test]
    fn selection_clamp_empty_repos() {
        let mut sel = TreeSelection {
            level: TreeLevel::Branch,
            repo_idx: 5,
            category_idx: 1,
            branch_idx: 3,
            ..TreeSelection::default()
        };
        sel.clamp(&[]);
        assert_eq!(sel.level, TreeLevel::Repo);
        assert_eq!(sel.repo_idx, 0);
    }

    #[test]
    fn selection_clamp_shrinks_indices() {
        let repos = sample_repos(); // 2 repos
        let mut sel = TreeSelection {
            level: TreeLevel::Repo,
            repo_idx: 10,
            category_idx: 0,
            branch_idx: 0,
            ..TreeSelection::default()
        };
        sel.clamp(&repos);
        assert_eq!(sel.repo_idx, 1); // clamped to max valid
    }

    #[test]
    fn selection_clamp_invalid_category_resets() {
        let repos = sample_repos();
        // beta (idx 1) has no remote branches
        let mut sel = TreeSelection {
            level: TreeLevel::Category,
            repo_idx: 1,
            category_idx: 1, // remote doesn't exist for beta
            branch_idx: 0,
            ..TreeSelection::default()
        };
        sel.clamp(&repos);
        assert_eq!(sel.category_idx, 0); // falls back to local
    }

    #[test]
    fn tree_selected_repo_shows_expanded_chevron() {
        let repos = sample_repos();
        let sel = TreeSelection::default(); // repo 0 selected, expanded by default
        let lines = build_repo_tree_lines(&repos, &sel, 80);
        let first = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(
            first.contains("▾"),
            "expanded repo should have down chevron"
        );
    }

    #[test]
    fn tree_non_selected_repo_shows_expanded_chevron() {
        let repos = sample_repos();
        let sel = TreeSelection::default(); // repo 0 selected, not repo 1
        let lines = build_repo_tree_lines(&repos, &sel, 80);
        let beta_line = lines
            .iter()
            .find(|l| {
                let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                text.contains("beta")
            })
            .expect("should find beta line");
        let text: String = beta_line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("▾"),
            "non-selected expanded repo should have down chevron"
        );
    }

    #[test]
    fn tree_selected_branch_shows_indicator() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos); // -> Category
        sel.descend(&repos); // -> Branch 0 (main)
        let lines = build_repo_tree_lines(&repos, &sel, 80);
        // Branch line for "main" should have indicator
        let main_line = lines
            .iter()
            .find(|l| {
                let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                text.contains("main") && !text.contains("origin") && !text.contains("Branches")
            })
            .expect("should find main branch line");
        let text: String = main_line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("▸"),
            "selected branch should have arrow indicator"
        );
    }

    // ── Agent selection state tests ──────────────────────────────

    fn make_agent(id: &str, pool: &str) -> AgentInfo {
        AgentInfo {
            id: id.to_string(),
            agent_binary: "claude".to_string(),
            started_at: "2026-03-26T10:00:00Z".to_string(),
            attached_clients: 0,
            pool: pool.to_string(),
            working_dir: "/tmp".to_string(),
            repo_path: None,
            branch_name: None,
        }
    }

    fn sample_agents() -> Vec<AgentInfo> {
        vec![
            make_agent("aaa111", "alpha"),
            make_agent("aaa222", "alpha"),
            make_agent("bbb111", "beta"),
        ]
    }

    #[test]
    fn agent_selection_default_is_first() {
        let sel = AgentSelection::default();
        assert_eq!(sel.group_idx, 0);
        assert_eq!(sel.agent_idx, 0);
    }

    #[test]
    fn agent_selection_clamp_empty() {
        let mut sel = AgentSelection {
            group_idx: 5,
            agent_idx: 3,
        };
        sel.clamp(&[], AgentViewMode::ByPool);
        assert_eq!(sel.group_idx, 0);
        assert_eq!(sel.agent_idx, 0);
    }

    #[test]
    fn agent_selection_clamp_shrinks() {
        let agents = sample_agents();
        let mut sel = AgentSelection {
            group_idx: 10,
            agent_idx: 10,
        };
        sel.clamp(&agents, AgentViewMode::ByPool);
        assert_eq!(sel.group_idx, 1); // 2 pools: alpha, beta
        assert_eq!(sel.agent_idx, 0); // beta has 1 agent
    }

    #[test]
    fn agent_selection_move_down_within_pool() {
        let agents = sample_agents();
        let mut sel = AgentSelection::default(); // pool 0 (alpha), agent 0
        sel.move_down(&agents, AgentViewMode::ByPool);
        assert_eq!(sel.agent_idx, 1); // alpha has 2 agents
        sel.move_down(&agents, AgentViewMode::ByPool);
        assert_eq!(sel.agent_idx, 1); // saturates
    }

    #[test]
    fn agent_selection_move_up_within_pool() {
        let agents = sample_agents();
        let mut sel = AgentSelection {
            group_idx: 0,
            agent_idx: 1,
        };
        sel.move_up(&agents, AgentViewMode::ByPool);
        assert_eq!(sel.agent_idx, 0);
        sel.move_up(&agents, AgentViewMode::ByPool);
        assert_eq!(sel.agent_idx, 0); // saturates
    }

    #[test]
    fn agent_selection_next_group() {
        let agents = sample_agents();
        let mut sel = AgentSelection::default();
        sel.next_group(&agents, AgentViewMode::ByPool);
        assert_eq!(sel.group_idx, 1);
        assert_eq!(sel.agent_idx, 0); // reset on group switch
        sel.next_group(&agents, AgentViewMode::ByPool);
        assert_eq!(sel.group_idx, 1); // saturates
    }

    #[test]
    fn agent_selection_prev_group() {
        let agents = sample_agents();
        let mut sel = AgentSelection {
            group_idx: 1,
            agent_idx: 0,
        };
        sel.prev_group(&agents, AgentViewMode::ByPool);
        assert_eq!(sel.group_idx, 0);
        assert_eq!(sel.agent_idx, 0);
        sel.prev_group(&agents, AgentViewMode::ByPool);
        assert_eq!(sel.group_idx, 0); // saturates
    }

    #[test]
    fn agent_selection_next_group_resets_agent_idx() {
        let agents = sample_agents();
        let mut sel = AgentSelection {
            group_idx: 0,
            agent_idx: 1,
        };
        sel.next_group(&agents, AgentViewMode::ByPool);
        assert_eq!(sel.group_idx, 1);
        assert_eq!(sel.agent_idx, 0);
    }

    #[test]
    fn group_names_by_pool_sorted_deduped() {
        let agents = sample_agents();
        let names = group_names(&agents, AgentViewMode::ByPool);
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn group_names_by_repo() {
        let agents = vec![
            AgentInfo {
                repo_path: Some("/home/user/project-a".into()),
                branch_name: Some("main".into()),
                ..make_agent("a1", "default")
            },
            AgentInfo {
                repo_path: Some("/home/user/project-b".into()),
                branch_name: Some("dev".into()),
                ..make_agent("b1", "default")
            },
            AgentInfo {
                repo_path: None,
                branch_name: None,
                ..make_agent("c1", "default")
            },
        ];
        let names = group_names(&agents, AgentViewMode::ByRepo);
        assert_eq!(
            names,
            vec![
                "/home/user/project-a",
                "/home/user/project-b",
                "No repository"
            ]
        );
    }

    // ── Collapse state tests ──────────────────────────────────────

    #[test]
    fn toggle_collapse_repo() {
        let mut sel = TreeSelection::default();
        assert!(!sel.is_repo_collapsed(0));
        sel.toggle_collapse(); // at Repo level
        assert!(sel.is_repo_collapsed(0));
        sel.toggle_collapse(); // toggle back
        assert!(!sel.is_repo_collapsed(0));
    }

    #[test]
    fn toggle_collapse_category() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos); // -> Category
        assert!(!sel.is_category_collapsed(0, 0));
        sel.toggle_collapse();
        assert!(sel.is_category_collapsed(0, 0));
        sel.toggle_collapse();
        assert!(!sel.is_category_collapsed(0, 0));
    }

    #[test]
    fn toggle_collapse_branch_noop() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos); // -> Category
        sel.descend(&repos); // -> Branch
        sel.toggle_collapse(); // should not panic
        assert_eq!(sel.level, TreeLevel::Branch);
    }

    #[test]
    fn tree_collapsed_repo_hides_branches() {
        let repo = make_repo(
            "myrepo",
            vec![make_branch("main", true, 0, false)],
            vec![make_branch("origin/main", false, 0, false)],
        );
        let mut sel = TreeSelection::default();
        sel.toggle_collapse(); // collapse repo 0

        let lines = build_repo_tree_lines(&[repo], &sel, 80);
        // Only the repo header line should remain
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("myrepo"));
        assert!(
            text.contains("▸"),
            "collapsed repo should have right chevron"
        );
    }

    #[test]
    fn tree_collapsed_category_hides_branches() {
        let repo = make_repo(
            "myrepo",
            vec![
                make_branch("main", true, 0, false),
                make_branch("feature", false, 0, false),
            ],
            vec![],
        );
        let mut sel = TreeSelection::default();
        sel.descend(std::slice::from_ref(&repo)); // -> Category
        sel.toggle_collapse(); // collapse local category

        let lines = build_repo_tree_lines(&[repo], &sel, 80);
        // repo header + category header (collapsed, no branches)
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn descend_into_collapsed_is_noop() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.toggle_collapse(); // collapse repo 0
        assert!(sel.is_repo_collapsed(0));
        assert_eq!(sel.level, TreeLevel::Repo);

        sel.descend(&repos); // should be a no-op — Enter required to expand
        assert!(sel.is_repo_collapsed(0));
        assert_eq!(sel.level, TreeLevel::Repo);
    }

    #[test]
    fn ascend_from_repo_is_noop() {
        let mut sel = TreeSelection::default();
        assert!(!sel.is_repo_collapsed(0));
        sel.ascend(); // already at top — no-op, does NOT collapse
        assert!(!sel.is_repo_collapsed(0));
        assert_eq!(sel.level, TreeLevel::Repo);
    }

    // ── Tab navigation tests ──────────────────────────────────────

    #[test]
    fn active_tab_next_cycles() {
        let tab = ActiveTab::Repositories;
        assert_eq!(tab.next(), ActiveTab::Overview);
        assert_eq!(tab.next().next(), ActiveTab::FocusMode);
        assert_eq!(tab.next().next().next(), ActiveTab::Repositories);
    }

    #[test]
    fn active_tab_prev_cycles() {
        let tab = ActiveTab::Repositories;
        assert_eq!(tab.prev(), ActiveTab::FocusMode);
        assert_eq!(tab.prev().prev(), ActiveTab::Overview);
        assert_eq!(tab.prev().prev().prev(), ActiveTab::Repositories);
    }

    #[test]
    fn active_tab_labels() {
        assert_eq!(ActiveTab::Repositories.label(), "Repositories");
        assert_eq!(ActiveTab::Overview.label(), "Overview");
        assert_eq!(ActiveTab::FocusMode.label(), "Focus");
    }

    #[test]
    fn ascend_from_category_goes_to_repo() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos); // -> Category
        assert_eq!(sel.level, TreeLevel::Category);
        sel.ascend(); // always goes to Repo level
        assert_eq!(sel.level, TreeLevel::Repo);
    }
}
