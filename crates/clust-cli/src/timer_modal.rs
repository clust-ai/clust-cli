use chrono::TimeZone;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
    Frame,
};

use crate::theme;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub enum TimerResult {
    Pending,
    Cancelled,
    /// The RFC 3339 timestamp string for the scheduled start time.
    Completed(String),
}

// ---------------------------------------------------------------------------
// Modal state
// ---------------------------------------------------------------------------

pub struct TimerModal {
    input: String,
    cursor_pos: usize,
    label: String,
    parsed_preview: Option<String>,
    error: Option<String>,
}

impl TimerModal {
    pub fn new(batch_title: String) -> Self {
        Self {
            input: String::new(),
            cursor_pos: 0,
            label: format!("Set Timer \u{2014} {}", batch_title),
            parsed_preview: None,
            error: None,
        }
    }

    // -----------------------------------------------------------------------
    // Parsing
    // -----------------------------------------------------------------------

    /// Try to parse the input as a duration or absolute time.
    /// Returns `(rfc3339_string, preview_text)` on success.
    fn try_parse(&self) -> Result<(String, String), String> {
        let input = self.input.trim();
        if input.is_empty() {
            return Err("Enter a duration (2h, 30m) or time (16:00)".to_string());
        }

        // Try duration first: patterns like "2h", "30m", "1h30m", "90m"
        if let Some(dur) = parse_duration(input) {
            if dur.as_secs() == 0 {
                return Err("Duration must be greater than zero".to_string());
            }
            let start = chrono::Utc::now() + dur;
            let local = start.with_timezone(&chrono::Local);
            let rfc = start.to_rfc3339();
            let preview = format!(
                "Starts at {} (in {})",
                local.format("%H:%M"),
                format_duration_short(dur),
            );
            return Ok((rfc, preview));
        }

        // Try absolute time: "16:00", "9:30"
        if let Some(start) = parse_time_of_day(input) {
            let now = chrono::Utc::now();
            let remaining = start.signed_duration_since(now);
            let dur = if remaining.num_seconds() > 0 {
                std::time::Duration::from_secs(remaining.num_seconds() as u64)
            } else {
                std::time::Duration::from_secs(0)
            };
            let local = start.with_timezone(&chrono::Local);
            let rfc = start.to_rfc3339();
            let preview = if dur.as_secs() > 0 {
                format!(
                    "Starts at {} (in {})",
                    local.format("%H:%M"),
                    format_duration_short(dur),
                )
            } else {
                format!("Starts at {} (immediately)", local.format("%H:%M"))
            };
            return Ok((rfc, preview));
        }

        Err("Invalid format. Use duration (2h, 30m) or time (16:00)".to_string())
    }

    fn update_preview(&mut self) {
        match self.try_parse() {
            Ok((_, preview)) => {
                self.parsed_preview = Some(preview);
                self.error = None;
            }
            Err(msg) => {
                self.parsed_preview = None;
                self.error = Some(msg);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Key handling
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> TimerResult {
        match key.code {
            KeyCode::Esc => TimerResult::Cancelled,
            KeyCode::Enter => match self.try_parse() {
                Ok((rfc, _)) => TimerResult::Completed(rfc),
                Err(msg) => {
                    self.error = Some(msg);
                    TimerResult::Pending
                }
            },
            KeyCode::Backspace => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.remove(self.cursor_pos);
                    self.update_preview();
                }
                TimerResult::Pending
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                TimerResult::Pending
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos += self.input[self.cursor_pos..]
                        .chars()
                        .next()
                        .unwrap()
                        .len_utf8();
                }
                TimerResult::Pending
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    return TimerResult::Pending;
                }
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                self.update_preview();
                TimerResult::Pending
            }
            _ => TimerResult::Pending,
        }
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let modal_width = 60u16.min(area.width.saturating_sub(4));
        let modal_height = 8u16.min(area.height.saturating_sub(2));

