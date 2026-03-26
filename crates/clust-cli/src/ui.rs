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
    style::Style,
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

            render_left_panel(frame, left_area, &repos);
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

fn render_left_panel(frame: &mut Frame, area: ratatui::layout::Rect, repos: &[RepoInfo]) {
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
        let lines = build_repo_tree_lines(repos);
        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }
}

fn build_repo_tree_lines(repos: &[RepoInfo]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    for (repo_idx, repo) in repos.iter().enumerate() {
        // Repo name header
        lines.push(Line::from(Span::styled(
            format!(" {}", repo.name),
            Style::default().fg(theme::R_ACCENT),
        )));

        let has_local = !repo.local_branches.is_empty();
        let has_remote = !repo.remote_branches.is_empty();

        // Local Branches section
        if has_local {
            let connector = if has_remote { "├─" } else { "└─" };
            lines.push(Line::from(Span::styled(
                format!("   {connector} Local Branches"),
                Style::default().fg(theme::R_TEXT_SECONDARY),
            )));

            let continuation = if has_remote { "│" } else { " " };
            for (i, branch) in repo.local_branches.iter().enumerate() {
                let is_last = i == repo.local_branches.len() - 1;
                let branch_connector = if is_last { "└─" } else { "├─" };
                lines.push(format_branch_line(branch, continuation, branch_connector));
            }
        }

        // Remote Branches section
        if has_remote {
            lines.push(Line::from(Span::styled(
                "   └─ Remote Branches".to_string(),
                Style::default().fg(theme::R_TEXT_SECONDARY),
            )));

            for (i, branch) in repo.remote_branches.iter().enumerate() {
                let is_last = i == repo.remote_branches.len() - 1;
                let branch_connector = if is_last { "└─" } else { "├─" };
                lines.push(format_branch_line(branch, " ", branch_connector));
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
) -> Line<'static> {
    let mut spans = Vec::new();

    // Tree structure prefix
    spans.push(Span::styled(
        format!("   {continuation}  {connector} "),
        Style::default().fg(theme::R_TEXT_TERTIARY),
    ));

    // Active agent indicator
    if branch.active_agent_id.is_some() {
        spans.push(Span::styled(
            "● ".to_string(),
            Style::default().fg(theme::R_SUCCESS),
        ));
    }

    // Branch name — head branch is highlighted
    let name_color = if branch.is_head {
        theme::R_ACCENT_BRIGHT
    } else {
        theme::R_TEXT_PRIMARY
    };
    spans.push(Span::styled(
        branch.name.clone(),
        Style::default().fg(name_color),
    ));

    // Worktree indicator
    if branch.is_worktree {
        spans.push(Span::styled(
            " ⎇".to_string(),
            Style::default().fg(theme::R_TEXT_SECONDARY),
        ));
    }

    Line::from(spans)
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
        let lines = build_repo_tree_lines(&[]);
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
        let lines = build_repo_tree_lines(&[repo]);

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
        let lines = build_repo_tree_lines(&[repo]);

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
        let lines = build_repo_tree_lines(&repos);

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
        let line = format_branch_line(&branch, "│", "├─");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("●"), "should have active agent indicator");
        assert!(text.contains("main"));
    }

    #[test]
    fn format_branch_line_no_agent_indicator() {
        let branch = make_branch("main", false, None, false);
        let line = format_branch_line(&branch, "│", "├─");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("●"), "should not have agent indicator");
    }

    #[test]
    fn format_branch_line_shows_worktree_indicator() {
        let branch = make_branch("feature", false, None, true);
        let line = format_branch_line(&branch, " ", "└─");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("⎇"), "should have worktree indicator");
    }

    #[test]
    fn format_branch_line_no_worktree_indicator() {
        let branch = make_branch("feature", false, None, false);
        let line = format_branch_line(&branch, " ", "└─");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("⎇"), "should not have worktree indicator");
    }

    #[test]
    fn format_branch_line_head_and_agent_and_worktree() {
        let branch = make_branch("main", true, Some("abc123"), true);
        let line = format_branch_line(&branch, "│", "├─");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("●"), "agent indicator");
        assert!(text.contains("main"), "branch name");
        assert!(text.contains("⎇"), "worktree indicator");
    }
}
