//! Identity-guarded persistence helpers for the periodic stream tick (#4259 R1).

use super::super::*;

pub(super) type GuardedSaveOutcome = crate::services::discord::inflight::GuardedSaveOutcome;

pub(in crate::services::discord::turn_bridge) struct StreamTickCandidateSaveContext<
    'a,
    G: TurnGateway + ?Sized,
> {
    pub(in crate::services::discord::turn_bridge) gateway: &'a G,
    pub(in crate::services::discord::turn_bridge) provider: &'a ProviderKind,
    pub(in crate::services::discord::turn_bridge) token_hash: &'a str,
    pub(in crate::services::discord::turn_bridge) channel_id: ChannelId,
    pub(in crate::services::discord::turn_bridge) persisted_baseline: &'a mut InflightTurnState,
    pub(in crate::services::discord::turn_bridge) inflight_state: &'a mut InflightTurnState,
    pub(in crate::services::discord::turn_bridge) expected_identity:
        &'a crate::services::discord::inflight::InflightTurnIdentity,
    pub(in crate::services::discord::turn_bridge) expected_current_message: &'a mut (u64, usize),
    pub(in crate::services::discord::turn_bridge) current_msg_id: &'a mut MessageId,
    pub(in crate::services::discord::turn_bridge) pending_current_message_candidate:
        &'a mut Option<MessageId>,
    pub(in crate::services::discord::turn_bridge) bridge_created_response_placeholder_msg_id:
        &'a mut Option<MessageId>,
}

/// Durable precondition for a Discord-visible stream mutation.
///
/// A successful identity guard is not sufficient by itself: the same turn may
/// have handed live delivery to a watcher/standby relay.  Only the historical
/// `None` owner is bridge authority.  Store failures fail closed for this tick
/// but remain retryable; a missing/reowned row or a durable non-bridge relay
/// owner permanently ends bridge authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord::turn_bridge) enum VisibleMutationAuthority {
    Authorized,
    Suppressed,
    Retry,
    AuthorityLost,
}

impl VisibleMutationAuthority {
    /// `None` is the only disposition that terminates stream lifecycle.
    /// Delegated owners suppress the individual Discord mutation while the
    /// tick continues through reclaim, post-loop finalization, and analytics.
    pub(in crate::services::discord::turn_bridge) fn mutation_permission(self) -> Option<bool> {
        match self {
            Self::Authorized => Some(true),
            Self::Suppressed | Self::Retry => Some(false),
            Self::AuthorityLost => None,
        }
    }
}

pub(in crate::services::discord::turn_bridge) fn visible_mutation_authority_after_guarded_save(
    outcome: GuardedSaveOutcome,
    inflight_state: &InflightTurnState,
    intended_authority: crate::services::discord::inflight::StreamRelayAuthority,
) -> VisibleMutationAuthority {
    use crate::services::discord::inflight::StreamRelayAuthority;

    match outcome {
        GuardedSaveOutcome::Saved
            if StreamRelayAuthority::from_state(inflight_state) == intended_authority
                && intended_authority.bridge_owns_relay() =>
        {
            VisibleMutationAuthority::Authorized
        }
        GuardedSaveOutcome::Saved
            if StreamRelayAuthority::from_state(inflight_state) == intended_authority =>
        {
            VisibleMutationAuthority::Suppressed
        }
        GuardedSaveOutcome::Saved
        | GuardedSaveOutcome::Missing
        | GuardedSaveOutcome::IdentityMismatch => VisibleMutationAuthority::AuthorityLost,
        GuardedSaveOutcome::IoError => VisibleMutationAuthority::Retry,
    }
}

pub(super) fn sync_stream_tick_tool_fields(
    inflight_state: &mut InflightTurnState,
    current_tool_line: &Option<String>,
    prev_tool_status: &Option<String>,
    last_tool_name: &Option<String>,
    last_tool_summary: &Option<String>,
) {
    inflight_state
        .current_tool_line
        .clone_from(current_tool_line);
    inflight_state.prev_tool_status.clone_from(prev_tool_status);
    inflight_state.last_tool_name.clone_from(last_tool_name);
    inflight_state
        .last_tool_summary
        .clone_from(last_tool_summary);
}

pub(in crate::services::discord::turn_bridge) fn persist_stream_tick_state(
    persisted_baseline: &mut InflightTurnState,
    inflight_state: &mut InflightTurnState,
    expected: &crate::services::discord::inflight::InflightTurnIdentity,
    expected_current_message: &mut (u64, usize),
    detached_current_msg_id: &mut MessageId,
    channel_id: ChannelId,
    caller: &'static str,
) -> GuardedSaveOutcome {
    persist_stream_tick_state_with_authority_mode(
        persisted_baseline,
        inflight_state,
        expected,
        expected_current_message,
        detached_current_msg_id,
        StreamTickSaveOperation {
            channel_id,
            caller,
            mode: StreamTickSaveMode::MergeConcurrentOwner,
        },
    )
}

fn persist_stream_tick_visible_mutation_fence(
    persisted_baseline: &mut InflightTurnState,
    inflight_state: &mut InflightTurnState,
    expected: &crate::services::discord::inflight::InflightTurnIdentity,
    expected_current_message: &mut (u64, usize),
    detached_current_msg_id: &mut MessageId,
    channel_id: ChannelId,
    caller: &'static str,
) -> GuardedSaveOutcome {
    persist_stream_tick_state_with_authority_mode(
        persisted_baseline,
        inflight_state,
        expected,
        expected_current_message,
        detached_current_msg_id,
        StreamTickSaveOperation {
            channel_id,
            caller,
            mode: StreamTickSaveMode::StrictVisibleMutationFence,
        },
    )
}

