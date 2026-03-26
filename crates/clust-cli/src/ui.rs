use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    layout::{Alignment, Constraint, Flex, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame, Terminal,
};

use clust_ipc::{AgentInfo, CliMessage, PoolMessage, RepoInfo};

use crate::{format::{format_attached, format_started}, ipc, theme, version};

const LOGO_LINES: &[&str] = &[
    "██████╗ ██╗     ██╗   ██╗███████╗████████╗",
    "██╔════╝██║     ██║   ██║██╔════╝╚══██╔══╝",
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

/// Tracks the user's cursor position within the three-level repo tree.
struct TreeSelection {
    level: TreeLevel,
    repo_idx: usize,
    category_idx: usize, // 0 = Local, 1 = Remote
    branch_idx: usize,
}

impl Default for TreeSelection {
    fn default() -> Self {
        Self {
            level: TreeLevel::Repo,
            repo_idx: 0,
            category_idx: 0,
            branch_idx: 0,
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

    /// Right arrow: descend one level deeper.
    fn descend(&mut self, repos: &[RepoInfo]) {
        if repos.is_empty() {
            return;
        }
        match self.level {
            TreeLevel::Repo => {
                let cats = self.visible_categories(repos);
                if !cats.is_empty() {
                    self.level = TreeLevel::Category;
                    self.category_idx = cats[0];
                    self.branch_idx = 0;
                }
            }
            TreeLevel::Category => {
                if self.branch_count(repos) > 0 {
                    self.level = TreeLevel::Branch;
                    self.branch_idx = 0;
                }
            }
            TreeLevel::Branch => {} // already deepest
        }
    }

    /// Left arrow: ascend one level (preserves indices for re-entry).
    fn ascend(&mut self) {
        match self.level {
            TreeLevel::Repo => {}   // already at top
            TreeLevel::Category => self.level = TreeLevel::Repo,
            TreeLevel::Branch => self.level = TreeLevel::Category,
        }
    }
}

pub fn run() -> io::Result<()> {
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
        if let Some(msg) = version::check_brew_update() {
            *notice_clone.lock().unwrap() = Some(msg);
        }
    });

    let mut agents: Vec<AgentInfo> = Vec::new();
    let mut repos: Vec<RepoInfo> = Vec::new();
    let mut selection = TreeSelection::default();
    let mut last_agent_fetch = Instant::now() - Duration::from_secs(10);
    let mut last_repo_fetch = Instant::now() - Duration::from_secs(10);

    loop {
        // Periodically fetch agent list and repo state from pool
        if pool_running && last_agent_fetch.elapsed() >= AGENT_FETCH_INTERVAL {
            agents = fetch_agents();
            last_agent_fetch = Instant::now();
        }
        if pool_running && last_repo_fetch.elapsed() >= AGENT_FETCH_INTERVAL {
            repos = fetch_repos();
            selection.clamp(&repos);
            last_repo_fetch = Instant::now();
        }

        let pool_status = pool_running;
        let notice = update_notice.lock().unwrap().clone();

        terminal.draw(|frame| {
            let area = frame.area();

            // Top-level: content area + status bar
            let [content_area, status_area] =
                Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);

            // Content: left (40%) + right (60%)
            let [left_area, right_area] =
                Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                    .areas(content_area);

            render_left_panel(frame, left_area, &repos, &selection);
            render_right_panel(frame, right_area, &agents);
            render_status_bar(frame, status_area, pool_status, &notice);
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('Q') => {
                            block_on_async(async {
                                if let Ok(mut stream) = ipc::try_connect().await {
                                    let _ = ipc::send_stop(&mut stream).await;
                                }
                            });
                            break;
                        }
                        KeyCode::Up => selection.move_up(&repos),
                        KeyCode::Down => selection.move_down(&repos),
                        KeyCode::Right => selection.descend(&repos),
                        KeyCode::Left => selection.ascend(),
                        _ => {}
                    }
                }
            }
        }
    }

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering functions
// ---------------------------------------------------------------------------

