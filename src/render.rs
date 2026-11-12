//! Plain-text terminal rendering (no external table crate).

use chrono::{DateTime, Utc};

use crate::model::Account;

pub fn table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|header| width(header)).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index < widths.len() {
                widths[index] = widths[index].max(width(cell));
            }
        }
    }
    let header_cells: Vec<String> = headers.iter().map(|header| header.to_string()).collect();
    println!("{}", format_row(&header_cells, &widths));
    println!(
        "{}",
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  ")
    );
    for row in rows {
        println!("{}", format_row(row, &widths));
    }
}

pub fn account_row(account: &Account, now: DateTime<Utc>) -> Vec<String> {
    vec![
        account.name.clone(),
        account.status_label(now),
        account.endpoint_label(),
        account.quota_label(),
        account.rate_label(),
        account.priority.to_string(),
        format!("{} {}", account.reset_at, account.timezone),
        account.expiry_label(),
        account
            .last_used_at
            .map(format_timestamp)
            .unwrap_or_else(|| "-".to_string()),
        truncate(
            account
                .last_error
                .clone()
                .or_else(|| account.disabled_reason.clone())
                .unwrap_or_default(),
            40,
        ),
    ]
}

pub fn account_headers() -> Vec<&'static str> {
    vec![
        "NAME",
        "STATUS",
        "ENDPOINT",
        "QUOTA",
        "RATE",
        "PRIO",
        "RESET",
        "EXPIRES",
        "LAST USED",
        "NOTE",
    ]
}

pub fn format_timestamp(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M").to_string()
}

fn format_row(cells: &[String], widths: &[usize]) -> String {
    cells
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let width = widths.get(index).copied().unwrap_or(0);
            let padding = width.saturating_sub(width_of(cell));
            format!("{cell}{}", " ".repeat(padding))
        })
        .collect::<Vec<_>>()
        .join("  ")
        .trim_end()
        .to_string()
}

fn width(input: &str) -> usize {
    width_of(input)
}

fn width_of(input: &str) -> usize {
    input.chars().count()
}

fn truncate(input: String, max: usize) -> String {
    if width_of(&input) <= max {
        return input;
    }
    let mut out: String = input.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
