//! Bridge-entry inflight persistence plus local-state reconciliation (#4259 R4).

use super::*;

pub(super) struct BridgeEntryRuntimeState<'a> {
    pub(super) inflight_state: &'a mut InflightTurnState,
    pub(super) full_response: &'a mut String,
    pub(super) response_sent_offset: &'a mut usize,
    pub(super) bridge_confirmed_response_sent_offset: &'a mut usize,
    pub(super) current_msg_id: &'a mut MessageId,
    pub(super) current_tool_line: &'a mut Option<String>,
    pub(super) prev_tool_status: &'a mut Option<String>,
    pub(super) last_tool_name: &'a mut Option<String>,
    pub(super) last_tool_summary: &'a mut Option<String>,
    pub(super) any_tool_used: &'a mut bool,
    pub(super) has_post_tool_text: &'a mut bool,
    pub(super) streaming_rollover_frozen_msg_ids: &'a mut Vec<MessageId>,
    pub(super) tmux_last_offset: &'a mut Option<u64>,
    pub(super) watcher_owner_channel_id: &'a mut ChannelId,
    pub(super) watcher_owns_assistant_relay: &'a mut bool,
    pub(super) watcher_relay_available_for_turn: &'a mut bool,
    pub(super) standby_relay_owns_output: &'a mut bool,
    pub(super) status_panel_msg_id: &'a mut Option<MessageId>,
    pub(super) status_panel_generation: &'a mut u64,
}

fn relay_owner_flags(
    owner_kind: crate::services::discord::inflight::RelayOwnerKind,
    watcher_registered: bool,
) -> (bool, bool, bool) {
    use crate::services::discord::inflight::RelayOwnerKind;

    let watcher_owns_assistant_relay = matches!(owner_kind, RelayOwnerKind::Watcher);
    let watcher_relay_available_for_turn = watcher_owns_assistant_relay && watcher_registered;
    let standby_relay_owns_output = matches!(
        owner_kind,
        RelayOwnerKind::StandbyRelay | RelayOwnerKind::SessionBoundRelay | RelayOwnerKind::Unknown
    );
    (
        watcher_owns_assistant_relay,
        watcher_relay_available_for_turn,
        standby_relay_owns_output,
    )
}

fn reconciled_watcher_owner_channel_id(
    durable_owner_channel_id: Option<u64>,
    delivery_channel_id: u64,
) -> ChannelId {
    durable_owner_channel_id
        .and_then(crate::services::discord::inflight::opt_channel_id)
        .unwrap_or_else(|| ChannelId::new(delivery_channel_id))
}

pub(super) fn bridge_stream_relay_suppressed(
    watcher_owns_assistant_relay: bool,
    standby_relay_owns_output: bool,
) -> bool {
    watcher_owns_assistant_relay || standby_relay_owns_output
}

/// Converts the guarded store result into the bridge lifecycle gate. No bridge
/// guard/finalizer may be constructed until this returns true.
pub(super) fn bridge_entry_lifecycle_can_continue(
    outcome: crate::services::discord::inflight::GuardedSaveOutcome,
) -> bool {
    use crate::services::discord::inflight::GuardedSaveOutcome;

    matches!(outcome, GuardedSaveOutcome::Saved)
}

/// Wakes a completion waiter on a pre-authority abort without registering a
/// finalizer or publishing `InflightSignal::Completed` for a successor turn.
pub(super) fn signal_bridge_entry_abort_completion(
    completion_tx: &mut Option<tokio::sync::oneshot::Sender<()>>,
) {
    if let Some(tx) = completion_tx.take() {
        let _ = tx.send(());
    }
}

