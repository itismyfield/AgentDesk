//! Idempotency helpers for recovery's no-anchor terminal-text delivery.

use std::sync::Arc;

use poise::serenity_prelude::{self as serenity, ChannelId, MessageId};

use super::super::{formatting, recovery_paths};
use super::RecoveryRelayOutcome;
use crate::services::discord::inflight::{opt_channel_id, opt_message_id};
use crate::services::discord::outbound::{delivery_frontier_probe, delivery_record};
use crate::services::discord::{
    DELIVERY_LEASE_DEADLINE_MS, DeliveryLeaseCell, DeliveryLeaseHeartbeat, DeliveryLeaseKey,
    LeaseHolder, LeaseOutcome, SharedData, inflight, lease_now_ms,
};
use crate::services::provider::ProviderKind;

#[derive(Clone)]
pub(in crate::services::discord) struct RecoveryDeliveryContext {
    provider: ProviderKind,
    channel_id: ChannelId,
    record_channel_id: ChannelId,
    tmux_session_name: Option<String>,
    lease_key: DeliveryLeaseKey,
    identity: inflight::InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    expected_current_msg_id: u64,
    durable_range: Option<(u64, u64)>,
    /// #4188: current transcript (output_path) byte length, snapshotted from the
    /// inflight state at construction. Bounds the durable frontier so a stale
    /// prior-generation/compaction frontier whose end exceeds the current
    /// transcript EOF is distrusted. `None` when the state has no output_path or
    /// it cannot be stat'd → fail-safe distrust.
    current_output_eof: Option<u64>,
    attempts: u32,
    reuse_recorded_anchor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum RecoveryAnchorReuse {
    DurableAlreadyDelivered(MessageId),
    InflightAnchor(MessageId),
}

impl RecoveryDeliveryContext {
    pub(in crate::services::discord) fn from_state(
        provider: &ProviderKind,
        state: &inflight::InflightTurnState,
        durable_range: Option<(u64, u64)>,
        delivery_generation: u64,
    ) -> Option<Self> {
        let channel_id = opt_channel_id(state.channel_id)?;
        Some(Self::from_state_for_channel(
            provider,
            state,
            channel_id,
            durable_range,
            delivery_generation,
            true,
        ))
    }

    pub(in crate::services::discord) fn send_new_after_gone_anchor(
        provider: &ProviderKind,
        state: &inflight::InflightTurnState,
        channel_id: ChannelId,
        durable_range: Option<(u64, u64)>,
        delivery_generation: u64,
    ) -> Self {
        Self::from_state_for_channel(
            provider,
            state,
            channel_id,
            durable_range,
            delivery_generation,
            false,
        )
    }

    pub(in crate::services::discord) fn with_record_channel_id(
        mut self,
        record_channel_id: ChannelId,
    ) -> Self {
        self.record_channel_id = record_channel_id;
        self
    }

    fn from_state_for_channel(
        provider: &ProviderKind,
        state: &inflight::InflightTurnState,
        channel_id: ChannelId,
        durable_range: Option<(u64, u64)>,
        delivery_generation: u64,
        reuse_recorded_anchor: bool,
    ) -> Self {
        let record_channel_id =
            opt_channel_id(state.delivery_record_owner_channel_id()).unwrap_or(channel_id);
        Self {
            provider: provider.clone(),
            channel_id,
            record_channel_id,
            tmux_session_name: state.tmux_session_name.clone(),
            lease_key: DeliveryLeaseKey::from_inflight_state_for_site(
                channel_id,
                delivery_generation,
                state,
                "recovery.no_anchor",
            ),
            identity: inflight::InflightTurnIdentity::from_state(state),
            expected_turn_start_offset: state.turn_start_offset,
            expected_current_msg_id: state.current_msg_id,
            durable_range,
            current_output_eof: state
                .output_path
                .as_deref()
                .and_then(|path| std::fs::metadata(path).ok().map(|meta| meta.len())),
            attempts: state.recovery_relay_attempts,
            reuse_recorded_anchor,
        }
    }

