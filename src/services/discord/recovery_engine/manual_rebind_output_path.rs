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

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionCacheSelectorState {
    selector_present: bool,
    selected_session_id: Option<String>,
    cached_session_id: Option<String>,
    raw_provider_session_id: Option<String>,
    cwd: Option<String>,
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
    output_path_override: Option<&str>,
) -> Option<String> {
    if let Some(output_path) = output_path_override {
        return Some(output_path.to_string());
    }
    let session_cache_selector_state =
        session_cache_selector_state_for_rebind(shared, provider, tmux_session_name).await;
    let session_cache_selector_present = session_cache_selector_state
        .as_ref()
        .map(|state| state.selector_present)
        .unwrap_or_else(|| existing_session_id.is_some_and(|id| !id.trim().is_empty()));
    saved_output_path_for_rebind_resolution_with_cache_state(
        existing_saved_output_path,
        session_cache_selector_present,
        provider,
        tmux_session_name,
        session_cache_selector_state.as_ref(),
        None,
    )
}

fn saved_output_path_for_rebind_resolution_with_cache_state<'a>(
    existing_saved_output_path: Option<&'a str>,
    session_cache_selector_present: bool,
    provider: &ProviderKind,
    tmux_session_name: &str,
    session_cache_selector_state: Option<&SessionCacheSelectorState>,
    claude_home: Option<&std::path::Path>,
) -> Option<String> {
    let saved_path_present = existing_saved_output_path
        .map(str::trim)
        .is_some_and(|path| !path.is_empty());
    let decision = decide_saved_output_path_for_manual_rebind(
        saved_path_present,
        session_cache_selector_present,
        existing_saved_output_path.and_then(saved_output_path_activity),
        latest_runtime_activity_age_secs(
            tmux_session_name,
            provider,
            session_cache_selector_state,
            claude_home,
        ),
        crate::services::tui_turn_state::STALE_USER_SUBMITTED_RECLAIM_SECS,
    );
    match decision {
        SavedOutputPathDecision::Keep => existing_saved_output_path.map(str::to_string),
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
            if let Some(transcript_path) = fresher_claude_tui_selector_transcript_path(
                provider,
                tmux_session_name,
                session_cache_selector_state,
                claude_home,
            ) {
                return Some(transcript_path.display().to_string());
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

fn latest_runtime_activity_age_secs(
    tmux_session_name: &str,
    provider: &ProviderKind,
    session_cache_selector_state: Option<&SessionCacheSelectorState>,
    claude_home: Option<&std::path::Path>,
) -> Option<i64> {
    let activity = latest_runtime_activity_unix_nanos_for_manual_rebind(
        tmux_session_name,
        provider,
        session_cache_selector_state,
        claude_home,
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

fn latest_runtime_activity_unix_nanos_for_manual_rebind(
    tmux_session_name: &str,
    provider: &ProviderKind,
    session_cache_selector_state: Option<&SessionCacheSelectorState>,
    claude_home: Option<&std::path::Path>,
) -> i64 {
    crate::services::dispatched_sessions::latest_runtime_activity_unix_nanos(tmux_session_name).max(
        claude_tui_selector_transcript_activity_unix_nanos(
            provider,
            session_cache_selector_state,
            claude_home,
        ),
    )
}

fn claude_tui_selector_transcript_activity_unix_nanos(
    provider: &ProviderKind,
    session_cache_selector_state: Option<&SessionCacheSelectorState>,
    claude_home: Option<&std::path::Path>,
) -> i64 {
    claude_tui_selector_transcript_candidates(provider, session_cache_selector_state, claude_home)
        .into_iter()
        .filter_map(|path| metadata_mtime_unix_nanos(&path))
        .max()
        .unwrap_or(0)
}

fn fresher_claude_tui_selector_transcript_path(
    provider: &ProviderKind,
    tmux_session_name: &str,
    session_cache_selector_state: Option<&SessionCacheSelectorState>,
    claude_home: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    let wrapper_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    let wrapper_activity =
        metadata_mtime_unix_nanos(std::path::Path::new(&wrapper_path)).unwrap_or(0);
    claude_tui_selector_transcript_candidates(provider, session_cache_selector_state, claude_home)
        .into_iter()
        .filter_map(|path| {
            let activity = metadata_mtime_unix_nanos(&path)?;
            (activity > wrapper_activity).then_some((activity, path))
        })
        .max_by_key(|(activity, _)| *activity)
        .map(|(_, path)| path)
}

fn claude_tui_selector_transcript_candidates(
    provider: &ProviderKind,
    session_cache_selector_state: Option<&SessionCacheSelectorState>,
    claude_home: Option<&std::path::Path>,
) -> Vec<std::path::PathBuf> {
    if provider != &ProviderKind::Claude {
        return Vec::new();
    }
    let Some(state) = session_cache_selector_state else {
        return Vec::new();
    };
    let Some(cwd) = state
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for session_id in [
        state.cached_session_id.as_deref(),
        state.raw_provider_session_id.as_deref(),
        state.selected_session_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|value| !value.is_empty())
    {
        if !seen.insert(session_id.to_string()) {
            continue;
        }
        if let Ok(path) = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            std::path::Path::new(cwd),
            session_id,
            claude_home,
        ) {
            candidates.push(path);
        }
    }
    candidates
}

fn metadata_mtime_unix_nanos(path: &std::path::Path) -> Option<i64> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
}

async fn session_cache_selector_state_for_rebind(
    shared: &SharedData,
    provider: &ProviderKind,
    tmux_session_name: &str,
) -> Option<SessionCacheSelectorState> {
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
            Ok(Some(ids)) => {
                let selected_session_id =
                    crate::services::dispatched_sessions::selected_provider_resume_selector_for_provider_recording_observation(
                        pool,
                        &session_key,
                        Some(provider.as_str()),
                        &ids,
                    )
                    .await;
                return Some(SessionCacheSelectorState {
                    selector_present: provider_session_ids_have_any_selector(&ids),
                    selected_session_id,
                    cached_session_id: ids.claude_session_id,
                    raw_provider_session_id: ids.raw_provider_session_id,
                    cwd: ids.cwd,
                });
            }
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
    Some(SessionCacheSelectorState {
        selector_present: false,
        selected_session_id: None,
        cached_session_id: None,
        raw_provider_session_id: None,
        cwd: None,
    })
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

    #[tokio::test]
    async fn health_rebind_output_override_bypasses_saved_path_and_selector_resolution() {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let selected = saved_output_path_for_rebind_resolution(
            &shared,
            &crate::services::provider::ProviderKind::Claude,
            Some("/stale/saved/transcript.jsonl"),
            Some("00000000-0000-4000-8000-000000000001"),
            "AgentDesk-claude-override-bypass",
            Some("/operator/selected/transcript.jsonl"),
        )
        .await;

        assert_eq!(
            selected.as_deref(),
            Some("/operator/selected/transcript.jsonl")
        );
    }

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

    #[test]
    fn claude_tui_selector_transcript_activity_re_resolves_stale_saved_path() {
        let saved_dir = tempfile::tempdir().expect("saved output tempdir");
        let saved_path = saved_dir.path().join("old-wrapper.jsonl");
        std::fs::write(&saved_path, b"old wrapper\n").expect("write saved output");
        filetime::set_file_mtime(
            &saved_path,
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() - std::time::Duration::from_secs(700),
            ),
        )
        .expect("stale saved mtime");

        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let session_id = uuid::Uuid::new_v4().to_string();
        let transcript_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            &session_id,
            Some(claude_home.path()),
        )
        .expect("transcript path");
        std::fs::create_dir_all(transcript_path.parent().expect("transcript parent"))
            .expect("create transcript parent");
        std::fs::write(&transcript_path, b"fresh transcript\n").expect("write transcript");
        filetime::set_file_mtime(
            &transcript_path,
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() - std::time::Duration::from_secs(5),
            ),
        )
        .expect("fresh transcript mtime");
        let selector_state = SessionCacheSelectorState {
            selector_present: true,
            selected_session_id: Some(session_id.clone()),
            cached_session_id: Some(session_id.clone()),
            raw_provider_session_id: None,
            cwd: Some(cwd.path().display().to_string()),
        };
        let saved_path_string = saved_path.display().to_string();
        let transcript_path_string = transcript_path.display().to_string();

        assert!(
            claude_tui_selector_transcript_activity_unix_nanos(
                &crate::services::provider::ProviderKind::Claude,
                Some(&selector_state),
                Some(claude_home.path()),
            ) > 0
        );
        assert_eq!(
            saved_output_path_for_rebind_resolution_with_cache_state(
                Some(saved_path_string.as_str()),
                true,
                &crate::services::provider::ProviderKind::Claude,
                "tmux-with-no-wrapper-activity",
                Some(&selector_state),
                Some(claude_home.path()),
            ),
            Some(transcript_path_string),
            "manual rebind must adopt the fresh Claude transcript instead of falling back to the wrapper jsonl"
        );
    }

    #[test]
    fn claude_tui_selector_transcript_activity_probes_cached_and_raw_candidates() {
        let saved_dir = tempfile::tempdir().expect("saved output tempdir");
        let saved_path = saved_dir.path().join("old-wrapper.jsonl");
        std::fs::write(&saved_path, b"old wrapper\n").expect("write saved output");
        filetime::set_file_mtime(
            &saved_path,
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() - std::time::Duration::from_secs(700),
            ),
        )
        .expect("stale saved mtime");

        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let cached_session_id = uuid::Uuid::new_v4().to_string();
        let raw_session_id = uuid::Uuid::new_v4().to_string();
        let cached_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            &cached_session_id,
            Some(claude_home.path()),
        )
        .expect("cached transcript path");
        let raw_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            &raw_session_id,
            Some(claude_home.path()),
        )
        .expect("raw transcript path");
        std::fs::create_dir_all(cached_path.parent().expect("cached parent"))
            .expect("create cached parent");
        std::fs::create_dir_all(raw_path.parent().expect("raw parent")).expect("create raw parent");
        std::fs::write(&cached_path, b"stale cached transcript\n")
            .expect("write cached transcript");
        std::fs::write(&raw_path, b"fresh raw transcript\n").expect("write raw transcript");
        filetime::set_file_mtime(
            &cached_path,
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() - std::time::Duration::from_secs(700),
            ),
        )
        .expect("stale cached mtime");
        filetime::set_file_mtime(
            &raw_path,
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() - std::time::Duration::from_secs(5),
            ),
        )
        .expect("fresh raw mtime");
        let selector_state = SessionCacheSelectorState {
            selector_present: true,
            selected_session_id: Some(cached_session_id.clone()),
            cached_session_id: Some(cached_session_id),
            raw_provider_session_id: Some(raw_session_id),
            cwd: Some(cwd.path().display().to_string()),
        };
        let saved_path_string = saved_path.display().to_string();

        assert_eq!(
            saved_output_path_for_rebind_resolution_with_cache_state(
                Some(saved_path_string.as_str()),
                true,
                &crate::services::provider::ProviderKind::Claude,
                "tmux-with-stale-selected-cached-transcript",
                Some(&selector_state),
                Some(claude_home.path()),
            ),
            Some(raw_path.display().to_string()),
            "manual rebind must probe the raw transcript even when the selected cached id is stale"
        );
    }
}