pub(super) fn reconcile_runtime_locals_from_inflight_state(
    shared: &SharedData,
    state: &mut BridgeEntryRuntimeState<'_>,
) {
    state
        .full_response
        .clone_from(&state.inflight_state.full_response);
    *state.response_sent_offset = state.inflight_state.response_sent_offset;
    *state.bridge_confirmed_response_sent_offset = bridge_confirmed_response_sent_offset_seed(
        state.inflight_state.effective_relay_owner_kind(),
        *state.response_sent_offset,
    );
    *state.current_msg_id =
        detached_current_msg_id_from_durable(state.inflight_state.current_msg_id);
    state
        .current_tool_line
        .clone_from(&state.inflight_state.current_tool_line);
    state
        .prev_tool_status
        .clone_from(&state.inflight_state.prev_tool_status);
    state
        .last_tool_name
        .clone_from(&state.inflight_state.last_tool_name);
    state
        .last_tool_summary
        .clone_from(&state.inflight_state.last_tool_summary);
    *state.any_tool_used = state.inflight_state.any_tool_used;
    *state.has_post_tool_text = state.inflight_state.has_post_tool_text;
    *state.streaming_rollover_frozen_msg_ids = state
        .inflight_state
        .streaming_rollover_frozen_msg_ids
        .iter()
        .filter_map(|id| crate::services::discord::inflight::optional_message_id(*id))
        .collect();
    if state.tmux_last_offset.is_some() {
        *state.tmux_last_offset = Some(state.inflight_state.last_offset);
    }
    *state.watcher_owner_channel_id = reconciled_watcher_owner_channel_id(
        state.inflight_state.watcher_owner_channel_id,
        state.inflight_state.channel_id,
    );
    let watcher_registered =
        live_watcher_registered_for_relay(shared, *state.watcher_owner_channel_id);
    (
        *state.watcher_owns_assistant_relay,
        *state.watcher_relay_available_for_turn,
        *state.standby_relay_owns_output,
    ) = relay_owner_flags(
        state.inflight_state.effective_relay_owner_kind(),
        watcher_registered,
    );
    *state.status_panel_msg_id = state
        .inflight_state
        .status_message_id
        .and_then(crate::services::discord::inflight::optional_message_id);
    *state.status_panel_generation = state.inflight_state.status_panel_generation;
}

pub(super) fn clear_last_edit_text_if_current_message_changed(
    before: MessageId,
    after: MessageId,
    last_edit_text: &mut String,
) {
    if before != after {
        last_edit_text.clear();
    }
}

pub(super) fn resumed_long_running_placeholder_notice_message_id(
    bridge_clear_applied: bool,
    before: &InflightTurnState,
    merged: &InflightTurnState,
) -> Option<MessageId> {
    (bridge_clear_applied
        && before.long_running_placeholder_active
        && !merged.long_running_placeholder_active
        && before.current_msg_id != 0
        && (before.current_msg_id, before.current_msg_len)
            == (merged.current_msg_id, merged.current_msg_len)
        && before.full_response == merged.full_response
        && before.response_sent_offset == merged.response_sent_offset
        && merged.effective_relay_owner_kind()
            == crate::services::discord::inflight::RelayOwnerKind::None)
        .then(|| MessageId::new(merged.current_msg_id))
}

/// Saves bridge-entry mutations without recreating or overwriting a row this
/// turn no longer owns. A successful store patch replaces `inflight_state` with
/// the lock-held merge; mirror that merge into detached loop locals so the next
/// stream tick cannot flush the pre-await snapshot back over watcher progress.
pub(super) fn persist_bridge_entry_inflight_state(
    before: &InflightTurnState,
    shared: &SharedData,
    runtime: &mut BridgeEntryRuntimeState<'_>,
    placeholder_clear_applied: &mut bool,
) -> crate::services::discord::inflight::GuardedSaveOutcome {
    use crate::services::discord::inflight::{
        GuardedSaveOutcome, patch_bridge_entry_state_if_identity_unchanged,
        patch_bridge_entry_state_tracking_placeholder_clear,
    };

    const CALLER: &str = "turn_bridge::spawn_turn_bridge::bridge_entry";
    let outcome = if before.long_running_placeholder_active
        && !runtime.inflight_state.long_running_placeholder_active
    {
        patch_bridge_entry_state_tracking_placeholder_clear(
            before,
            &mut *runtime.inflight_state,
            placeholder_clear_applied,
            CALLER,
        )
    } else {
        *placeholder_clear_applied = false;
        patch_bridge_entry_state_if_identity_unchanged(before, &mut *runtime.inflight_state, CALLER)
    };
    match outcome {
        GuardedSaveOutcome::Saved => {
            reconcile_runtime_locals_from_inflight_state(shared, runtime);
        }
        GuardedSaveOutcome::Missing => tracing::warn!(
            channel_id = before.channel_id,
            caller = CALLER,
            "bridge-entry inflight patch skipped: durable row missing; row was not recreated"
        ),
        GuardedSaveOutcome::IdentityMismatch => tracing::warn!(
            channel_id = before.channel_id,
            caller = CALLER,
            "bridge-entry inflight patch skipped: durable row belongs to another turn"
        ),
        GuardedSaveOutcome::IoError => tracing::warn!(
            channel_id = before.channel_id,
            caller = CALLER,
            "bridge-entry inflight patch failed: inflight store I/O error"
        ),
    }
    outcome
}

