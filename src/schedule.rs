//! Per-account time windows: when the account may be used and which priority
//! it takes while a window is active.

use chrono::{Datelike, NaiveDateTime, Timelike, Weekday};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

/// A single window, e.g. `mon-fri 08:00-17:00@1`.
///
/// `start_min`/`end_min` are minutes since local midnight. When
/// `start_min > end_min` the window wraps past midnight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeWindow {
    /// Days the window starts on, Monday = 0 … Sunday = 6. Empty means every day.
    #[serde(default)]
    pub days: Vec<u8>,
    pub start_min: u32,
    pub end_min: u32,
    /// Priority override while this window is active (lower wins).
    #[serde(default)]
    pub priority: Option<i32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    #[serde(default)]
    pub windows: Vec<TimeWindow>,
}

impl Schedule {
    /// `None` when the account must not be used right now, otherwise the
    /// optional priority override of the matching window.
    pub fn priority_at(&self, local: NaiveDateTime) -> Option<Option<i32>> {
        if self.windows.is_empty() {
            return Some(None);
        }
        let today = day_index(local.weekday());
        let yesterday = day_index(local.weekday().pred());
        let minute = local.hour() * 60 + local.minute();
        for window in &self.windows {
            let days = &window.days;
            let day_ok = |d: u8| days.is_empty() || days.contains(&d);
            let inside = if window.start_min <= window.end_min {
                day_ok(today) && minute >= window.start_min && minute < window.end_min
            } else {
                // wraps midnight: starts today before midnight, ends tomorrow
                (day_ok(today) && minute >= window.start_min)
                    || (day_ok(yesterday) && minute < window.end_min)
            };
            if inside {
                return Some(window.priority);
            }
        }
        None
    }
}

pub fn day_index(day: Weekday) -> u8 {
    match day {
        Weekday::Mon => 0,
        Weekday::Tue => 1,
        Weekday::Wed => 2,
        Weekday::Thu => 3,
        Weekday::Fri => 4,
        Weekday::Sat => 5,
        Weekday::Sun => 6,
    }
}

/// Parse `[days] HH:MM-HH:MM[@priority]`, e.g. `mon-fri 08:00-17:00@1`.
pub fn parse_window(spec: &str) -> Result<TimeWindow> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(AppError::Invalid("empty window spec".into()));
    }
    let (days_part, time_part, priority) = if let Some((head, priority)) = spec.rsplit_once('@') {
        let priority: i32 = priority.trim().parse().map_err(|_| {
            AppError::Invalid(format!("window `{spec}`: priority must be an integer"))
        })?;
        match head.trim().split_once(' ') {
            Some((days, time)) => (Some(days.trim()), time.trim(), Some(priority)),
            None => (None, head.trim(), Some(priority)),
        }
    } else {
        match spec.split_once(' ') {
            Some((days, time)) => (Some(days.trim()), time.trim(), None),
            None => (None, spec, None),
        }
    };

    let days = match days_part {
        Some(days) => parse_days(days)?,
        None => Vec::new(),
    };

    let (start, end) = time_part
        .split_once('-')
        .ok_or_else(|| AppError::Invalid(format!("window `{spec}`: expected HH:MM-HH:MM")))?;
    let start_min = parse_hhmm(start.trim(), spec)?;
    let end_min = parse_hhmm(end.trim(), spec)?;
    if start_min == end_min {
        return Err(AppError::Invalid(format!(
            "window `{spec}`: start and end must differ"
        )));
    }
    Ok(TimeWindow {
        days,
        start_min,
        end_min,
        priority,
    })
}

pub fn parse_days(input: &str) -> Result<Vec<u8>> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed == "*" {
        return Ok(Vec::new());
    }
    let mut days = Vec::new();
    for part in trimmed.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(AppError::Invalid(format!("invalid day list `{input}`")));
        }
        match part.split_once('-') {
            Some((from, to)) => {
                let from = parse_day(from)?;
                let to = parse_day(to)?;
                if from > to {
                    return Err(AppError::Invalid(format!(
                        "invalid day range `{part}`: start is after end"
                    )));
                }
                for d in from..=to {
                    if !days.contains(&d) {
                        days.push(d);
                    }
                }
            }
            None => {
                let d = parse_day(part)?;
                if !days.contains(&d) {
                    days.push(d);
                }
            }
        }
    }
    Ok(days)
}

