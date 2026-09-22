//! Validated managed-event intent, independent of provider response DTOs.
use chrono::{DateTime, Offset, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventTime {
    pub start_at: String,
    pub end_at: String,
    #[serde(default = "default_zone")]
    pub time_zone: String,
}
fn default_zone() -> String {
    "Asia/Seoul".to_string()
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Location {
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventContent {
    pub title: String,
    pub time: EventTime,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub location: Option<Location>,
    #[serde(default)]
    pub reminders: Option<Vec<i32>>,
}

impl EventContent {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.title.trim().is_empty() || self.title.chars().count() > 50 {
            return Err("title must contain 1 to 50 characters");
        }
        if self
            .description
            .as_ref()
            .is_some_and(|s| s.chars().count() > 5000)
        {
            return Err("description exceeds 5000 characters");
        }
        if self
            .location
            .as_ref()
            .is_some_and(|s| s.name.trim().is_empty() || s.name.chars().count() > 100)
        {
            return Err("location name must contain 1 to 100 characters");
        }
        let zone = self
            .time
            .time_zone
            .parse::<chrono_tz::Tz>()
            .map_err(|_| "invalid IANA time zone")?;
        let parse = |raw: &str| -> Result<DateTime<Utc>, &'static str> {
            let time =
                DateTime::parse_from_rfc3339(raw).map_err(|_| "time requires RFC3339 offset")?;
            if time.with_timezone(&zone).offset().fix() != *time.offset() {
                return Err("time offset does not match IANA time zone");
            }
            if time.timestamp_subsec_nanos() != 0 {
                return Err("fractional seconds are unsupported");
            }
            Ok(time.with_timezone(&Utc))
        };
        if parse(&self.time.start_at)? >= parse(&self.time.end_at)? {
            return Err("endAt must follow startAt");
        }
        if let Some(reminders) = &self.reminders {
            if reminders.is_empty()
                || reminders.len() > 2
                || reminders.iter().any(|n| *n < 0 || *n > 43200 || n % 5 != 0)
                || (reminders.len() == 2 && reminders[0] == reminders[1])
            {
                return Err(
                    "reminders require one or two distinct supported five-minute offsets; disabling is unsupported",
                );
            }
        }
        Ok(())
    }

    pub fn patched(&self, patch: &Value) -> Result<Self, &'static str> {
        let fields = patch.as_object().ok_or("patch must be an object")?;
        let mut next = serde_json::to_value(self).map_err(|_| "invalid event")?;
        for (key, value) in fields {
            if key == "expectedRevision" {
                continue;
            }
            if !["title", "time", "description", "location", "reminders"].contains(&key.as_str()) {
                return Err("unknown patch field");
            }
            if value.is_null() && ["title", "time", "reminders", "location"].contains(&key.as_str())
            {
                return Err("required fields and reminders cannot be cleared");
            }
            next[key] = value.clone();
        }
        let next: Self = serde_json::from_value(next).map_err(|_| "invalid patch value")?;
        next.validate()?;
        Ok(next)
    }

    pub fn provider_json(&self) -> Result<Value, &'static str> {
        self.validate()?;
        let mut value = serde_json::json!({
            "title": self.title,
            "time": {"start_at": utc_string(&self.time.start_at)?, "end_at": utc_string(&self.time.end_at)?, "time_zone": self.time.time_zone},
            "description": self.description.as_deref().unwrap_or("")
        });
        if let Some(location) = &self.location {
            value["location"] = serde_json::json!({"name": location.name});
        }
        if let Some(reminders) = &self.reminders {
            value["reminders"] = serde_json::json!(reminders);
        }
        Ok(value)
    }
}

fn utc_string(raw: &str) -> Result<String, &'static str> {
    DateTime::parse_from_rfc3339(raw)
        .map(|value| {
            value
                .with_timezone(&Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
        .map_err(|_| "time requires RFC3339 offset")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event() -> EventContent {
        serde_json::from_value(serde_json::json!({"title":"meeting","time":{"startAt":"2026-09-30T10:01:00+09:00","endAt":"2026-09-30T11:02:00+09:00"},"description":"keep"})).unwrap()
    }
    #[test]
    fn time_is_not_rounded_and_offsets_must_agree() {
        let mut event = event();
        assert!(event.validate().is_ok());
        assert_eq!(
            event.provider_json().unwrap()["time"]["start_at"],
            "2026-09-30T01:01:00Z"
        );
        event.time.time_zone = "UTC".into();
        assert!(event.validate().is_err());
        assert!(event.provider_json().is_err());
        event.time.start_at = "invalid".into();
        assert!(event.provider_json().is_err());
    }
    #[test]
    fn patches_distinguish_keep_set_and_clear() {
        let event = event();
        assert_eq!(
            event
                .patched(&serde_json::json!({"title":"new"}))
                .unwrap()
                .description,
            event.description
        );
        assert!(
            event
                .patched(&serde_json::json!({"description":null}))
                .unwrap()
                .description
                .is_none()
        );
        assert!(event.patched(&serde_json::json!({"title":null})).is_err());
        assert!(
            event
                .patched(&serde_json::json!({"recurrence":"daily"}))
                .is_err()
        );
        assert!(event.patched(&serde_json::json!({"reminders":[]})).is_err());
    }
}