    pub(in crate::services::discord) fn anchor_reuse_decision(
        &self,
    ) -> Option<RecoveryAnchorReuse> {
        if !self.reuse_recorded_anchor {
            return None;
        }
        if let Some(anchor) = self.durable_recorded_anchor().and_then(opt_message_id) {
            return Some(RecoveryAnchorReuse::DurableAlreadyDelivered(anchor));
        }
        self.inflight_recorded_anchor()
            .and_then(opt_message_id)
            .map(RecoveryAnchorReuse::InflightAnchor)
    }

    #[cfg(test)]
    pub(in crate::services::discord) fn recorded_anchor(&self) -> Option<MessageId> {
        match self.anchor_reuse_decision() {
            Some(RecoveryAnchorReuse::DurableAlreadyDelivered(anchor))
            | Some(RecoveryAnchorReuse::InflightAnchor(anchor)) => Some(anchor),
            None => None,
        }
    }

    fn durable_recorded_anchor(&self) -> Option<u64> {
        let range = self.durable_range?;
        let tmux_session_name = self.tmux_session_name.as_deref()?;
        let anchor = delivery_frontier_probe::current_generation_delivered_anchor(
            &self.provider,
            self.record_channel_id,
            tmux_session_name,
            self.current_output_eof,
        )?;
        (anchor.panel_channel_id == self.channel_id.get() && anchor.range == range)
            .then_some(anchor.panel_msg_id)
    }

    fn inflight_recorded_anchor(&self) -> Option<u64> {
        inflight::recovery_anchor_msg_id_if_matches_identity(
            &self.provider,
            self.channel_id.get(),
            &self.identity,
            self.expected_turn_start_offset,
        )
    }

    pub(in crate::services::discord) fn try_acquire_fresh_send_lease(
        &self,
        shared: &Arc<SharedData>,
        text: &str,
    ) -> Option<RecoveryFreshSendLease> {
        let cell = shared.delivery_lease(self.channel_id);
        cell.reclaim_if_expired(lease_now_ms());
        let (start, end) = self.lease_range(text);
        let holder = LeaseHolder::Sink;
        let deadline = lease_now_ms().saturating_add(DELIVERY_LEASE_DEADLINE_MS);
        if !cell.try_acquire(self.lease_key.clone(), holder, start, end, deadline) {
            return None;
        }
        Some(RecoveryFreshSendLease {
            cell: cell.clone(),
            holder,
            key: self.lease_key.clone(),
            start,
            end,
            heartbeat: Some(DeliveryLeaseHeartbeat::spawn(
                cell,
                holder,
                self.lease_key.clone(),
            )),
            released: false,
        })
    }

    fn lease_range(&self, text: &str) -> (u64, u64) {
        if let Some((start, end)) = self.durable_range {
            if end > start {
                return (start, end);
            }
        }
        let start = self.expected_turn_start_offset.unwrap_or(0);
        let width = u64::try_from(text.len().max(1)).unwrap_or(u64::MAX);
        (start, start.saturating_add(width))
    }

    pub(in crate::services::discord) fn record_successful_fresh_send(
        &self,
        anchor: MessageId,
        text: &str,
    ) {
        let bind = inflight::bind_recovery_anchor_if_matches_identity(
            &self.provider,
            self.channel_id.get(),
            &self.identity,
            self.expected_turn_start_offset,
            self.expected_current_msg_id,
            anchor.get(),
            text.len(),
        );
        if matches!(
            bind,
            inflight::GuardedSaveOutcome::Saved | inflight::GuardedSaveOutcome::Missing
        ) {
            self.record_durable_frontier(anchor);
        } else {
            tracing::warn!(
                provider = %self.provider.as_str(),
                channel_id = self.channel_id.get(),
                anchor_msg_id = anchor.get(),
                outcome = ?bind,
                "recovery no-anchor delivery: inflight anchor bind did not persist; skipping durable anchor write"
            );
        }
    }