        let [_, modal_h_area, _] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(modal_width),
            Constraint::Fill(1),
        ])
        .areas(area);

        let [_, modal_area, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(modal_height),
            Constraint::Fill(1),
        ])
        .areas(modal_h_area);

        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::R_ACCENT_DIM))
            .title(Span::styled(
                format!(" {} ", self.label),
                Style::default()
                    .fg(theme::R_ACCENT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(theme::R_BG_OVERLAY))
            .padding(Padding::new(1, 1, 0, 0));

        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let [hint_area, input_area, preview_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(1),
        ])
        .areas(inner);

        // Hint
        frame.render_widget(
            Paragraph::new(Span::styled(
                "Duration (2h, 30m, 1h30m) or time (16:00). Enter to set, Esc to cancel",
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )),
            hint_area,
        );

        // Input field
        self.render_input(frame, input_area);

        // Preview or error
        if let Some(ref preview) = self.parsed_preview {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    preview.as_str(),
                    Style::default().fg(theme::R_SUCCESS),
                )),
                preview_area,
            );
        } else if let Some(ref err) = self.error {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    err.as_str(),
                    Style::default().fg(theme::R_TEXT_TERTIARY),
                )),
                preview_area,
            );
        }
    }

    fn render_input(&self, frame: &mut Frame, area: Rect) {
        let before_cursor = &self.input[..self.cursor_pos];
        let (cursor_char, after_cursor) = if self.cursor_pos < self.input.len() {
            let ch_len = self.input[self.cursor_pos..]
                .chars()
                .next()
                .unwrap()
                .len_utf8();
            (
                &self.input[self.cursor_pos..self.cursor_pos + ch_len],
                &self.input[self.cursor_pos + ch_len..],
            )
        } else {
            (" ", "")
        };

        let line = Line::from(vec![
            Span::styled(
                "> ",
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
            Paragraph::new(line)
                .style(Style::default().bg(theme::R_BG_INPUT))
                .wrap(Wrap { trim: false }),
            area,
        );
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Parse a duration string like "2h", "30m", "1h30m", "90m", "45s".
fn parse_duration(input: &str) -> Option<std::time::Duration> {
    let input = input.trim().to_lowercase();
    let mut total_secs: u64 = 0;
    let mut num_buf = String::new();
    let mut found_unit = false;

    for ch in input.chars() {
        if ch.is_ascii_digit() {
            num_buf.push(ch);
        } else {
            let val: u64 = num_buf.parse().ok()?;
            num_buf.clear();
            match ch {
                'h' => total_secs += val * 3600,
                'm' => total_secs += val * 60,
                's' => total_secs += val,
                _ => return None,
            }
            found_unit = true;
        }
    }

    // Handle bare number without unit (treat as minutes)
    if !num_buf.is_empty() {
        if found_unit {
            return None; // trailing digits after a unit like "2h30"
        }
        // bare number = minutes
        let val: u64 = num_buf.parse().ok()?;
        total_secs += val * 60;
        found_unit = true;
    }

    if found_unit && total_secs > 0 {
        Some(std::time::Duration::from_secs(total_secs))
    } else {
        None
    }
}

/// Parse a time-of-day string like "16:00", "9:30".
/// Returns the next occurrence of that time (today if still ahead, tomorrow if past).
fn parse_time_of_day(input: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let parts: Vec<&str> = input.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let hour: u32 = parts[0].parse().ok()?;
    let minute: u32 = parts[1].parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }

    let local_now = chrono::Local::now();
    let today = local_now.date_naive();
    let target_time = chrono::NaiveTime::from_hms_opt(hour, minute, 0)?;
    let mut target_dt = today.and_time(target_time);

    // If the time has already passed today, schedule for tomorrow
    if target_dt <= local_now.naive_local() {
        target_dt += chrono::Duration::days(1);
    }

    let local_tz = local_now.timezone();
    let local_dt = local_tz.from_local_datetime(&target_dt).single()?;
    Some(local_dt.with_timezone(&chrono::Utc))
}

/// Format a duration as a short human-readable string like "1h 30m" or "45m".
fn format_duration_short(dur: std::time::Duration) -> String {
    let total_secs = dur.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;

    if hours > 0 && minutes > 0 {
        format!("{}h {}m", hours, minutes)
    } else if hours > 0 {
        format!("{}h", hours)
    } else if minutes > 0 {
        format!("{}m", minutes)
    } else {
        format!("{}s", total_secs)
    }
}

/// Format a countdown from now until a target RFC 3339 timestamp.
/// Returns something like "1h 23m" or "starts at 16:00".
pub fn format_countdown(scheduled_at: &str) -> String {
    let target = match chrono::DateTime::parse_from_rfc3339(scheduled_at) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(_) => return scheduled_at.to_string(),
    };
    let now = chrono::Utc::now();
    let remaining = target.signed_duration_since(now);

    if remaining.num_seconds() <= 0 {
        return "starting...".to_string();
    }

    let total_secs = remaining.num_seconds() as u64;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;

    let local = target.with_timezone(&chrono::Local);
    let time_str = local.format("%H:%M").to_string();

    if hours > 0 {
        format!("{} ({}h {}m)", time_str, hours, minutes)
    } else if minutes > 0 {
        format!("{} ({}m)", time_str, minutes)
    } else {
        format!("{} (<1m)", time_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_hours() {
        assert_eq!(parse_duration("2h").unwrap().as_secs(), 7200);
    }

    #[test]
    fn parse_duration_minutes() {
        assert_eq!(parse_duration("30m").unwrap().as_secs(), 1800);
    }

    #[test]
    fn parse_duration_combined() {
        assert_eq!(parse_duration("1h30m").unwrap().as_secs(), 5400);
    }

    #[test]
    fn parse_duration_seconds() {
        assert_eq!(parse_duration("45s").unwrap().as_secs(), 45);
    }

    #[test]
    fn parse_duration_bare_number_as_minutes() {
        assert_eq!(parse_duration("90").unwrap().as_secs(), 5400);
    }

    #[test]
    fn parse_duration_zero_returns_none() {
        assert!(parse_duration("0").is_none());
    }

    #[test]
    fn parse_duration_invalid() {
        assert!(parse_duration("abc").is_none());
    }

    #[test]
    fn parse_time_valid() {
        let result = parse_time_of_day("16:00");
        assert!(result.is_some());
    }

    #[test]
    fn parse_time_invalid_hour() {
        assert!(parse_time_of_day("25:00").is_none());
    }

    #[test]
    fn parse_time_invalid_format() {
        assert!(parse_time_of_day("1600").is_none());
    }
}
