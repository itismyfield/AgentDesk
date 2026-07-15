//! Shared schedule parsing and next-occurrence calculation.
//!
//! Routines and scheduled messages intentionally share the same recurrence
//! grammar (`@every <duration>` or a five-field cron expression). Keeping the
//! implementation here prevents either domain from depending on the other's
//! persistence or execution layer.

use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, Timelike, Utc};
use chrono_tz::Tz;
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use std::str::FromStr;

#[allow(clippy::large_enum_variant)]
enum ParsedSchedule {
    Every(Duration),
    Cron(Cron),
}

pub(crate) fn validate_schedule(schedule: &str) -> Result<()> {
    parse_schedule(schedule).map(|_| ())
}

pub(crate) fn next_due_after(
    schedule: &str,
    default_timezone: &str,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    match parse_schedule(schedule)? {
        ParsedSchedule::Every(duration) => next_every_due_after(duration, now),
        ParsedSchedule::Cron(cron) => next_cron_due_after(cron, default_timezone, now),
    }
}

pub(crate) fn next_due_after_anchor(
    schedule: &str,
    default_timezone: &str,
    anchor: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    match parse_schedule(schedule)? {
        ParsedSchedule::Every(duration) => next_every_due_after_anchor(duration, anchor, now),
        ParsedSchedule::Cron(cron) => next_cron_due_after(cron, default_timezone, now),
    }
}

fn parse_schedule(schedule: &str) -> Result<ParsedSchedule> {
    let trimmed = schedule.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "unsupported schedule '{schedule}'; expected @every <duration> or 5-field cron"
        ));
    }
    if trimmed.starts_with("@every ") || trimmed.starts_with("every ") {
        return parse_schedule_interval(trimmed).map(ParsedSchedule::Every);
    }
    if trimmed.starts_with('@') {
        return Err(anyhow!(
            "unsupported schedule '{schedule}'; expected @every <duration> or 5-field cron"
        ));
    }

    let field_count = trimmed.split_whitespace().count();
    if field_count != 5 {
        return Err(anyhow!(
            "unsupported cron schedule '{schedule}'; expected exactly 5 fields"
        ));
    }
    let cron = CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
        .parse(trimmed)
        .map_err(|e| anyhow!("invalid cron schedule '{schedule}': {e}"))?;
    Ok(ParsedSchedule::Cron(cron))
}

fn next_every_due_after(duration: Duration, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    checked_add_duration(
        truncate_to_second(now),
        duration,
        "compute next interval occurrence",
    )
}

fn next_every_due_after_anchor(
    duration: Duration,
    anchor: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    let interval_secs = duration.num_seconds();
    if interval_secs <= 0 {
        return Err(anyhow!("schedule duration must be greater than zero"));
    }

    let anchor = truncate_to_second(anchor);
    let reference = truncate_to_second(now);
    let elapsed_secs = reference.signed_duration_since(anchor).num_seconds();
    let steps = if elapsed_secs < 0 {
        1
    } else {
        elapsed_secs
            .checked_div(interval_secs)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| anyhow!("compute anchored interval occurrence: overflow"))?
    };
    let total_secs = interval_secs
        .checked_mul(steps)
        .ok_or_else(|| anyhow!("compute anchored interval occurrence: overflow"))?;

    checked_add_duration(
        anchor,
        Duration::seconds(total_secs),
        "compute anchored interval occurrence",
    )
}

fn next_cron_due_after(
    cron: Cron,
    default_timezone: &str,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    let timezone = Tz::from_str(default_timezone)
        .map_err(|_| anyhow!("invalid schedule timezone '{default_timezone}'"))?;
    let zoned_now = now.with_timezone(&timezone);
    cron.find_next_occurrence(&zoned_now, false)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|e| anyhow!("compute next cron occurrence: {e}"))
}

fn truncate_to_second(value: DateTime<Utc>) -> DateTime<Utc> {
    value
        .with_nanosecond(0)
        .expect("DateTime<Utc> nanosecond truncation should be valid")
}

fn checked_add_duration(
    base: DateTime<Utc>,
    duration: Duration,
    context: &'static str,
) -> Result<DateTime<Utc>> {
    base.checked_add_signed(duration)
        .ok_or_else(|| anyhow!("{context}: timestamp overflow"))
}

