use super::authority::{
    VisibleMutationAuthority, reconcile_tool_arm_locals_after_guarded_save,
    stream_tool_outcome_after_restart_authority, terminal_tool_result_transition_permission,
};
use super::*;
use crate::services::discord::turn_bridge::stream_tick::guarded_persist::visible_mutation_authority_after_guarded_save;

fn bridge_state(channel_id: u64) -> InflightTurnState {
    InflightTurnState::new(
        ProviderKind::Codex,
        channel_id,
        Some("adk-4259-r8".to_string()),
        343_742_347_365_974_026,
        77_010,
        18,
        "queued restart".to_string(),
        Some("session".to_string()),
        Some("AgentDesk-codex-r8-restart".to_string()),
        Some("/tmp/AgentDesk-codex-r8-restart.jsonl".to_string()),
        None,
        512,
    )
}

#[test]
fn queued_restart_foreign_authority_propagates_loss_while_self_delegation_continues() {
    let bridge = bridge_state(42_593_120);
    let intended = crate::services::discord::inflight::StreamRelayAuthority::from_state(&bridge);
    let mut foreign = bridge.clone();
    foreign.set_watcher_owner_channel_id(foreign.channel_id + 1);
    foreign.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);
    let foreign_authority = visible_mutation_authority_after_guarded_save(
        crate::services::discord::inflight::GuardedSaveOutcome::IdentityMismatch,
        &foreign,
        intended,
    );
    assert_eq!(foreign_authority, VisibleMutationAuthority::AuthorityLost);
    assert_eq!(
        stream_tool_outcome_after_restart_authority(Some(foreign_authority)),
        StreamToolArmOutcome::AuthorityLost,
    );

    let delegated = crate::services::discord::inflight::StreamRelayAuthority::from_state(&foreign);
    let self_delegated = visible_mutation_authority_after_guarded_save(
        crate::services::discord::inflight::GuardedSaveOutcome::Saved,
        &foreign,
        delegated,
    );
    assert_eq!(self_delegated, VisibleMutationAuthority::Suppressed);
    assert_eq!(
        stream_tool_outcome_after_restart_authority(Some(self_delegated)),
        StreamToolArmOutcome::Continue,
    );
}

#[test]
fn terminal_tool_result_fence_maps_handoff_loss_and_io_retry_fail_closed() {
    assert_eq!(
        terminal_tool_result_transition_permission(VisibleMutationAuthority::Authorized),
        Ok(true),
    );
    assert_eq!(
        terminal_tool_result_transition_permission(VisibleMutationAuthority::Suppressed),
        Ok(false),
    );
    assert_eq!(
        terminal_tool_result_transition_permission(VisibleMutationAuthority::AuthorityLost),
        Err(StreamToolArmOutcome::AuthorityLost),
    );
    assert_eq!(
        terminal_tool_result_transition_permission(VisibleMutationAuthority::Retry),
        Err(StreamToolArmOutcome::RetryExactFrame),
    );
}

#[test]
fn transient_terminal_tool_result_fence_requeues_the_exact_frame_at_front() {
    let mut pending = std::collections::VecDeque::from([StreamMessage::Text {
        content: "later frame".to_string(),
    }]);
    let frame = StreamMessage::ToolResult {
        content: "exact terminal payload".to_string(),
        is_error: true,
        tool_use_id: Some("tool-4259-r9".to_string()),
    };
    let mut retry_retained = false;
    assert!(reconcile_exact_stream_frame_after_tool_outcome(
        &mut pending,
        frame,
        StreamToolArmOutcome::RetryExactFrame,
        &mut retry_retained,
    ));
    assert!(retry_retained);
    let Some(StreamMessage::ToolResult {
        content,
        is_error,
        tool_use_id,
    }) = pending.pop_front()
    else {
        panic!("exact ToolResult must remain at queue front");
    };
    assert_eq!(content, "exact terminal payload");
    assert!(is_error);
    assert_eq!(tool_use_id.as_deref(), Some("tool-4259-r9"));
    assert!(matches!(
        pending.pop_front(),
        Some(StreamMessage::Text { content }) if content == "later frame"
    ));
}

