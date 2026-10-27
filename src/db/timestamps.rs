use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};

// Offsetless legacy timestamps use this host's local wall clock.
// Reject ambiguous or nonexistent local times instead of guessing an instant.
pub(crate) fn parse_timestamp(value: &str) -> Result<DateTime<Utc>> {
    parse_timestamp_in_timezone(value, &Local)
}

fn parse_timestamp_in_timezone<Tz: TimeZone>(value: &str, zone: &Tz) -> Result<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value)
        .or_else(|_| DateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f%#z"))
    {
        return Ok(timestamp.with_timezone(&Utc));
    }
    let naive = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f"))
        .with_context(|| format!("invalid timestamp: {value}"))?;
    zone.from_local_datetime(&naive)
        .single()
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .ok_or_else(|| anyhow!("ambiguous or nonexistent local timestamp: {value}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::{America::New_York, Asia::Seoul};

    #[test]
    fn explicit_offsets_preserve_instants_and_fractional_seconds() {
        let expected = DateTime::parse_from_rfc3339("2026-10-27T15:00:00.123456Z").unwrap();
        for input in [
            "2026-10-28T00:00:00.123456+09:00",
            "2026-10-27T11:00:00.123456-04:00",
            " 2026-10-27T15:00:00.123456Z ",
            "2026-10-27 15:00:00.123456+00",
        ] {
            for zone in [Seoul, New_York] {
                assert_eq!(parse_timestamp_in_timezone(input, &zone).unwrap(), expected);
            }
        }
    }

    #[test]
    fn legacy_wall_clock_uses_local_zone_across_date_boundaries() {
        for input in [" 2026-10-28 00:00:00 ", "2026-10-28T00:00:00"] {
            assert_eq!(
                parse_timestamp_in_timezone(input, &Seoul).unwrap(),
                DateTime::parse_from_rfc3339("2026-10-27T15:00:00Z").unwrap()
            );
        }
        assert_eq!(
            parse_timestamp_in_timezone("2026-07-01 23:30:00", &New_York).unwrap(),
            DateTime::parse_from_rfc3339("2026-07-02T03:30:00Z").unwrap()
        );
    }

    #[test]
    fn invalid_ambiguous_and_nonexistent_wall_times_are_rejected() {
        for input in [
            "",
            "not-a-timestamp",
            "2026-02-30 12:00:00",
            "2026-03-08 02:30:00",
            "2026-11-01 01:30:00",
        ] {
            assert!(parse_timestamp_in_timezone(input, &New_York).is_err());
        }
    }
}