    fn record_durable_frontier(&self, anchor: MessageId) {
        let Some(range) = self.durable_range else {
            return;
        };
        if range.1 <= range.0 {
            tracing::warn!(
                provider = %self.provider.as_str(),
                channel_id = self.channel_id.get(),
                range = ?range,
                "recovery no-anchor delivery: refusing to record empty durable range"
            );
            return;
        }
        let Some(tmux_session_name) = self.tmux_session_name.as_deref() else {
            tracing::warn!(
                provider = %self.provider.as_str(),
                channel_id = self.channel_id.get(),
                "recovery no-anchor delivery: no tmux session name; durable anchor unavailable"
            );
            return;
        };
        let generation_mtime_ns = delivery_record::current_generation_mtime_ns(tmux_session_name);
        if generation_mtime_ns == 0 {
            tracing::warn!(
                provider = %self.provider.as_str(),
                channel_id = self.channel_id.get(),
                tmux_session_name,
                "recovery no-anchor delivery: no current generation marker; durable anchor unavailable"
            );
            return;
        }
        let commit = delivery_record::DeliveredCommit {
            range,
            generation_mtime_ns,
            attempts: self.attempts,
            panel_msg_id: Some(anchor.get()),
            panel_channel_id: Some(self.channel_id.get()),
        };
        if let Err(error) = delivery_record::write_delivered_frontier(
            &self.provider,
            self.record_channel_id.get(),
            commit,
        ) {
            tracing::warn!(
                provider = %self.provider.as_str(),
                channel_id = self.channel_id.get(),
                record_channel = self.record_channel_id.get(),
                error = %error,
                "recovery no-anchor delivery: durable anchor write failed"
            );
        }
    }
}

pub(in crate::services::discord) async fn replace_anchored_terminal_text(
    http: &serenity::Http,
    channel_id: ChannelId,
    placeholder: MessageId,
    text: &str,
    shared: &Arc<SharedData>,
    recovery_context: Option<&RecoveryDeliveryContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let outcome = formatting::replace_long_message_raw_with_outcome(
        http,
        channel_id,
        placeholder,
        text,
        shared,
        &mut None,
    )
    .await?;
    record_anchored_fallback_replacement(recovery_context, channel_id, &outcome, text);
    formatting::replace_long_message_outcome_to_result(outcome)
}

fn record_anchored_fallback_replacement(
    recovery_context: Option<&RecoveryDeliveryContext>,
    channel_id: ChannelId,
    outcome: &formatting::ReplaceLongMessageOutcome,
    text: &str,
) {
    let formatting::ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
        replacement_anchor: Some(anchor),
        ..
    } = outcome
    else {
        return;
    };
    if let Some(context) = recovery_context {
        context.record_successful_fresh_send(*anchor, text);
    } else {
        tracing::warn!(
            channel_id = channel_id.get(),
            anchor_msg_id = anchor.get(),
            "recovery anchored delivery fell back to fresh send without D1 context; replacement anchor not recorded"
        );
    }
}

pub(in crate::services::discord) async fn relay_no_anchor_terminal_text(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    text: &str,
    recovery_context: Option<&RecoveryDeliveryContext>,
) -> RecoveryRelayOutcome {
    let Some(context) = recovery_context else {
        tracing::warn!(
            channel_id = channel_id.get(),
            "recovery no-anchor delivery has no D1 idempotency context; falling back to legacy fresh send"
        );
        return match formatting::send_long_message_raw(http, channel_id, text, shared).await {
            Ok(()) => RecoveryRelayOutcome::Delivered,
            Err(error) => {
                let classified =
                    recovery_paths::shared::classify_recovery_relay_error(error.as_ref());
                recovery_paths::shared::escalate_transient_relay_outcome_with_probe(
                    classified,
                    || recovery_paths::restart::probe_channel_liveness(http, channel_id),
                )
                .await
            }
        };
    };
    let Some(mut lease) = context.try_acquire_fresh_send_lease(shared, text) else {
        tracing::warn!(
            channel_id = channel_id.get(),
            "recovery no-anchor delivery lease busy; skipping fresh send for retry"
        );
        return RecoveryRelayOutcome::TransientFailure;
    };
    let result = formatting::send_long_message_raw_with_reference_returning_message_ids(
        http, channel_id, text, shared, None,
    )
    .await;
    match result {
        Ok(message_ids) => {
            let committed = lease.commit(LeaseOutcome::Delivered);
            // Record chunk 0's message id. If only the inflight row proves reuse
            // later, the anchored replace arm must edit the first message and
            // regenerate continuations, not edit a tail continuation.
            if let Some(anchor) = message_ids.first().copied() {
                if committed {
                    context.record_successful_fresh_send(anchor, text);
                } else {
                    tracing::warn!(
                        channel_id = channel_id.get(),
                        anchor_msg_id = anchor.get(),
                        "recovery no-anchor delivery posted but lease commit failed; durable anchor not recorded"
                    );
                }
            } else {
                tracing::warn!(
                    channel_id = channel_id.get(),
                    "recovery no-anchor delivery posted without a message id; anchor not recorded"
                );
            }
            lease.release();
            RecoveryRelayOutcome::Delivered
        }
        Err(error) => {
            let _ = lease.commit(LeaseOutcome::Unknown);
            lease.release();
            let classified = recovery_paths::shared::classify_recovery_relay_error(error.as_ref());
            recovery_paths::shared::escalate_transient_relay_outcome_with_probe(classified, || {
                recovery_paths::restart::probe_channel_liveness(http, channel_id)
            })
            .await
        }
    }
}

