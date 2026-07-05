use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SavedOutputPathActivity {
    pub(super) exists: bool,
    pub(super) len: u64,
    pub(super) mtime_age_secs: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SavedOutputPathDecision {
    Keep,
    ReResolve(&'static str),
}

pub(super) fn decide_saved_output_path_for_manual_rebind(
    saved_path_present: bool,
    session_selector_present: bool,
    saved_activity: Option<SavedOutputPathActivity>,
    runtime_activity_age_secs: Option<i64>,
    stale_after_secs: i64,
) -> SavedOutputPathDecision {
    if !saved_path_present {
        return SavedOutputPathDecision::ReResolve("missing_saved_output_path");
    }
    if !session_selector_present {
        return SavedOutputPathDecision::ReResolve("session_selector_cleared");
    }
    if saved_path_has_no_recent_growth(saved_activity, stale_after_secs)
        && runtime_activity_age_secs.is_some_and(|age_secs| age_secs < stale_after_secs)
    {
        return SavedOutputPathDecision::ReResolve("saved_output_path_stale_runtime_active");
    }
    SavedOutputPathDecision::Keep
}

fn saved_path_has_no_recent_growth(
    activity: Option<SavedOutputPathActivity>,
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

pub(super) async fn saved_output_path_for_rebind_resolution<'a>(
    shared: &SharedData,
    provider: &ProviderKind,
    existing_saved_output_path: Option<&'a str>,
    existing_session_id: Option<&str>,
    tmux_session_name: &str,
) -> Option<&'a str> {
    let session_cache_selector_present = session_cache_selector_present_for_rebind(
        shared,
        provider,
        tmux_session_name,
    )
    .await
    .unwrap_or_else(|| existing_session_id.is_some_and(|id| !id.trim().is_empty()));
    saved_output_path_for_rebind_resolution_with_cache_state(
        existing_saved_output_path,
        session_cache_selector_present,
        tmux_session_name,
    )
}

fn saved_output_path_for_rebind_resolution_with_cache_state<'a>(
    existing_saved_output_path: Option<&'a str>,
    session_cache_selector_present: bool,
    tmux_session_name: &str,
) -> Option<&'a str> {
    let saved_path_present = existing_saved_output_path
        .map(str::trim)
        .is_some_and(|path| !path.is_empty());
    let decision = decide_saved_output_path_for_manual_rebind(
        saved_path_present,
        session_cache_selector_present,
        existing_saved_output_path.and_then(saved_output_path_activity),
        latest_runtime_activity_age_secs(tmux_session_name),
        crate::services::tui_turn_state::STALE_USER_SUBMITTED_RECLAIM_SECS,
    );
    match decision {
        SavedOutputPathDecision::Keep => existing_saved_output_path,
        SavedOutputPathDecision::ReResolve(reason) => {
            if let Some(path) = existing_saved_output_path {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ♻ rebind ignoring persisted output path for {}: {} ({})",
                    tmux_session_name,
                    path,
                    reason
                );
            }
            None
        }
    }
}

fn saved_output_path_activity(path: &str) -> Option<SavedOutputPathActivity> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let Ok(metadata) = std::fs::metadata(path) else {
        return Some(SavedOutputPathActivity {
            exists: false,
            len: 0,
            mtime_age_secs: None,
        });
    };
    Some(SavedOutputPathActivity {
        exists: true,
        len: metadata.len(),
        mtime_age_secs: metadata.modified().ok().and_then(|modified| {
            i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(modified)
                    .unwrap_or_default()
                    .as_secs(),
            )
            .ok()
        }),
    })
}

fn latest_runtime_activity_age_secs(tmux_session_name: &str) -> Option<i64> {
    let activity = crate::services::dispatched_sessions::latest_runtime_activity_unix_nanos(
        tmux_session_name,
    );
    if activity <= 0 {
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok())?;
    Some(now.saturating_sub(activity) / 1_000_000_000)
}

async fn session_cache_selector_present_for_rebind(
    shared: &SharedData,
    provider: &ProviderKind,
    tmux_session_name: &str,
) -> Option<bool> {
    let pool = shared.pg_pool.as_ref()?;
    for session_key in super::super::adk_session::build_session_key_candidates(
        &shared.token_hash,
        provider,
        tmux_session_name,
    ) {
        match crate::db::dispatched_sessions::load_provider_session_ids_pg(
            pool,
            &session_key,
            Some(provider.as_str()),
        )
        .await
        {
            Ok(Some(ids)) => return Some(provider_session_ids_have_any_selector(&ids)),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    session_key,
                    provider = provider.as_str(),
                    error,
                    "manual rebind could not inspect dispatched-session selector cache"
                );
                return None;
            }
        }
    }
    Some(false)
}

fn provider_session_ids_have_any_selector(
    ids: &crate::db::dispatched_sessions::ProviderSessionIds,
) -> bool {
    ids.claude_session_id
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
        || ids
            .raw_provider_session_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleared_cache_or_zero_growth_saved_path_re_resolves_manual_rebind() {
        let stale_after_secs = 600;
        let stale_saved = SavedOutputPathActivity {
            exists: true,
            len: 512_146,
            mtime_age_secs: Some(stale_after_secs + 1),
        };

        assert_eq!(
            decide_saved_output_path_for_manual_rebind(
                true,
                false,
                Some(stale_saved),
                Some(10),
                stale_after_secs,
            ),
            SavedOutputPathDecision::ReResolve("session_selector_cleared")
        );
        assert_eq!(
            decide_saved_output_path_for_manual_rebind(
                true,
                true,
                Some(stale_saved),
                Some(10),
                stale_after_secs,
            ),
            SavedOutputPathDecision::ReResolve("saved_output_path_stale_runtime_active")
        );
    }

    #[test]
    fn quiet_runtime_keeps_saved_path_for_manual_rebind() {
        let stale_after_secs = 600;
        let stale_saved = SavedOutputPathActivity {
            exists: true,
            len: 512_146,
            mtime_age_secs: Some(stale_after_secs + 1),
        };

        assert_eq!(
            decide_saved_output_path_for_manual_rebind(
                true,
                true,
                Some(stale_saved),
                Some(stale_after_secs + 1),
                stale_after_secs,
            ),
            SavedOutputPathDecision::Keep
        );
    }
}
