use std::fs::File;
use std::path::Path;

use super::{DateTime, Quota, Utc, Value, Window, fresh};
use crate::services::claude_tui::hook_bundle::statusline::record_path;

pub(super) struct Record {
    value: Value,
    observed_at: DateTime<Utc>,
}

impl Record {
    pub(super) fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    pub(super) fn model(&self) -> Option<&str> {
        ["/model/display_name", "/model/id"]
            .into_iter()
            .find_map(|pointer| {
                self.value
                    .pointer(pointer)?
                    .as_str()
                    .and_then(super::super::model_name)
            })
    }

    pub(super) fn effort(&self) -> Option<&str> {
        match self.value.pointer("/effort/level")?.as_str()?.trim() {
            "" => None,
            "ultracode" => Some("ultra"),
            effort => Some(effort),
        }
    }

    pub(super) fn fast(&self) -> bool {
        self.value["fast_mode"].as_bool() == Some(true)
    }

    pub(super) fn quota(&self, now: DateTime<Utc>) -> Option<Quota> {
        let limits = self.value.get("rate_limits")?;
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
            fable_seven_day: None,
            as_of: now,
        }
        .current(now)
    }
}

pub(super) fn read(root: &Path, session: &str, now: DateTime<Utc>) -> Option<Record> {
    if session.is_empty() {
        return None;
    }
    let file = File::open(record_path(root, session)).ok()?;
    let metadata = file.metadata().ok()?;
    let observed_at = metadata.modified().ok()?.into();
    if metadata.len() > 32 * 1024 || !fresh(observed_at, now) {
        return None;
    }
    let value: Value = serde_json::from_reader(std::io::BufReader::new(file)).ok()?;
    if value.get("session_id").and_then(Value::as_str) != Some(session) {
        return None;
    }
    Some(Record { value, observed_at })
}

#[cfg(test)]
#[path = "statusline_tests.rs"]
mod tests;
