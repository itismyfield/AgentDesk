//! Host guard on the session-died recovery retry, through the recovery handler itself.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::recovery_retry::{
    RecoveryRetryContext, RecoveryRetryMessage, RecoveryRetryOutcome, RecoveryRetryState,
    handle_recovery_retry,
};
use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedState;
use crate::db::dispatched_sessions::hosted_execution::tests::{owner, record, wire};
use crate::services::discord::inflight::seed_session_row;
use crate::services::provider::CancelToken;

/// Counts retry-with-history scheduling and accepts the recovery notice edit.
struct RetryCounter(Arc<AtomicUsize>);

impl TurnGateway for RetryCounter {
    fn send_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        panic!("recovery retry must not send a message")
    }

    fn edit_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn replace_message_with_outcome<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        _content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        panic!("recovery retry must not replace a message")
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        _channel_id: ChannelId,
        _user_message_id: MessageId,
        _user_text: &'a str,
    ) -> GatewayFuture<'a, ()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        _channel_id: ChannelId,
        _intervention: &'a Intervention,
        _request_owner_name: &'a str,
        _has_more_queued_turns: bool,
        _dispatch_lease: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        panic!("recovery retry must not dispatch a queued turn")
    }

    fn validate_live_routing<'a>(
        &'a self,
        _channel_id: ChannelId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn requester_mention(&self) -> Option<String> {
        None
    }

    fn can_chain_locally(&self) -> bool {
        false
    }

    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        Some(ProviderKind::Claude)
    }
}

fn resumable_session() -> crate::services::discord::DiscordSession {
    crate::services::discord::DiscordSession {
        session_id: Some("sid-core".to_string()),
        memento_context_loaded: false,
        memento_reflected: false,
        current_path: None,
        history: Vec::new(),
        pending_uploads: Vec::new(),
        cleared: false,
        remote_profile_name: None,
        channel_id: None,
        channel_name: None,
        category_name: None,
        last_active: tokio::time::Instant::now(),
        worktree: None,
        born_generation: 0,
    }
}

// A session that died during restart recovery, with a user message to retry: only a
// turn whose own key finds a legacy row loses its resume ids, is killed and is requeued.
#[tokio::test]
async fn session_died_recovery_retries_only_a_session_the_host_guard_admits_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let bound = wire(&record(
        &owner("1479671301387059502"),
        "n1",
        HostedState::Bound,
    ));
    let legacy = seed_session_row(&pool, "p4c3w1-retry-legacy", 1479671301387059501, None);
    let legacy = legacy.await;
    let bound = seed_session_row(
        &pool,
        "p4c3w1-retry-bound",
        1479671301387059502,
        Some(bound),
    );
    let bound = bound.await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let cases = [
        (Some(legacy), "p4c3w1-retry-legacy", 501, true),
        (Some(bound), "p4c3w1-retry-bound", 502, false),
        (None, "p4c3w1-retry-no-key", 503, false),
    ];
    for (key, name, channel, admitted) in cases {
        let channel_id = ChannelId::new(1479671301387059000 + channel);
        let session = resumable_session();
        shared
            .core
            .lock()
            .await
            .sessions
            .insert(channel_id, session);
        let token = Arc::new(CancelToken::new());
        token.bind_unmanaged_session_name(name);
        let scheduled = Arc::new(AtomicUsize::new(0));
        let gateway: Arc<dyn TurnGateway> = Arc::new(RetryCounter(scheduled.clone()));
        let (mut sid, mut raw) = (Some("sid".to_string()), Some("raw".to_string()));
        let mut full_response = "partial".to_string();
        let mut inflight = InflightTurnState::new(
            ProviderKind::Claude,
            channel_id.get(),
            None,
            1,
            7,
            8,
            "retry me".to_string(),
            Some("sid".to_string()),
            Some(name.to_string()),
            None,
            None,
            0,
        );
        let text = "retry me".to_string();
        let outcome = handle_recovery_retry(
            RecoveryRetryMessage::SessionDiedDuringRecovery,
            RecoveryRetryContext {
                shared_owned: &shared,
                gateway: &gateway,
                cancel_token: &token,
                channel_id,
                user_msg_id: Some(MessageId::new(7)),
                current_msg_id: MessageId::new(8),
                adk_session_key: &key,
                user_text_owned: &text,
            },
            RecoveryRetryState {
                full_response: &mut full_response,
                new_session_id: &mut sid,
                new_raw_provider_session_id: &mut raw,
                inflight_state: &mut inflight,
            },
        )
        .await;
        assert_eq!(outcome, RecoveryRetryOutcome::Continue, "{name}");
        // Scheduling runs on a spawned task; a refused turn never spawns one.
        for _ in 0..200 {
            if scheduled.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let kept = |id: &str| (!admitted).then(|| id.to_string());
        assert_eq!((sid, raw), (kept("sid"), kept("raw")), "{name}");
        assert_eq!(inflight.session_id, kept("sid"), "{name}");
        let core_sid = shared.core.lock().await.sessions[&channel_id]
            .session_id
            .clone();
        assert_eq!(core_sid, kept("sid-core"), "{name}");
        let requeued = scheduled.load(Ordering::SeqCst);
        assert_eq!(requeued, usize::from(admitted), "{name}");
        let exit_reason = crate::services::tmux_common::session_temp_path(name, "exit_reason");
        let killed = std::path::Path::new(&exit_reason).exists();
        assert_eq!(killed, admitted, "{name}");
    }
    pool.close().await;
    db.drop().await;
}
