use super::*;
use crate::services::discord::scheduled_turn::ChannelEffects;

const CHANNEL: u64 = 7_420_201;

fn live_session() -> crate::services::discord::DiscordSession {
    crate::services::discord::DiscordSession {
        session_id: Some("live-sid".to_string()),
        memento_context_loaded: false,
        memento_reflected: false,
        current_path: None,
        history: Vec::new(),
        pending_uploads: Vec::new(),
        cleared: false,
        remote_profile_name: None,
        channel_id: Some(CHANNEL),
        channel_name: Some("live-channel".to_string()),
        category_name: None,
        last_active: tokio::time::Instant::now(),
        worktree: None,
        born_generation: 0,
    }
}

/// The watcher's selector writeback: an isolated turn saves only its own observed
/// id and leaves the channel's in-memory session as it was.
#[tokio::test]
async fn an_isolated_turn_never_saves_or_rewrites_the_channel_selector() {
    let cases = [
        (ChannelEffects::Allowed, None, Some("live-sid"), "live-sid"),
        (
            ChannelEffects::Allowed,
            Some("fresh-sid"),
            Some("fresh-sid"),
            "fresh-sid",
        ),
        (ChannelEffects::Suppressed, None, None, "live-sid"),
        (
            ChannelEffects::Suppressed,
            Some("snapshot-sid"),
            Some("snapshot-sid"),
            "live-sid",
        ),
    ];
    for (effects, observed, saved, in_memory) in cases {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(CHANNEL);
        shared
            .core
            .lock()
            .await
            .sessions
            .insert(channel, live_session());
        let persisted = persist_watcher_provider_session_id(
            &shared,
            channel,
            &ProviderKind::Claude,
            "AgentDesk-claude-i7420-isolated",
            observed,
            effects,
        )
        .await;
        let case = format!("{effects:?} observed={observed:?}");
        assert_eq!(persisted.as_deref(), saved, "{case}");
        let channel_sid = shared.core.lock().await.sessions[&channel]
            .session_id
            .clone();
        assert_eq!(channel_sid.as_deref(), Some(in_memory), "{case}");
    }
}