fn persist_stream_tick_state_with_authority_mode(
    persisted_baseline: &mut InflightTurnState,
    inflight_state: &mut InflightTurnState,
    expected: &crate::services::discord::inflight::InflightTurnIdentity,
    expected_current_message: &mut (u64, usize),
    detached_current_msg_id: &mut MessageId,
    operation: StreamTickSaveOperation,
) -> GuardedSaveOutcome {
    use crate::services::discord::inflight::{
        GuardedSaveOutcome, save_stream_tick_state_if_bridge_authority,
        save_stream_tick_state_preserving_current_message_races,
    };
    let expected_current_before_save = *expected_current_message;
    let outcome = if operation.mode == StreamTickSaveMode::StrictVisibleMutationFence {
        save_stream_tick_state_if_bridge_authority(
            persisted_baseline,
            inflight_state,
            expected,
            expected_current_message.0,
            expected_current_message.1,
            operation.caller,
        )
    } else {
        save_stream_tick_state_preserving_current_message_races(
            persisted_baseline,
            inflight_state,
            expected,
            expected_current_message.0,
            expected_current_message.1,
            operation.caller,
        )
    };
    if outcome == GuardedSaveOutcome::Saved {
        *expected_current_message = (
            inflight_state.current_msg_id,
            inflight_state.current_msg_len,
        );
        *detached_current_msg_id =
            detached_current_msg_id_from_durable(inflight_state.current_msg_id);
    } else if operation.mode == StreamTickSaveMode::StrictVisibleMutationFence
        && outcome == GuardedSaveOutcome::IdentityMismatch
        && expected.matches_state(inflight_state)
        && ((
            inflight_state.current_msg_id,
            inflight_state.current_msg_len,
        ) != expected_current_before_save
            || inflight_state.effective_relay_owner_kind()
                != crate::services::discord::inflight::RelayOwnerKind::None)
    {
        // The strict lock-held fence adopted the exact same-turn durable row.
        // Make candidate cleanup fall back to that authoritative message, not
        // the stale pre-fence epoch.
        *expected_current_message = (
            inflight_state.current_msg_id,
            inflight_state.current_msg_len,
        );
        *detached_current_msg_id =
            detached_current_msg_id_from_durable(inflight_state.current_msg_id);
    }
    if matches!(
        outcome,
        GuardedSaveOutcome::Missing | GuardedSaveOutcome::IdentityMismatch
    ) {
        tracing::warn!(
            channel_id = operation.channel_id.get(),
            caller = operation.caller,
            ?outcome,
            "stream tick guarded save skipped because durable row is no longer owned by this turn"
        );
    }
    outcome
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamTickSaveMode {
    MergeConcurrentOwner,
    StrictVisibleMutationFence,
}

#[derive(Clone, Copy, Debug)]
struct StreamTickSaveOperation {
    channel_id: ChannelId,
    caller: &'static str,
    mode: StreamTickSaveMode,
}

pub(in crate::services::discord::turn_bridge) async fn persist_stream_tick_state_with_candidate_cleanup<
    G: TurnGateway + ?Sized,
>(
    mut context: StreamTickCandidateSaveContext<'_, G>,
    caller: &'static str,
) -> GuardedSaveOutcome {
    persist_stream_tick_state_with_candidate_cleanup_mode(
        &mut context,
        caller,
        StreamTickSaveMode::MergeConcurrentOwner,
    )
    .await
}

pub(super) async fn fence_stream_tick_visible_mutation_with_candidate_cleanup<
    G: TurnGateway + ?Sized,
>(
    mut context: StreamTickCandidateSaveContext<'_, G>,
    caller: &'static str,
) -> GuardedSaveOutcome {
    persist_stream_tick_state_with_candidate_cleanup_mode(
        &mut context,
        caller,
        StreamTickSaveMode::StrictVisibleMutationFence,
    )
    .await
}

async fn persist_stream_tick_state_with_candidate_cleanup_mode<G: TurnGateway + ?Sized>(
    context: &mut StreamTickCandidateSaveContext<'_, G>,
    caller: &'static str,
    mode: StreamTickSaveMode,
) -> GuardedSaveOutcome {
    let outcome = if mode == StreamTickSaveMode::StrictVisibleMutationFence {
        persist_stream_tick_visible_mutation_fence(
            context.persisted_baseline,
            context.inflight_state,
            context.expected_identity,
            context.expected_current_message,
            context.current_msg_id,
            context.channel_id,
            caller,
        )
    } else {
        persist_stream_tick_state(
            context.persisted_baseline,
            context.inflight_state,
            context.expected_identity,
            context.expected_current_message,
            context.current_msg_id,
            context.channel_id,
            caller,
        )
    };
    if outcome == GuardedSaveOutcome::IoError {
        return outcome;
    }
    let Some(candidate) = *context.pending_current_message_candidate else {
        return outcome;
    };
    if outcome == GuardedSaveOutcome::Saved
        && context.inflight_state.current_msg_id == candidate.get()
    {
        context.pending_current_message_candidate.take();
        return outcome;
    }
    discard_pending_current_message_candidate(context).await;
    outcome
}

async fn discard_pending_current_message_candidate<G: TurnGateway + ?Sized>(
    context: &mut StreamTickCandidateSaveContext<'_, G>,
) {
    let Some(candidate) = context.pending_current_message_candidate.take() else {
        return;
    };
    if *context.bridge_created_response_placeholder_msg_id == Some(candidate) {
        *context.bridge_created_response_placeholder_msg_id = None;
    }
    context.inflight_state.current_msg_id = context.expected_current_message.0;
    context.inflight_state.current_msg_len = context.expected_current_message.1;
    *context.current_msg_id =
        detached_current_msg_id_from_durable(context.expected_current_message.0);
    cleanup_unbound_bridge_anchor(
        context.gateway,
        context.provider,
        context.token_hash,
        context.channel_id,
        candidate,
    )
    .await;
}

/// A stream-loop break may happen before the next periodic tick. Give a pending
/// response candidate one final guarded bind; if the store is unavailable,
/// discard the unbound Discord message instead of returning an orphan.
pub(in crate::services::discord::turn_bridge) async fn settle_pending_current_message_candidate_on_loop_exit<
    G: TurnGateway + ?Sized,
>(
    mut context: StreamTickCandidateSaveContext<'_, G>,
) -> bool {
    if context.pending_current_message_candidate.is_none() {
        return false;
    }
    let outcome = persist_stream_tick_state_with_candidate_cleanup_mode(
        &mut context,
        "turn_bridge::stream_loop::exit_candidate_flush",
        StreamTickSaveMode::MergeConcurrentOwner,
    )
    .await;
    if outcome == GuardedSaveOutcome::IoError {
        tracing::warn!(
            channel_id = context.channel_id.get(),
            "stream-loop exit could not bind response candidate; discarding unbound message"
        );
        discard_pending_current_message_candidate(&mut context).await;
    }
    debug_assert!(context.pending_current_message_candidate.is_none());
    outcome == GuardedSaveOutcome::Saved
}

pub(super) fn persist_stream_tick_heartbeat(
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected: &crate::services::discord::inflight::InflightTurnIdentity,
) -> GuardedSaveOutcome {
    crate::services::discord::inflight::touch_inflight_state_if_matches_identity(
        provider,
        channel_id.get(),
        expected,
        "turn_bridge::stream_tick::long_running_heartbeat",
    )
}

pub(super) fn dirty_after_guarded_save(outcome: GuardedSaveOutcome) -> bool {
    matches!(outcome, GuardedSaveOutcome::IoError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::inflight::{
        GuardedSaveOutcome, load_inflight_state, save_inflight_state,
    };

    fn owner_state(channel_id: u64, user_msg_id: u64) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-stream-tick".to_string()),
            343_742_347_365_974_026,
            user_msg_id,
            0,
            "user prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-codex-stream-tick".to_string()),
            Some("/tmp/AgentDesk-codex-stream-tick.jsonl".to_string()),
            Some("/tmp/AgentDesk-codex-stream-tick.input".to_string()),
            512,
        );
        state.last_offset = 512;
        state
    }

    fn with_runtime_root<T>(test: impl FnOnce() -> T) -> T {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env_reset =
            crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        test()
    }

    #[test]
    fn visible_authority_distinguishes_bridge_self_delegation_and_foreign_projection() {
        let bridge = owner_state(4_259_119, 77_010);
        let bridge_authority =
            crate::services::discord::inflight::StreamRelayAuthority::from_state(&bridge);
        assert_eq!(
            visible_mutation_authority_after_guarded_save(
                GuardedSaveOutcome::Saved,
                &bridge,
                bridge_authority,
            ),
            VisibleMutationAuthority::Authorized,
        );

        for (relay_owner, watcher_live) in [
            (
                crate::services::discord::inflight::RelayOwnerKind::Watcher,
                true,
            ),
            (
                crate::services::discord::inflight::RelayOwnerKind::StandbyRelay,
                false,
            ),
        ] {
            let mut delegated = bridge.clone();
            delegated.set_watcher_owner_channel_id(delegated.channel_id + 1);
            delegated.watcher_owns_live_relay = watcher_live;
            delegated.set_relay_owner_kind(relay_owner);
            let intended =
                crate::services::discord::inflight::StreamRelayAuthority::from_state(&delegated);
            assert_eq!(
                visible_mutation_authority_after_guarded_save(
                    GuardedSaveOutcome::Saved,
                    &delegated,
                    intended,
                ),
                VisibleMutationAuthority::Suppressed,
            );
            assert_eq!(
                VisibleMutationAuthority::Suppressed.mutation_permission(),
                Some(false),
                "self delegation suppresses the visible mutation without terminating lifecycle",
            );

            let mut foreign = delegated.clone();
            foreign.set_watcher_owner_channel_id(delegated.channel_id + 2);
            assert_eq!(
                visible_mutation_authority_after_guarded_save(
                    GuardedSaveOutcome::Saved,
                    &foreign,
                    intended,
                ),
                VisibleMutationAuthority::AuthorityLost,
            );
        }
    }

    #[test]
    fn same_owner_flush_persists_and_clears_dirty() {
        with_runtime_root(|| {
            let channel = ChannelId::new(4_259_101);
            let mut state = owner_state(channel.get(), 77_010);
            save_inflight_state(&state).expect("seed owner row");
            let expected =
                crate::services::discord::inflight::InflightTurnIdentity::from_state(&state);
            let mut persisted_baseline = state.clone();

            state.full_response = "streamed answer".to_string();
            state.last_offset = 1_024;
            let mut expected_current_message = (state.current_msg_id, state.current_msg_len);
            let mut detached_current_msg_id =
                detached_current_msg_id_from_durable(state.current_msg_id);
            let outcome = persist_stream_tick_state(
                &mut persisted_baseline,
                &mut state,
                &expected,
                &mut expected_current_message,
                &mut detached_current_msg_id,
                channel,
                "turn_bridge::stream_tick::dirty_flush_test",
            );

            assert_eq!(outcome, GuardedSaveOutcome::Saved);
            assert!(!dirty_after_guarded_save(outcome));
            let persisted =
                load_inflight_state(&ProviderKind::Codex, channel.get()).expect("persisted row");
            assert_eq!(persisted.full_response, "streamed answer");
            assert_eq!(persisted.last_offset, 1_024);
        });
    }

    #[test]
    fn reowned_flush_skips_without_clobbering_or_retrying_dirty() {
        with_runtime_root(|| {
            let channel = ChannelId::new(4_259_102);
            let mut stale = owner_state(channel.get(), 77_010);
            let expected =
                crate::services::discord::inflight::InflightTurnIdentity::from_state(&stale);
            let mut persisted_baseline = stale.clone();
            stale.full_response = "stale answer".to_string();

            let mut successor = owner_state(channel.get(), 99_999);
            successor.full_response = "new owner answer".to_string();
            successor.last_offset = 8_192;
            save_inflight_state(&successor).expect("seed successor row");
            let mut expected_current_message = (stale.current_msg_id, stale.current_msg_len);
            let mut detached_current_msg_id =
                detached_current_msg_id_from_durable(stale.current_msg_id);

            let outcome = persist_stream_tick_state(
                &mut persisted_baseline,
                &mut stale,
                &expected,
                &mut expected_current_message,
                &mut detached_current_msg_id,
                channel,
                "turn_bridge::stream_tick::dirty_flush_test",
            );

            assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);
            assert!(!dirty_after_guarded_save(outcome));
            let persisted =
                load_inflight_state(&ProviderKind::Codex, channel.get()).expect("persisted row");
            assert_eq!(persisted.user_msg_id, 99_999);
            assert_eq!(persisted.full_response, "new owner answer");
            assert_eq!(persisted.last_offset, 8_192);
        });
    }

    #[test]
    fn same_owner_clear_after_bind_survives_dirty_flush() {
        with_runtime_root(|| {
            let channel = ChannelId::new(4_259_103);
            let mut local = owner_state(channel.get(), 77_010);
            local.current_msg_id = 900_001;
            local.current_msg_len = 17;
            save_inflight_state(&local).expect("seed bound owner row");
            let expected =
                crate::services::discord::inflight::InflightTurnIdentity::from_state(&local);
            let mut persisted_baseline = local.clone();
            let mut expected_current_message = (900_001, 17);
            let mut detached_current_msg_id = MessageId::new(900_001);

            let mut cleared = local.clone();
            cleared.current_msg_id = 0;
            cleared.current_msg_len = 0;
            save_inflight_state(&cleared).expect("same owner clears anchor");
            local.full_response = "bridge tick survives".to_string();

            let outcome = persist_stream_tick_state(
                &mut persisted_baseline,
                &mut local,
                &expected,
                &mut expected_current_message,
                &mut detached_current_msg_id,
                channel,
                "turn_bridge::stream_tick::same_owner_clear_test",
            );

            assert_eq!(outcome, GuardedSaveOutcome::Saved);
            assert_eq!(expected_current_message, (0, 0));
            assert_eq!(
                durable_current_msg_id_from_detached(detached_current_msg_id),
                0
            );
            assert_eq!((local.current_msg_id, local.current_msg_len), (0, 0));
            let persisted =
                load_inflight_state(&ProviderKind::Codex, channel.get()).expect("persisted row");
            assert_eq!(
                (persisted.current_msg_id, persisted.current_msg_len),
                (0, 0)
            );
            assert_eq!(persisted.full_response, "bridge tick survives");
        });
    }

    #[test]
    fn same_owner_competing_bind_wins_dirty_flush() {
        with_runtime_root(|| {
            let channel = ChannelId::new(4_259_104);
            let mut local = owner_state(channel.get(), 77_010);
            save_inflight_state(&local).expect("seed absent owner row");
            let expected =
                crate::services::discord::inflight::InflightTurnIdentity::from_state(&local);
            let mut persisted_baseline = local.clone();
            let mut expected_current_message = (0, 0);
            let mut detached_current_msg_id = detached_current_msg_id_from_durable(0);

            local.current_msg_id = 900_002;
            local.current_msg_len = 19;
            local.full_response = "bridge tick survives".to_string();
            let mut competing = local.clone();
            competing.current_msg_id = 900_003;
            competing.current_msg_len = 29;
            competing.full_response.clear();
            save_inflight_state(&competing).expect("same owner binds competing anchor");

            let outcome = persist_stream_tick_state(
                &mut persisted_baseline,
                &mut local,
                &expected,
                &mut expected_current_message,
                &mut detached_current_msg_id,
                channel,
                "turn_bridge::stream_tick::same_owner_bind_test",
            );

            assert_eq!(outcome, GuardedSaveOutcome::Saved);
            assert_eq!(expected_current_message, (900_003, 29));
            assert_eq!(detached_current_msg_id, MessageId::new(900_003));
            assert_eq!((local.current_msg_id, local.current_msg_len), (900_003, 29));
            let persisted =
                load_inflight_state(&ProviderKind::Codex, channel.get()).expect("persisted row");
            assert_eq!(
                (persisted.current_msg_id, persisted.current_msg_len),
                (900_003, 29)
            );
            assert_eq!(persisted.full_response, "bridge tick survives");
        });
    }

    #[test]
    fn dirty_and_side_effect_transitions_follow_guarded_outcome() {
        use GuardedSaveOutcome::*;
        assert!(!dirty_after_guarded_save(Saved));
        assert!(dirty_after_guarded_save(IoError));
        assert!(!dirty_after_guarded_save(Missing));
        assert!(!dirty_after_guarded_save(IdentityMismatch));
        assert!(matches!(Saved, GuardedSaveOutcome::Saved));
        assert!(!matches!(IoError, GuardedSaveOutcome::Saved));
        assert!(!matches!(Missing, GuardedSaveOutcome::Saved));
        assert!(!matches!(IdentityMismatch, GuardedSaveOutcome::Saved));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn io_error_candidate_retries_until_loop_exit_then_is_cleaned() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let blocked_root = temp.path().join("blocked-root");
        std::fs::write(&blocked_root, b"not a directory").expect("blocked runtime root");
        let _env_reset =
            crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", &blocked_root);

        let channel = ChannelId::new(4_259_105);
        let mut state = owner_state(channel.get(), 77_010);
        let expected = crate::services::discord::inflight::InflightTurnIdentity::from_state(&state);
        let mut persisted_baseline = state.clone();
        state.current_msg_id = 2;
        state.current_msg_len = 10;
        let mut expected_current_message = (0, 0);
        let mut current_msg_id = MessageId::new(2);
        let mut pending_candidate = Some(current_msg_id);
        let mut bridge_created_candidate = Some(current_msg_id);
        let gateway = super::super::provider_output_guard_tests::CapturingGateway::default();

        let outcome = persist_stream_tick_state_with_candidate_cleanup(
            StreamTickCandidateSaveContext {
                gateway: &gateway,
                provider: &ProviderKind::Codex,
                token_hash: "candidate-retry-test",
                channel_id: channel,
                persisted_baseline: &mut persisted_baseline,
                inflight_state: &mut state,
                expected_identity: &expected,
                expected_current_message: &mut expected_current_message,
                current_msg_id: &mut current_msg_id,
                pending_current_message_candidate: &mut pending_candidate,
                bridge_created_response_placeholder_msg_id: &mut bridge_created_candidate,
            },
            "turn_bridge::stream_tick::io_error_candidate_test",
        )
        .await;
        assert_eq!(outcome, GuardedSaveOutcome::IoError);
        assert_eq!(pending_candidate, Some(MessageId::new(2)));
        assert!(gateway.deletes.lock().expect("deletes lock").is_empty());

        assert!(
            !settle_pending_current_message_candidate_on_loop_exit(
                StreamTickCandidateSaveContext {
                    gateway: &gateway,
                    provider: &ProviderKind::Codex,
                    token_hash: "candidate-retry-test",
                    channel_id: channel,
                    persisted_baseline: &mut persisted_baseline,
                    inflight_state: &mut state,
                    expected_identity: &expected,
                    expected_current_message: &mut expected_current_message,
                    current_msg_id: &mut current_msg_id,
                    pending_current_message_candidate: &mut pending_candidate,
                    bridge_created_response_placeholder_msg_id: &mut bridge_created_candidate,
                },
            )
            .await
        );

        assert_eq!(pending_candidate, None);
        assert_eq!(bridge_created_candidate, None);
        assert_eq!((state.current_msg_id, state.current_msg_len), (0, 0));
        assert_eq!(durable_current_msg_id_from_detached(current_msg_id), 0);
        assert_eq!(
            gateway.deletes.lock().expect("deletes lock").as_slice(),
            &[2]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn candidate_cleanup_covers_saved_competing_reowned_and_missing_rows() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env_reset =
            crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let gateway = super::super::provider_output_guard_tests::CapturingGateway::default();

        let bound_channel = ChannelId::new(4_259_106);
        let mut bound = owner_state(bound_channel.get(), 77_010);
        save_inflight_state(&bound).expect("seed absent owner row");
        let bound_identity =
            crate::services::discord::inflight::InflightTurnIdentity::from_state(&bound);
        let mut bound_baseline = bound.clone();
        bound.current_msg_id = 2;
        bound.current_msg_len = 10;
        let mut bound_expected = (0, 0);
        let mut bound_current = MessageId::new(2);
        let mut bound_pending = Some(bound_current);
        let mut bound_created = Some(bound_current);
        assert!(
            settle_pending_current_message_candidate_on_loop_exit(StreamTickCandidateSaveContext {
                gateway: &gateway,
                provider: &ProviderKind::Codex,
                token_hash: "candidate-matrix-test",
                channel_id: bound_channel,
                persisted_baseline: &mut bound_baseline,
                inflight_state: &mut bound,
                expected_identity: &bound_identity,
                expected_current_message: &mut bound_expected,
                current_msg_id: &mut bound_current,
                pending_current_message_candidate: &mut bound_pending,
                bridge_created_response_placeholder_msg_id: &mut bound_created,
            },)
            .await
        );
        assert_eq!(bound_pending, None);
        assert_eq!(bound_created, Some(MessageId::new(2)));
        assert_eq!(bound_expected, (2, 10));
        assert_eq!(
            load_inflight_state(&ProviderKind::Codex, bound_channel.get())
                .expect("exit settle binds candidate")
                .current_msg_id,
            2
        );

        let competing_channel = ChannelId::new(4_259_107);
        let mut competing_local = owner_state(competing_channel.get(), 77_010);
        let competing_identity =
            crate::services::discord::inflight::InflightTurnIdentity::from_state(&competing_local);
        let mut competing_baseline = competing_local.clone();
        let mut competing_durable = competing_local.clone();
        competing_durable.current_msg_id = 900_003;
        competing_durable.current_msg_len = 29;
        save_inflight_state(&competing_durable).expect("seed competing same-owner bind");
        competing_local.current_msg_id = 3;
        competing_local.current_msg_len = 11;
        let mut competing_expected = (0, 0);
        let mut competing_current = MessageId::new(3);
        let mut competing_pending = Some(competing_current);
        let mut competing_created = Some(competing_current);
        assert!(
            settle_pending_current_message_candidate_on_loop_exit(StreamTickCandidateSaveContext {
                gateway: &gateway,
                provider: &ProviderKind::Codex,
                token_hash: "candidate-matrix-test",
                channel_id: competing_channel,
                persisted_baseline: &mut competing_baseline,
                inflight_state: &mut competing_local,
                expected_identity: &competing_identity,
                expected_current_message: &mut competing_expected,
                current_msg_id: &mut competing_current,
                pending_current_message_candidate: &mut competing_pending,
                bridge_created_response_placeholder_msg_id: &mut competing_created,
            },)
            .await
        );
        assert_eq!(competing_pending, None);
        assert_eq!(competing_created, None);
        assert_eq!(competing_expected, (900_003, 29));
        assert_eq!(competing_current, MessageId::new(900_003));

        let reowned_channel = ChannelId::new(4_259_108);
        let mut stale = owner_state(reowned_channel.get(), 77_010);
        let stale_identity =
            crate::services::discord::inflight::InflightTurnIdentity::from_state(&stale);
        let mut stale_baseline = stale.clone();
        let successor = owner_state(reowned_channel.get(), 99_999);
        save_inflight_state(&successor).expect("seed successor owner row");
        stale.current_msg_id = 4;
        stale.current_msg_len = 12;
        let mut stale_expected = (0, 0);
        let mut stale_current = MessageId::new(4);
        let mut stale_pending = Some(stale_current);
        let mut stale_created = Some(stale_current);
        assert_eq!(
            persist_stream_tick_state_with_candidate_cleanup(
                StreamTickCandidateSaveContext {
                    gateway: &gateway,
                    provider: &ProviderKind::Codex,
                    token_hash: "candidate-matrix-test",
                    channel_id: reowned_channel,
                    persisted_baseline: &mut stale_baseline,
                    inflight_state: &mut stale,
                    expected_identity: &stale_identity,
                    expected_current_message: &mut stale_expected,
                    current_msg_id: &mut stale_current,
                    pending_current_message_candidate: &mut stale_pending,
                    bridge_created_response_placeholder_msg_id: &mut stale_created,
                },
                "turn_bridge::stream_tick::candidate_reowned_test",
            )
            .await,
            GuardedSaveOutcome::IdentityMismatch
        );
        assert_eq!(stale_pending, None);
        assert_eq!(stale_created, None);
        assert_eq!(durable_current_msg_id_from_detached(stale_current), 0);
        assert_eq!(
            load_inflight_state(&ProviderKind::Codex, reowned_channel.get())
                .expect("successor survives")
                .user_msg_id,
            99_999
        );

        let missing_channel = ChannelId::new(4_259_109);
        let mut missing = owner_state(missing_channel.get(), 77_010);
        let missing_identity =
            crate::services::discord::inflight::InflightTurnIdentity::from_state(&missing);
        let mut missing_baseline = missing.clone();
        missing.current_msg_id = 5;
        missing.current_msg_len = 13;
        let mut missing_expected = (0, 0);
        let mut missing_current = MessageId::new(5);
        let mut missing_pending = Some(missing_current);
        let mut missing_created = Some(missing_current);
        assert_eq!(
            persist_stream_tick_state_with_candidate_cleanup(
                StreamTickCandidateSaveContext {
                    gateway: &gateway,
                    provider: &ProviderKind::Codex,
                    token_hash: "candidate-matrix-test",
                    channel_id: missing_channel,
                    persisted_baseline: &mut missing_baseline,
                    inflight_state: &mut missing,
                    expected_identity: &missing_identity,
                    expected_current_message: &mut missing_expected,
                    current_msg_id: &mut missing_current,
                    pending_current_message_candidate: &mut missing_pending,
                    bridge_created_response_placeholder_msg_id: &mut missing_created,
                },
                "turn_bridge::stream_tick::candidate_missing_test",
            )
            .await,
            GuardedSaveOutcome::Missing
        );
        assert_eq!(missing_pending, None);
        assert_eq!(missing_created, None);
        assert_eq!(durable_current_msg_id_from_detached(missing_current), 0);
        assert_eq!(
            gateway.deletes.lock().expect("deletes lock").as_slice(),
            &[3, 4, 5]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn strict_fence_loses_authority_before_visible_mutation() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env_reset =
            crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let channel = ChannelId::new(4_259_110);
        let mut stale = owner_state(channel.get(), 77_010);
        stale.current_msg_id = 1;
        stale.current_msg_len = 9;
        save_inflight_state(&stale).expect("seed bridge owner row");
        let expected = crate::services::discord::inflight::InflightTurnIdentity::from_state(&stale);
        let mut persisted_baseline = stale.clone();

        stale.full_response = "stale bridge response".to_string();
        stale.current_msg_id = 2;
        stale.current_msg_len = 10;
        let mut expected_current_message = (1, 9);
        let mut current_msg_id = MessageId::new(2);
        let mut pending_candidate = Some(current_msg_id);
        let mut bridge_created_candidate = Some(current_msg_id);

        let mut watcher = persisted_baseline.clone();
        watcher.full_response = "watcher-owned response".to_string();
        watcher.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);
        save_inflight_state(&watcher).expect("watcher takes relay authority");

        let gateway = super::super::provider_output_guard_tests::CapturingGateway::default();
        let intended_authority =
            crate::services::discord::inflight::StreamRelayAuthority::from_state(&stale);
        let outcome = fence_stream_tick_visible_mutation_with_candidate_cleanup(
            StreamTickCandidateSaveContext {
                gateway: &gateway,
                provider: &ProviderKind::Codex,
                token_hash: "strict-fence-test",
                channel_id: channel,
                persisted_baseline: &mut persisted_baseline,
                inflight_state: &mut stale,
                expected_identity: &expected,
                expected_current_message: &mut expected_current_message,
                current_msg_id: &mut current_msg_id,
                pending_current_message_candidate: &mut pending_candidate,
                bridge_created_response_placeholder_msg_id: &mut bridge_created_candidate,
            },
            "turn_bridge::stream_tick::strict_authority_interleaving_test",
        )
        .await;
        let authority =
            visible_mutation_authority_after_guarded_save(outcome, &stale, intended_authority);
        if authority == VisibleMutationAuthority::Authorized {
            TurnGateway::edit_message(
                &gateway,
                channel,
                current_msg_id,
                "forbidden stale mutation",
            )
            .await
            .expect("test mutation");
        }

        assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);
        assert_eq!(authority, VisibleMutationAuthority::AuthorityLost);
        assert!(gateway.edits.lock().expect("edits lock").is_empty());
        assert_eq!(
            gateway.deletes.lock().expect("deletes lock").as_slice(),
            &[2]
        );
        assert_eq!(pending_candidate, None);
        assert_eq!(bridge_created_candidate, None);
        assert_eq!(expected_current_message, (1, 9));
        assert_eq!(current_msg_id, MessageId::new(1));
        assert_eq!(stale.full_response, "watcher-owned response");
        let durable =
            load_inflight_state(&ProviderKind::Codex, channel.get()).expect("watcher row survives");
        assert_eq!(
            durable.effective_relay_owner_kind(),
            crate::services::discord::inflight::RelayOwnerKind::Watcher
        );
        assert_eq!(
            serde_json::to_value(&stale).unwrap(),
            serde_json::to_value(&durable).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&persisted_baseline).unwrap(),
            serde_json::to_value(&durable).unwrap()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn second_rollover_failure_keeps_bound_m2_and_deletes_only_unbound_m3() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let blocked_root = temp.path().join("blocked-root");
        std::fs::write(&blocked_root, b"not a directory").expect("blocked runtime root");
        let _env_reset =
            crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", &blocked_root);

        let channel = ChannelId::new(4_259_111);
        let mut state = owner_state(channel.get(), 77_010);
        // M2 is the already-guarded first rollover. M3 is the only unbound
        // candidate when the second rollover's immediate bind hits I/O error.
        state.current_msg_id = 2;
        state.current_msg_len = 11;
        let expected = crate::services::discord::inflight::InflightTurnIdentity::from_state(&state);
        let mut persisted_baseline = state.clone();
        let mut expected_current_message = (2, 11);
        state.current_msg_id = 3;
        state.current_msg_len = 12;
        let mut current_msg_id = MessageId::new(3);
        let mut pending_candidate = Some(current_msg_id);
        let mut bridge_created_candidate = Some(current_msg_id);
        let gateway = super::super::provider_output_guard_tests::CapturingGateway::default();
        let intended_authority =
            crate::services::discord::inflight::StreamRelayAuthority::from_state(&state);

        let outcome = fence_stream_tick_visible_mutation_with_candidate_cleanup(
            StreamTickCandidateSaveContext {
                gateway: &gateway,
                provider: &ProviderKind::Codex,
                token_hash: "multi-rollover-test",
                channel_id: channel,
                persisted_baseline: &mut persisted_baseline,
                inflight_state: &mut state,
                expected_identity: &expected,
                expected_current_message: &mut expected_current_message,
                current_msg_id: &mut current_msg_id,
                pending_current_message_candidate: &mut pending_candidate,
                bridge_created_response_placeholder_msg_id: &mut bridge_created_candidate,
            },
            "turn_bridge::stream_tick::second_rollover_bind_test",
        )
        .await;
        assert_eq!(outcome, GuardedSaveOutcome::IoError);
        assert_eq!(
            visible_mutation_authority_after_guarded_save(outcome, &state, intended_authority,),
            VisibleMutationAuthority::Retry
        );
        assert_eq!(pending_candidate, Some(MessageId::new(3)));
        assert!(gateway.deletes.lock().expect("deletes lock").is_empty());

        assert!(
            !settle_pending_current_message_candidate_on_loop_exit(
                StreamTickCandidateSaveContext {
                    gateway: &gateway,
                    provider: &ProviderKind::Codex,
                    token_hash: "multi-rollover-test",
                    channel_id: channel,
                    persisted_baseline: &mut persisted_baseline,
                    inflight_state: &mut state,
                    expected_identity: &expected,
                    expected_current_message: &mut expected_current_message,
                    current_msg_id: &mut current_msg_id,
                    pending_current_message_candidate: &mut pending_candidate,
                    bridge_created_response_placeholder_msg_id: &mut bridge_created_candidate,
                },
            )
            .await
        );
        assert_eq!(pending_candidate, None);
        assert_eq!(bridge_created_candidate, None);
        assert_eq!((state.current_msg_id, state.current_msg_len), (2, 11));
        assert_eq!(current_msg_id, MessageId::new(2));
        assert_eq!(
            gateway.deletes.lock().expect("deletes lock").as_slice(),
            &[3]
        );
        assert!(
            !gateway.deletes.lock().expect("deletes lock").contains(&2),
            "already-bound M2 must never be treated as an orphan"
        );
    }

    #[test]
    fn production_tick_reconciles_anchor_dirty_flush_and_exit_candidate_merges() {
        let tick = include_str!("../stream_tick.rs");
        let production_tick = tick
            .split("#[cfg(test)]")
            .next()
            .expect("production stream tick prefix");
        let flush_predicate = production_tick
            .find("if state_dirty")
            .expect("production flush predicate remains present");
        let pending = production_tick[flush_predicate..]
            .find("|| pending_current_message_candidate.is_some()")
            .map(|offset| flush_predicate + offset)
            .expect("pending response candidate forces a guarded retry");
        let persist = production_tick[pending..]
            .find("persist_stream_tick_state_with_candidate_cleanup(")
            .map(|offset| pending + offset)
            .expect("forced retry reaches candidate-aware persistence");
        assert!(flush_predicate < pending && pending < persist);

        let anchor_preflight = production_tick
            .find("turn_bridge::stream_tick::anchor_preflight")
            .expect("unsaved response is persisted before anchor send");
        let ensure = production_tick
            .find("ensure_bridge_current_message_anchor(")
            .expect("production tick materializes an absent anchor");
        let reconcile = production_tick[ensure..]
            .find("reconcile_tick_runtime_from_inflight!(")
            .map(|offset| ensure + offset)
            .expect("anchor await refreshes every detached tick snapshot");
        let relay_gate = production_tick[reconcile..]
            .find("if !bridge_stream_relay_suppressed(")
            .map(|offset| reconcile + offset)
            .expect("refreshed relay ownership gates bridge output");
        assert!(anchor_preflight < ensure && ensure < reconcile && reconcile < relay_gate);

        let dirty_flush = production_tick
            .find("turn_bridge::stream_tick::dirty_flush")
            .expect("ordinary dirty flush remains guarded");
        let dirty_anchor = production_tick[..dirty_flush]
            .rfind("let current_msg_id_before_flush = current_msg_id;")
            .expect("dirty flush captures the pre-save edit-cache anchor");
        let saved_reconcile = production_tick[dirty_flush..]
            .find("reconcile_tick_runtime_from_inflight!(current_msg_id_before_flush);")
            .map(|offset| dirty_flush + offset)
            .expect("ordinary Saved flush refreshes detached tick state");
        let tick_writeback = production_tick[saved_reconcile..]
            .find("writeback_tick_state!();")
            .map(|offset| saved_reconcile + offset)
            .expect("reconciled tick state is written back");
        assert!(
            dirty_anchor < dirty_flush
                && dirty_flush < saved_reconcile
                && saved_reconcile < tick_writeback
        );

        let rollover_send = production_tick
            .find("TurnGateway::send_message(gateway.as_ref(), channel_id, &status_block)")
            .expect("rollover creates a fresh tail candidate");
        let immediate_rollover_bind = production_tick[rollover_send..]
            .find("turn_bridge::stream_tick::rollover_candidate_bind")
            .map(|offset| rollover_send + offset)
            .expect("every rollover candidate is guarded before another loop iteration");
        let post_rollover_panel = production_tick[immediate_rollover_bind..]
            .find("turn_bridge::stream_tick::panel_reanchor")
            .map(|offset| immediate_rollover_bind + offset)
            .expect("post-rollover work starts only after the candidate bind");
        assert!(rollover_send < immediate_rollover_bind);
        assert!(immediate_rollover_bind < post_rollover_panel);

        let stream_loop = include_str!("../stream_loop.rs");
        let persistent_dirty = stream_loop
            .find("let mut state_dirty = false;")
            .expect("save retry state is initialized once");
        let outer = stream_loop
            .find("'outer: while")
            .expect("production stream loop remains present");
        let tick_call = stream_loop[outer..]
            .find("let tick_outcome = run_bridge_stream_tick(")
            .map(|offset| outer + offset)
            .expect("stream loop observes the explicit tick outcome");
        let authority_loss = stream_loop[tick_call..]
            .find("if tick_outcome == StreamTickOutcome::AuthorityLost")
            .map(|offset| tick_call + offset)
            .expect("tick authority loss reaches the stream loop");
        let authority_break = stream_loop[authority_loss..]
            .find("break 'outer;")
            .map(|offset| authority_loss + offset)
            .expect("authority loss immediately stops stream draining");
        let writeback = stream_loop[outer..]
            .find("*state.inflight_state = inflight_state;")
            .map(|offset| outer + offset)
            .expect("detached state is staged for exit settlement");
        let exit_settlement_call = stream_loop[writeback..]
            .find("settle_and_reconcile_exit_candidate(")
            .map(|offset| writeback + offset)
            .expect("stream-loop exit delegates remaining-candidate settlement");
        assert!(
            persistent_dirty < outer
                && outer < tick_call
                && tick_call < authority_loss
                && authority_loss < authority_break
                && authority_break < writeback
                && writeback < exit_settlement_call
        );

        let exit_reconcile = include_str!("../stream_loop/exit_reconcile.rs");
        let pre_settle_anchor = exit_reconcile
            .find("let current_msg_id_before_exit_settle = *context.state.current_msg_id;")
            .expect("candidate edit-cache identity is captured before settlement");
        let settle = exit_reconcile[pre_settle_anchor..]
            .find("settle_pending_current_message_candidate_on_loop_exit(")
            .map(|offset| pre_settle_anchor + offset)
            .expect("stream-loop exit settles a remaining candidate");
        let saved_reconcile = exit_reconcile[settle..]
            .find("reconcile_saved_exit_candidate(")
            .map(|offset| settle + offset)
            .expect("successful exit merge refreshes caller-owned state");
        assert!(pre_settle_anchor < settle && settle < saved_reconcile);
    }

    #[test]
    fn heartbeat_touches_same_owner_but_skips_successor() {
        with_runtime_root(|| {
            let channel = ChannelId::new(4_259_103);
            let owner = owner_state(channel.get(), 77_010);
            save_inflight_state(&owner).expect("seed owner row");
            let expected =
                crate::services::discord::inflight::InflightTurnIdentity::from_state(&owner);

            assert_eq!(
                persist_stream_tick_heartbeat(&ProviderKind::Codex, channel, &expected),
                GuardedSaveOutcome::Saved
            );

            let successor = owner_state(channel.get(), 99_999);
            save_inflight_state(&successor).expect("seed successor row");
            assert_eq!(
                persist_stream_tick_heartbeat(&ProviderKind::Codex, channel, &expected),
                GuardedSaveOutcome::IdentityMismatch
            );
            let persisted =
                load_inflight_state(&ProviderKind::Codex, channel.get()).expect("persisted row");
            assert_eq!(persisted.user_msg_id, 99_999);
        });
    }
}
