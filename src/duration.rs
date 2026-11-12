//! Parsing of human expiration durations such as `15d`, `3m`, `2y`.

use chrono::{DateTime, Months, TimeDelta, Utc};

use crate::error::{AppError, Result};

/// A single-unit duration used for account expiration.
///
/// `m` is **months** (the tool is used for account lifetimes, not timeouts),
/// `mo` is accepted as an explicit alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationSpec {
    Hours(i64),
    Days(i64),
    Weeks(i64),
    Months(u32),
    Years(u32),
}

impl DurationSpec {
    pub fn parse(input: &str) -> Result<Self> {
        let raw = input.trim().to_ascii_lowercase();
        if raw.is_empty() {
            return Err(AppError::Invalid("empty duration".into()));
        }
        let split = raw.find(|c: char| !c.is_ascii_digit()).ok_or_else(|| {
            AppError::Invalid(format!(
                "duration `{input}` needs a unit: h, d, w, mo, m, y"
            ))
        })?;
        let (digits, unit) = raw.split_at(split);
        if digits.is_empty() {
            return Err(AppError::Invalid(format!(
                "duration `{input}` needs a positive number"
            )));
        }
        let value: u64 = digits
            .parse()
            .map_err(|_| AppError::Invalid(format!("duration `{input}`: number too large")))?;
        if value == 0 {
            return Err(AppError::Invalid(format!(
                "duration `{input}` must be greater than zero"
            )));
        }
        let as_i64 = |v: u64| {
            i64::try_from(v)
                .map_err(|_| AppError::Invalid(format!("duration `{input}` is too large")))
        };
        let as_u32 = |v: u64| {
            u32::try_from(v)
                .map_err(|_| AppError::Invalid(format!("duration `{input}` is too large")))
        };
        match unit {
            "h" => Ok(Self::Hours(as_i64(value)?)),
            "d" => Ok(Self::Days(as_i64(value)?)),
            "w" => Ok(Self::Weeks(as_i64(value)?)),
            "mo" | "m" => Ok(Self::Months(as_u32(value)?)),
            "y" => Ok(Self::Years(as_u32(value)?)),
            other => Err(AppError::Invalid(format!(
                "duration `{input}` has unknown unit `{other}` (use h, d, w, mo, m, y)"
            ))),
        }
    }

    /// Add this duration to `now`, honouring calendar months/years.
    pub fn apply(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
        let out = match *self {
            Self::Hours(n) => now.checked_add_signed(TimeDelta::hours(n)),
            Self::Days(n) => now.checked_add_signed(TimeDelta::days(n)),
            Self::Weeks(n) => now.checked_add_signed(TimeDelta::days(n * 7)),
            Self::Months(n) => now.checked_add_months(Months::new(n)),
            Self::Years(n) => n
                .checked_mul(12)
                .and_then(|months| now.checked_add_months(Months::new(months))),
        };
        out.ok_or_else(|| {
            AppError::Invalid("duration overflows the representable date range".into())
        })
    }

    pub fn describe(&self) -> String {
        match *self {
            Self::Hours(n) => format!("{n}h"),
            Self::Days(n) => format!("{n}d"),
            Self::Weeks(n) => format!("{n}w"),
            Self::Months(n) => format!("{n}mo"),
            Self::Years(n) => format!("{n}y"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 31, 10, 0, 0).unwrap()
    }

    #[test]
    fn parses_supported_units() {
        assert_eq!(DurationSpec::parse("15d").unwrap(), DurationSpec::Days(15));
        assert_eq!(DurationSpec::parse("3m").unwrap(), DurationSpec::Months(3));
        assert_eq!(DurationSpec::parse("3mo").unwrap(), DurationSpec::Months(3));
        assert_eq!(DurationSpec::parse("2y").unwrap(), DurationSpec::Years(2));
        assert_eq!(DurationSpec::parse("36h").unwrap(), DurationSpec::Hours(36));
        assert_eq!(DurationSpec::parse("2w").unwrap(), DurationSpec::Weeks(2));
        assert_eq!(
            DurationSpec::parse(" 15D ").unwrap(),
            DurationSpec::Days(15)
        );
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "d", "0d", "-1d", "15", "15x", "1.5d", "1d2h"] {
            assert!(DurationSpec::parse(bad).is_err(), "{bad} should fail");
        }
    }

    #[test]
    fn calendar_months_clamp_to_month_end() {
        let exp = DurationSpec::Months(1).apply(now()).unwrap();
        // 2026-01-31 + 1 month => 2026-02-28
        assert_eq!(exp, Utc.with_ymd_and_hms(2026, 2, 28, 10, 0, 0).unwrap());
        let exp = DurationSpec::Years(2).apply(now()).unwrap();
        assert_eq!(exp, Utc.with_ymd_and_hms(2028, 1, 31, 10, 0, 0).unwrap());
        let exp = DurationSpec::Days(15).apply(now()).unwrap();
        assert_eq!(exp, Utc.with_ymd_and_hms(2026, 2, 15, 10, 0, 0).unwrap());
    }
}
