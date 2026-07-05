use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const RECENT_MISSING_CACHED_TRANSCRIPT_GRACE_SECS: i64 = 60;
const SELECTOR_OBSERVATION_RETENTION_SECS: u64 = 6 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SelectorFileActivity {
    pub(crate) exists: bool,
    pub(crate) len: u64,
    pub(crate) mtime_age_secs: Option<i64>,
    pub(crate) observed_growth_since_previous_sample: bool,
}

pub(crate) fn choose_provider_session_selector<'a>(
    claude_session_id: Option<&'a str>,
    raw_provider_session_id: Option<&'a str>,
    claude_activity: Option<SelectorFileActivity>,
    raw_activity: Option<SelectorFileActivity>,
    cache_entry_age_secs: Option<i64>,
    stale_after_secs: i64,
) -> Option<&'a str> {
    let cached = normalized(claude_session_id);
    let raw = normalized(raw_provider_session_id);

    if let Some(cached_value) = cached
        && raw.is_some()
        && selector_file_missing(claude_activity)
        && cache_entry_recent(cache_entry_age_secs)
    {
        return Some(cached_value);
    }

    if let (Some(cached_value), Some(raw_value)) = (cached, raw)
        && cached_value != raw_value
        && selector_file_stale_or_missing(claude_activity, stale_after_secs)
        && selector_file_recently_growing(raw_activity, stale_after_secs)
    {
        return Some(raw_value);
    }

    cached.or(raw)
}

#[derive(Clone, Copy, Debug)]
struct SelectorObservation {
    len: u64,
    observed_at: Instant,
}

static SELECTOR_OBSERVATIONS: LazyLock<Mutex<HashMap<String, SelectorObservation>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn activity_with_observed_growth(
    selector: &str,
    activity: SelectorFileActivity,
    persisted_len_watermark: Option<u64>,
) -> SelectorFileActivity {
    let mut activity = activity;
    let persisted_growth = activity.exists
        && activity.len > 0
        && persisted_len_watermark.is_some_and(|watermark| activity.len > watermark);
    activity.observed_growth_since_previous_sample =
        persisted_growth || selector_file_observed_growth(selector, activity);
    activity
}

fn selector_file_observed_growth(selector: &str, activity: SelectorFileActivity) -> bool {
    let Some(selector) = normalized(Some(selector)) else {
        return false;
    };
    if !activity.exists || activity.len == 0 {
        return false;
    }
    let mut observations = SELECTOR_OBSERVATIONS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let now = Instant::now();
    observations.retain(|_, previous| {
        now.duration_since(previous.observed_at)
            <= Duration::from_secs(SELECTOR_OBSERVATION_RETENTION_SECS)
    });
    let grew = observations
        .get(selector)
        .is_some_and(|previous| activity.len > previous.len);
    observations.insert(
        selector.to_string(),
        SelectorObservation {
            len: activity.len,
            observed_at: now,
        },
    );
    grew
}

#[cfg(test)]
pub(crate) fn clear_selector_observations_for_tests() {
    SELECTOR_OBSERVATIONS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clear();
}

