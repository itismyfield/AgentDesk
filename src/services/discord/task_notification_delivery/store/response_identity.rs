//! Logical turn identity parsing shared by the response delivery fence.

use chrono::TimeZone;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ResponseTurnCoordinates {
    pub(super) start_offset: Option<i64>,
    pub(super) end_offset: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TurnRelation {
    Same,
    Distinct,
    Unknown,
}

impl TurnRelation {
    pub(super) fn absorb(&mut self, next: Self) -> bool {
        match next {
            Self::Same => {
                *self = Self::Same;
                true
            }
            Self::Unknown if *self == Self::Distinct => {
                *self = Self::Unknown;
                false
            }
            _ => false,
        }
    }
}

impl ResponseTurnCoordinates {
    pub(super) fn try_new(
        start_offset: Option<u64>,
        end_offset: Option<u64>,
    ) -> Result<Self, String> {
        fn db_offset(value: Option<u64>, name: &str) -> Result<Option<i64>, String> {
            value
                .map(|offset| i64::try_from(offset).map_err(|_| format!("{name} exceeds BIGINT")))
                .transpose()
        }
        let coordinates = Self {
            start_offset: db_offset(start_offset, "turn_start_offset")?,
            end_offset: db_offset(end_offset, "turn_end_offset")?,
        };
        if matches!(
            (coordinates.start_offset, coordinates.end_offset),
            (Some(start), Some(end)) if end < start
        ) {
            return Err("turn_end_offset precedes turn_start_offset".to_string());
        }
        Ok(coordinates)
    }

    pub(super) fn relation(self, persisted: Self) -> TurnRelation {
        match (self.start_offset, persisted.start_offset) {
            (Some(incoming), Some(current)) => {
                return if incoming == current {
                    TurnRelation::Same
                } else {
                    TurnRelation::Distinct
                };
            }
            _ => {}
        }
        match (self.end_offset, persisted.end_offset) {
            (Some(incoming), Some(current)) if incoming == current => TurnRelation::Same,
            (Some(_), Some(_)) => TurnRelation::Distinct,
            _ => TurnRelation::Unknown,
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::{ResponseTurnCoordinates, TurnRelation};

    #[test]
    fn start_offset_is_authoritative_when_actor_consumed_ends_differ() {
        let watcher = ResponseTurnCoordinates::try_new(Some(100), Some(180)).unwrap();
        let sink = ResponseTurnCoordinates::try_new(Some(100), Some(220)).unwrap();
        assert_eq!(watcher.relation(sink), TurnRelation::Same);
    }

    #[test]
    fn invalid_or_unrepresentable_coordinates_fail_before_claiming() {
        assert!(ResponseTurnCoordinates::try_new(Some(101), Some(100)).is_err());
        assert!(ResponseTurnCoordinates::try_new(Some(i64::MAX as u64 + 1), None).is_err());
        assert!(ResponseTurnCoordinates::try_new(None, Some(i64::MAX as u64 + 1)).is_err());
    }
}
