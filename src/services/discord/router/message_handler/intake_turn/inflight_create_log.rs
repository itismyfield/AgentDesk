use poise::serenity_prelude as serenity;

use crate::services::discord::inflight::{
    CreateNewInflightError, InflightTurnState, observe_inflight_create_collision,
};
use crate::services::provider::ProviderKind;

pub(crate) async fn record_turn_start_origin(
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    state: &InflightTurnState,
) {
    crate::services::discord::adk_session::record_turn_start_origin(
        state.session_key.as_deref(),
        provider,
        channel_id,
        state.turn_nonce.as_deref(),
        state.dispatch_id.is_some(),
    )
    .await;
}

pub(crate) fn log_create_new_inflight_outcome(
    result: Result<(), CreateNewInflightError>,
    provider: &ProviderKind,
    state: &InflightTurnState,
) {
    let channel_id = state.channel_id;
    let user_msg_id = state.user_msg_id;
    match result {
        Ok(()) => {}
        Err(CreateNewInflightError::AlreadyExists) => {
            // Measures turns that run without a durable row because a hidden
            // row still occupies the channel; never touches that row.
            let (verdict, collision) = observe_inflight_create_collision(provider, channel_id);
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
                user_msg_id,
                ?verdict,
                %collision,
                "inflight create skipped because a durable row already exists; continuing fail-closed"
            );
            crate::services::observability::emit_inflight_lifecycle_event(
                provider.as_str(),
                channel_id,
                state.dispatch_id.as_deref(),
                state.session_key.as_deref(),
                None,
                "inflight_create_collision",
                collision,
            );
        }
        Err(CreateNewInflightError::Internal(error)) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
                user_msg_id,
                %error,
                "inflight create failed internally; continuing without durable row"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // C3: a collision with a hidden legacy row is reported as one event, and
    // the observation neither refreshes the row (backfill) nor locks it (sidecar).
    #[test]
    fn already_exists_observation_leaves_a_hidden_legacy_row_untouched() {
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::tempdir().unwrap();
        let _root_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let (claude, text) = (ProviderKind::Claude, "prompt".to_string());
        let mut legacy = InflightTurnState::new(
            claude.clone(),
            5_996_111,
            None,
            1,
            2,
            3,
            text,
            None,
            None,
            None,
            None,
            0,
        );
        legacy.finalizer_turn_id = 0;
        let path = root
            .path()
            .join("runtime/discord_inflight/claude/5996111.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string(&legacy).unwrap()).unwrap();
        let aged = filetime::FileTime::from_unix_time(chrono::Utc::now().timestamp() - 301, 0);
        filetime::set_file_mtime(&path, aged).unwrap();
        let snapshot = || {
            (
                std::fs::read(&path).unwrap(),
                std::fs::metadata(&path).unwrap().modified().unwrap(),
            )
        };
        let (before, sidecar) = (snapshot(), path.with_extension("json.lock"));
        assert!(!sidecar.exists());

        let ((), events) =
            crate::services::observability::events::test_capture::capture_sync(|| {
                log_create_new_inflight_outcome(
                    Err(CreateNewInflightError::AlreadyExists),
                    &claude,
                    &legacy,
                )
            });
        let collision: Vec<_> = events
            .iter()
            .filter(|event| event.payload["kind"] == "inflight_create_collision")
            .map(|event| (event.channel_id, &event.payload["extra"]))
            .collect();
        assert_eq!(collision.len(), 1, "{events:?}");
        assert_eq!(collision[0].0, Some(5_996_111));
        assert_eq!(collision[0].1["verdict"], "hidden_stale");
        assert_eq!(collision[0].1["existing_user_msg_id"], legacy.user_msg_id);
        assert_eq!(snapshot(), before);
        assert!(
            !sidecar.exists(),
            "the observation must not take the row lock"
        );
    }
}