/// #5150 item 4 kept FOUR of the six tool-state locals out of the re-seed —
/// `current_tool_line`, `prev_tool_status`, `last_tool_name`,
/// `last_tool_summary` — and the rationale recorded at `authority.rs` for those
/// four is an ORDERING claim about `tool_arms.rs`: each is a value the arm
/// DERIVES FROM THE FRAME it is handling, around its own fence, never a stale
/// read of a durable row, so re-seeding it from `on_disk` would replace fresh
/// data with older data. Pin that ordering — moving any one of these
/// assignments past its fence would silently turn the rationale into a false
/// statement.
///
/// Scope: these four DISPLAY fields only. The two behaviour flags
/// (`any_tool_used` / `has_post_tool_text`) are NOT covered by this argument and
/// are not excluded — `reconcile_tool_arm_locals_after_guarded_save` re-seeds
/// them, because leaving them behind demonstrably rewinds the durable row (see
/// `watcher_stamped_tool_flags_survive_the_fence_and_the_next_real_stream_tick`).
#[test]
fn tool_arms_derive_the_four_excluded_tool_line_locals_around_their_fences() {
    let arms = include_str!("../tool_arms.rs");
    let find = |needle: &str| {
        arms.find(needle)
            .unwrap_or_else(|| panic!("tool_arms.rs must still contain `{needle}`"))
    };

    let restart_fence = find("fence_restart_visible_mutation(");
    let result_fence = find("fence_terminal_tool_result_transition(");
    assert!(
        restart_fence < result_fence,
        "the ToolUse arm and its restart fence precede the ToolResult arm",
    );

    // `current_tool_line` / `prev_tool_status` / `last_tool_name` /
    // `last_tool_summary`: all four are assigned from the ToolUse frame before
    // the restart fence in that same arm.
    for needle in [
        "preserve_previous_tool_status(",
        "current_tool_line = Some(display.clone());",
        "last_tool_name = Some(name.clone());",
        "last_tool_summary = Some(display_summary.clone());",
    ] {
        assert!(
            find(needle) < restart_fence,
            "`{needle}` must stay ahead of the restart fence, or the loop's tool \
             projection becomes older than the durable row",
        );
    }

    // The ToolResult arm fences FIRST and only then re-derives its two display
    // fields, so a re-seed there would be overwritten anyway.
    let result_arm_tool_line = arms
        .rfind("current_tool_line = Some(detail);")
        .expect("the ToolResult arm must still re-derive current_tool_line");
    let result_arm_preserve = arms
        .rfind("preserve_previous_tool_status(")
        .expect("the ToolResult arm must still re-derive prev_tool_status");
    assert!(
        result_fence < result_arm_preserve && result_arm_preserve < result_arm_tool_line,
        "the ToolResult arm must re-derive its tool projection after its fence",
    );
}

