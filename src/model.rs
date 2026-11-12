//! Account documents stored in MongoDB.

use std::fmt;
use std::str::FromStr;

use bson::oid::ObjectId;
use chrono::{DateTime, Days, LocalResult, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};
use crate::schedule::Schedule;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Preset for `smtp.gmail.com` (or Google Workspace) with an app password.
    Google,
    /// Any SMTP relay.
    Custom,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Custom => "smtp",
        }
    }
}

impl FromStr for Provider {
    type Err = AppError;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "google" | "gmail" => Ok(Self::Google),
            "smtp" | "custom" => Ok(Self::Custom),
            other => Err(AppError::Invalid(format!(
                "unknown provider `{other}` (use google or smtp)"
            ))),
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// Plaintext SMTP, no TLS (trusted local relay only).
    None,
    /// Upgrade with STARTTLS after EHLO (port 587, Gmail default).
    Starttls,
    /// TLS from the first byte (port 465).
    Implicit,
}

impl TlsMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Starttls => "starttls",
            Self::Implicit => "implicit",
        }
    }
}

impl FromStr for TlsMode {
    type Err = AppError;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "plain" | "off" => Ok(Self::None),
            "starttls" | "start-tls" | "tls" => Ok(Self::Starttls),
            "implicit" | "ssl" | "smtps" => Ok(Self::Implicit),
            other => Err(AppError::Invalid(format!(
                "unknown tls mode `{other}` (use starttls, implicit or none)"
            ))),
        }
    }
}

impl fmt::Display for TlsMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Runtime-toggled settings for the optional admin dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminSettings {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_admin_listen")]
    pub listen: String,
    #[serde(default)]
    pub token: String,
    #[serde(default = "epoch", with = "crate::serde_bson::datetime")]
    pub updated_at: DateTime<Utc>,
}

fn default_admin_listen() -> String {
    AdminSettings::DEFAULT_LISTEN.to_string()
}

fn epoch() -> DateTime<Utc> {
    DateTime::<Utc>::UNIX_EPOCH
}

impl AdminSettings {
    pub const DEFAULT_LISTEN: &'static str = "127.0.0.1:8787";
    pub const ID: &'static str = "dashboard";

    pub fn defaults() -> Self {
        Self {
            id: Self::ID.to_string(),
            enabled: false,
            listen: Self::DEFAULT_LISTEN.to_string(),
            token: String::new(),
            updated_at: DateTime::<Utc>::UNIX_EPOCH,
        }
    }

    pub fn token_masked(&self) -> String {
        if self.token.is_empty() {
            "-".to_string()
        } else {
            let prefix: String = self.token.chars().take(4).collect();
            format!("{prefix}…")
        }
    }
}

/// One SMTP account plus its quota, rate-limit and scheduling state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    #[serde(rename = "_id")]
    pub id: ObjectId,
    /// Unique, human-friendly identifier used by the CLI.
    pub name: String,
    pub provider: Provider,
    pub host: String,
    pub port: u16,
    pub tls: TlsMode,
    pub username: String,
    pub password: String,
    /// Address used as envelope sender for Google accounts and as the default
    /// sender when the client does not supply one.
    pub from_address: String,
    #[serde(default)]
    pub from_name: Option<String>,
    pub active: bool,
    /// Lower number wins in `priority` and `time-window` modes.
    pub priority: i32,
    /// Sends allowed per quota window. `0` means unlimited.
    pub count_max: i64,
    pub count_used: i64,
    /// Successful sends over the account lifetime.
    #[serde(default)]
    pub total_sent: i64,
    /// Lifetime send cap for the purchased package. `0` means unlimited.
    #[serde(default)]
    pub total_max: i64,
    /// Sends per minute. `0` means unlimited. The full minute's quota may burst.
    pub rate_limit_per_min: i64,
    #[serde(default)]
    pub rate_tokens: f64,
    #[serde(with = "crate::serde_bson::datetime")]
    pub rate_updated_at: DateTime<Utc>,
    /// Local time of the daily quota reset, `HH:MM`.
    pub reset_at: String,
    /// IANA timezone used for `reset_at` and schedules, e.g. `Asia/Ho_Chi_Minh`.
    pub timezone: String,
    /// Start of the current quota window; maintained by the router.
    #[serde(with = "crate::serde_bson::datetime")]
    pub window_start: DateTime<Utc>,
    #[serde(default, with = "crate::serde_bson::datetime_opt")]
    pub expires_at: Option<DateTime<Utc>>,
    pub disabled_reason: Option<String>,
    #[serde(default)]
    pub schedule: Option<Schedule>,
    #[serde(default)]
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
    #[serde(default, with = "crate::serde_bson::datetime_opt")]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default, with = "crate::serde_bson::datetime_opt")]
    pub last_success_at: Option<DateTime<Utc>>,
    #[serde(with = "crate::serde_bson::datetime")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "crate::serde_bson::datetime")]
    pub updated_at: DateTime<Utc>,
}

