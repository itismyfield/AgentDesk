use super::super::completion_delivery::CapturedRecoveryDelivery;
use super::super::idle_captured_response::settle_ready_without_output_for_actor;
use super::super::terminal_watcher::output_has_bytes_after_offset;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::{SharedData, inflight, mailbox_snapshot};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::{self as serenity, ChannelId};
use std::sync::Arc;

pub(in crate::services::discord) struct Fixture {
    pub(in crate::services::discord) state: inflight::InflightTurnState,
    pub(in crate::services::discord) shared: Arc<SharedData>,
    _env: crate::config::TestEnvVarGuard,
    _root: tempfile::TempDir,
}

impl Fixture {
    pub(in crate::services::discord) fn new(
        channel: u64,
        root: tempfile::TempDir,
        env: crate::config::TestEnvVarGuard,
        shared: Arc<SharedData>,
    ) -> Self {
        let mut state = recovery_state(ProviderKind::Claude, channel);
        state.born_generation = 0;
        state.full_response = "published prefix\nunposted suffix".to_string();
        state.response_sent_offset = "published prefix\n".len();
        let output = root.path().join("source.jsonl");
        std::fs::write(&output, b"{\"type\":\"assistant\"}\n").expect("source");
        state.output_path = Some(output.to_string_lossy().into_owned());
        state.last_offset = std::fs::metadata(&output).expect("source EOF").len();
        state.turn_start_offset = Some(0);
        Self {
            state,
            shared,
            _env: env,
            _root: root,
        }
    }

    pub(in crate::services::discord) fn persist(&self) {
        inflight::save_inflight_state(&self.state).expect("persist recovery obligation");
        assert!(
            !output_has_bytes_after_offset(
                self.state.output_path.as_deref().expect("source"),
                self.state.last_offset,
            ),
            "fixture is source EOF, not unread JSONL"
        );
    }

    pub(in crate::services::discord) async fn claim(&self) {
        self.persist();
        assert!(
            super::super::reregister_active_turn_from_inflight(&self.shared, &self.state).await
        );
    }

    /// A next input claims the released mailbox as a new turn, not a recovery.
    pub(in crate::services::discord) async fn next_input_claims(
        &self,
        next: &inflight::InflightTurnState,
    ) -> bool {
        self.shared
            .mailbox(ChannelId::new(next.channel_id))
            .try_start_turn(
                Arc::new(
                    crate::services::provider::CancelToken::from_persisted_turn_nonce(
                        next.turn_nonce.clone(),
                    ),
                ),
                serenity::model::id::UserId::new(next.request_owner_user_id),
                serenity::model::id::MessageId::new(next.effective_finalizer_turn_id()),
            )
            .await
    }

    pub(in crate::services::discord) async fn settle<F, Fut>(
        &self,
        state: &inflight::InflightTurnState,
        relay: F,
    ) -> bool
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future,
        Fut::Output: Into<CapturedRecoveryDelivery>,
    {
        let owner = mailbox_snapshot(&self.shared, ChannelId::new(state.channel_id)).await;
        let mut captured = state.clone();
        let persisted = self.load().expect("persisted fixture");
        captured.save_generation = persisted.save_generation;
        captured.updated_at = persisted.updated_at;
        settle_ready_without_output_for_actor(
            &self.shared,
            &captured.provider_kind().expect("fixture provider"),
            &captured,
            owner.cancel_token.as_ref(),
            relay,
        )
        .await
    }

    pub(in crate::services::discord) fn load(&self) -> Option<inflight::InflightTurnState> {
        inflight::load_inflight_state(
            &self.state.provider_kind().expect("fixture provider"),
            self.state.channel_id,
        )
    }
}

pub(in crate::services::discord) fn recovery_state(
    provider: ProviderKind,
    channel_id: u64,
) -> InflightTurnState {
    InflightTurnState::new(
        provider,
        channel_id,
        None,
        1,
        2,
        3,
        "restore reconcile gate".to_string(),
        Some(format!("session-{channel_id}")),
        Some(format!("AgentDesk-claude-{channel_id}")),
        Some(format!("/tmp/{channel_id}.jsonl")),
        None,
        0,
    )
}
