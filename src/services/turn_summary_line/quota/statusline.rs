use std::fs::File;
use std::path::Path;

use super::{DateTime, Quota, Utc, Value, Window, fresh};
use crate::services::claude_tui::hook_bundle::statusline::record_path;

fn read(root: &Path, session: &str, now: DateTime<Utc>) -> Option<Quota> {
    if session.is_empty() {
        return None;
    }
    let file = File::open(record_path(root, session)).ok()?;
    let metadata = file.metadata().ok()?;
    if metadata.len() > 32 * 1024 || !fresh(metadata.modified().ok()?.into(), now) {
        return None;
    }
    let value: Value = serde_json::from_reader(std::io::BufReader::new(file)).ok()?;
    if value.get("session_id").and_then(Value::as_str) != Some(session) {
        return None;
    }
    let limits = value.get("rate_limits")?;
    let window = |name: &str| {
        let limit = limits.get(name)?;
        Window::new(
            limit.get("used_percentage")?.as_f64()?,
            DateTime::from_timestamp(limit.get("resets_at")?.as_i64()?, 0)?,
            now,
        )
    };
    Quota {
        five_hour: window("five_hour"),
        seven_day: window("seven_day"),
    }
    .current(now)
}

pub(super) fn with_fallback(
    root: &Path,
    session: Option<&str>,
    now: DateTime<Utc>,
    fallback: impl FnOnce() -> Option<Quota>,
) -> Option<Quota> {
    session
        .and_then(|session| read(root, session, now))
        .or_else(fallback)
}

#[cfg(test)]
#[path = "statusline_tests.rs"]
mod tests;
