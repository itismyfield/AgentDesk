#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SelectorFileActivity {
    pub(crate) exists: bool,
    pub(crate) len: u64,
    pub(crate) mtime_age_secs: Option<i64>,
}

pub(crate) fn choose_provider_session_selector<'a>(
    claude_session_id: Option<&'a str>,
    raw_provider_session_id: Option<&'a str>,
    claude_activity: Option<SelectorFileActivity>,
    raw_activity: Option<SelectorFileActivity>,
    stale_after_secs: i64,
) -> Option<&'a str> {
    let cached = normalized(claude_session_id);
    let raw = normalized(raw_provider_session_id);

    if let (Some(cached_value), Some(raw_value)) = (cached, raw)
        && cached_value != raw_value
        && selector_file_stale_or_missing(claude_activity, stale_after_secs)
        && selector_file_recently_growing(raw_activity, stale_after_secs)
    {
        return Some(raw_value);
    }

    cached.or(raw)
}

fn normalized(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
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
        };
        let raw = SelectorFileActivity {
            exists: true,
            len: 14_400_000,
            mtime_age_secs: Some(12),
        };

        assert_eq!(
            choose_provider_session_selector(
                Some("c62c2dc8-0000-4000-8000-000000000000"),
                Some("48fdb7f3-0000-4000-8000-000000000000"),
                Some(cached),
                Some(raw),
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
        };
        let raw = SelectorFileActivity {
            exists: true,
            len: 65_536,
            mtime_age_secs: Some(4),
        };

        assert_eq!(
            choose_provider_session_selector(
                Some("c62c2dc8-0000-4000-8000-000000000000"),
                Some("48fdb7f3-0000-4000-8000-000000000000"),
                Some(cached),
                Some(raw),
                stale_after_secs,
            ),
            Some("c62c2dc8-0000-4000-8000-000000000000")
        );
    }
}