fn normalized(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn selector_file_missing(activity: Option<SelectorFileActivity>) -> bool {
    match activity {
        Some(activity) => !activity.exists,
        None => true,
    }
}

fn cache_entry_recent(age_secs: Option<i64>) -> bool {
    age_secs.is_some_and(|age_secs| age_secs < RECENT_MISSING_CACHED_TRANSCRIPT_GRACE_SECS)
}

fn selector_file_stale_or_missing(
    activity: Option<SelectorFileActivity>,
    stale_after_secs: i64,
) -> bool {
    match activity {
        Some(activity) if !activity.exists => true,
        Some(activity) if activity.len == 0 => true,
        Some(activity) => activity
            .mtime_age_secs
            .is_some_and(|age_secs| age_secs >= stale_after_secs),
        None => true,
    }
}

fn selector_file_recently_growing(
    activity: Option<SelectorFileActivity>,
    stale_after_secs: i64,
) -> bool {
    activity.is_some_and(|activity| {
        activity.exists
            && activity.len > 0
            && activity.observed_growth_since_previous_sample
            && activity
                .mtime_age_secs
                .is_some_and(|age_secs| age_secs < stale_after_secs)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_cache_but_growing_raw_id_selects_raw_provider_session_id() {
        let stale_after_secs = 600;
        let cached = SelectorFileActivity {
            exists: true,
            len: 512_146,
            mtime_age_secs: Some(stale_after_secs + 1),
            observed_growth_since_previous_sample: false,
        };
        let raw = SelectorFileActivity {
            exists: true,
            len: 14_400_000,
            mtime_age_secs: Some(12),
            observed_growth_since_previous_sample: true,
        };

        assert_eq!(
            choose_provider_session_selector(
                Some("c62c2dc8-0000-4000-8000-000000000000"),
                Some("48fdb7f3-0000-4000-8000-000000000000"),
                Some(cached),
                Some(raw),
                Some(stale_after_secs + 1),
                stale_after_secs,
            ),
            Some("48fdb7f3-0000-4000-8000-000000000000")
        );
    }

    #[test]
    fn fresh_cached_id_keeps_legacy_selector_precedence() {
        let stale_after_secs = 600;
        let cached = SelectorFileActivity {
            exists: true,
            len: 32_768,
            mtime_age_secs: Some(5),
            observed_growth_since_previous_sample: false,
        };
        let raw = SelectorFileActivity {
            exists: true,
            len: 65_536,
            mtime_age_secs: Some(4),
            observed_growth_since_previous_sample: true,
        };

        assert_eq!(
            choose_provider_session_selector(
                Some("c62c2dc8-0000-4000-8000-000000000000"),
                Some("48fdb7f3-0000-4000-8000-000000000000"),
                Some(cached),
                Some(raw),
                Some(5),
                stale_after_secs,
            ),
            Some("c62c2dc8-0000-4000-8000-000000000000")
        );
    }

    #[test]
    fn raw_recent_mtime_without_observed_growth_keeps_cached_session_id() {
        let stale_after_secs = 600;
        let cached = SelectorFileActivity {
            exists: true,
            len: 512_146,
            mtime_age_secs: Some(stale_after_secs + 1),
            observed_growth_since_previous_sample: false,
        };
        let raw = SelectorFileActivity {
            exists: true,
            len: 14_400_000,
            mtime_age_secs: Some(12),
            observed_growth_since_previous_sample: false,
        };

        assert_eq!(
            choose_provider_session_selector(
                Some("c62c2dc8-0000-4000-8000-000000000000"),
                Some("48fdb7f3-0000-4000-8000-000000000000"),
                Some(cached),
                Some(raw),
                Some(stale_after_secs + 1),
                stale_after_secs,
            ),
            Some("c62c2dc8-0000-4000-8000-000000000000")
        );
    }

    #[test]
    fn recently_written_cache_with_missing_transcript_keeps_cached_session_id() {
        let stale_after_secs = 600;
        let cached_missing = SelectorFileActivity {
            exists: false,
            len: 0,
            mtime_age_secs: None,
            observed_growth_since_previous_sample: false,
        };
        let raw = SelectorFileActivity {
            exists: true,
            len: 14_400_000,
            mtime_age_secs: Some(12),
            observed_growth_since_previous_sample: true,
        };

        assert_eq!(
            choose_provider_session_selector(
                Some("c62c2dc8-0000-4000-8000-000000000000"),
                Some("48fdb7f3-0000-4000-8000-000000000000"),
                Some(cached_missing),
                Some(raw),
                Some(5),
                stale_after_secs,
            ),
            Some("c62c2dc8-0000-4000-8000-000000000000")
        );
        assert_eq!(
            choose_provider_session_selector(
                Some("c62c2dc8-0000-4000-8000-000000000000"),
                Some("48fdb7f3-0000-4000-8000-000000000000"),
                Some(cached_missing),
                Some(raw),
                Some(RECENT_MISSING_CACHED_TRANSCRIPT_GRACE_SECS),
                stale_after_secs,
            ),
            Some("48fdb7f3-0000-4000-8000-000000000000"),
            "once the cached-id recorded-at grace window expires, raw growth evidence must win"
        );
    }

    #[test]
    fn selector_growth_evidence_requires_two_length_samples() {
        clear_selector_observations_for_tests();
        let first = SelectorFileActivity {
            exists: true,
            len: 10,
            mtime_age_secs: Some(1),
            observed_growth_since_previous_sample: false,
        };
        let second_same = SelectorFileActivity { len: 10, ..first };
        let third_grown = SelectorFileActivity { len: 11, ..first };

        assert!(
            !activity_with_observed_growth("48fdb7f3-0000-4000-8000-000000000000", first, None,)
                .observed_growth_since_previous_sample
        );
        assert!(
            !activity_with_observed_growth(
                "48fdb7f3-0000-4000-8000-000000000000",
                second_same,
                None,
            )
            .observed_growth_since_previous_sample
        );
        assert!(
            activity_with_observed_growth(
                "48fdb7f3-0000-4000-8000-000000000000",
                third_grown,
                None,
            )
            .observed_growth_since_previous_sample
        );
    }

    #[test]
    fn persisted_watermark_proves_growth_after_observation_reset() {
        clear_selector_observations_for_tests();
        let raw = SelectorFileActivity {
            exists: true,
            len: 11,
            mtime_age_secs: Some(1),
            observed_growth_since_previous_sample: false,
        };

        assert!(
            activity_with_observed_growth("48fdb7f3-0000-4000-8000-000000000000", raw, Some(10),)
                .observed_growth_since_previous_sample
        );
    }

    #[test]
    fn equal_persisted_watermark_is_not_growth_evidence() {
        clear_selector_observations_for_tests();
        let raw = SelectorFileActivity {
            exists: true,
            len: 11,
            mtime_age_secs: Some(1),
            observed_growth_since_previous_sample: false,
        };

        assert!(
            !activity_with_observed_growth("48fdb7f3-0000-4000-8000-000000000000", raw, Some(11),)
                .observed_growth_since_previous_sample
        );
    }
}