pub(in crate::services::discord) struct RecoveryFreshSendLease {
    cell: Arc<DeliveryLeaseCell>,
    holder: LeaseHolder,
    key: DeliveryLeaseKey,
    start: u64,
    end: u64,
    heartbeat: Option<DeliveryLeaseHeartbeat>,
    released: bool,
}

impl RecoveryFreshSendLease {
    pub(in crate::services::discord) fn commit(&mut self, outcome: LeaseOutcome) -> bool {
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.stop();
        }
        self.cell
            .commit(self.holder, self.key.clone(), self.start, self.end, outcome)
    }

    pub(in crate::services::discord) fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.cell
            .release(self.holder, self.key.clone(), self.start, self.end);
    }
}

impl Drop for RecoveryFreshSendLease {
    fn drop(&mut self) {
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.stop();
        }
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::make_shared_data_for_tests;

    struct EnvReset(Option<std::ffi::OsString>);

    impl Drop for EnvReset {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn state(provider: ProviderKind, channel_id: u64) -> inflight::InflightTurnState {
        let mut state = inflight::InflightTurnState::new(
            provider,
            channel_id,
            Some("adk-test".to_string()),
            343_742_347_365_974_026,
            0,
            0,
            "recover this".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-codex-adk-test".to_string()),
            Some("/tmp/recovery-idempotent.jsonl".to_string()),
            None,
            128,
        );
        state.turn_start_offset = Some(128);
        state.save_generation = 9;
        state.full_response = "answer".to_string();
        state
    }

    fn set_runtime_root() -> (tempfile::TempDir, EnvReset) {
        let reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::TempDir::new().expect("runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        (temp, reset)
    }

    fn write_generation_marker(tmux_session_name: &str) {
        let path = crate::services::tmux_common::session_temp_path(tmux_session_name, "generation");
        if let Some(parent) = std::path::Path::new(&path).parent() {
            std::fs::create_dir_all(parent).expect("generation parent");
        }
        std::fs::write(path, "1").expect("generation marker");
    }

    fn inflight_state_path_for_test(
        agentdesk_root: &std::path::Path,
        provider: &ProviderKind,
        channel_id: u64,
    ) -> std::path::PathBuf {
        agentdesk_root
            .join("runtime")
            .join("discord_inflight")
            .join(provider.as_str())
            .join(format!("{channel_id}.json"))
    }

    #[test]
    fn zero_channel_or_anchor_ids_skip_recovery_context_without_panicking() {
        let provider = ProviderKind::Codex;
        let zero_channel_state = state(provider.clone(), 0);
        assert!(
            RecoveryDeliveryContext::from_state(&provider, &zero_channel_state, None, 42).is_none()
        );

        let context = RecoveryDeliveryContext::from_state(
            &provider,
            &state(provider.clone(), 44_099),
            Some((128, 256)),
            42,
        )
        .expect("non-zero test channel id");
        assert_eq!(context.inflight_recorded_anchor(), None);
        assert_eq!(context.anchor_reuse_decision(), None);
    }

    #[tokio::test]
    async fn same_turn_retry_after_anchor_persist_keeps_same_lease_key() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = state(provider.clone(), 44_000);
        inflight::save_inflight_state(&state).expect("save inflight");

        let delivery_generation = 42;
        let ctx = RecoveryDeliveryContext::from_state(&provider, &state, None, delivery_generation)
            .expect("non-zero test channel id");
        ctx.record_successful_fresh_send(MessageId::new(77_000), "answer");

        let persisted =
            inflight::load_inflight_state(&provider, state.channel_id).expect("persisted row");
        assert!(
            persisted.save_generation > state.save_generation,
            "anchor bind should bump the per-file save generation"
        );
        let retry_ctx =
            RecoveryDeliveryContext::from_state(&provider, &persisted, None, delivery_generation)
                .expect("non-zero test channel id");

        assert_eq!(
            ctx.lease_key, retry_ctx.lease_key,
            "same-turn retry must keep the same delivery lease key after anchor persistence"
        );
        assert_eq!(retry_ctx.lease_key.generation, delivery_generation);
    }