pub(super) struct BridgeEntryAuthorityContext<'a> {
    pub(super) bridge: &'a mut TurnBridgeContext,
    pub(super) shared: &'a SharedData,
    pub(super) bridge_created_placeholder: &'a mut Option<MessageId>,
    pub(super) last_edit_text: &'a mut String,
    pub(super) resumed_placeholder_clear_applied: &'a mut bool,
}

/// Proves durable bridge authority, then materializes an absent Discord anchor.
/// The caller may construct finalizer/broadcast/cleanup guards only after true.
pub(super) async fn establish_bridge_entry_authority(
    ctx: BridgeEntryAuthorityContext<'_>,
    mut runtime: BridgeEntryRuntimeState<'_>,
    anchor_text: &str,
) -> bool {
    let outcome = persist_bridge_entry_inflight_state(
        &ctx.bridge.inflight_state,
        ctx.shared,
        &mut runtime,
        ctx.resumed_placeholder_clear_applied,
    );
    // #5464 T5 S2: record what this gate answers and what the AC2-R gate would
    // answer, for the cohort only. Observation returns `()`, so the gate below
    // reads the same `outcome` it always did.
    crate::services::discord::relay_recovery::authority_observation::record_bridge_entry_gate(
        ctx.shared,
        &ctx.bridge.inflight_state,
        outcome,
    );
    if !bridge_entry_lifecycle_can_continue(outcome) {
        signal_bridge_entry_abort_completion(&mut ctx.bridge.completion_tx);
        return false;
    }

    let anchor_was_absent = durable_current_msg_id_from_detached(*runtime.current_msg_id) == 0;
    let identity = crate::services::discord::inflight::InflightTurnIdentity::from_state(
        runtime.inflight_state,
    );
    if !ensure_bridge_current_message_anchor(
        ctx.bridge.gateway.as_ref(),
        &ctx.bridge.provider,
        &ctx.shared.token_hash,
        ctx.bridge.channel_id,
        &identity,
        runtime.current_msg_id,
        ctx.bridge_created_placeholder,
        runtime.inflight_state,
        anchor_text,
    )
    .await
    {
        signal_bridge_entry_abort_completion(&mut ctx.bridge.completion_tx);
        return false;
    }
    // The Discord send above is an await boundary. Anchor bind/reuse refreshes
    // the lock-held row so watcher progress during that gap cannot be flushed
    // back from the pre-await detached locals.
    reconcile_runtime_locals_from_inflight_state(ctx.shared, &mut runtime);
    if anchor_was_absent {
        ctx.last_edit_text.clear();
        if *ctx.bridge_created_placeholder == Some(*runtime.current_msg_id) {
            ctx.last_edit_text.push_str(anchor_text);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::inflight::{
        GuardedSaveOutcome, InflightTurnState, RelayOwnerKind,
    };
    use crate::services::provider::ProviderKind;

    #[test]
    fn bridge_entry_failure_outcomes_abort_without_arming_cleanup() {
        for outcome in [
            GuardedSaveOutcome::Missing,
            GuardedSaveOutcome::IdentityMismatch,
            GuardedSaveOutcome::IoError,
        ] {
            assert!(!bridge_entry_lifecycle_can_continue(outcome));
        }

        assert!(bridge_entry_lifecycle_can_continue(
            GuardedSaveOutcome::Saved
        ));
    }

    /// #5464 T5 S2: the recorded `old` verdict has to BE the gate that ships,
    /// or the promotion window compares the AC2-R gate against a fiction. This
    /// asserts the mirror against the production predicate over its whole input
    /// domain, so a change to either side fails here instead of silently
    /// re-basing the evidence.
    #[test]
    fn recorded_entry_gate_old_mirrors_the_shipped_lifecycle_gate() {
        use crate::services::discord::relay_recovery::authority_observation::{
            entry_gate_new, entry_gate_old,
        };

        for outcome in [
            GuardedSaveOutcome::Saved,
            GuardedSaveOutcome::Missing,
            GuardedSaveOutcome::IdentityMismatch,
            GuardedSaveOutcome::IoError,
        ] {
            assert_eq!(
                entry_gate_old(outcome).ends_lifecycle(),
                !bridge_entry_lifecycle_can_continue(outcome),
                "{outcome:?}: recorded old entry verdict disagrees with the shipped gate"
            );
        }
        assert!(
            !bridge_entry_lifecycle_can_continue(GuardedSaveOutcome::Missing)
                && !entry_gate_new(GuardedSaveOutcome::Missing).ends_lifecycle(),
            "AC1: the shipped gate ends the turn on a missing row and the new one must not"
        );
    }

    #[test]
    fn saved_reconciliation_preserves_existing_anchor_edit_cache() {
        let current = MessageId::new(42_590_001);
        let mut last_edit_text = "already rendered".to_string();
        clear_last_edit_text_if_current_message_changed(current, current, &mut last_edit_text);
        assert_eq!(last_edit_text, "already rendered");
    }

    #[test]
    fn saved_competing_bind_reconciliation_invalidates_candidate_edit_cache() {
        let mut last_edit_text = "candidate render".to_string();
        clear_last_edit_text_if_current_message_changed(
            MessageId::new(42_590_001),
            MessageId::new(42_590_002),
            &mut last_edit_text,
        );
        assert!(last_edit_text.is_empty());
    }

    #[test]
    fn durable_placeholder_clear_and_response_advance_suppresses_restart_notice_edit() {
        let mut before = InflightTurnState::new(
            ProviderKind::Codex,
            42_590_003,
            Some("notice-race".to_string()),
            343_742_347_365_974_026,
            77_010,
            18,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-notice-race".to_string()),
            Some("/tmp/notice-race.jsonl".to_string()),
            Some("/tmp/notice-race.input".to_string()),
            512,
        );
        before.long_running_placeholder_active = true;
        before.current_msg_id = 901;
        before.current_msg_len = 12;
        before.full_response = "partial".to_string();

        let mut durable = before.clone();
        durable.long_running_placeholder_active = false;
        durable.current_msg_id = 902;
        durable.current_msg_len = 24;
        durable.full_response = "partial watcher completion".to_string();
        durable.response_sent_offset = durable.full_response.len();

        assert_eq!(
            resumed_long_running_placeholder_notice_message_id(true, &before, &durable),
            None
        );
        durable.current_msg_id = before.current_msg_id;
        durable.current_msg_len = before.current_msg_len;
        assert_eq!(
            resumed_long_running_placeholder_notice_message_id(true, &before, &durable),
            None,
            "same-id watcher response progress must also suppress the destructive edit"
        );

        let mut bridge_cleared = before.clone();
        bridge_cleared.long_running_placeholder_active = false;
        assert_eq!(
            resumed_long_running_placeholder_notice_message_id(true, &before, &bridge_cleared),
            Some(MessageId::new(901))
        );
        assert_eq!(
            resumed_long_running_placeholder_notice_message_id(false, &before, &bridge_cleared),
            None,
            "a durable writer that already cleared the flag owns the visible result"
        );
        bridge_cleared.set_relay_owner_kind(RelayOwnerKind::Watcher);
        assert_eq!(
            resumed_long_running_placeholder_notice_message_id(true, &before, &bridge_cleared),
            None,
            "a live external relay owns visible response edits"
        );
    }

    #[test]
    fn pre_authority_abort_signals_waiter_without_completed_broadcast() {
        let (completion_tx, mut completion_rx) = tokio::sync::oneshot::channel();
        let mut completion_tx = Some(completion_tx);
        let (signals, mut signal_rx) = tokio::sync::broadcast::channel::<
            crate::services::discord::inflight::InflightSignal,
        >(1);

        signal_bridge_entry_abort_completion(&mut completion_tx);

        assert!(completion_tx.is_none());
        assert_eq!(completion_rx.try_recv(), Ok(()));
        assert!(matches!(
            signal_rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        drop(signals);
    }

    #[test]
    fn pre_authority_abort_preserves_same_id_successor_bytes() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let provider = ProviderKind::Codex;
        let channel_id = 4_259_605;
        let successor = InflightTurnState::new(
            provider.clone(),
            channel_id,
            Some("same-id-successor".to_string()),
            343_742_347_365_974_026,
            77_605,
            91,
            "successor prompt".to_string(),
            Some("successor-session".to_string()),
            Some("AgentDesk-same-id-successor".to_string()),
            Some("/tmp/same-id-successor.jsonl".to_string()),
            Some("/tmp/same-id-successor.input".to_string()),
            9_100,
        );
        let mut stale = successor.clone();
        stale.started_at = "stale-started-at".to_string();
        stale.tmux_session_name = Some("AgentDesk-stale-same-id-owner".to_string());
        stale.current_msg_id = 90;
        let before_stale_patch = stale.clone();
        crate::services::discord::inflight::save_inflight_state(&successor)
            .expect("seed same-id successor row");
        let root =
            crate::services::discord::inflight::inflight_runtime_root().expect("runtime root");
        let path =
            crate::services::discord::inflight::inflight_state_path(&root, &provider, channel_id);
        let before = std::fs::read(&path).expect("read successor bytes");
        let (completion_tx, mut completion_rx) = tokio::sync::oneshot::channel();
        let mut completion_tx = Some(completion_tx);

        stale.full_response = "stale bytes must not land".to_string();
        let outcome =
            crate::services::discord::inflight::patch_bridge_entry_state_if_identity_unchanged(
                &before_stale_patch,
                &mut stale,
                "turn_bridge::bridge_entry_persist::same_id_successor_test",
            );
        assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);
        assert!(!bridge_entry_lifecycle_can_continue(outcome));
        signal_bridge_entry_abort_completion(&mut completion_tx);

        assert_eq!(completion_rx.try_recv(), Ok(()));
        assert_eq!(std::fs::read(path).expect("successor survives"), before);
    }

    #[test]
    fn bridge_entry_failure_gate_precedes_emit_stream_and_finalize() {
        let normalize_ws = |source: &str| source.split_whitespace().collect::<Vec<_>>().join(" ");
        let caller = normalize_ws(include_str!("mod.rs"));
        let helper = normalize_ws(include_str!("bridge_entry_persist.rs"));
        let spawn = caller
            .find("pub(super) fn spawn_turn_bridge")
            .expect("production bridge entry remains present");
        let authority = caller[spawn..]
            .find("if !bridge_entry_persist::establish_bridge_entry_authority")
            .map(|offset| spawn + offset)
            .expect("production caller establishes authority");
        let guards = caller[authority..]
            .find("let (mut completion_guard, mut inflight_guard) = make_bridge_guards(")
            .map(|offset| authority + offset)
            .expect("production caller constructs guards");
        let entry_owner = caller[authority..guards]
            .find("let mut bridge_entry_watcher_owner_epoch_current = inflight_state .effective_relay_owner_kind()")
            .map(|offset| authority + offset)
            .expect("post-authority relay owner snapshot remains explicit");
        let guard_call = &caller[guards
            ..caller[guards..]
                .find(");")
                .map(|offset| guards + offset + 2)
                .expect("guard call remains bounded")];
        let notice_decision = caller[guards..]
            .find("resumed_long_running_placeholder_notice_message_id(")
            .map(|offset| guards + offset)
            .expect("restart notice is decided from the post-authority merge");
        let notice_edit = caller[notice_decision..]
            .find("resumed_msg_id,")
            .map(|offset| notice_decision + offset)
            .expect("restart notice edits only the predicate-approved anchor");
        let emit = caller[notice_edit..]
            .find("crate::services::observability::emit_turn_started")
            .map(|offset| notice_edit + offset)
            .expect("turn-start emit remains present");
        let stream = caller[emit..]
            .find("stream_loop::run_stream_loop")
            .map(|offset| emit + offset)
            .expect("stream loop remains present");
        let finalize = caller[stream..]
            .find("post_loop_finalize::run_post_loop_finalize")
            .map(|offset| stream + offset)
            .expect("post-loop finalize remains present");
        let finalize_context = &caller[finalize..];

        let establish = helper
            .find("pub(super) async fn establish_bridge_entry_authority")
            .expect("authority helper remains present");
        let persist = helper[establish..]
            .find("let outcome =")
            .map(|offset| establish + offset)
            .expect("authority helper persists first");
        let gate = helper[persist..]
            .find("if !bridge_entry_lifecycle_can_continue")
            .map(|offset| persist + offset)
            .expect("authority helper gates persistence");
        let anchor = helper[gate..]
            .find("if !ensure_bridge_current_message_anchor")
            .map(|offset| gate + offset)
            .expect("authority helper guarded-binds an absent anchor");
        let refresh = helper[anchor..]
            .find("reconcile_runtime_locals_from_inflight_state")
            .map(|offset| anchor + offset)
            .expect("post-await durable anchor state refreshes detached locals");

        assert!(persist < gate && gate < anchor && anchor < refresh);
        assert!(
            authority < entry_owner
                && entry_owner < guards
                && guards < notice_decision
                && notice_decision < notice_edit
                && notice_edit < emit
                && emit < stream
                && stream < finalize
        );
        assert!(
            helper[gate..anchor].contains("signal_bridge_entry_abort_completion")
                && helper[gate..anchor].contains("return false;"),
            "failed persistence must signal only the waiter and abort"
        );
        assert!(
            !caller[spawn..authority].contains("make_bridge_guards("),
            "pre-authority path must not register a finalizer or broadcast guard"
        );
        assert!(
            !caller[spawn..authority].contains("send_message"),
            "pre-authority path must not create a Discord placeholder"
        );
        assert!(
            guard_call.contains("&inflight_state"),
            "finalizer and cleanup guards must use the exact post-authority merge"
        );
        assert!(
            finalize_context.contains("bridge_entry_watcher_owner_epoch_current,"),
            "post-loop recovery classification must receive the entry owner epoch verdict"
        );
    }

    #[test]
    fn post_authority_owner_snapshot_observes_same_identity_watcher_adoption() {
        let mut detached = InflightTurnState::new(
            ProviderKind::Codex,
            42_590_611,
            Some("entry-owner-adoption".to_string()),
            343_742_347_365_974_026,
            77_611,
            18,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-entry-owner-adoption".to_string()),
            Some("/tmp/entry-owner-adoption.jsonl".to_string()),
            Some("/tmp/entry-owner-adoption.input".to_string()),
            512,
        );
        let pre_authority_owner_kind = detached.effective_relay_owner_kind();
        detached.set_relay_owner_kind(RelayOwnerKind::Watcher);
        let bridge_entry_relay_owner_kind = detached.effective_relay_owner_kind();

        assert_eq!(pre_authority_owner_kind, RelayOwnerKind::None);
        assert_eq!(bridge_entry_relay_owner_kind, RelayOwnerKind::Watcher);
    }

    #[test]
    fn stream_authority_loss_relinquishes_guards_before_visible_finalization() {
        let caller = include_str!("mod.rs");
        let outcome_match = caller
            .find("match stream_loop_output.outcome")
            .expect("stream-loop outcome remains handled");
        let authority_lost = caller[outcome_match..]
            .find("StreamLoopOutcome::AuthorityLost")
            .map(|offset| outcome_match + offset)
            .expect("authority loss remains explicit");
        let relinquish = caller[authority_lost..]
            .find("completion_guard.relinquish_bridge_authority()")
            .map(|offset| authority_lost + offset)
            .expect("authority loss suppresses the stale completion broadcast");
        let defuse = caller[relinquish..]
            .find("inflight_guard.defuse()")
            .map(|offset| relinquish + offset)
            .expect("authority loss suppresses stale durable cleanup");
        let early_return = caller[defuse..]
            .find("return;")
            .map(|offset| defuse + offset)
            .expect("authority loss exits the bridge immediately");
        let finalize = caller[outcome_match..]
            .find("post_loop_finalize::run_post_loop_finalize")
            .map(|offset| outcome_match + offset)
            .expect("normal bridge still has visible finalization");

        assert!(
            authority_lost < relinquish
                && relinquish < defuse
                && defuse < early_return
                && early_return < finalize
        );
    }

    #[test]
    fn same_turn_owner_advancement_suppresses_bridge_stream_relay() {
        for owner_kind in [
            RelayOwnerKind::Watcher,
            RelayOwnerKind::StandbyRelay,
            RelayOwnerKind::SessionBoundRelay,
            RelayOwnerKind::Unknown,
        ] {
            let (watcher_owns, watcher_available, standby_owns) =
                relay_owner_flags(owner_kind, true);
            assert!(
                bridge_stream_relay_suppressed(watcher_owns, standby_owns),
                "merged owner {owner_kind:?} must suppress bridge stream delivery"
            );
            assert_eq!(watcher_available, owner_kind == RelayOwnerKind::Watcher);
        }

        let (watcher_owns, watcher_available, standby_owns) =
            relay_owner_flags(RelayOwnerKind::Watcher, false);
        assert_eq!(
            (watcher_owns, watcher_available, standby_owns),
            (true, false, false)
        );
        assert!(bridge_stream_relay_suppressed(watcher_owns, standby_owns));
    }

    #[test]
    fn cleared_watcher_owner_falls_back_to_delivery_channel() {
        assert_eq!(
            reconciled_watcher_owner_channel_id(None, 4_259_603),
            ChannelId::new(4_259_603)
        );
        assert_eq!(
            reconciled_watcher_owner_channel_id(Some(4_259_604), 4_259_603),
            ChannelId::new(4_259_604)
        );
    }
}
