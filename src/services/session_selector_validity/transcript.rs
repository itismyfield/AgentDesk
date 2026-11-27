use crate::db::dispatched_sessions as dispatched_sessions_db;

pub(crate) fn selected_provider_resume_selector_for_provider<'a>(
    provider_name: Option<&str>,
    ids: &'a dispatched_sessions_db::ProviderSessionIds,
) -> Option<&'a str> {
    selected_provider_session(provider_name, ids, None)
}

pub(crate) fn selected_provider_session<'a>(
    provider_name: Option<&str>,
    ids: &'a dispatched_sessions_db::ProviderSessionIds,
    claude_home: Option<&std::path::Path>,
) -> Option<&'a str> {
    if provider_is_claude(provider_name) {
        selected_provider_resume_selector_with_claude_home(ids, claude_home)
    } else {
        selected_provider_resume_selector(ids)
    }
}

fn selected_provider_resume_selector(
    ids: &dispatched_sessions_db::ProviderSessionIds,
) -> Option<&str> {
    ids.claude_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| raw_provider_resume_selector(ids))
}

pub(crate) fn raw_provider_resume_selector(
    ids: &dispatched_sessions_db::ProviderSessionIds,
) -> Option<&str> {
    ids.raw_provider_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(crate) fn selected_provider_resume_selector_with_claude_home<'a>(
    ids: &'a dispatched_sessions_db::ProviderSessionIds,
    claude_home: Option<&std::path::Path>,
) -> Option<&'a str> {
    let cached = ids
        .claude_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let raw = raw_provider_resume_selector(ids);
    let cwd = ids
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let cached_activity = cwd.zip(cached).and_then(|(cwd, selector)| {
        claude_selector_file_activity(cwd, selector, claude_home, None, false)
    });
    let raw_activity = cwd.zip(raw).and_then(|(cwd, selector)| {
        let evidence = raw_transcript_persisted_growth_evidence(ids, selector);
        claude_selector_file_activity(
            cwd,
            selector,
            claude_home,
            evidence.len_watermark,
            evidence.growth_proven,
        )
    });

    super::choose_provider_session_selector(
        cached,
        raw,
        cached_activity,
        raw_activity,
        ids.cache_entry_age_secs,
        crate::services::tui_turn_state::STALE_USER_SUBMITTED_RECLAIM_SECS,
    )
}

fn claude_selector_file_activity(
    cwd: &str,
    selector: &str,
    claude_home: Option<&std::path::Path>,
    persisted_len_watermark: Option<u64>,
    persisted_growth_proven: bool,
) -> Option<super::SelectorFileActivity> {
    claude_selector_file_activity_sample(cwd, selector, claude_home).map(|activity| {
        super::activity_with_observed_growth(
            selector,
            activity,
            persisted_len_watermark,
            persisted_growth_proven,
        )
    })
}

pub(crate) fn claude_selector_file_activity_sample(
    cwd: &str,
    selector: &str,
    claude_home: Option<&std::path::Path>,
) -> Option<super::SelectorFileActivity> {
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        std::path::Path::new(cwd),
        selector,
        claude_home,
    )
    .ok()?;
    let Ok(metadata) = std::fs::metadata(&path) else {
        return Some(super::SelectorFileActivity {
            exists: false,
            len: 0,
            mtime_age_secs: None,
            observed_growth_since_previous_sample: false,
        });
    };
    Some(super::SelectorFileActivity {
        exists: true,
        len: metadata.len(),
        mtime_age_secs: file_mtime_age_secs(&metadata),
        observed_growth_since_previous_sample: false,
    })
}

fn positive_raw_transcript_len_watermark(
    ids: &dispatched_sessions_db::ProviderSessionIds,
) -> Option<u64> {
    ids.raw_provider_transcript_len_watermark
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0)
}

struct RawTranscriptPersistedGrowthEvidence {
    len_watermark: Option<u64>,
    growth_proven: bool,
}

fn raw_transcript_persisted_growth_evidence(
    ids: &dispatched_sessions_db::ProviderSessionIds,
    raw_provider_session_id: &str,
) -> RawTranscriptPersistedGrowthEvidence {
    let raw_provider_session_id = raw_provider_session_id.trim();
    let watermark_matches_raw_id = !raw_provider_session_id.is_empty()
        && ids
            .raw_provider_transcript_watermark_session_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|watermark_session_id| watermark_session_id == raw_provider_session_id);
    if !watermark_matches_raw_id {
        return RawTranscriptPersistedGrowthEvidence {
            len_watermark: None,
            growth_proven: false,
        };
    }
    RawTranscriptPersistedGrowthEvidence {
        len_watermark: positive_raw_transcript_len_watermark(ids),
        growth_proven: ids.raw_provider_transcript_growth_proven,
    }
}

fn file_mtime_age_secs(metadata: &std::fs::Metadata) -> Option<i64> {
    let modified = metadata.modified().ok()?;
    let age = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    i64::try_from(age.as_secs()).ok()
}

pub(crate) fn provider_is_claude(provider_name: Option<&str>) -> bool {
    provider_name.is_some_and(|provider| provider.eq_ignore_ascii_case("claude"))
}