fn render_left_panel(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    repos: &[RepoInfo],
    selection: &TreeSelection,
) {
    let block = Block::bordered()
        .title(Line::from(Span::styled(
            " Repositories ",
            Style::default().fg(theme::R_TEXT_PRIMARY),
        )))
        .border_style(Style::default().fg(theme::R_TEXT_TERTIARY));

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
        let lines = build_repo_tree_lines(repos, selection, inner.width);
        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }
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

        // Repo name header
        let (indicator, name_color, bg) = if repo_selected {
            ("▸ ", theme::R_ACCENT_BRIGHT, Some(theme::R_BG_HOVER))
        } else if repo_open {
            ("▸ ", theme::R_ACCENT_DIM, None)
        } else {
            ("  ", theme::R_ACCENT, None)
        };

        let text = format!(" {indicator}{}", repo.name);
        let mut style = Style::default().fg(name_color);
        if let Some(bg_color) = bg {
            style = style.bg(bg_color);
        }
        lines.push(pad_line(vec![Span::styled(text, style)], width, bg));

        let has_local = !repo.local_branches.is_empty();
        let has_remote = !repo.remote_branches.is_empty();

        // Local Branches section
        if has_local {
            let cat_selected =
                is_this_repo && selection.level == TreeLevel::Category && selection.category_idx == 0;
            let cat_open =
                is_this_repo && selection.level == TreeLevel::Branch && selection.category_idx == 0;

            let connector = if has_remote { "├─" } else { "└─" };
            let (cat_indicator, cat_fg, cat_bg) = if cat_selected {
                ("▸ ", theme::R_TEXT_PRIMARY, Some(theme::R_BG_HOVER))
            } else if cat_open {
                ("▸ ", theme::R_TEXT_TERTIARY, None)
            } else {
                ("  ", theme::R_TEXT_SECONDARY, None)
            };

            let cat_text = format!("   {connector} {cat_indicator}Local Branches");
            let mut cat_style = Style::default().fg(cat_fg);
            if let Some(bg_color) = cat_bg {
                cat_style = cat_style.bg(bg_color);
            }
            lines.push(pad_line(vec![Span::styled(cat_text, cat_style)], width, cat_bg));

            let continuation = if has_remote { "│" } else { " " };
            for (i, branch) in repo.local_branches.iter().enumerate() {
                let is_last = i == repo.local_branches.len() - 1;
                let branch_connector = if is_last { "└─" } else { "├─" };
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

        // Remote Branches section
        if has_remote {
            let cat_selected =
                is_this_repo && selection.level == TreeLevel::Category && selection.category_idx == 1;
            let cat_open =
                is_this_repo && selection.level == TreeLevel::Branch && selection.category_idx == 1;

            let (cat_indicator, cat_fg, cat_bg) = if cat_selected {
                ("▸ ", theme::R_TEXT_PRIMARY, Some(theme::R_BG_HOVER))
            } else if cat_open {
                ("▸ ", theme::R_TEXT_TERTIARY, None)
            } else {
                ("  ", theme::R_TEXT_SECONDARY, None)
            };

            let cat_text = format!("   └─ {cat_indicator}Remote Branches");
            let mut cat_style = Style::default().fg(cat_fg);
            if let Some(bg_color) = cat_bg {
                cat_style = cat_style.bg(bg_color);
            }
            lines.push(pad_line(vec![Span::styled(cat_text, cat_style)], width, cat_bg));

            for (i, branch) in repo.remote_branches.iter().enumerate() {
                let is_last = i == repo.remote_branches.len() - 1;
                let branch_connector = if is_last { "└─" } else { "├─" };
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

        // Blank line between repos (not after last)
        if repo_idx < repos.len() - 1 {
            lines.push(Line::from(""));
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
    if branch.active_agent_id.is_some() {
        let mut dot_style = Style::default().fg(theme::R_SUCCESS);
        if let Some(bg_color) = bg {
            dot_style = dot_style.bg(bg_color);
        }
        spans.push(Span::styled("● ".to_string(), dot_style));
    }

    let name_color = if is_selected || branch.is_head {
        theme::R_ACCENT_BRIGHT
    } else {
        theme::R_TEXT_PRIMARY
    };
    let mut name_style = Style::default().fg(name_color);
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
    area: ratatui::layout::Rect,
    agents: &[AgentInfo],
) {
    if agents.is_empty() {
        render_logo(frame, area);
    } else {
        render_agent_list(frame, area, agents);
    }
}

fn render_logo(frame: &mut Frame, area: ratatui::layout::Rect) {
    let mut lines: Vec<Line> = Vec::new();

    // Top border
    lines.push(Line::from(Span::styled(
        "┌──────────────────────────────────────────────┐",
        Style::default().fg(theme::R_TEXT_TERTIARY),
    )));

    // Empty line inside box
    lines.push(boxed_line(vec![Span::raw(
        "                                              ",
    )]));

    // Logo lines with accent colors
    for (i, text) in LOGO_LINES.iter().enumerate() {
        let color = if i == 2 || i == 3 {
            theme::R_ACCENT_BRIGHT
        } else {
            theme::R_ACCENT
        };
        let padded = format!("  {:<44}", text);
        lines.push(boxed_line(vec![Span::styled(
            padded,
            Style::default().fg(color),
        )]));
    }

    // Empty line
    lines.push(boxed_line(vec![Span::raw(
        "                                              ",
    )]));

    // Gradient bar
    lines.push(boxed_line(vec![
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

    // Bottom border
    lines.push(Line::from(Span::styled(
        "└──────────────────────────────────────────────┘",
        Style::default().fg(theme::R_TEXT_TERTIARY),
    )));

    let block_height = lines.len() as u16;
    let block_width = 48u16;

    let [vert_area] = Layout::vertical([Constraint::Length(block_height)])
        .flex(Flex::Center)
        .areas(area);

    let [horz_area] = Layout::horizontal([Constraint::Length(block_width)])
        .flex(Flex::Center)
        .areas(vert_area);

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, horz_area);
}

fn render_agent_list(frame: &mut Frame, area: ratatui::layout::Rect, agents: &[AgentInfo]) {
    let block = Block::bordered()
        .title(Line::from(Span::styled(
            " Agents ",
            Style::default().fg(theme::R_TEXT_PRIMARY),
        )))
        .border_style(Style::default().fg(theme::R_TEXT_TERTIARY));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Group agents by pool (sorted)
    let mut sorted: Vec<&AgentInfo> = agents.iter().collect();
    sorted.sort_by(|a, b| a.pool.cmp(&b.pool).then(a.started_at.cmp(&b.started_at)));

    let mut pool_names: Vec<&str> = sorted.iter().map(|a| a.pool.as_str()).collect();
    pool_names.dedup();

    // Build layout: for each pool, 1 row header + 4 rows per agent card
    let mut constraints: Vec<Constraint> = Vec::new();
    for pool_name in &pool_names {
        constraints.push(Constraint::Length(1)); // pool header
        let count = sorted.iter().filter(|a| a.pool == *pool_name).count();
        for _ in 0..count {
            constraints.push(Constraint::Length(4)); // agent card
        }
    }
    constraints.push(Constraint::Min(0)); // absorb remaining space

    let areas = Layout::vertical(constraints).split(inner);

    let mut area_idx = 0;
    for pool_name in &pool_names {
        // Pool header
        let header = Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {pool_name}"),
                Style::default().fg(theme::R_ACCENT),
            ),
        ]));
        frame.render_widget(header, areas[area_idx]);
        area_idx += 1;

        // Agent cards for this pool
        for agent in sorted.iter().filter(|a| a.pool == *pool_name) {
            render_agent_card(frame, areas[area_idx], agent);
            area_idx += 1;
        }
    }
}

fn render_agent_card(frame: &mut Frame, area: ratatui::layout::Rect, agent: &AgentInfo) {
    let block = Block::bordered()
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(&agent.id, Style::default().fg(theme::R_ACCENT)),
            Span::raw(" "),
        ]))
        .border_style(Style::default().fg(theme::R_TEXT_TERTIARY));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let started = format_started(&agent.started_at);
    let attached = format_attached(agent.attached_clients);

    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!(" {}", &agent.agent_binary),
                Style::default().fg(theme::R_TEXT_PRIMARY),
            ),
            Span::raw("  "),
            Span::styled("● running", Style::default().fg(theme::R_SUCCESS)),
        ]),
        Line::from(vec![
            Span::styled(
                format!(" started {started}"),
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
    area: ratatui::layout::Rect,
    pool_running: bool,
    update_notice: &Option<String>,
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
        Span::styled(
            "  ",
            Style::default().bg(theme::R_BG_RAISED),
        ),
        Span::styled(
            "q to quit",
            Style::default()
                .fg(theme::R_TEXT_TERTIARY)
                .bg(theme::R_BG_RAISED),
        ),
        Span::styled(
            "  ",
            Style::default().bg(theme::R_BG_RAISED),
        ),
        Span::styled(
            "Q to quit and stop pool",
            Style::default()
                .fg(theme::R_TEXT_TERTIARY)
                .bg(theme::R_BG_RAISED),
        ),
        Span::styled(
            "  ",
            Style::default().bg(theme::R_BG_RAISED),
        ),
        Span::styled(
            "↑↓←→ navigate",
            Style::default()
                .fg(theme::R_TEXT_TERTIARY)
                .bg(theme::R_BG_RAISED),
        ),
    ];

    if let Some(ref msg) = *update_notice {
        left_spans.push(Span::styled(
            "  ",
            Style::default().bg(theme::R_BG_RAISED),
        ));
        left_spans.push(Span::styled(
            msg.clone(),
            Style::default()
                .fg(theme::R_WARNING)
                .bg(theme::R_BG_RAISED),
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

    let [left_area, right_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(version_width),
    ])
    .areas(area);

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

/// Wraps inner spans in box-drawing border characters.
fn boxed_line<'a>(inner: Vec<Span<'a>>) -> Line<'a> {
    let border = Style::default().fg(theme::R_TEXT_TERTIARY);
    let mut spans = vec![Span::styled("│", border)];
    spans.extend(inner);
    spans.push(Span::styled("│", border));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boxed_line_wraps_single_span() {
        let line = boxed_line(vec![Span::raw("hello")]);
        assert_eq!(line.spans.len(), 3); // │ + hello + │
        assert_eq!(line.spans[0].content, "│");
        assert_eq!(line.spans[1].content, "hello");
        assert_eq!(line.spans[2].content, "│");
    }

    #[test]
    fn boxed_line_wraps_multiple_spans() {
        let line = boxed_line(vec![Span::raw("a"), Span::raw("b"), Span::raw("c")]);
        assert_eq!(line.spans.len(), 5); // │ + a + b + c + │
        assert_eq!(line.spans[0].content, "│");
        assert_eq!(line.spans[1].content, "a");
        assert_eq!(line.spans[2].content, "b");
        assert_eq!(line.spans[3].content, "c");
        assert_eq!(line.spans[4].content, "│");
    }

    #[test]
    fn boxed_line_empty_inner() {
        let line = boxed_line(vec![]);
        assert_eq!(line.spans.len(), 2); // just │ │
        assert_eq!(line.spans[0].content, "│");
        assert_eq!(line.spans[1].content, "│");
    }

    // ── Repository tree rendering tests ──────────────────────────

    fn make_branch(name: &str, is_head: bool, agent_id: Option<&str>, is_worktree: bool) -> clust_ipc::BranchInfo {
        clust_ipc::BranchInfo {
            name: name.to_string(),
            is_head,
            active_agent_id: agent_id.map(|s| s.to_string()),
            is_worktree,
        }
    }

    fn make_repo(name: &str, local: Vec<clust_ipc::BranchInfo>, remote: Vec<clust_ipc::BranchInfo>) -> clust_ipc::RepoInfo {
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
                make_branch("main", true, None, false),
                make_branch("feature", false, None, false),
            ],
            vec![],
        );
        let sel = TreeSelection::default();
        let lines = build_repo_tree_lines(&[repo], &sel, 80);

        // Should have: repo name + "Local Branches" header + 2 branch lines
        assert_eq!(lines.len(), 4);

        // First line is repo name
        let first = lines[0].spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert!(first.contains("myrepo"));

        // Second line is section header
        let second = lines[1].spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert!(second.contains("Local Branches"));
    }

    #[test]
    fn tree_repo_with_local_and_remote() {
        let repo = make_repo(
            "myrepo",
            vec![make_branch("main", true, None, false)],
            vec![make_branch("origin/main", false, None, false)],
        );
        let sel = TreeSelection::default();
        let lines = build_repo_tree_lines(&[repo], &sel, 80);

        // repo name + local header + 1 local branch + remote header + 1 remote branch
        assert_eq!(lines.len(), 5);

        let texts: Vec<String> = lines.iter().map(|l| {
            l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
        }).collect();

        assert!(texts[0].contains("myrepo"));
        assert!(texts[1].contains("Local Branches"));
        assert!(texts[2].contains("main"));
        assert!(texts[3].contains("Remote Branches"));
        assert!(texts[4].contains("origin/main"));
    }

    #[test]
    fn tree_multiple_repos_separated_by_blank_line() {
        let repos = vec![
            make_repo("alpha", vec![make_branch("main", true, None, false)], vec![]),
            make_repo("beta", vec![make_branch("main", true, None, false)], vec![]),
        ];
        let sel = TreeSelection::default();
        let lines = build_repo_tree_lines(&repos, &sel, 80);

        // alpha: name + header + branch = 3
        // blank line = 1
        // beta: name + header + branch = 3
        assert_eq!(lines.len(), 7);

        // Line 3 (index 3) should be the blank separator
        let blank = lines[3].spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert!(blank.trim().is_empty());
    }

    #[test]
    fn format_branch_line_shows_agent_indicator() {
        let branch = make_branch("main", false, Some("abc123"), false);
        let line = format_branch_line(&branch, "│", "├─", false, 80);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("●"), "should have active agent indicator");
        assert!(text.contains("main"));
    }

    #[test]
    fn format_branch_line_no_agent_indicator() {
        let branch = make_branch("main", false, None, false);
        let line = format_branch_line(&branch, "│", "├─", false, 80);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("●"), "should not have agent indicator");
    }

    #[test]
    fn format_branch_line_shows_worktree_indicator() {
        let branch = make_branch("feature", false, None, true);
        let line = format_branch_line(&branch, " ", "└─", false, 80);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("⎇"), "should have worktree indicator");
    }

    #[test]
    fn format_branch_line_no_worktree_indicator() {
        let branch = make_branch("feature", false, None, false);
        let line = format_branch_line(&branch, " ", "└─", false, 80);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("⎇"), "should not have worktree indicator");
    }

    #[test]
    fn format_branch_line_head_and_agent_and_worktree() {
        let branch = make_branch("main", true, Some("abc123"), true);
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
                    make_branch("main", true, None, false),
                    make_branch("dev", false, None, false),
                ],
                vec![make_branch("origin/main", false, None, false)],
            ),
            make_repo(
                "beta",
                vec![make_branch("main", true, None, false)],
                vec![],
            ),
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
        sel.ascend();        // -> Category
        assert_eq!(sel.level, TreeLevel::Category);
        sel.ascend();        // -> Repo
        assert_eq!(sel.level, TreeLevel::Repo);
        sel.ascend();        // no-op
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
        let mut sel = TreeSelection { repo_idx: 1, ..TreeSelection::default() };
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
        };
        sel.clamp(&repos);
        assert_eq!(sel.category_idx, 0); // falls back to local
    }

    #[test]
    fn tree_selected_repo_shows_indicator() {
        let repos = sample_repos();
        let sel = TreeSelection::default(); // repo 0 selected
        let lines = build_repo_tree_lines(&repos, &sel, 80);
        let first = lines[0].spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert!(first.contains("▸"), "selected repo should have arrow indicator");
    }

    #[test]
    fn tree_non_selected_repo_no_indicator() {
        let repos = sample_repos();
        let sel = TreeSelection::default(); // repo 0 selected, not repo 1
        let lines = build_repo_tree_lines(&repos, &sel, 80);
        // Find the beta repo line (after blank separator)
        let beta_line = lines.iter().find(|l| {
            let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            text.contains("beta")
        }).expect("should find beta line");
        let text: String = beta_line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("▸"), "non-selected repo should not have arrow indicator");
    }

    #[test]
    fn tree_selected_branch_shows_indicator() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos); // -> Category
        sel.descend(&repos); // -> Branch 0 (main)
        let lines = build_repo_tree_lines(&repos, &sel, 80);
        // Branch line for "main" should have indicator
        let main_line = lines.iter().find(|l| {
            let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            text.contains("main") && !text.contains("origin") && !text.contains("Branches")
        }).expect("should find main branch line");
        let text: String = main_line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("▸"), "selected branch should have arrow indicator");
    }
}