fn parse_day(input: &str) -> Result<u8> {
    let lower = input.trim().to_ascii_lowercase();
    let day = match lower.as_str() {
        "mon" | "monday" => 0,
        "tue" | "tues" | "tuesday" => 1,
        "wed" | "wednesday" => 2,
        "thu" | "thur" | "thurs" | "thursday" => 3,
        "fri" | "friday" => 4,
        "sat" | "saturday" => 5,
        "sun" | "sunday" => 6,
        other => match other.parse::<u8>() {
            Ok(n) if n <= 6 => n,
            _ => return Err(AppError::Invalid(format!("unknown day `{input}`"))),
        },
    };
    Ok(day)
}

fn parse_hhmm(input: &str, spec: &str) -> Result<u32> {
    let (h, m) = input
        .split_once(':')
        .ok_or_else(|| AppError::Invalid(format!("window `{spec}`: expected HH:MM")))?;
    let hour: u32 = h
        .trim()
        .parse()
        .map_err(|_| AppError::Invalid(format!("window `{spec}`: bad hour `{h}`")))?;
    let minute: u32 = m
        .trim()
        .parse()
        .map_err(|_| AppError::Invalid(format!("window `{spec}`: bad minute `{m}`")))?;
    if hour > 23 || minute > 59 {
        return Err(AppError::Invalid(format!(
            "window `{spec}`: time out of range"
        )));
    }
    Ok(hour * 60 + minute)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(y: i32, m: u32, d: u32, hh: u32, mm: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(hh, mm, 0)
            .unwrap()
    }

    #[test]
    fn parses_weekday_window() {
        let w = parse_window("mon-fri 08:00-17:00@1").unwrap();
        assert_eq!(w.days, vec![0, 1, 2, 3, 4]);
        assert_eq!(w.start_min, 480);
        assert_eq!(w.end_min, 1020);
        assert_eq!(w.priority, Some(1));
    }

    #[test]
    fn parses_window_without_days_or_priority() {
        let w = parse_window("23:00-02:00").unwrap();
        assert!(w.days.is_empty());
        assert_eq!(w.priority, None);
        assert_eq!(w.start_min, 1380);
        assert_eq!(w.end_min, 120);
        assert!(parse_window("08:00-08:00").is_err());
        assert!(parse_window("08:00").is_err());
        assert!(parse_window("24:00-25:00").is_err());
    }

    #[test]
    fn empty_schedule_is_always_available() {
        let s = Schedule::default();
        assert_eq!(s.priority_at(at(2026, 5, 4, 12, 0)), Some(None));
    }

    #[test]
    fn matches_days_and_hours() {
        let s = Schedule {
            windows: vec![parse_window("mon-fri 08:00-17:00@1").unwrap()],
        };
        assert_eq!(s.priority_at(at(2026, 5, 4, 9, 0)), Some(Some(1))); // Monday
        assert_eq!(s.priority_at(at(2026, 5, 4, 7, 59)), None);
        assert_eq!(s.priority_at(at(2026, 5, 4, 17, 0)), None);
        assert_eq!(s.priority_at(at(2026, 5, 9, 9, 0)), None); // Saturday
    }

    #[test]
    fn wrapped_window_spans_midnight() {
        let s = Schedule {
            windows: vec![parse_window("fri 22:00-02:00@3").unwrap()],
        };
        // Friday 23:00 starts the window; Saturday 01:00 is still inside it.
        assert_eq!(s.priority_at(at(2026, 5, 8, 23, 0)), Some(Some(3)));
        assert_eq!(s.priority_at(at(2026, 5, 9, 1, 0)), Some(Some(3)));
        assert_eq!(s.priority_at(at(2026, 5, 9, 2, 0)), None);
        assert_eq!(s.priority_at(at(2026, 5, 7, 23, 0)), None); // Thursday
    }

    #[test]
    fn wildcard_days() {
        assert!(parse_days("*").unwrap().is_empty());
        assert_eq!(parse_days("mon,wed-fri").unwrap(), vec![0, 2, 3, 4]);
        assert!(parse_days("sun-mon").is_err());
        assert!(parse_days("funday").is_err());
    }
}