/// The tool arms consume `current_msg_id` and `full_response` as loop locals and
/// write both back — `edit_bound_current_message` puts the anchor back into
/// `inflight_state`, and the ToolUse arm assigns `inflight_state.full_response`.
/// When a guarded save reports `Saved` after resolving against a durable row the
/// loop never staged, those locals are stale, and writing them back makes the
/// NEXT merge flush push a rewound epoch and body onto the durable row.
/// `stream_tick` avoids this with `reconcile_tick_runtime_from_inflight!`; these
/// arms had no equivalent until this reconcile was added.
#[test]
fn tool_arm_locals_follow_the_durable_row_after_a_guarded_save() {
    let mut durable = bridge_state(42_593_122);
    durable.current_msg_id = 1_534_511_625_615_311_000;
    durable.current_msg_len = 21;
    durable.full_response = "base plus shared plus watcher tail".to_string();
    durable.response_sent_offset = durable.full_response.len();
    durable.any_tool_used = true;
    durable.has_post_tool_text = true;

    // What the loop still holds: the epoch and body it staged before the save.
    let stale_anchor = MessageId::new(1_534_511_598_012_600_371);
    let mut current_msg_id = stale_anchor;
    let mut full_response = "base plus shared".to_string();
    let mut expected_current_message = (1_534_511_598_012_600_371_u64, 21_usize);
    let stale_offset = "base plus shared".len();
    let mut response_sent_offset = stale_offset;
    let mut bridge_confirmed_response_sent_offset = stale_offset;
    let mut any_tool_used = false;
    let mut has_post_tool_text = false;

    reconcile_tool_arm_locals_after_guarded_save(
        &durable,
        &mut expected_current_message,
        &mut current_msg_id,
        &mut full_response,
        &mut response_sent_offset,
        &mut bridge_confirmed_response_sent_offset,
        &mut any_tool_used,
        &mut has_post_tool_text,
    );

    assert_eq!(
        expected_current_message,
        (durable.current_msg_id, durable.current_msg_len),
        "the fence baseline must follow the durable epoch",
    );
    assert_ne!(
        current_msg_id, stale_anchor,
        "the arm must not keep editing the pre-save anchor",
    );
    assert_eq!(
        crate::services::discord::turn_bridge::current_message_anchor::durable_current_msg_id_from_detached(
            current_msg_id,
        ),
        durable.current_msg_id,
        "the arm's anchor must resolve to the durable current message",
    );
    assert_eq!(
        full_response, durable.full_response,
        "the arm must not write a rewound body back into inflight_state",
    );
    // `response_sent_offset` is one stream-loop local shared with `stream_tick`,
    // which stages it back on the next tick. Leaving it behind lets
    // `merge_stream_response_progress`'s `durable == before => return local` arm
    // rewind the durable offset and resend already-delivered text.
    assert_ne!(
        response_sent_offset, stale_offset,
        "the arm must not keep the pre-save delivery watermark",
    );
    assert_eq!(
        response_sent_offset, durable.response_sent_offset,
        "the delivery watermark must follow the durable row",
    );
    assert_eq!(
        bridge_confirmed_response_sent_offset,
        crate::services::discord::turn_bridge::retry_state::bridge_confirmed_response_sent_offset_seed(
            durable.effective_relay_owner_kind(),
            durable.response_sent_offset,
        ),
        "the bridge-confirmed watermark must be re-seeded exactly as stream_tick re-seeds it",
    );
    assert!(
        any_tool_used && has_post_tool_text,
        "the two behaviour flags must follow the durable row too, or the next \
         tick stages the pre-save value and rewinds it",
    );
}