fn parse_schedule_interval(schedule: &str) -> Result<Duration> {
    let trimmed = schedule.trim();
    let duration = trimmed
        .strip_prefix("@every ")
        .or_else(|| trimmed.strip_prefix("every "))
        .unwrap_or(trimmed)
        .trim();
    if duration.is_empty() {
        return Err(anyhow!(
            "unsupported schedule '{schedule}'; expected @every <duration>"
        ));
    }

    let split_at = duration
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(duration.len());
    let (amount, unit) = duration.split_at(split_at);
    if amount.is_empty() || unit.trim().is_empty() {
        return Err(anyhow!(
            "unsupported schedule '{schedule}'; expected @every <duration>"
        ));
    }
    let amount: i64 = amount
        .parse()
        .map_err(|e| anyhow!("invalid schedule amount '{amount}': {e}"))?;
    if amount <= 0 {
        return Err(anyhow!("schedule duration must be greater than zero"));
    }

    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60 * 60,
        "d" | "day" | "days" => 60 * 60 * 24,
        other => {
            return Err(anyhow!(
                "unsupported schedule unit '{other}'; expected s, m, h, or d"
            ));
        }
    };
    let seconds = amount
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("schedule duration is too large"))?;
    Ok(Duration::seconds(seconds))
}

#[cfg(test)]
mod tests {
    use super::{
        next_due_after, next_due_after_anchor, parse_schedule_interval, validate_schedule,
    };
    use chrono::{TimeZone, Timelike, Utc};

    #[test]
    fn parses_supported_interval_schedules() {
        assert_eq!(
            parse_schedule_interval("@every 30s").unwrap().num_seconds(),
            30
        );
        assert_eq!(
            parse_schedule_interval("every 5m").unwrap().num_seconds(),
            300
        );
        assert_eq!(parse_schedule_interval("2h").unwrap().num_seconds(), 7200);
        assert_eq!(parse_schedule_interval("1d").unwrap().num_seconds(), 86_400);
    }

    #[test]
    fn rejects_invalid_schedules() {
        let error = validate_schedule("").unwrap_err().to_string();
        assert!(error.contains("unsupported schedule"));
        assert!(!error.contains("routine"));
        assert!(validate_schedule("@every 0s").is_err());
        assert!(validate_schedule("@daily").is_err());
        assert!(validate_schedule("* * * * * *").is_err());
        assert!(validate_schedule("60 9 * * *").is_err());
    }

    #[test]
    fn accepts_standard_cron_schedules() {
        assert!(validate_schedule("*/5 * * * *").is_ok());
        assert!(validate_schedule("30 9 * * 1-5").is_ok());
    }

    #[test]
    fn cron_next_due_uses_default_timezone() {
        let now = Utc.with_ymd_and_hms(2026, 4, 29, 0, 0, 0).unwrap();
        let next_due = next_due_after("30 9 * * 1-5", "Asia/Seoul", now).unwrap();
        let next_due_kst = next_due.with_timezone(&chrono_tz::Asia::Seoul);
        assert_eq!(next_due_kst.hour(), 9);
        assert_eq!(next_due_kst.minute(), 30);
    }

    #[test]
    fn every_next_due_uses_utc_interval() {
        let now = Utc.with_ymd_and_hms(2026, 4, 29, 0, 0, 0).unwrap();
        let next_due = next_due_after("@every 1h", "Asia/Seoul", now).unwrap();
        assert_eq!(
            next_due,
            Utc.with_ymd_and_hms(2026, 4, 29, 1, 0, 0).unwrap()
        );
    }

    #[test]
    fn every_next_due_truncates_subsecond_jitter() {
        let now = Utc
            .with_ymd_and_hms(2026, 4, 30, 3, 32, 8)
            .unwrap()
            .with_nanosecond(830_000_000)
            .unwrap();
        let next_due = next_due_after("@every 1m", "Asia/Seoul", now).unwrap();
        assert_eq!(
            next_due,
            Utc.with_ymd_and_hms(2026, 4, 30, 3, 33, 8).unwrap()
        );
    }

    #[test]
    fn anchored_every_next_due_skips_missed_intervals_and_stays_second_aligned() {
        let anchor = Utc
            .with_ymd_and_hms(2026, 4, 30, 3, 31, 8)
            .unwrap()
            .with_nanosecond(830_000_000)
            .unwrap();
        let now = Utc
            .with_ymd_and_hms(2026, 4, 30, 3, 32, 8)
            .unwrap()
            .with_nanosecond(831_000_000)
            .unwrap();
        let next_due = next_due_after_anchor("@every 1m", "Asia/Seoul", anchor, now).unwrap();
        assert_eq!(
            next_due,
            Utc.with_ymd_and_hms(2026, 4, 30, 3, 33, 8).unwrap()
        );
    }
}