impl Account {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: impl Into<String>,
        provider: Provider,
        username: impl Into<String>,
        password: impl Into<String>,
        from_address: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Self {
        let (host, port, tls) = match provider {
            Provider::Google => ("smtp.gmail.com".to_string(), 587, TlsMode::Starttls),
            Provider::Custom => ("localhost".to_string(), 587, TlsMode::Starttls),
        };
        Self {
            id: ObjectId::new(),
            name: name.into(),
            provider,
            host,
            port,
            tls,
            username: username.into(),
            password: password.into(),
            from_address: from_address.into(),
            from_name: None,
            active: true,
            priority: 100,
            count_max: 0,
            count_used: 0,
            total_sent: 0,
            total_max: 0,
            rate_limit_per_min: 0,
            rate_tokens: 0.0,
            rate_updated_at: now,
            reset_at: "00:00".to_string(),
            timezone: "UTC".to_string(),
            window_start: now,
            expires_at: None,
            disabled_reason: None,
            schedule: None,
            consecutive_failures: 0,
            last_error: None,
            last_used_at: None,
            last_success_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn tz(&self) -> Result<Tz> {
        self.timezone
            .parse::<Tz>()
            .map_err(|_| AppError::Invalid(format!("unknown timezone `{}`", self.timezone)))
    }

    pub fn local_now(&self, now: DateTime<Utc>) -> Result<DateTime<Tz>> {
        Ok(now.with_timezone(&self.tz()?))
    }

    /// Start of the current daily quota window in UTC.
    ///
    /// The window starts every day at `reset_at` in the account's timezone.
    pub fn window_anchor(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
        let (hour, minute) = parse_reset_at(&self.reset_at)?;
        let tz = self.tz()?;
        let local = now.with_timezone(&tz);
        let mut date = local.date_naive();
        let mut anchor = resolve_local(tz, date, hour, minute)?;
        if anchor > now {
            date = date
                .checked_sub_days(Days::new(1))
                .ok_or_else(|| AppError::Invalid("quota window underflows the calendar".into()))?;
            anchor = resolve_local(tz, date, hour, minute)?;
        }
        Ok(anchor)
    }

    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|expires| expires <= now)
    }

    pub fn is_available(&self, now: DateTime<Utc>) -> bool {
        self.active && !self.is_expired(now)
    }

    /// Priority right now, or `None` when a schedule excludes this account.
    pub fn effective_priority(&self, now: DateTime<Utc>) -> Option<i32> {
        let Some(schedule) = &self.schedule else {
            return Some(self.priority);
        };
        if schedule.windows.is_empty() {
            return Some(self.priority);
        }
        let tz = self.tz().unwrap_or(chrono_tz::UTC);
        let local = now.with_timezone(&tz).naive_local();
        schedule
            .priority_at(local)
            .map(|override_priority| override_priority.unwrap_or(self.priority))
    }

    pub fn rate_per_sec(&self) -> f64 {
        self.rate_limit_per_min as f64 / 60.0
    }

    /// Google requires the envelope sender to be the authenticated mailbox.
    pub fn uses_account_sender(&self) -> bool {
        self.provider == Provider::Google
    }

    pub fn expiry_label(&self) -> String {
        match self.expires_at {
            Some(at) => at.format("%Y-%m-%d %H:%M UTC").to_string(),
            None => "-".to_string(),
        }
    }

    pub fn quota_label(&self) -> String {
        if self.count_max <= 0 {
            format!("{}/∞", self.count_used)
        } else {
            format!("{}/{}", self.count_used, self.count_max)
        }
    }

    pub fn total_label(&self) -> String {
        if self.total_max <= 0 {
            format!("{}/∞", self.total_sent)
        } else {
            format!("{}/{}", self.total_sent, self.total_max)
        }
    }

    pub fn rate_label(&self) -> String {
        if self.rate_limit_per_min <= 0 {
            "∞".to_string()
        } else {
            format!("{}/min", self.rate_limit_per_min)
        }
    }

    pub fn status_label(&self, now: DateTime<Utc>) -> String {
        if self.is_expired(now) {
            return "expired".to_string();
        }
        if !self.active {
            return match &self.disabled_reason {
                Some(reason) if !reason.is_empty() => format!("off ({reason})"),
                _ => "off".to_string(),
            };
        }
        match self.effective_priority(now) {
            Some(_) => "active".to_string(),
            None => "scheduled-out".to_string(),
        }
    }

    pub fn endpoint_label(&self) -> String {
        format!("{}:{} ({})", self.host, self.port, self.tls)
    }

    /// Copy with the password masked, for logs and terminal output.
    pub fn redacted(&self) -> Self {
        let mut copy = self.clone();
        copy.password = "***".to_string();
        copy
    }
}

