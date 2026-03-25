use std::io;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    layout::{Constraint, Flex, Layout},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Terminal,
};

use crate::theme;

const LOGO_LINES: &[&str] = &[
    "██████╗██╗     ██╗   ██╗███████╗████████╗",
    "██╔════╝██║     ██║   ██║██╔════╝╚══██╔══╝",
    "██║     ██║     ██║   ██║███████╗   ██║   ",
    "██║     ██║     ██║   ██║╚════██║   ██║   ",
    "╚██████╗███████╗╚██████╔╝███████║   ██║   ",
    " ╚═════╝╚══════╝ ╚═════╝ ╚══════╝   ╚═╝   ",
];

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

    loop {
        terminal.draw(|frame| {
            let area = frame.area();

            // Build styled lines
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

            // Blank line
            lines.push(Line::raw(""));

            // "press q to quit"
            lines.push(Line::from(Span::styled(
                "press q to quit",
                Style::default().fg(theme::R_TEXT_SECONDARY),
            )).centered());

            let block_height = lines.len() as u16;
            let [vert_area] = Layout::vertical([Constraint::Length(block_height)])
                .flex(Flex::Center)
                .areas(area);

            let paragraph = Paragraph::new(lines).centered();
            frame.render_widget(paragraph, vert_area);
        })?;

        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
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

/// Wraps inner spans in box-drawing border characters.
fn boxed_line<'a>(inner: Vec<Span<'a>>) -> Line<'a> {
    let border = Style::default().fg(theme::R_TEXT_TERTIARY);
    let mut spans = vec![Span::styled("│", border)];
    spans.extend(inner);
    spans.push(Span::styled("│", border));
    Line::from(spans)
}