    #[tokio::test]
    async fn same_turn_second_recovery_attempt_uses_recorded_anchor_not_fresh_post() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = state(provider.clone(), 44_001);
        inflight::save_inflight_state(&state).expect("save inflight");
        let shared = make_shared_data_for_tests();
        let ctx = RecoveryDeliveryContext::from_state(
            &provider,
            &state,
            None,
            shared.restart.current_generation,
        )
        .expect("non-zero test channel id");

        let mut fresh_posts = 0;
        assert!(ctx.recorded_anchor().is_none());
        let mut lease = ctx
            .try_acquire_fresh_send_lease(&shared, "answer")
            .expect("first attempt acquires");
        fresh_posts += 1;
        assert!(lease.commit(LeaseOutcome::Delivered));
        ctx.record_successful_fresh_send(MessageId::new(77_001), "answer");
        lease.release();

        assert_eq!(ctx.recorded_anchor(), Some(MessageId::new(77_001)));
        if ctx.recorded_anchor().is_none() {
            fresh_posts += 1;
        }
        assert_eq!(fresh_posts, 1, "second attempt must edit/skip, not POST");
    }

    #[tokio::test]
    async fn bind_rejected_skips_durable_frontier_write() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = state(provider.clone(), 44_002);
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        let ctx = RecoveryDeliveryContext::from_state(&provider, &state, Some((128, 256)), 42)
            .expect("non-zero test channel id");

        let mut newer = state.clone();
        newer.user_msg_id = newer.user_msg_id.saturating_add(1);
        newer.turn_start_offset = Some(512);
        inflight::save_inflight_state(&newer).expect("save newer inflight");

        ctx.record_successful_fresh_send(MessageId::new(77_002), "answer");