fn resolve_local(tz: Tz, date: NaiveDate, hour: u32, minute: u32) -> Result<DateTime<Utc>> {
    let naive: NaiveDateTime = date
        .and_hms_opt(hour, minute, 0)
        .ok_or_else(|| AppError::Invalid(format!("invalid reset time {hour:02}:{minute:02}")))?;
    let resolved = match tz.from_local_datetime(&naive) {
        LocalResult::Single(dt) => dt,
        LocalResult::Ambiguous(first, _) => first,
        // DST gap: the wall clock never reaches this time; use the UTC
        // interpretation shifted by the zone's standard offset.
        LocalResult::None => {
            let offset = tz.offset_from_utc_datetime(&naive).fix();
            return Ok(Utc.from_utc_datetime(&(naive - offset)));
        }
    };
    Ok(resolved.with_timezone(&Utc))
}

/// Parse `HH:MM` into `(hour, minute)`.
pub fn parse_reset_at(input: &str) -> Result<(u32, u32)> {
    let (h, m) = input
        .trim()
        .split_once(':')
        .ok_or_else(|| AppError::Invalid(format!("reset time `{input}` must be HH:MM")))?;
    let hour: u32 = h
        .parse()
        .map_err(|_| AppError::Invalid(format!("reset time `{input}`: bad hour")))?;
    let minute: u32 = m
        .parse()
        .map_err(|_| AppError::Invalid(format!("reset time `{input}`: bad minute")))?;
    if hour > 23 || minute > 59 {
        return Err(AppError::Invalid(format!(
            "reset time `{input}` is out of range"
        )));
    }
    Ok((hour, minute))
}

pub fn validate_timezone(input: &str) -> Result<()> {
    input
        .parse::<Tz>()
        .map(|_| ())
        .map_err(|_| AppError::Invalid(format!("unknown timezone `{input}` (use an IANA name)")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(tz: &str, reset: &str) -> Account {
        let now = Utc.with_ymd_and_hms(2026, 5, 4, 12, 0, 0).unwrap();
        let mut a = Account::new("a", Provider::Custom, "u", "p", "u@example.com", now);
        a.timezone = tz.to_string();
        a.reset_at = reset.to_string();
        a
    }

    #[test]
    fn window_anchor_utc() {
        let a = account("UTC", "00:00");
        let now = Utc.with_ymd_and_hms(2026, 5, 4, 12, 0, 0).unwrap();
        assert_eq!(
            a.window_anchor(now).unwrap(),
            Utc.with_ymd_and_hms(2026, 5, 4, 0, 0, 0).unwrap()
        );
    }

    #[test]
    fn window_anchor_respects_hour_and_timezone() {
        let a = account("Asia/Ho_Chi_Minh", "03:30");
        // 2026-05-04 12:00 UTC == 19:00 +07, after 03:30 local.
        let now = Utc.with_ymd_and_hms(2026, 5, 4, 12, 0, 0).unwrap();
        assert_eq!(
            a.window_anchor(now).unwrap(),
            Utc.with_ymd_and_hms(2026, 5, 3, 20, 30, 0).unwrap()
        );
        // 2026-05-04 00:10 +07 is before the reset -> previous day's window.
        let early = Utc.with_ymd_and_hms(2026, 5, 3, 17, 10, 0).unwrap();
        assert_eq!(
            a.window_anchor(early).unwrap(),
            Utc.with_ymd_and_hms(2026, 5, 2, 20, 30, 0).unwrap()
        );
    }

    #[test]
    fn expiry_and_availability() {
        let now = Utc.with_ymd_and_hms(2026, 5, 4, 12, 0, 0).unwrap();
        let mut a = account("UTC", "00:00");
        a.expires_at = Some(now + chrono::TimeDelta::seconds(1));
        assert!(a.is_available(now));
        assert!(!a.is_expired(now));
        assert!(a.is_expired(now + chrono::TimeDelta::seconds(2)));
        assert!(!a.is_available(now + chrono::TimeDelta::seconds(2)));
    }

    #[test]
    fn schedule_priority_override() {
        let now = Utc.with_ymd_and_hms(2026, 5, 4, 9, 0, 0).unwrap(); // Monday 09:00 UTC
        let mut a = account("UTC", "00:00");
        a.priority = 50;
        a.schedule = Some(Schedule {
            windows: vec![crate::schedule::parse_window("mon-fri 08:00-10:00@7").unwrap()],
        });
        assert_eq!(a.effective_priority(now), Some(7));
        assert_eq!(
            a.effective_priority(Utc.with_ymd_and_hms(2026, 5, 4, 11, 0, 0).unwrap()),
            None
        );
    }

    #[test]
    fn reset_at_validation() {
        assert_eq!(parse_reset_at("03:30").unwrap(), (3, 30));
        assert_eq!(parse_reset_at("00:00").unwrap(), (0, 0));
        for bad in ["", "24:00", "12:60", "1230", "aa:bb"] {
            assert!(parse_reset_at(bad).is_err(), "{bad}");
        }
    }
}
