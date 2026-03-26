use chrono::{DateTime, Local, Utc};

/// Format an attachment count into a human-readable string.
pub fn format_attached(count: usize) -> String {
    if count == 1 {
        "1 terminal".to_string()
    } else {
        format!("{count} terminals")
    }
}

/// Format an RFC 3339 timestamp into a human-readable relative time string.
pub fn format_started(rfc3339: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let now = Utc::now();
        let ts = now.to_rfc3339();
        let result = format_started(&ts);
        let local = now.with_timezone(&Local);
        let expected = local.format("%H:%M").to_string();
        assert_eq!(result, expected);
    }

    #[test]
    fn format_started_other_day_shows_date_and_time() {
        let result = format_started("2025-01-15T10:30:00Z");
        assert!(result.contains("Jan"));
        assert!(result.contains("15"));
    }

    #[test]
    fn format_started_invalid_returns_original() {
        assert_eq!(format_started("not-a-date"), "not-a-date");
        assert_eq!(format_started(""), "");
    }
}