/// The demonstrated post-save damage that moved `any_tool_used` /
/// `has_post_tool_text` OUT of the exclusion list and INTO the re-seed, driven
/// through production code at every link — no hand-wired staging:
///
/// 1. `persist_watcher_stream_progress_locked` (the real watcher writer) stamps
///    both flags `true` on the durable row. It writes them unconditionally and
///    touches no relay-owner field, so the strict fence's `authority_changed`
///    check does not trip.
/// 2. The real `fence_restart_visible_mutation` saves and returns `Saved`, and
///    the merge keeps the durable `true` because the loop's `false` equals the
///    baseline (`apply_local_change_if_durable_unchanged` needs
///    `local != before`). `inflight_state` now says `true`.
/// 3. The real `run_bridge_stream_tick` runs its `authority_preflight`, whose
///    `stage_tick_state_for_guard!` pushes the loop locals back into the row.
/// 4. Without the re-seed those locals are still `false` while the baseline is
///    `true`, so `local != before && durable == before` fires and the durable
///    row is REWOUND to `false` — structurally the same rewind this module
///    already records for `response_sent_offset`.
#[tokio::test(flavor = "current_thread")]
async fn watcher_stamped_tool_flags_survive_the_fence_and_the_next_real_stream_tick() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let _env_guard = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

    let channel = ChannelId::new(42_593_140);
    let mut inflight_state = bridge_state(channel.get());
    inflight_state.current_msg_id = 1_534_511_598_012_600_371;
    inflight_state.current_msg_len = 4;
    inflight_state.full_response = "base".to_string();
    assert!(
        !inflight_state.any_tool_used && !inflight_state.has_post_tool_text,
        "the seeded row must start where the bridge loop starts",
    );
    crate::services::discord::inflight::save_inflight_state(&inflight_state)
        .expect("seed bridge-owned row");
    let expected =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(&inflight_state);
    let mut baseline = inflight_state.clone();
    let mut expected_current_message = (
        inflight_state.current_msg_id,
        inflight_state.current_msg_len,
    );

    assert_eq!(
        crate::services::discord::inflight::persist_watcher_stream_progress_locked(
            &ProviderKind::Codex,
            channel.get(),
            Some(&expected),
            "AgentDesk-codex-r8-restart",
            crate::services::discord::inflight::WatcherStreamProgressPatch {
                current_msg_id: None,
                full_response: "base".to_string(),
                response_sent_offset: 0,
                current_tool_line: None,
                prev_tool_status: None,
                task_notification_kind: None,
                any_tool_used: true,
                has_post_tool_text: true,
                streaming_rollover_frozen_msg_ids: Vec::new(),
            },
        ),
        crate::services::discord::inflight::WatcherProgressOutcome::Saved,
    );

    let shared = crate::services::discord::make_shared_data_for_tests();
    let gateway: std::sync::Arc<dyn TurnGateway> =
        std::sync::Arc::new(crate::services::discord::gateway::HeadlessGateway);

    let mut current_msg_id = crate::services::discord::turn_bridge::current_message_anchor::detached_current_msg_id_from_durable(
        inflight_state.current_msg_id,
    );
    let mut full_response = "base".to_string();
    let mut response_sent_offset = 0usize;
    let mut confirmed_offset = 0usize;
    // The tool-arm loop locals. The bridge has not observed the tool use the
    // watcher already recorded, so both are still the turn's starting value.
    let mut any_tool_used = false;
    let mut has_post_tool_text = false;

    let authority = fence_restart_visible_mutation(StreamToolAuthorityContext {
        shared_owned: &shared,
        gateway: &gateway,
        persisted_inflight_baseline: &mut baseline,
        inflight_state: &mut inflight_state,
        stream_tick_expected_identity: &expected,
        expected_current_message: &mut expected_current_message,
        current_msg_id: &mut current_msg_id,
        full_response: &mut full_response,
        response_sent_offset: &mut response_sent_offset,
        confirmed_offset: &mut confirmed_offset,
        any_tool_used: &mut any_tool_used,
        has_post_tool_text: &mut has_post_tool_text,
    });
    assert_eq!(
        authority,
        VisibleMutationAuthority::Authorized,
        "a watcher that touches no relay-owner field must not end bridge authority",
    );
    assert!(
        inflight_state.any_tool_used && inflight_state.has_post_tool_text,
        "the guarded save must have adopted the watcher's flags into the row",
    );
    // Snapshotted, not asserted yet: the tick below re-seeds these locals from
    // the row on its own, so the DURABLE assert at the end is what carries the
    // end-to-end claim. Asserting here first would short-circuit a mutant before
    // it could show the rewind.
    let loop_locals_after_fence = (any_tool_used, has_post_tool_text);

    let mut state_dirty = false;
    let mut last_session_panel_lifecycle_refresh = tokio::time::Instant::now();
    let mut status_panel_dirty = false;
    let mut spin_idx = 0usize;
    let mut last_status_panel_edit = tokio::time::Instant::now();
    let mut last_status_edit = tokio::time::Instant::now();
    let mut status_panel_msg_id: Option<MessageId> = None;
    let mut last_status_panel_text = String::new();
    let mut watcher_owns_assistant_relay = false;
    let mut watcher_relay_available_for_turn = false;
    let mut standby_relay_owns_output = false;
    let mut watcher_owner_channel_id = ChannelId::new(1);
    let mut streaming_rollover_frozen_msg_ids: Vec<MessageId> = Vec::new();
    let mut pending_current_message_candidate: Option<MessageId> = None;
    let mut bridge_created_response_placeholder_msg_id: Option<MessageId> = None;
    let mut last_edit_text = String::new();
    let mut first_answer_relayed = true;
    let mut current_tool_line: Option<String> = None;
    let mut prev_tool_status: Option<String> = None;
    let mut last_tool_name: Option<String> = None;
    let mut last_tool_summary: Option<String> = None;
    let mut tmux_last_offset: Option<u64> = None;
    let mut bridge_spans = crate::services::discord::turn_bridge::bridge_latency_spans::BridgeLatencySpans::starting_at(
        std::time::Instant::now(),
    );
    let mut status_panel_generation = 0u64;
    let mut pending_long_running_open_after_state_save = None;
    let mut pending_long_running_retarget_after_state_save = None;
    let mut long_running_placeholder_active = None;
    let mut last_adk_heartbeat = std::time::Instant::now();
    let mut last_inflight_long_run_heartbeat = std::time::Instant::now();

    let tick_outcome = crate::services::discord::turn_bridge::stream_tick::run_bridge_stream_tick(
        crate::services::discord::turn_bridge::stream_tick::BridgeStreamTickContext {
            shared_owned: shared.clone(),
            gateway: gateway.clone(),
            channel_id: channel,
            provider: &ProviderKind::Codex,
            turn_id: "tool-flag-reseed-turn",
            expected_identity: &expected,
            status_interval: std::time::Duration::from_secs(3_600),
            single_message_panel_footer_mode: false,
            footer_owner:
                crate::services::discord::footer_view_reconciler::CompletionFooterOwner::new(
                    77_010, 0,
                ),
            status_panel_started_at: 0,
            done: false,
            dispatch_id: None,
            adk_session_key: None,
            adk_session_name: None,
            adk_session_info: None,
            adk_cwd: None,
            role_binding: None,
            spinner: &["|"],
            live_long_run_heartbeat_interval: std::time::Duration::from_secs(3_600),
        },
        crate::services::discord::turn_bridge::stream_tick::BridgeStreamTickState {
            state_dirty: &mut state_dirty,
            last_session_panel_lifecycle_refresh: &mut last_session_panel_lifecycle_refresh,
            status_panel_dirty: &mut status_panel_dirty,
            spin_idx: &mut spin_idx,
            last_status_panel_edit: &mut last_status_panel_edit,
            last_status_edit: &mut last_status_edit,
            status_panel_msg_id: &mut status_panel_msg_id,
            last_status_panel_text: &mut last_status_panel_text,
            watcher_owns_assistant_relay: &mut watcher_owns_assistant_relay,
            watcher_relay_available_for_turn: &mut watcher_relay_available_for_turn,
            standby_relay_owns_output: &mut standby_relay_owns_output,
            watcher_owner_channel_id: &mut watcher_owner_channel_id,
            full_response: &mut full_response,
            response_sent_offset: &mut response_sent_offset,
            bridge_confirmed_response_sent_offset: &mut confirmed_offset,
            streaming_rollover_frozen_msg_ids: &mut streaming_rollover_frozen_msg_ids,
            current_msg_id: &mut current_msg_id,
            expected_current_message: &mut expected_current_message,
            pending_current_message_candidate: &mut pending_current_message_candidate,
            bridge_created_response_placeholder_msg_id:
                &mut bridge_created_response_placeholder_msg_id,
            last_edit_text: &mut last_edit_text,
            first_answer_relayed: &mut first_answer_relayed,
            current_tool_line: &mut current_tool_line,
            prev_tool_status: &mut prev_tool_status,
            last_tool_name: &mut last_tool_name,
            last_tool_summary: &mut last_tool_summary,
            any_tool_used: &mut any_tool_used,
            has_post_tool_text: &mut has_post_tool_text,
            tmux_last_offset: &mut tmux_last_offset,
            persisted_inflight_baseline: &mut baseline,
            inflight_state: &mut inflight_state,
            bridge_spans: &mut bridge_spans,
            status_panel_generation: &mut status_panel_generation,
            pending_long_running_open_after_state_save:
                &mut pending_long_running_open_after_state_save,
            pending_long_running_retarget_after_state_save:
                &mut pending_long_running_retarget_after_state_save,
            long_running_placeholder_active: &mut long_running_placeholder_active,
            last_adk_heartbeat: &mut last_adk_heartbeat,
            last_inflight_long_run_heartbeat: &mut last_inflight_long_run_heartbeat,
        },
    )
    .await;
    assert_eq!(
        tick_outcome,
        crate::services::discord::turn_bridge::stream_tick::StreamTickOutcome::Continue,
        "the tick must reach its staging path rather than bail out early",
    );

    let durable = crate::services::discord::inflight::load_inflight_state(
        &ProviderKind::Codex,
        channel.get(),
    )
    .expect("durable row survives the tick");
    assert!(
        durable.any_tool_used && durable.has_post_tool_text,
        "the next real tick must not rewind the watcher's tool flags on the durable row",
    );
    assert_eq!(
        loop_locals_after_fence,
        (true, true),
        "and the mechanism that prevents the rewind is the fence's re-seed",
    );
}
