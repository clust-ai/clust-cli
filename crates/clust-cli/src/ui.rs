use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
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

use clust_ipc::{AgentInfo, CliMessage, PoolMessage};

use crate::{ipc, theme, version};

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

    let mut pool_running = block_on_async(async { ipc::try_connect().await.is_ok() });

    let update_notice: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let notice_clone = update_notice.clone();
    std::thread::spawn(move || {
        if let Some(msg) = version::check_brew_update() {
            *notice_clone.lock().unwrap() = Some(msg);
        }
    });

    let mut agents: Vec<AgentInfo> = Vec::new();
    let mut last_agent_fetch = Instant::now() - Duration::from_secs(10);

    loop {
        // Periodically fetch agent list from pool
        if pool_running && last_agent_fetch.elapsed() >= AGENT_FETCH_INTERVAL {
            agents = fetch_agents();
            last_agent_fetch = Instant::now();
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

            render_left_panel(frame, left_area);
            render_right_panel(frame, right_area, &agents);
            render_status_bar(frame, status_area, pool_status, &notice);
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('S') => {
                            pool_running = block_on_async(async {
                                if pool_running {
                                    if let Ok(mut stream) = ipc::try_connect().await {
                                        let _ = ipc::send_stop(&mut stream).await;
                                    }
                                    false
                                } else {
                                    ipc::connect_to_pool().await.is_ok()
                                }
                            });
                            // Clear agents and force re-fetch on next cycle
                            agents.clear();
                            last_agent_fetch = Instant::now() - Duration::from_secs(10);
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

fn render_left_panel(frame: &mut Frame, area: ratatui::layout::Rect) {
    let block = Block::bordered()
        .title(Line::from(Span::styled(
            " Agents ",
            Style::default().fg(theme::R_TEXT_PRIMARY),
        )))
        .border_style(Style::default().fg(theme::R_TEXT_TERTIARY));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = Paragraph::new(Line::from(Span::styled(
        "No agents running",
        Style::default().fg(theme::R_TEXT_TERTIARY),
    )))
    .alignment(Alignment::Center);

    // Center vertically
    let [centered] = Layout::vertical([Constraint::Length(1)])
        .flex(Flex::Center)
        .areas(inner);

    frame.render_widget(text, centered);
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

    // Each agent card is 4 rows tall
    let mut constraints: Vec<Constraint> = agents
        .iter()
        .map(|_| Constraint::Length(4))
        .collect();
    constraints.push(Constraint::Min(0)); // absorb remaining space

    let card_areas = Layout::vertical(constraints).split(inner);

    for (i, agent) in agents.iter().enumerate() {
        render_agent_card(frame, card_areas[i], agent);
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
    let (dot_color, status_label, toggle_hint) = if pool_running {
        (theme::R_SUCCESS, "connected", "S to stop")
    } else {
        (theme::R_TEXT_TERTIARY, "disconnected", "S to start")
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
            toggle_hint,
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
        if clust_ipc::send_message(&mut stream, &CliMessage::ListAgents)
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

/// Format an RFC 3339 timestamp into a short human-readable string.
fn format_started(rfc3339: &str) -> String {
    let Ok(dt) = rfc3339.parse::<DateTime<Utc>>() else {
        return rfc3339.to_string();
    };
    let local = dt.with_timezone(&Local);
    let now = Local::now();
    if local.date_naive() == now.date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%b %d %H:%M").to_string()
    }
}

/// Format an attached client count.
fn format_attached(count: usize) -> String {
    if count == 1 {
        "1 terminal".to_string()
    } else {
        format!("{count} terminals")
    }
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

    #[test]
    fn format_attached_singular() {
        assert_eq!(format_attached(1), "1 terminal");
    }

    #[test]
    fn format_attached_plural() {
        assert_eq!(format_attached(0), "0 terminals");
        assert_eq!(format_attached(3), "3 terminals");
    }

    #[test]
    fn format_started_today_shows_time_only() {
        // Build an RFC3339 timestamp for "today" in UTC
        let now = Utc::now();
        let ts = now.to_rfc3339();
        let result = format_started(&ts);
        // Should be HH:MM format (5 chars)
        let local = now.with_timezone(&Local);
        let expected = local.format("%H:%M").to_string();
        assert_eq!(result, expected);
    }

    #[test]
    fn format_started_other_day_shows_date_and_time() {
        let result = format_started("2025-01-15T10:30:00Z");
        // Should include month, day, and time
        assert!(result.contains("Jan"));
        assert!(result.contains("15"));
    }

    #[test]
    fn format_started_invalid_returns_original() {
        assert_eq!(format_started("not-a-date"), "not-a-date");
        assert_eq!(format_started(""), "");
    }
}
