//! Logical turn identity parsing shared by the response delivery fence.

use chrono::TimeZone;

pub(super) fn parse_turn_started_at(
    value: Option<&str>,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(Some(parsed.with_timezone(&chrono::Utc)));
    }
    let naive = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        .map_err(|error| format!("parse task response turn start timestamp: {error}"))?;
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .map(|parsed| Some(parsed.with_timezone(&chrono::Utc)))
        .ok_or_else(|| "task response turn start timestamp is ambiguous in local time".to_string())
}