        assert!(
            delivery_frontier_probe::current_generation_delivered_anchor(
                &provider,
                ChannelId::new(state.delivery_record_owner_channel_id()),
                tmux,
                Some(u64::MAX),
            )
            .is_none(),
            "identity-mismatched bind must not write a durable frontier"
        );
    }

    #[tokio::test]
    async fn bind_read_io_error_skips_durable_frontier_write() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = state(provider.clone(), 44_006);
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        let path = inflight_state_path_for_test(temp.path(), &provider, state.channel_id);
        std::fs::create_dir_all(path.parent().expect("inflight parent")).expect("inflight parent");
        std::fs::create_dir(&path).expect("directory at inflight path forces read_to_string error");
        let ctx = RecoveryDeliveryContext::from_state(&provider, &state, Some((128, 256)), 42)
            .expect("non-zero test channel id");

        ctx.record_successful_fresh_send(MessageId::new(77_006), "answer");

        assert!(
            delivery_frontier_probe::current_generation_delivered_anchor(
                &provider,
                ChannelId::new(state.delivery_record_owner_channel_id()),
                tmux,
                Some(u64::MAX),
            )
            .is_none(),
            "non-NotFound read failure must block the durable frontier write"
        );
    }

    #[tokio::test]
    async fn bind_missing_row_allows_durable_frontier_write() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = state(provider.clone(), 44_007);
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        let ctx = RecoveryDeliveryContext::from_state(&provider, &state, Some((128, 256)), 42)
            .expect("non-zero test channel id");

        ctx.record_successful_fresh_send(MessageId::new(77_007), "answer");

        let anchor = delivery_frontier_probe::current_generation_delivered_anchor(
            &provider,
            ChannelId::new(state.delivery_record_owner_channel_id()),
            tmux,
            Some(u64::MAX),
        )
        .expect("genuine absence is safe to record as durable delivered");
        assert_eq!(anchor.panel_msg_id, 77_007);
        assert_eq!(anchor.panel_channel_id, state.channel_id);
        assert_eq!(anchor.range, (128, 256));
    }

    #[tokio::test]
    async fn durable_matched_reuse_returns_delivered_without_discord_post() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = state(provider.clone(), 44_003);
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        inflight::save_inflight_state(&state).expect("save inflight");
        // #4188: a genuinely durable-delivered anchor implies the transcript
        // (output_path) exists and is at least as long as the recorded range end
        // (256). Seed it so the EOF-bound guard trusts the current-generation
        // frontier instead of fail-safe distrusting an absent transcript.
        std::fs::write(
            state.output_path.as_deref().expect("output_path"),
            vec![b'x'; 512],
        )
        .expect("seed transcript at/above the durable frontier end");

        let shared = make_shared_data_for_tests();
        let ctx = RecoveryDeliveryContext::from_state(
            &provider,
            &state,
            Some((128, 256)),
            shared.restart.current_generation,
        )
        .expect("non-zero test channel id");
        let mut lease = ctx
            .try_acquire_fresh_send_lease(&shared, "answer")
            .expect("first attempt acquires");
        assert!(lease.commit(LeaseOutcome::Delivered));
        ctx.record_successful_fresh_send(MessageId::new(77_003), "answer");
        lease.release();

        let fresh_shared_after_restart = make_shared_data_for_tests();
        let retry_ctx = RecoveryDeliveryContext::from_state(
            &provider,
            &state,
            Some((128, 256)),
            fresh_shared_after_restart.restart.current_generation,
        )
        .expect("non-zero test channel id");
        assert_eq!(
            retry_ctx.anchor_reuse_decision(),
            Some(RecoveryAnchorReuse::DurableAlreadyDelivered(
                MessageId::new(77_003)
            )),
            "durable terminal anchor has range proof and must be treated as already delivered"
        );

        let http = Arc::new(poise::serenity_prelude::Http::new("Bot test-token"));
        let outcome = super::super::relay_recovered_terminal_text_to_placeholder(
            &http,
            &fresh_shared_after_restart,
            ChannelId::new(state.channel_id),
            None,
            "answer",
            Some(&retry_ctx),
        )
        .await;
        assert!(
            outcome.delivered(),
            "durable reuse should return Delivered before any Discord POST can be attempted"
        );
    }

    #[tokio::test]
    async fn gone_anchor_repost_context_records_replacement_to_matched_record_channel() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let mut state = state(provider.clone(), 44_008);
        state.current_msg_id = 77_008;
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        inflight::save_inflight_state(&state).expect("save inflight");
        let matched_record_channel = ChannelId::new(55_008);
        let generation_mtime_ns = delivery_record::current_generation_mtime_ns(tmux);
        delivery_record::write_delivered_frontier(
            &provider,
            matched_record_channel.get(),
            delivery_record::DeliveredCommit {
                range: (128, 256),
                generation_mtime_ns,
                attempts: 1,
                panel_msg_id: Some(77_008),
                panel_channel_id: Some(state.channel_id),
            },
        )
        .expect("old matched-owner durable anchor");

        let shared = make_shared_data_for_tests();
        let ctx = RecoveryDeliveryContext::send_new_after_gone_anchor(
            &provider,
            &state,
            ChannelId::new(state.channel_id),
            Some((128, 256)),
            shared.restart.current_generation,
        )
        .with_record_channel_id(matched_record_channel);
        let mut lease = ctx
            .try_acquire_fresh_send_lease(&shared, "replacement")
            .expect("repost attempt acquires");
        assert!(lease.commit(LeaseOutcome::Delivered));
        ctx.record_successful_fresh_send(MessageId::new(88_008), "replacement");
        lease.release();

        let matched_anchor = delivery_frontier_probe::current_generation_delivered_anchor(
            &provider,
            matched_record_channel,
            tmux,
            Some(u64::MAX),
        )
        .expect("replacement durable anchor should overwrite the matched owner record");
        assert_eq!(matched_anchor.panel_msg_id, 88_008);
        assert_eq!(matched_anchor.panel_channel_id, state.channel_id);
        assert!(
            delivery_frontier_probe::current_generation_delivered_anchor(
                &provider,
                ChannelId::new(state.delivery_record_owner_channel_id()),
                tmux,
                Some(u64::MAX),
            )
            .is_none(),
            "replacement must not be written to the stale state-derived owner record"
        );
    }

    #[tokio::test]
    async fn gone_anchor_repost_context_does_not_reuse_old_anchor_but_records_replacement() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let mut state = state(provider.clone(), 44_004);
        state.current_msg_id = 77_004;
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        inflight::save_inflight_state(&state).expect("save inflight");
        let generation_mtime_ns = delivery_record::current_generation_mtime_ns(tmux);
        delivery_record::write_delivered_frontier(
            &provider,
            state.delivery_record_owner_channel_id(),
            delivery_record::DeliveredCommit {
                range: (128, 256),
                generation_mtime_ns,
                attempts: 1,
                panel_msg_id: Some(77_004),
                panel_channel_id: Some(state.channel_id),
            },
        )
        .expect("old durable anchor");

        let shared = make_shared_data_for_tests();
        let ctx = RecoveryDeliveryContext::send_new_after_gone_anchor(
            &provider,
            &state,
            ChannelId::new(state.channel_id),
            Some((128, 256)),
            shared.restart.current_generation,
        );
        assert_eq!(
            ctx.recorded_anchor(),
            None,
            "gone-anchor repost must not edit the old anchor it just proved missing"
        );
        let mut lease = ctx
            .try_acquire_fresh_send_lease(&shared, "replacement")
            .expect("repost attempt acquires");
        assert!(lease.commit(LeaseOutcome::Delivered));
        ctx.record_successful_fresh_send(MessageId::new(88_004), "replacement");
        lease.release();

        let retry_ctx = RecoveryDeliveryContext::from_state(
            &provider,
            &state,
            Some((128, 256)),
            shared.restart.current_generation,
        )
        .expect("non-zero test channel id");
        assert_eq!(
            retry_ctx.recorded_anchor(),
            Some(MessageId::new(88_004)),
            "replacement anchor should be reused by later ordinary recovery retries"
        );
    }

    #[test]
    fn anchored_fallback_fresh_send_records_replacement_anchor() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let mut state = state(provider.clone(), 44_005);
        state.current_msg_id = 77_005;
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        inflight::save_inflight_state(&state).expect("save inflight");

        let ctx = RecoveryDeliveryContext::from_state(&provider, &state, Some((128, 256)), 42)
            .expect("non-zero test channel id");
        let outcome =
            crate::services::discord::formatting::ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                edit_error: "404 stale anchor".to_string(),
                replacement_anchor: Some(MessageId::new(88_005)),
            };

        record_anchored_fallback_replacement(
            Some(&ctx),
            ChannelId::new(state.channel_id),
            &outcome,
            "replacement",
        );

        let anchor = delivery_frontier_probe::current_generation_delivered_anchor(
            &provider,
            ChannelId::new(state.delivery_record_owner_channel_id()),
            tmux,
            Some(u64::MAX),
        )
        .expect("replacement durable anchor");
        assert_eq!(anchor.panel_msg_id, 88_005);
        assert_eq!(
            inflight::load_inflight_state(&provider, state.channel_id)
                .expect("inflight row")
                .current_msg_id,
            88_005,
            "fallback replacement should become the next anchored-edit target"
        );
    }
}
