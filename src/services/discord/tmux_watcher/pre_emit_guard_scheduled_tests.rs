use super::*;
use crate::services::discord::scheduled_turn::ScheduledTurnMark;

/// A stale resume of an isolated scheduled turn surrenders nothing of the
/// channel's session; an ordinary turn's stale resume still clears it.
#[tokio::test]
async fn a_stale_isolated_resume_keeps_the_channel_session() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (mark, surrendered, kept) in [
        (
            ScheduledTurnMark::Scheduled {
                turn_id: "snapshot-turn".to_string(),
                isolated: true,
                outcome: None,
            },
            None,
            Some("live-sid"),
        ),
        (ScheduledTurnMark::NotScheduled, Some("live-sid"), None),
    ] {
        let channel = ChannelId::new(7_420_301);
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            ProviderKind::Claude,
            channel.get(),
            None,
            7,
            7_420_302,
            7_420_303,
            "prompt".to_string(),
            Some("snapshot-sid".to_string()),
            None,
            None,
            None,
            0,
        );
        state.scheduled = mark.clone();
        crate::services::discord::inflight::save_inflight_state(&state).unwrap();
        let shared = crate::services::discord::make_shared_data_for_tests();
        shared.core.lock().await.sessions.insert(
            channel,
            crate::services::discord::DiscordSession {
                session_id: Some("live-sid".to_string()),
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                channel_id: Some(channel.get()),
                channel_name: None,
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: 0,
            },
        );

        let stale = surrender_stale_channel_session(&shared, &ProviderKind::Claude, channel).await;

        assert_eq!(stale.as_deref(), surrendered, "{mark:?}");
        let sid = shared.core.lock().await.sessions[&channel]
            .session_id
            .clone();
        assert_eq!(sid.as_deref(), kept, "{mark:?}");
    }
}
