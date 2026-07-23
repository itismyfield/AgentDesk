use super::{
    FreshIdleFinalizeDecision, RelaySlotGuard, RestoredWatcherTurn, SessionBoundRelayAckOutcome,
    TuiCompletionGateOutcome, WATCHER_RELAY_EMISSION_TIMEOUT, WatcherTerminalKind,
    WatcherTerminalRewindSeedInput, build_watcher_streaming_edit_text,
    committed_anchor_cleanup_is_stale_for_newer_turn,
    discard_restored_response_seed_before_no_inflight_terminal_relay,
    legacy_wrapper_prompt_candidates_from_pane, local_cmd_no_output,
    mark_watcher_terminal_delivery_committed, merge_persisted_rollover_frozen_msg_ids,
    reacquire_watcher_inflight_for_active_stream, refresh_watcher_turn_identity,
    should_probe_tmux_liveness, terminal_relay_decision, watcher_batch_contains_assistant_event,
    watcher_batch_contains_relayable_response,
    watcher_fallback_edit_failure_can_delete_original_placeholder,
    watcher_fresh_idle_finalize_decision, watcher_fresh_idle_session_bound_retry_plan,
    watcher_handle_no_dispatch_post_work_idle_body, watcher_inflight_absence_is_abandonment,
    watcher_output_progressed_recently, watcher_relay_emission_timeout_failure_plan,
    watcher_relay_emission_with_timeout, watcher_should_clear_stale_terminal_message_ids,
    watcher_should_delete_suppressed_placeholder,
    watcher_should_direct_send_after_session_bound_ack,
    watcher_should_reclaim_orphan_turn_placeholder,
    watcher_stream_seed_after_restored_seed_discard, watcher_terminal_commit_side_effects_for_test,
    watcher_terminal_edit_consumes_placeholder, watcher_terminal_response_for_direct_send,
    watcher_terminal_rewind_seed, watcher_wait_inflight_retry_plan,
};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::InflightTurnState;
use crate::services::discord::formatting::ReplaceLongMessageOutcome;
use crate::services::discord::inflight::{InflightTurnIdentity, RelayOwnerKind, TurnSource};
use crate::services::discord::replace_outcome_policy::{
    WatcherRewindAttemptDisposition, WatcherSendFailureClass,
    classify_watcher_send_failure_message, watcher_full_send_failure_retry_plan,
    watcher_rewind_attempt_disposition, watcher_send_failure_retry_plan,
};
use crate::services::discord::{
    mailbox_enqueue_intervention, mailbox_snapshot, mailbox_take_next_soft_intervention,
    mailbox_try_start_turn,
};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{Intervention, InterventionMode};
use serenity::all::{ChannelId, Http, MessageId, UserId};

struct AgentdeskRootGuard(Option<std::ffi::OsString>);

impl AgentdeskRootGuard {
    fn set(path: &std::path::Path) -> Self {
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
        Self(previous)
    }
}

impl Drop for AgentdeskRootGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}

#[test]
fn persisted_bridge_frozen_prefix_ids_merge_into_watcher_cleanup_list() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let tmux_session_name = "AgentDesk-claude-adk-cc";
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        42,
        Some("adk-cc".to_string()),
        7,
        1001,
        1002,
        "prompt".to_string(),
        Some("session".to_string()),
        Some(tmux_session_name.to_string()),
        Some("/tmp/out.jsonl".to_string()),
        None,
        64,
    );
    state.streaming_rollover_frozen_msg_ids = vec![10, 11];
    let mut local = vec![MessageId::new(10)];

    merge_persisted_rollover_frozen_msg_ids(&mut local, Some(&state), tmux_session_name);

    assert_eq!(local, vec![MessageId::new(10), MessageId::new(11)]);
}

#[test]
fn terminal_readiness_stale_newer_frozen_prefix_ids_do_not_merge() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let tmux_session_name = "AgentDesk-claude-adk-cc";
    let current_offset = 100;
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        42,
        Some("adk-cc".to_string()),
        7,
        1001,
        1002,
        "prompt".to_string(),
        Some("session".to_string()),
        Some(tmux_session_name.to_string()),
        Some("/tmp/out.jsonl".to_string()),
        None,
        150,
    );
    state.turn_start_offset = Some(150);
    state.streaming_rollover_frozen_msg_ids = vec![10, 11];
    let should_adopt = true;
    let stale_newer = committed_anchor_cleanup_is_stale_for_newer_turn(
        Some(&state),
        None,
        tmux_session_name,
        current_offset,
    );
    let mut local = Vec::new();

    if should_adopt && !stale_newer {
        merge_persisted_rollover_frozen_msg_ids(&mut local, Some(&state), tmux_session_name);
    }

    assert!(stale_newer);
    assert!(
        local.is_empty(),
        "stale newer frozen prefix ids must not enter the old turn cleanup list"
    );
}

#[test]
fn watcher_terminal_delivery_commit_mirrors_bridge_inflight_fields() {
    // Serialize on the PROCESS-WIDE `AGENTDESK_ROOT_DIR` lock (shared with
    // standby_relay / turn_finalizer / config tests) so a concurrent
    // root-mutating test cannot stomp our tempdir env. A module-local mutex
    // only serialized within this module and let the leak through.
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(987_2999);
    let tmux_session_name = "AgentDesk-claude-adk-cc";
    let mut state = InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        Some("adk-cc".to_string()),
        42,
        1001,
        1002,
        "prompt".to_string(),
        Some("session-2999".to_string()),
        Some(tmux_session_name.to_string()),
        Some("/tmp/agentdesk-2999-output.jsonl".to_string()),
        None,
        64,
    );
    state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    state.turn_start_offset = Some(64);
    crate::services::discord::inflight::save_inflight_state(&state).expect("save inflight");
    let identity = crate::services::discord::inflight::InflightTurnIdentity::from_state(&state);

    assert!(mark_watcher_terminal_delivery_committed(
        &provider,
        channel_id,
        tmux_session_name,
        Some(&identity),
        "delivered response",
        64,
        Some(7),
        128,
    ));

    let persisted =
        crate::services::discord::inflight::load_inflight_state(&provider, channel_id.get())
            .expect("load inflight");
    assert!(persisted.terminal_delivery_committed);
    assert_eq!(persisted.full_response, "delivered response");
    assert_eq!(persisted.response_sent_offset, "delivered response".len());
    assert_eq!(persisted.last_offset, 128);
    assert_eq!(persisted.last_watcher_relayed_offset, Some(64));
    assert_eq!(persisted.last_watcher_relayed_generation_mtime_ns, Some(7));
}

// #3169 P1: a self-paced loop turn (`user_msg_id == 0`) must now set
// `terminal_delivery_committed` on a fully-anchored completion. The original
// guard rejected every `user_msg_id == 0` turn, so loop sessions never got the
// architectural signal the #3126 stall-watchdog guard relies on (death #1).
#[test]
fn watcher_terminal_delivery_commit_marks_loop_turn_with_zero_user_msg_id() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(987_3169);
    let tmux_session_name = "AgentDesk-claude-adk-cc";
    let mut state = InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        Some("adk-cc".to_string()),
        42,
        0, // user_msg_id == 0 -> self-paced loop turn (no anchored Discord message)
        1002,
        "loop prompt".to_string(),
        Some("session-3169".to_string()),
        Some(tmux_session_name.to_string()),
        Some("/tmp/agentdesk-3169-output.jsonl".to_string()),
        None,
        64,
    );
    state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    state.turn_start_offset = Some(64);
    crate::services::discord::inflight::save_inflight_state(&state).expect("save inflight");
    let identity = crate::services::discord::inflight::InflightTurnIdentity::from_state(&state);
    assert_eq!(identity.user_msg_id, 0, "fixture is a loop turn");

    assert!(
        mark_watcher_terminal_delivery_committed(
            &provider,
            channel_id,
            tmux_session_name,
            Some(&identity),
            "loop delivered response",
            64,
            Some(7),
            128,
        ),
        "a fully-anchored loop turn (user_msg_id == 0) must commit"
    );

    let persisted =
        crate::services::discord::inflight::load_inflight_state(&provider, channel_id.get())
            .expect("load inflight");
    assert!(
        persisted.terminal_delivery_committed,
        "loop turn must set terminal_delivery_committed for the #3126 guard"
    );

    // A loop turn whose frame-carried `turn_start_offset` is missing cannot be
    // safely disambiguated from a sibling same-second loop turn, so it is still
    // skipped (NOT a blanket relaxation).
    crate::services::discord::inflight::clear_inflight_state(&provider, channel_id.get());
    crate::services::discord::inflight::save_inflight_state(&state).expect("re-save inflight");
    let mut unanchored_identity = identity.clone();
    unanchored_identity.turn_start_offset = None;
    assert!(
        !mark_watcher_terminal_delivery_committed(
            &provider,
            channel_id,
            tmux_session_name,
            Some(&unanchored_identity),
            "loop delivered response",
            64,
            Some(7),
            128,
        ),
        "a loop turn without a known turn_start_offset must NOT commit"
    );
}

// #3107 (CHANGE 3): a missing inflight is abandonment ONLY when the pane is
// not actively streaming. An actively-streaming pane is a live turn that
// merely lost its inflight, so its status panel must be preserved; a
// ready-for-input / idle pane is a genuine orphan and is still reclaimed.
#[test]
fn watcher_inflight_absence_is_abandonment_requires_idle_pane() {
    assert!(
        !watcher_inflight_absence_is_abandonment(true),
        "actively-streaming pane (busy) -> live turn -> NOT abandoned (panel preserved)"
    );
    assert!(
        watcher_inflight_absence_is_abandonment(false),
        "ready-for-input/idle pane -> real orphan -> still reclaimed"
    );
}

// #3107 codex re-review (P2#3): the abandonment progress gate. A live turn
// whose session JSONL was written recently counts as "progressing"; a
// finished/stopped turn whose pane shows a STALE lingering frame (no recent
// output) does not — so a frozen spinner can no longer pin the panel.
#[test]
fn watcher_output_progress_gate_distinguishes_fresh_from_stale_output() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let fresh = tmp.path().join("fresh.jsonl");
    std::fs::write(&fresh, "{\"type\":\"assistant\"}\n").expect("write fresh output");
    assert!(
        watcher_output_progressed_recently(fresh.to_str().unwrap()),
        "a just-written output file must read as recent progress"
    );

    // A stale file (mtime well past the window) reads as no progress, so a
    // finished turn with a lingering busy frame is still declared abandoned.
    let stale = tmp.path().join("stale.jsonl");
    let stale_file = std::fs::File::create(&stale).expect("create stale output");
    stale_file
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(120))
        .expect("backdate stale output mtime");
    assert!(
        !watcher_output_progressed_recently(stale.to_str().unwrap()),
        "a stale output file (frozen turn) must NOT read as progress -> reclaimable"
    );

    // A missing output file cannot prove progress.
    assert!(
        !watcher_output_progressed_recently(tmp.path().join("missing.jsonl").to_str().unwrap()),
        "a missing output file must read as no progress"
    );

    // #3107 codex re-review (P2, F4): a FUTURE mtime (clock drift / NTP jump /
    // an external write with a skewed clock) makes `elapsed()` return Err. The
    // safe direction is to PRESERVE a live turn's panel, so an unresolvable
    // elapsed must read as "in progress" — NOT as reclaimable.
    let future = tmp.path().join("future.jsonl");
    let future_file = std::fs::File::create(&future).expect("create future output");
    future_file
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3_600))
        .expect("post-date future output mtime");
    assert!(
        watcher_output_progressed_recently(future.to_str().unwrap()),
        "a future mtime (clock skew) must bias to in-progress so a live turn's panel is preserved"
    );
}

// #3107 (CHANGE 2): when the pane is actively streaming but no inflight
// exists, the watcher re-establishes a minimal Watcher-owned inflight so
// subsequent edits relay and the terminal ack has a target. The re-acquire
// is idempotent — it must never clobber an already-present inflight.
#[test]
fn reacquire_watcher_inflight_registers_watcher_owned_state_and_is_idempotent() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(987_3107);
    let tmux_session_name = "AgentDesk-claude-adk-cc";
    let output_path = "/tmp/agentdesk-3107-output.jsonl";
    let panel_id = MessageId::new(5_555);
    let placeholder_id = MessageId::new(6_666);

    // No inflight yet -> a fresh active-stream observation re-acquires one.
    assert!(
        crate::services::discord::inflight::load_inflight_state(&provider, channel_id.get())
            .is_none()
    );
    assert!(reacquire_watcher_inflight_for_active_stream(
        &provider,
        channel_id,
        tmux_session_name,
        output_path,
        128,
        Some(panel_id),
        Some(placeholder_id),
        // #3107 P2#3: a recoverable hourglass anchor is preserved.
        Some(7_777),
    ));

    let restored =
        crate::services::discord::inflight::load_inflight_state(&provider, channel_id.get())
            .expect("inflight re-acquired");
    assert_eq!(
        restored.effective_relay_owner_kind(),
        crate::services::discord::inflight::RelayOwnerKind::Watcher,
        "re-acquired inflight must be watcher-owned"
    );
    assert_eq!(
        restored.tmux_session_name.as_deref(),
        Some(tmux_session_name)
    );
    assert_eq!(restored.output_path.as_deref(), Some(output_path));
    assert_eq!(restored.turn_start_offset, Some(128));
    // The still-present placeholder is pinned as the streaming-edit target
    // (kills frame_ack MissingTarget); the status panel id is preserved too.
    assert_eq!(restored.current_msg_id, placeholder_id.get());
    assert_eq!(restored.status_message_id, Some(panel_id.get()));
    // #3107 P2#3: the #3099 hourglass anchor is preserved when recoverable.
    assert_eq!(restored.injected_prompt_message_id, Some(7_777));

    // Idempotent: a second observation must NOT clobber the existing row.
    assert!(
        !reacquire_watcher_inflight_for_active_stream(
            &provider,
            channel_id,
            tmux_session_name,
            output_path,
            256,
            Some(panel_id),
            Some(placeholder_id),
            None,
        ),
        "re-acquire must be a no-op when an inflight already exists"
    );
    let unchanged =
        crate::services::discord::inflight::load_inflight_state(&provider, channel_id.get())
            .expect("inflight still present");
    assert_eq!(
        unchanged.turn_start_offset,
        Some(128),
        "existing inflight offset must be left intact"
    );
}

// #3107 codex re-review (P1): the re-acquire must NOT clobber a REAL inflight
// that the intake path created on the same (provider, channel) between the
// (now removed) preflight check and the write. With the atomic
// compare-and-set save the concurrent intake inflight always wins and the
// re-acquire degrades to a no-op.
#[test]
fn reacquire_watcher_inflight_does_not_clobber_concurrent_intake_inflight() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(987_31071);
    let tmux_session_name = "AgentDesk-claude-adk-cc";
    let output_path = "/tmp/agentdesk-3107-cas-output.jsonl";

    // Simulate the intake path having already created a REAL user-authored
    // inflight (non-zero user_msg_id) for a brand new turn on this channel.
    let real = InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        Some("adk-cc".to_string()),
        777,    // request_owner_user_id
        12_345, // user_msg_id — a REAL Discord user turn
        54_321, // current_msg_id
        "real turn".to_string(),
        None,
        Some(tmux_session_name.to_string()),
        Some(output_path.to_string()),
        None,
        999,
    );
    crate::services::discord::inflight::save_inflight_state(&real)
        .expect("seed real intake inflight");

    // The watcher-owned re-acquire must see the row and no-op (intake wins).
    assert!(
        !reacquire_watcher_inflight_for_active_stream(
            &provider,
            channel_id,
            tmux_session_name,
            output_path,
            128,
            Some(MessageId::new(5_555)),
            Some(MessageId::new(6_666)),
            None,
        ),
        "re-acquire must no-op when a concurrent intake inflight exists"
    );

    let persisted =
        crate::services::discord::inflight::load_inflight_state(&provider, channel_id.get())
            .expect("intake inflight must survive");
    assert_eq!(
        persisted.user_msg_id, 12_345,
        "the legitimate intake turn must NOT be overwritten by the synthetic re-acquire"
    );
    assert_eq!(persisted.current_msg_id, 54_321);
}

// SAFETY (await_holding_lock): see the inline comment — the process-wide
// env-dir Mutex is held across awaits to serialize env-mutating tests, which
// is sound on the current-thread test runtime. Test-only.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn terminal_delivery_timeout_cleanup_releases_mailbox_and_preserves_followup_queue() {
    // Serialize on the PROCESS-WIDE `AGENTDESK_ROOT_DIR` lock (shared with
    // standby_relay / turn_finalizer / config tests). The guard is held
    // across awaits, which is sound because `#[tokio::test]` runs on a
    // current-thread runtime (the future is never moved across threads).
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(987_3000);
    let tmux_session_name = "AgentDesk-claude-adk-cc";
    assert!(
        mailbox_try_start_turn(
            &shared,
            channel_id,
            std::sync::Arc::new(CancelToken::new()),
            UserId::new(42),
            MessageId::new(1001),
        )
        .await
    );

    let enqueue = mailbox_enqueue_intervention(
        &shared,
        &provider,
        channel_id,
        Intervention {
            author_id: UserId::new(99),
            author_is_bot: false,
            message_id: MessageId::new(2001),
            queued_generation: crate::services::discord::runtime_store::process_generation(),
            source_message_ids: vec![MessageId::new(2001)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: "queued follow-up".to_string(),
            mode: InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        },
    )
    .await;
    assert!(enqueue.enqueued);

    let mut state = InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        Some("adk-cc".to_string()),
        42,
        1001,
        1002,
        "prompt".to_string(),
        Some("session-2999".to_string()),
        Some(tmux_session_name.to_string()),
        Some("/tmp/agentdesk-2999-output.jsonl".to_string()),
        None,
        64,
    );
    state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    state.turn_start_offset = Some(64);
    crate::services::discord::inflight::save_inflight_state(&state).expect("save inflight");
    let identity = crate::services::discord::inflight::InflightTurnIdentity::from_state(&state);
    assert!(mark_watcher_terminal_delivery_committed(
        &provider,
        channel_id,
        tmux_session_name,
        Some(&identity),
        "delivered response",
        64,
        Some(7),
        128,
    ));

    let side_effects = watcher_terminal_commit_side_effects_for_test(
        true,
        TuiCompletionGateOutcome::BusyObserved,
        true,
    );
    assert!(side_effects.clear_inflight);
    assert!(side_effects.finish_restored_turn);
    crate::services::discord::inflight::clear_inflight_state(&provider, channel_id.get());
    super::finish_restored_watcher_active_turn(
        &shared,
        &provider,
        channel_id,
        state.user_msg_id,
        true,  // finish_mailbox_on_completion (restore semantics)
        false, // normal_completion (#3016: this path is restore-gated, not the decoupled normal-completion arm)
        false, // kickoff_queue
        None,  // claim_snapshot (#3350 r1-1: not a synthetic-claim path)
        "terminal_delivery_timeout_cleanup_test",
    )
    .await;

    assert!(
        crate::services::discord::inflight::load_inflight_state(&provider, channel_id.get())
            .is_none()
    );
    let snapshot = mailbox_snapshot(&shared, channel_id).await;
    assert!(snapshot.cancel_token.is_none());
    assert_eq!(snapshot.intervention_queue.len(), 1);
    let next = mailbox_take_next_soft_intervention(&shared, &provider, channel_id)
        .await
        .into_intervention()
        .map(|(intervention, _, _)| intervention.text);
    assert_eq!(next.as_deref(), Some("queued follow-up"));
}

// #3419 C1: the turn-watchdog TIMEOUT path now routes through the SAME
// single-authority entry (`finish_restored_watcher_active_turn` →
// `submit_terminal` → `do_finalize`) with the EXACT argument shape the
// tmux_watcher.rs timeout guard uses: `finish_mailbox_on_completion = true`,
// `normal_completion = false`, `kickoff_queue = true`. This proves the wedge
// fix: the mailbox cancel_token IS released (so the soft-queue advance gate
// opens) and a queued follow-up survives for the kicked-off drain. Pre-#3419
// the timeout fell through WITHOUT any finalize, so the token leaked and the
// queue wedged forever. The double-submit asserts the once-gate
// (Pending→Finalizing→Finalized) makes a later normal-completion finalize an
// idempotent no-op — the timeout path cannot collide with the normal path.
#[test]
fn watchdog_timeout_path_releases_mailbox_via_finalizer_and_does_not_double_finalize() {
    // The serialization guard protects the PROCESS-WIDE `AGENTDESK_ROOT_DIR`
    // env (set via `AgentdeskRootGuard`) that the async inflight/mailbox
    // helpers read while they run, so it must be held for the whole test —
    // including the async work. Driving the async body on a current-thread
    // runtime via `block_on` keeps the std guard inside a synchronous frame so
    // it never crosses an `.await` suspension point (no `await_holding_lock`),
    // while the serialization + env stability are fully preserved.
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    runtime.block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(987_3419);
        let tmux_session_name = "AgentDesk-claude-adk-cc-9873419";
        shared
            .tmux_watchers
            .insert(channel_id, test_watcher_handle(tmux_session_name));

        // Live active turn whose long run will "time out" (cancel token held).
        assert!(
            mailbox_try_start_turn(
                &shared,
                channel_id,
                std::sync::Arc::new(CancelToken::new()),
                UserId::new(42),
                MessageId::new(5001),
            )
            .await
        );
        // A queued follow-up that the wedge would have stranded forever.
        let enqueue = mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel_id,
            Intervention {
                author_id: UserId::new(99),
                author_is_bot: false,
                message_id: MessageId::new(6001),
                queued_generation: crate::services::discord::runtime_store::process_generation(),
                source_message_ids: vec![MessageId::new(6001)],
                source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
                text: "post-timeout follow-up".to_string(),
                mode: InterventionMode::Soft,
                created_at: std::time::Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
                voice_announcement: None,
            },
        )
        .await;
        assert!(enqueue.enqueued);

        // The guard clears inflight inline before calling the helper; mirror that.
        crate::services::discord::inflight::clear_inflight_state(&provider, channel_id.get());
        let drove = super::finish_restored_watcher_active_turn(
            &shared,
            &provider,
            channel_id,
            5001,  // real user_msg_id captured pre-clear
            true,  // finish_mailbox_on_completion — release the watcher-owned token
            false, // normal_completion — a watchdog timeout is NOT a confirmed completion
            true,  // kickoff_queue — admit the next queued turn
            None,
            "watcher turn watchdog timeout (#3419)",
        )
        .await;
        assert!(
            drove,
            "timeout path must drive the finalizer (finish_mailbox_on_completion gate)"
        );

        // The wedge fix: the mailbox token is released so the advance gate opens.
        let snapshot = mailbox_snapshot(&shared, channel_id).await;
        assert!(
            snapshot.cancel_token.is_none(),
            "#3419: timeout finalize must release the mailbox cancel_token"
        );
        // The queued follow-up survived the finalize and is now drainable (the
        // kickoff would dispatch it; here we assert it is admittable, the property
        // the pre-#3419 wedge denied).
        assert_eq!(snapshot.intervention_queue.len(), 1);
        let next = mailbox_take_next_soft_intervention(&shared, &provider, channel_id)
            .await
            .into_intervention()
            .map(|(intervention, _, _)| intervention.text);
        assert_eq!(next.as_deref(), Some("post-timeout follow-up"));

        // Once-gate: a subsequent NORMAL-completion submit for the SAME turn must
        // be an idempotent no-op — the timeout finalize already won, so the normal
        // path cannot collide with it (single-authority preserved). The helper
        // always returns `true` past its early-return gate (it does not surface
        // `AlreadyFinalized`), so we assert the OBSERVABLE once-gate property: the
        // mailbox stays released and the second finalize neither re-arms a token nor
        // underflows the active counter / panics. A start of a BRAND-NEW turn must
        // still succeed afterwards (the channel is not wedged), confirming the
        // second submit was a clean no-op rather than a corrupting double-finalize.
        super::finish_restored_watcher_active_turn(
            &shared,
            &provider,
            channel_id,
            5001,
            false,
            true,
            false,
            None,
            "watcher fresh ready-for-input idle (structural/pane-idle completion)",
        )
        .await;
        let snapshot_after = mailbox_snapshot(&shared, channel_id).await;
        assert!(
            snapshot_after.cancel_token.is_none(),
            "#3419: a second finalize must not re-arm the mailbox token (idempotent once-gate)"
        );
        // The channel is healthy: a brand-new turn can start (no wedge, no
        // counter corruption from the double submit).
        assert!(
            mailbox_try_start_turn(
                &shared,
                channel_id,
                std::sync::Arc::new(CancelToken::new()),
                UserId::new(43),
                MessageId::new(7001),
            )
            .await,
            "#3419: channel must accept a new turn after the timeout finalize + idempotent re-submit"
        );
        });
}

// #3419 R3 (codex HIGH): the turn-stealing regression. A STALE turn A times
// out while a NEWER turn B is the LIVE mailbox active-turn (token) owner. The
// production timeout guard now consults `watcher_timeout_finalize_decision`
// with A's PINNED `startup_inflight_snapshot` and the mailbox's CURRENT
// `active_user_message_id` (= B): they MISMATCH, so the decision is `Skip` and
// the guard NEVER calls the finalizer — B's cancel_token and inflight survive.
// Then the POSITIVE case: the watcher's OWN pinned turn (still the mailbox
// token holder) times out → `Finalize` releases its token + drains the queue.
//
// This is the real-state regression: we start B in the mailbox (token live)
// and assert via the production decision that A's timeout does not steal B.
#[test]
fn timeout_finalize_does_not_steal_a_newer_live_turn_but_drains_its_own() {
    // Hold the process-wide root/env serialization guard across the whole test
    // (the async helpers read `AGENTDESK_ROOT_DIR` as they run); drive the async
    // body via a current-thread `block_on` so the std guard never crosses an
    // `.await` (no `await_holding_lock`) while serialization is preserved.
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    runtime.block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(987_3420);
        let tmux_session_name = "AgentDesk-claude-adk-cc-9873420";
        shared
            .tmux_watchers
            .insert(channel_id, test_watcher_handle(tmux_session_name));

        // Turn A: the turn this watcher instance ATTACHED to (its pinned
        // `startup_inflight_snapshot`). It started early in the JSONL transcript.
        let pinned_a = fresh_idle_inflight(
            provider.clone(),
            channel_id.get(),
            tmux_session_name,
            3001,
            10,
        );

        // Turn B took over the session DURING A's long timeout window: B is the
        // live mailbox active turn (it holds the token, user_msg_id 4002).
        let token_b = std::sync::Arc::new(CancelToken::new());
        assert!(
            mailbox_try_start_turn(
                &shared,
                channel_id,
                token_b.clone(),
                UserId::new(42),
                MessageId::new(4002),
            )
            .await
        );

        // The PRODUCTION decision: A's pinned snapshot vs the mailbox's CURRENT
        // active-turn id (= B). Mismatch → Skip.
        let mailbox_active = mailbox_snapshot(&shared, channel_id)
            .await
            .active_user_message_id
            .map(MessageId::get);
        let decision = super::watcher_timeout_finalize_decision(
            Some(&pinned_a),
            mailbox_active,
            tmux_session_name,
        );
        assert_eq!(
            decision,
            super::TimeoutFinalizeDecision::Skip {
                pinned_user_msg_id: 3001
            },
            "A's timeout must SKIP when B holds the mailbox token — no finalize, no steal"
        );

        // Skip ⇒ the guard does not finalize. Assert B survives: token still held.
        assert!(
            mailbox_snapshot(&shared, channel_id)
                .await
                .cancel_token
                .is_some(),
            "#3419 turn-steal: B's cancel_token MUST survive A's timeout"
        );

        // POSITIVE case: B's OWN watcher times out while B still holds the token.
        // The pinned id equals the mailbox active id → Finalize with B's real id.
        let on_disk_b = fresh_idle_inflight(
            provider.clone(),
            channel_id.get(),
            tmux_session_name,
            4002,
            900,
        );
        let finalize_decision = super::watcher_timeout_finalize_decision(
            Some(&on_disk_b),
            Some(4002),
            tmux_session_name,
        );
        assert_eq!(
            finalize_decision,
            super::TimeoutFinalizeDecision::Finalize { user_msg_id: 4002 },
            "the watcher's OWN pinned turn (still the mailbox token holder) finalizes on timeout"
        );
        // Drive the same finalize the production Finalize arm runs and assert the
        // wedge fix: B's token is released and the queue can drain.
        let drove = super::finish_restored_watcher_active_turn(
            &shared,
            &provider,
            channel_id,
            4002,
            true,
            false,
            true,
            None,
            "watcher turn watchdog timeout (#3419)",
        )
        .await;
        assert!(drove, "own-turn timeout drives the finalizer");
        assert!(
            mailbox_snapshot(&shared, channel_id)
                .await
                .cancel_token
                .is_none(),
            "#3419: own-turn timeout finalize releases the mailbox token (wedge fix)"
        );
    });
}

// #3419 R3 (codex HIGH — the re-acquire id-0 wedge drain): turn A is the live
// mailbox token holder, but `reacquire_watcher_inflight_for_active_stream`
// minted a `user_msg_id == 0` synthetic inflight on disk (A lost its row
// mid-stream). R2 keyed the decision on the on-disk row → pinned A (nonzero)
// mismatched id-0 → Skip → A's token stayed WEDGED. R3 keys on the mailbox
// active id (still A) → Finalize: A drains (token released, queued follow-up
// admitted) and a DIFFERENT turn is never stolen.
#[test]
fn timeout_finalize_drains_reacquired_id_zero_wedge_for_live_pinned_turn() {
    // Hold the process-wide root/env serialization guard across the whole test
    // (the async helpers read `AGENTDESK_ROOT_DIR` as they run); drive the async
    // body via a current-thread `block_on` so the std guard never crosses an
    // `.await` (no `await_holding_lock`) while serialization is preserved.
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    runtime.block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(987_3422);
        let tmux_session_name = "AgentDesk-claude-adk-cc-9873422";
        shared
            .tmux_watchers
            .insert(channel_id, test_watcher_handle(tmux_session_name));

        // Turn A: the real restored Discord turn this watcher attached to. Its
        // mailbox token is LIVE (started for real, never finished).
        let pinned_a = fresh_idle_inflight(
            provider.clone(),
            channel_id.get(),
            tmux_session_name,
            3500,
            10,
        );
        let token_a = std::sync::Arc::new(CancelToken::new());
        assert!(
            mailbox_try_start_turn(
                &shared,
                channel_id,
                token_a.clone(),
                UserId::new(7),
                MessageId::new(3500),
            )
            .await
        );
        // A follow-up message is queued behind A's live turn (the queue the wedge
        // would trap).
        let enqueue = mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel_id,
            Intervention {
                author_id: UserId::new(99),
                author_is_bot: false,
                message_id: MessageId::new(3600),
                queued_generation: crate::services::discord::runtime_store::process_generation(),
                source_message_ids: vec![MessageId::new(3600)],
                source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
                text: "follow-up while A runs".to_string(),
                mode: InterventionMode::Soft,
                created_at: std::time::Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
                voice_announcement: None,
            },
        )
        .await;
        assert!(enqueue.enqueued, "follow-up queues behind A's live turn");

        // The re-acquire path minted an id-0 synthetic inflight (A's real row was
        // cleared mid-stream); A's mailbox token is still live.
        let reacquired_id0 = fresh_idle_inflight(
            provider.clone(),
            channel_id.get(),
            tmux_session_name,
            0,
            900,
        );
        crate::services::discord::inflight::save_inflight_state(&reacquired_id0)
            .expect("persist re-acquired id-0 inflight");

        // The PRODUCTION decision keys on the mailbox active id (still A == 3500),
        // NOT the id-0 on-disk row. R2 would have read id-0 here and Skipped.
        let mailbox_active = mailbox_snapshot(&shared, channel_id)
            .await
            .active_user_message_id
            .map(MessageId::get);
        assert_eq!(
            mailbox_active,
            Some(3500),
            "A still holds the mailbox token"
        );
        let decision = super::watcher_timeout_finalize_decision(
            Some(&pinned_a),
            mailbox_active,
            tmux_session_name,
        );
        assert_eq!(
            decision,
            super::TimeoutFinalizeDecision::Finalize { user_msg_id: 3500 },
            "A's timeout must FINALIZE (drain) when A still holds the token, even with an id-0 on-disk row"
        );

        // Drive the production Finalize arm: identity-guarded clear (id-0 row ≠ A's
        // identity → no-op, leaving the row for the live drain) + finalize on A's id.
        let drove = super::finish_restored_watcher_active_turn(
            &shared,
            &provider,
            channel_id,
            3500,
            true,
            false,
            true,
            None,
            "watcher turn watchdog timeout (#3419)",
        )
        .await;
        assert!(drove, "the wedged A turn drives the finalizer");
        // The wedge is gone: A's token is released so the queued follow-up can run.
        assert!(
            mailbox_snapshot(&shared, channel_id)
                .await
                .cancel_token
                .is_none(),
            "#3419 R3: draining A's wedge releases the mailbox token"
        );
        });
}

// #3419 R3 (codex HIGH): the id-0 escape — a synthetic / re-acquired
// watcher-owned PINNED turn (user_msg_id == 0, no mailbox token) must NEVER
// drive an id-0 finalize from the timeout path. The decision is always `Skip`
// regardless of the mailbox active id.
#[test]
fn timeout_finalize_skips_id_zero_pinned_turn() {
    let provider = ProviderKind::Claude;
    let session = "AgentDesk-claude-adk-cc-9873421";
    // Pinned is an id-0 synthetic turn: the id-0 filter forces Skip (no token
    // of its own to drain, no id-0 submit) even if the mailbox is busy.
    let synthetic = fresh_idle_inflight(provider.clone(), 987_3421, session, 0, 10);
    assert_eq!(
        super::watcher_timeout_finalize_decision(Some(&synthetic), Some(4242), session),
        super::TimeoutFinalizeDecision::Skip {
            pinned_user_msg_id: 0
        },
        "id-0 synthetic pinned turn must never id-0-finalize from the timeout path"
    );
    // No pinned snapshot at all → Skip too (no inflight to authenticate).
    assert_eq!(
        super::watcher_timeout_finalize_decision(None, None, session),
        super::TimeoutFinalizeDecision::Skip {
            pinned_user_msg_id: 0
        },
    );
    // Pinned A is nonzero but the mailbox has NO active turn → Skip (no token
    // wedged to drain).
    let pinned_a = fresh_idle_inflight(provider.clone(), 987_3421, session, 3001, 10);
    assert_eq!(
        super::watcher_timeout_finalize_decision(Some(&pinned_a), None, session),
        super::TimeoutFinalizeDecision::Skip {
            pinned_user_msg_id: 3001
        },
    );
}

/// #3419 B: the watcher turn-active predicate is the SINGLE AUTHORITY shared
/// by the read loop (`while active`) and the timeout-finalize gate (`if
/// !active`). It must hold the turn active while BOTH timers are within
/// bounds, and release it the instant EITHER expires — independently — so a
/// turn that keeps emitting output (idle reset) survives until it idles, and
/// a turn that idles is released even far below the absolute cap.
#[test]
fn watcher_turn_still_active_releases_on_idle_or_cap_independently() {
    use std::time::Duration;
    let idle_window = Duration::from_secs(3600);
    let cap = Duration::from_secs(6 * 3600);

    // Active turn (codex still producing output): idle just reset, well
    // under both windows → keep reading.
    assert!(
        super::watcher_turn_still_active(
            Duration::from_secs(2),
            idle_window,
            Duration::from_secs(120),
            cap
        ),
        "a turn with recent output and short total age must stay active"
    );

    // A LIVE long/interactive turn: huge total age (near the cap) but
    // output keeps arriving so idle stays tiny → still active. This is the
    // exact case absolute-time timeouts killed pre-B.
    assert!(
        super::watcher_turn_still_active(
            Duration::from_secs(5),
            idle_window,
            cap - Duration::from_secs(1),
            cap
        ),
        "a long turn that keeps emitting output (idle reset) must survive"
    );

    // Idle expired (no real byte for the whole window) but total age is
    // small → NOT active. Idle fires independently of the cap; this is the
    // genuinely-stuck turn C then drains.
    assert!(
        !super::watcher_turn_still_active(idle_window, idle_window, Duration::from_secs(60), cap),
        "reaching the idle window with no output must release the turn"
    );

    // Absolute cap expired even though idle is tiny (pathological: output
    // that never stops yet never finishes) → NOT active. Cap fires
    // independently of idle.
    assert!(
        !super::watcher_turn_still_active(Duration::from_secs(1), idle_window, cap, cap),
        "reaching the absolute cap must release the turn even while output flows"
    );

    // Boundary: strictly LESS-THAN keeps it active one tick before the
    // window, and `>=` releases at the window — no off-by-one straddle.
    assert!(super::watcher_turn_still_active(
        idle_window - Duration::from_nanos(1),
        idle_window,
        Duration::ZERO,
        cap
    ));
    assert!(!super::watcher_turn_still_active(
        idle_window,
        idle_window,
        Duration::ZERO,
        cap
    ));
}

// #3016 test helper: a real, non-stale watcher handle so the registry slot
// exists for the finalize. Mirrors the `live_watcher_handle` builder in
// mod.rs's registry tests. (#3016 phase-5b2: the `mailbox_finalize_owed`
// field has been removed, so the helper no longer carries that flag.)
fn test_watcher_handle(tmux_session_name: &str) -> crate::services::discord::TmuxWatcherHandle {
    crate::services::discord::TmuxWatcherHandle {
        tmux_session_name: tmux_session_name.to_string(),
        output_path: format!("/tmp/{tmux_session_name}.jsonl"),
        paused: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        resume_offset: std::sync::Arc::new(std::sync::Mutex::new(None)),
        cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pause_epoch: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        turn_delivered: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        last_heartbeat_ts_ms: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
            crate::services::discord::tmux_watcher_now_ms(),
        )),
    }
}

// #3016 option A (watcher normal-completion finalize decouple).
//
// Proves the decoupling directly: a *normal completion* drives the
// single-authority finalizer with `finish_mailbox_on_completion = false`
// (fresh live watcher, see tmux.rs:`tmux_output_watcher` default). Under the
// OLD flag-only gate the watcher's normal live bridge→watcher delegation turn
// would only finalize when the now-removed `mailbox_finalize_owed` flag was
// set; after option A the finalize fires from the confirmed-completion signal
// instead, so the flag was redundant for this path. The finalizer's
// idempotence (proven by the #3140 matrix) keeps this from over-finalizing
// when the bridge already finalized first.
//
// #3016 phase-5b2: with the flag removed, `finish_mailbox_on_completion =
// false` is now the only legacy gate, and `normal_completion = true` is the
// sole finalize driver.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn normal_completion_finalizes_with_both_legacy_flags_false() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(987_3016);
    let tmux_session_name = "AgentDesk-claude-adk-cc-9873016";

    // Register a REAL watcher handle so the finalize acts on an ACTUAL slot
    // (not the vacuous "no handle exists" case the original test had).
    shared
        .tmux_watchers
        .insert(channel_id, test_watcher_handle(tmux_session_name));

    // Seed a live active mailbox turn (cancel token registered) so we can
    // observe the finalize releasing it.
    assert!(
        mailbox_try_start_turn(
            &shared,
            channel_id,
            std::sync::Arc::new(CancelToken::new()),
            UserId::new(42),
            MessageId::new(3001),
        )
        .await
    );

    crate::services::discord::inflight::clear_inflight_state(&provider, channel_id.get());
    let drove = super::finish_restored_watcher_active_turn(
        &shared,
        &provider,
        channel_id,
        3001,  // real user_msg_id (exact ledger match)
        false, // finish_mailbox_on_completion — fresh live watcher
        true,  // normal_completion — confirmed terminal-output-committed point
        false, // kickoff_queue
        None,
        "normal_completion_decouple_test",
    )
    .await;
    assert!(
        drove,
        "normal_completion must drive the finalize (helper must not early-return)"
    );

    // The finalize fired purely on `normal_completion`: the active mailbox
    // turn's cancel token is released even with `finish_mailbox_on_completion`
    // false. Under the OLD flag-only gate this call would have early-returned
    // and left the token in place.
    let snapshot = mailbox_snapshot(&shared, channel_id).await;
    assert!(
        snapshot.cancel_token.is_none(),
        "normal completion must finalize and release the mailbox token with the legacy gate off"
    );

    // Idempotent: a second normal-completion submit for the same turn is a
    // no-op (AlreadyFinalized) — no over-finalize, no underflow.
    super::finish_restored_watcher_active_turn(
        &shared,
        &provider,
        channel_id,
        3001,
        false,
        true,
        false,
        None,
        "normal_completion_decouple_test_double",
    )
    .await;
    let snapshot_after = mailbox_snapshot(&shared, channel_id).await;
    assert!(
        snapshot_after.cancel_token.is_none(),
        "second normal-completion submit stays a no-op (idempotent finalizer)"
    );
}

#[test]
fn pre_panel_release_decrements_before_same_channel_followup_claims() {
    use std::sync::atomic::Ordering;

    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    runtime.block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(987_4106);
        let tmux_session_name = "AgentDesk-claude-adk-cc-9874106";
        let turn_a = 4106_001;
        let turn_b = 4106_002;

        shared
            .tmux_watchers
            .insert(channel_id, test_watcher_handle(tmux_session_name));

        assert!(
            mailbox_try_start_turn(
                &shared,
                channel_id,
                std::sync::Arc::new(CancelToken::new()),
                UserId::new(42),
                MessageId::new(turn_a),
            )
            .await
        );
        crate::services::discord::increment_global_active(&shared, "test_turn_a_start");
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);

        assert!(super::should_submit_restored_watcher_finalize(
            false, turn_a
        ));
        // #4106 review-fix: the early release must ALSO run the finalizer's
        // D-side channel cleanup (the late do_finalize will guarded-miss and skip
        // it). Seed a channel role override and assert the hoist clears it.
        shared
            .dispatch
            .role_overrides
            .insert(channel_id, ChannelId::new(555_4106));
        assert!(
            super::release_restored_watcher_active_turn_before_panel_edit(
                &shared, &provider, channel_id, turn_a,
            )
            .await,
            "the pre-panel hoist must release and decrement turn A before the awaited edit"
        );
        assert_eq!(
            shared.restart.global_active.load(Ordering::Relaxed),
            0,
            "turn A's global_active count is gone before the panel edit can race"
        );
        assert!(
            !shared.dispatch.role_overrides.contains_key(&channel_id),
            "the pre-panel hoist must run turn A's D-side channel cleanup (role override removed), not just the decrement"
        );

        // Test double for the awaited Discord status-panel edit: while it is
        // suspended, a same-channel follow-up claims the mailbox slot.
        let (panel_started_tx, panel_started_rx) = tokio::sync::oneshot::channel();
        let (panel_done_tx, panel_done_rx) = tokio::sync::oneshot::channel();
        let panel_edit = tokio::spawn(async move {
            let _ = panel_started_tx.send(());
            panel_done_rx.await.expect("panel edit released");
        });
        panel_started_rx.await.expect("panel edit started");

        assert!(
            mailbox_try_start_turn(
                &shared,
                channel_id,
                std::sync::Arc::new(CancelToken::new()),
                UserId::new(43),
                MessageId::new(turn_b),
            )
            .await,
            "the follow-up should be able to claim the mailbox during the panel await"
        );
        crate::services::discord::increment_global_active(&shared, "test_turn_b_start");
        assert_eq!(
            shared.restart.global_active.load(Ordering::Relaxed),
            1,
            "only the follow-up turn should be counted while the panel edit is in flight"
        );
        panel_done_tx.send(()).expect("release panel edit");
        panel_edit.await.expect("panel edit task");

        let drove_late_a = super::finish_restored_watcher_active_turn(
            &shared,
            &provider,
            channel_id,
            turn_a,
            false,
            true,
            false,
            None,
            "pre_panel_release_race_late_turn_a_finalize",
        )
        .await;
        assert!(drove_late_a);
        assert_eq!(
            shared.restart.global_active.load(Ordering::Relaxed),
            1,
            "turn A's late finalizer must not double-decrement turn B's active count"
        );
        assert!(
            mailbox_snapshot(&shared, channel_id)
                .await
                .cancel_token
                .is_some(),
            "turn B remains the live active mailbox owner after turn A's late finalize"
        );

        super::finish_restored_watcher_active_turn(
            &shared,
            &provider,
            channel_id,
            turn_b,
            false,
            true,
            false,
            None,
            "pre_panel_release_race_turn_b_finalize",
        )
        .await;
        assert_eq!(
            shared.restart.global_active.load(Ordering::Relaxed),
            0,
            "after the follow-up finalizes, global_active returns to zero with no permanent leak"
        );
    });
}

// #3016 codex R1 (wrong-turn finalize guard). Companion to the decouple
// test above. Exercises the SAFETY PROPERTY the Issue-1 call-site fix
// depends on: once `normal_completion = true` finalizes UNCONDITIONALLY,
// the id handed to the finalizer must name the SAME turn the watcher just
// completed — otherwise a stale/follow-up id would `finish_turn_if_matches`
// and release the WRONG (newer) live turn.
//
// Scenario: turn A (id 3001) is finalized correctly; then a NEWER turn B
// (id 4002) becomes the live active turn; a stale normal-completion submit
// that mistakenly carries turn A's id (3001) must NOT release turn B. The
// call site avoids this by deriving the id from the turn-PINNED pre-relay
// snapshot (falling back to 0), but the finalizer's exact-id match is the
// backstop this asserts.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn stale_normal_completion_does_not_release_newer_active_turn() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(987_3017);
    let tmux_session_name = "AgentDesk-claude-adk-cc-9873017";

    // Real watcher handle so the finalize acts on an actual registry slot.
    shared
        .tmux_watchers
        .insert(channel_id, test_watcher_handle(tmux_session_name));

    // Turn A is the live active turn (id 3001).
    assert!(
        mailbox_try_start_turn(
            &shared,
            channel_id,
            std::sync::Arc::new(CancelToken::new()),
            UserId::new(42),
            MessageId::new(3001),
        )
        .await
    );

    crate::services::discord::inflight::clear_inflight_state(&provider, channel_id.get());
    // Finalize turn A with its OWN id — releases turn A.
    let drove_a = super::finish_restored_watcher_active_turn(
        &shared,
        &provider,
        channel_id,
        3001,
        false,
        true,
        false,
        None,
        "stale_guard_turn_a",
    )
    .await;
    assert!(drove_a, "correct-turn finalize must drive");
    assert!(
        mailbox_snapshot(&shared, channel_id)
            .await
            .cancel_token
            .is_none(),
        "turn A must be released by its matching finalize"
    );

    // A NEWER turn B (id 4002) becomes the live active turn.
    let token_b = std::sync::Arc::new(CancelToken::new());
    assert!(
        mailbox_try_start_turn(
            &shared,
            channel_id,
            token_b.clone(),
            UserId::new(42),
            MessageId::new(4002),
        )
        .await
    );

    // A STALE normal-completion submit mistakenly carrying turn A's id
    // (3001) must NOT release turn B (4002). It drove the finalizer (past
    // the gate) but the exact-id match misses, so turn B stays live.
    let drove_stale = super::finish_restored_watcher_active_turn(
        &shared,
        &provider,
        channel_id,
        3001, // STALE id (turn A), while turn B (4002) is live
        false,
        true, // normal_completion fires unconditionally
        false,
        None,
        "stale_guard_stale_id",
    )
    .await;
    assert!(
        drove_stale,
        "the stale submit still passes the gate (normal_completion = true)"
    );
    let snapshot_b = mailbox_snapshot(&shared, channel_id).await;
    assert!(
        snapshot_b.cancel_token.is_some(),
        "a stale id MUST NOT release the newer active turn B (wrong-turn guard)"
    );

    // Sanity: turn B finalizes correctly when handed its OWN id.
    super::finish_restored_watcher_active_turn(
        &shared,
        &provider,
        channel_id,
        4002,
        false,
        true,
        false,
        None,
        "stale_guard_turn_b",
    )
    .await;
    assert!(
        mailbox_snapshot(&shared, channel_id)
            .await
            .cancel_token
            .is_none(),
        "turn B is released by its matching finalize"
    );
}

// #3016 S3 test helper: build an inflight snapshot with explicit
// turn_start_offset / last_offset so the fresh-idle decision's OUTPUT-RANGE
// gate (`pinned_finalize_user_msg_id` /
// `committed_completion_is_stale_for_newer_turn`) can be exercised against
// current vs. newer turns.
fn fresh_idle_inflight(
    provider: ProviderKind,
    channel_id: u64,
    tmux_session_name: &str,
    user_msg_id: u64,
    turn_start_offset: u64,
) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        provider,
        channel_id,
        Some("adk-cc".to_string()),
        42,
        user_msg_id,
        user_msg_id + 1,
        "prompt".to_string(),
        Some("session".to_string()),
        Some(tmux_session_name.to_string()),
        Some("/tmp/out.jsonl".to_string()),
        None,
        turn_start_offset,
    );
    // `InflightTurnState::new` sets turn_start_offset == last_offset; keep
    // them equal (the registration invariant) so the range tests behave like
    // production.
    state.last_offset = turn_start_offset;
    state.turn_start_offset = Some(turn_start_offset);
    state
}

// #3016 S3 — drives the REAL fresh-idle decision helper that the production
// watcher branch calls (`watcher_fresh_idle_finalize_decision`), proving the
// completion-signal routing without re-implementing it.
//
// (b) PausedLive (no structural terminator) → DeferPausedLive. This is the
// paused-at-selector / permission-prompt / subagent-running / long-silent-tool
// case. The defer keys on the STRUCTURAL TERMINATOR, NOT on response
// emptiness, so it cannot be made unreachable the way the first A2 attempt
// was. The A2 guards (paused/epoch, stale-skip) are NOT consulted here — a
// paused-live turn is deferred regardless.
#[test]
fn fresh_idle_paused_live_defers_via_completion_signal() {
    use crate::services::discord::turn_finalizer::CompletionSignal;
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let session = "AgentDesk-claude-adk-cc-9873100";
    let current_turn = fresh_idle_inflight(provider.clone(), 987_3100, session, 9001, 10);
    // Even with a perfectly valid current-turn snapshot, no epoch change, and
    // not paused, PausedLive defers — the signal is the disambiguator.
    assert_eq!(
        watcher_fresh_idle_finalize_decision(
            CompletionSignal::PausedLive,
            false, // full_response_is_empty — irrelevant: PausedLive defers first
            false,
            false,
            Some(&current_turn),
            session,
            50,
        ),
        FreshIdleFinalizeDecision::DeferPausedLive,
        "no terminator (selector/permission/subagent/long-silent-tool) → defer"
    );
}

// #3016 S3 — (a/c) Done (structural JSONL terminator proven) for a genuine
// current-turn completion → Finalize with the turn's REAL pinned id, EVEN when
// the response is empty (the whole point of S3: a structural terminator is
// authoritative regardless of emptiness).
//
// #3016 phase-5b1 (codex HIGH fix) — Unknown (non-JSONL runtime) routing is
// EMPTINESS-keyed, NOT flag-keyed and NOT unconditional:
//   * NON-empty Unknown at proven pane-idle → Finalize PROMPTLY (flag-independent,
//     the intended 5b1 improvement: no 1800s far-backstop latency). Reaching this
//     helper for an `Unknown` signal already PROVES pane idle (the fresh-idle gate
//     fires only after `watcher_session_ready_for_input` held over the idle
//     timeout). Visible output + pane-idle is a genuine completion.
//   * EMPTY Unknown → DeferEmptyUnknown. A non-JSONL runtime (Gemini / OpenCode /
//     Qwen / LegacyTmuxWrapper) has NO structured PausedLive signal, so a turn
//     awaiting a selector / permission / interactive prompt can look pane-idle
//     with empty output. Finalizing it would kill the turn mid-work. Deferring on
//     emptiness is the flag-independent reconstruction of the OLD (pre-5b1)
//     `delegated_finalize_owed && empty → defer` condition (`owed` was ~always
//     true for a delegated `Unknown` here); the 5a 1800s far-backstop remains its
//     finalizer. This is the regression-prevention case — the previous 5b1 build
//     finalized empty Unknown IMMEDIATELY here, which was the codex HIGH defect.
#[test]
fn fresh_idle_done_finalizes_and_unknown_routes_by_emptiness() {
    use crate::services::discord::turn_finalizer::CompletionSignal;
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let session = "AgentDesk-claude-adk-cc-9873101";
    let channel_id = 987_3101u64;
    let current_offset = 50u64;
    // Current turn started at offset 10 < current_offset 50 → in range.
    let current_turn = fresh_idle_inflight(provider.clone(), channel_id, session, 9001, 10);

    // (a/c) Done + EMPTY response + current turn + not paused + epoch unchanged
    // → Finalize with the REAL id. A structural terminator finalizes regardless
    // of emptiness (degenerate-empty-offset safe: turn_start_offset 10 < 50).
    assert_eq!(
        watcher_fresh_idle_finalize_decision(
            CompletionSignal::Done,
            true, // full_response_is_empty — Done finalizes even when empty
            false,
            false,
            Some(&current_turn),
            session,
            current_offset,
        ),
        FreshIdleFinalizeDecision::Finalize { user_msg_id: 9001 },
        "Done terminator finalizes the current turn even with an empty response"
    );

    // NON-empty Unknown (non-JSONL runtime) at proven pane-idle → Finalize
    // PROMPTLY with the turn's REAL id, flag-independent (the intended 5b1
    // improvement). No 1800s far-backstop wait for a turn that produced output.
    assert_eq!(
        watcher_fresh_idle_finalize_decision(
            CompletionSignal::Unknown,
            false, // full_response_is_empty — NON-empty
            false,
            false,
            Some(&current_turn),
            session,
            current_offset,
        ),
        FreshIdleFinalizeDecision::Finalize { user_msg_id: 9001 },
        "non-empty Unknown at proven pane-idle → prompt flag-independent finalize"
    );

    // EMPTY Unknown → DEFER (codex HIGH fix). Even with a perfectly valid
    // current-turn snapshot, no pause, and no epoch change, an empty Unknown is
    // NOT finalized on this pass — it relies on the 5a far-backstop. This is the
    // case the previous 5b1 build finalized prematurely.
    assert_eq!(
        watcher_fresh_idle_finalize_decision(
            CompletionSignal::Unknown,
            true, // full_response_is_empty — EMPTY
            false,
            false,
            Some(&current_turn),
            session,
            current_offset,
        ),
        FreshIdleFinalizeDecision::DeferEmptyUnknown,
        "empty Unknown (non-JSONL prompt could be awaiting input) → defer, not finalize"
    );
}

/// #3293: the `fresh_idle_inflight` fixtures below resolve the runtime store
/// root, so these tests pin `AGENTDESK_ROOT_DIR` to a tempdir under the shared
/// env lock and clear it on drop like every other env-touching watcher test.
struct RootEnvGuard;

impl Drop for RootEnvGuard {
    fn drop(&mut self) {
        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
    }
}

/// #4210: tuple fields drop in declaration order, so the guard (which removes
/// the env var) and the tempdir MUST come before the lock — otherwise the lock
/// releases first and the deferred `remove_var` can delete a var another test
/// (already holding the freed lock) just set.
fn pin_runtime_root_for_test() -> (
    RootEnvGuard,
    tempfile::TempDir,
    std::sync::MutexGuard<'static, ()>,
) {
    let lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("runtime root");
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
    (RootEnvGuard, root, lock)
}

#[test]
fn fresh_idle_done_uncommitted_session_bound_tail_rewinds_before_finalize_4169() {
    let _env = pin_runtime_root_for_test();
    let provider = ProviderKind::Claude;
    let session = "AgentDesk-claude-adk-cc-9874169";
    let channel_id = 987_4169u64;
    let turn_start_offset = 100u64;
    let committed_offset = 150u64;
    let consumed_end = 240u64;
    let mut current_turn =
        fresh_idle_inflight(provider, channel_id, session, 9169, turn_start_offset);
    current_turn.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    current_turn.session_bound_delivered = false;

    let plan = watcher_fresh_idle_session_bound_retry_plan(
        Some(&current_turn),
        session,
        consumed_end,
        committed_offset,
    )
    .expect("uncommitted session-bound tail must block fresh-idle finalize");
    assert_eq!(plan.turn_start_offset, turn_start_offset);
    assert_eq!(
        plan.retry_offset, committed_offset,
        "retry resumes at the committed floor, preserving the undelivered tail"
    );

    let mut all_data = b"{\"type\":\"system\",\"ready_for_input\":true}\n".to_vec();
    let current_offset = plan.retry_offset;
    all_data.clear();
    let all_data_start_offset = current_offset;

    assert!(
        current_offset < consumed_end,
        "fresh-idle guard must not advance the watcher watermark past the undelivered body"
    );
    assert_eq!(all_data_start_offset, committed_offset);
    assert!(all_data.is_empty());
    assert_eq!(
        current_turn.effective_relay_owner_kind(),
        RelayOwnerKind::SessionBoundRelay,
        "the inflight row remains retry evidence instead of being identity-cleared"
    );
    assert!(!current_turn.session_bound_delivered);
}

#[test]
fn fresh_idle_done_committed_session_bound_tail_finalizes_normally_4169() {
    use crate::services::discord::turn_finalizer::CompletionSignal;

    let _env = pin_runtime_root_for_test();
    let provider = ProviderKind::Claude;
    let session = "AgentDesk-claude-adk-cc-9874170";
    let channel_id = 987_4170u64;
    let turn_start_offset = 100u64;
    let consumed_end = 240u64;
    let mut current_turn =
        fresh_idle_inflight(provider, channel_id, session, 9170, turn_start_offset);
    current_turn.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    current_turn.session_bound_delivered = true;

    assert_eq!(
        watcher_fresh_idle_session_bound_retry_plan(
            Some(&current_turn),
            session,
            consumed_end,
            consumed_end,
        ),
        None,
        "committed >= consumed end keeps the legit fresh-idle finalize path unchanged"
    );
    assert_eq!(
        watcher_fresh_idle_finalize_decision(
            CompletionSignal::Done,
            false,
            false,
            false,
            Some(&current_turn),
            session,
            consumed_end,
        ),
        FreshIdleFinalizeDecision::Finalize { user_msg_id: 9170 },
        "normal delivered Done completion still finalizes with the pinned turn id"
    );
}

#[test]
fn wait_inflight_defer_arms_same_rewind_seed_4169() {
    let plan = watcher_wait_inflight_retry_plan();
    assert!(!plan.relay_ok, "WaitInFlight is not a terminal commit");
    assert!(
        plan.retry_offset,
        "WaitInFlight must re-arm the #4115 same-range rewind"
    );

    let rollover_ids = vec![MessageId::new(4101), MessageId::new(4102)];
    let seed = watcher_terminal_rewind_seed(WatcherTerminalRewindSeedInput {
        placeholder_msg_id: Some(MessageId::new(4169)),
        status_panel_msg_id: Some(MessageId::new(4170)),
        response_sent_offset: 0,
        last_edit_text: "streamed terminal body",
        task_notification_kind: None,
        finish_mailbox_on_completion: true,
        injected_prompt_message_id: Some(9171),
        streaming_rollover_frozen_msg_ids: &rollover_ids,
    })
    .expect("WaitInFlight retry keeps restored-turn evidence for the next pass");
    assert!(seed.same_turn_rewind);
    assert_eq!(seed.current_msg_id, MessageId::new(4169));
    assert_eq!(seed.status_message_id, Some(MessageId::new(4170)));
    assert_eq!(seed.last_edit_text, "streamed terminal body");
    assert_eq!(seed.streaming_rollover_frozen_msg_ids, rollover_ids);
}

// #3016 phase-5b1 — Unknown (non-JSONL runtime) keeps the SAME wrong-turn-race
// guards as Done, so prompt finalize never releases a follow-up turn:
//   * paused_now / epoch_changed → AbortFollowupTookOver (no premature finalize);
//   * a NEWER follow-up in the pinned snapshot → SkipStale (no stale finalize
//     of a superseded turn).
#[test]
fn fresh_idle_unknown_keeps_wrong_turn_race_guards() {
    use crate::services::discord::turn_finalizer::CompletionSignal;
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let session = "AgentDesk-claude-adk-cc-9873108";
    let channel_id = 987_3108u64;
    let current_offset = 50u64;
    let current_turn = fresh_idle_inflight(provider.clone(), channel_id, session, 9001, 10);

    // The race guards only matter on the finalize path, i.e. NON-empty Unknown
    // (empty Unknown defers before the guards). So every call below is non-empty.
    //
    // paused_now → abort regardless of the snapshot (a Discord turn took over).
    assert_eq!(
        watcher_fresh_idle_finalize_decision(
            CompletionSignal::Unknown,
            false, // full_response_is_empty — NON-empty (on the finalize path)
            true,
            false,
            Some(&current_turn),
            session,
            current_offset,
        ),
        FreshIdleFinalizeDecision::AbortFollowupTookOver,
        "Unknown + paused_now → abort before finalize (follow-up took over)"
    );
    // epoch_changed → abort.
    assert_eq!(
        watcher_fresh_idle_finalize_decision(
            CompletionSignal::Unknown,
            false,
            false,
            true,
            Some(&current_turn),
            session,
            current_offset,
        ),
        FreshIdleFinalizeDecision::AbortFollowupTookOver,
        "Unknown + epoch_changed → abort before finalize"
    );
    // The pinned snapshot is a NEWER follow-up turn that begins AT/AFTER the
    // committed range → SkipStale (pinned id 0), so the newer turn is NOT
    // released by this older idle.
    let newer = fresh_idle_inflight(provider, channel_id, session, 9002, 50);
    assert_eq!(
        watcher_fresh_idle_finalize_decision(
            CompletionSignal::Unknown,
            false,
            false,
            false,
            Some(&newer),
            session,
            current_offset,
        ),
        FreshIdleFinalizeDecision::SkipStale {
            pinned_user_msg_id: 0
        },
        "Unknown + newer follow-up snapshot → SkipStale, follow-up NOT finalized"
    );
}

// #3016 S3 — (d) wrong-turn race: a Done signal that would finalize must NOT
// release a follow-up turn that took over the session during the cleanup
// awaits. Two sub-paths, both reusing the #3197 A2 defenses:
//   * paused/epoch changed → AbortFollowupTookOver (mirrors the canonical
//     pause/epoch guard, evaluated before the destructive clear);
//   * the pinned snapshot is a NEWER turn (turn_start_offset >= current_offset)
//     → SkipStale (pinned id 0), so the follow-up is NOT released.
#[test]
fn fresh_idle_done_wrong_turn_race_does_not_finalize_followup() {
    use crate::services::discord::turn_finalizer::CompletionSignal;
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let session = "AgentDesk-claude-adk-cc-9873102";
    let channel_id = 987_3102u64;
    let current_offset = 50u64;
    let current_turn = fresh_idle_inflight(provider.clone(), channel_id, session, 9001, 10);

    // Done is empty-independent — every call below passes non-empty for clarity;
    // the routing is identical for an empty Done (terminator is authoritative).
    //
    // paused_now → abort regardless of the snapshot.
    assert_eq!(
        watcher_fresh_idle_finalize_decision(
            CompletionSignal::Done,
            false, // full_response_is_empty
            true,
            false,
            Some(&current_turn),
            session,
            current_offset,
        ),
        FreshIdleFinalizeDecision::AbortFollowupTookOver,
        "Done + paused_now → abort before the destructive clear"
    );
    // epoch_changed → abort.
    assert_eq!(
        watcher_fresh_idle_finalize_decision(
            CompletionSignal::Done,
            false,
            false,
            true,
            Some(&current_turn),
            session,
            current_offset,
        ),
        FreshIdleFinalizeDecision::AbortFollowupTookOver,
        "Done + epoch_changed → abort before the destructive clear"
    );

    // The pinned snapshot is a NEWER follow-up turn that begins AT/AFTER the
    // committed range (turn_start_offset 50 >= current_offset 50) → SkipStale
    // (pinned id 0), so the newer turn is NOT released by this older idle.
    let newer = fresh_idle_inflight(provider.clone(), channel_id, session, 9002, 50);
    assert_eq!(
        watcher_fresh_idle_finalize_decision(
            CompletionSignal::Done,
            false,
            false,
            false,
            Some(&newer),
            session,
            current_offset,
        ),
        FreshIdleFinalizeDecision::SkipStale {
            pinned_user_msg_id: 0
        },
        "a newer follow-up (start >= current_offset) → SkipStale, follow-up NOT finalized"
    );
    // A strictly-after start is also skipped.
    let after = fresh_idle_inflight(provider, channel_id, session, 9003, 60);
    assert_eq!(
        watcher_fresh_idle_finalize_decision(
            CompletionSignal::Done,
            false,
            false,
            false,
            Some(&after),
            session,
            current_offset,
        ),
        FreshIdleFinalizeDecision::SkipStale {
            pinned_user_msg_id: 0
        },
        "a strictly-after follow-up is also skipped"
    );
}

// #3016 S3 (Concern 2 — residual TOCTOU CLOSED): the Done/Finalize arm now
// performs the on-disk clear with the ATOMIC compare-and-clear helper
// `clear_inflight_state_if_matches_identity` (read+validate+unlink under a
// SINGLE sidecar lock), keyed on the PINNED turn's identity. This test
// exercises the REAL atomic helper against REAL on-disk inflight (no separate
// re-read + recheck window) and proves the two distinct failure modes:
//
//   1. Follow-up preserved: if a follow-up turn saved its inflight DURING the
//      cleanup awaits (a DIFFERENT identity than the pinned turn is on disk at
//      clear time), the atomic clear is a guaranteed no-op (`UserMsgMismatch`)
//      — the follow-up's inflight survives byte-for-byte. There is no window
//      between the identity check and the unlink because they share one lock.
//   2. Current turn cleared: if the on-disk inflight is STILL the pinned turn
//      (no follow-up), the atomic clear removes it (`Cleared`), exactly like
//      the old unconditional clear did for the happy path.
//
// The finalize decision is a SEPARATE concern, still derived from the pinned
// snapshot by `watcher_fresh_idle_finalize_decision` (asserted Finalize here);
// only the destructive CLEAR — the one that carried the TOCTOU — was swapped to
// the atomic identity-matched helper.
#[test]
fn fresh_idle_clear_gate_skips_when_late_reread_is_newer_turn() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let session = "AgentDesk-claude-adk-cc-9873200";
    let channel_id = 987_3200u64;
    let current_offset = 50u64;

    // Pinned pre-cleanup snapshot: the CURRENT turn (start 10 < 50). On this
    // snapshot alone the decision helper returns Finalize (NOT stale), so the
    // Done arm is entered and the (now atomic) clear is reached. The pinned id
    // 9001 is exactly the id the finalize runs on.
    let pinned_current = fresh_idle_inflight(provider.clone(), channel_id, session, 9001, 10);
    assert_eq!(
        watcher_fresh_idle_finalize_decision(
            crate::services::discord::turn_finalizer::CompletionSignal::Done,
            false, // full_response_is_empty
            false,
            false,
            Some(&pinned_current),
            session,
            current_offset,
        ),
        FreshIdleFinalizeDecision::Finalize { user_msg_id: 9001 },
        "pinned snapshot alone is the current turn → Finalize (clear arm entered)"
    );
    // The identity the Done arm builds from the pinned snapshot for the atomic
    // clear (same `InflightTurnIdentity::from_state` the production code uses).
    let pinned_identity =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(&pinned_current);

    // ── (1) Follow-up preserved ──────────────────────────────────────────
    // Simulate a follow-up turn that saved a DIFFERENT inflight (id 9002,
    // start 50 >= current_offset) on another worker thread DURING the cleanup
    // awaits — i.e. it is what is on disk at clear time, NOT the pinned turn.
    let late_followup = fresh_idle_inflight(provider.clone(), channel_id, session, 9002, 50);
    crate::services::discord::inflight::save_inflight_state(&late_followup)
        .expect("save follow-up inflight");

    // The atomic clear keyed on the PINNED identity is a no-op: the on-disk
    // identity (id 9002) does NOT match the pinned id 9001 → UserMsgMismatch,
    // and crucially the read-and-delete happen under ONE lock so there is no
    // re-read window a follow-up could slip through.
    let outcome = crate::services::discord::inflight::clear_inflight_state_if_matches_identity(
        &provider,
        channel_id,
        &pinned_identity,
    );
    assert_eq!(
        outcome,
        crate::services::discord::inflight::GuardedClearOutcome::UserMsgMismatch,
        "atomic clear keyed on the pinned turn is a no-op when a follow-up's inflight is on disk"
    );
    // The follow-up's inflight survives intact (NOT wiped).
    let survived = crate::services::discord::inflight::load_inflight_state(&provider, channel_id)
        .expect("follow-up inflight must still be on disk");
    assert_eq!(
        survived.user_msg_id, 9002,
        "the follow-up turn's inflight is preserved — the TOCTOU clear cannot wipe it"
    );

    // ── (2) Current turn cleared ─────────────────────────────────────────
    // No follow-up: the pinned turn itself is on disk at clear time. The atomic
    // clear removes it, exactly like the old happy path.
    crate::services::discord::inflight::clear_inflight_state(&provider, channel_id);
    crate::services::discord::inflight::save_inflight_state(&pinned_current)
        .expect("save pinned inflight");
    let outcome = crate::services::discord::inflight::clear_inflight_state_if_matches_identity(
        &provider,
        channel_id,
        &pinned_identity,
    );
    assert_eq!(
        outcome,
        crate::services::discord::inflight::GuardedClearOutcome::Cleared,
        "atomic clear removes the inflight when it is STILL the pinned turn (happy path)"
    );
    assert!(
        crate::services::discord::inflight::load_inflight_state(&provider, channel_id).is_none(),
        "pinned turn's inflight is gone after the atomic clear"
    );
}

#[test]
fn committed_clear_with_captured_turn_nonce_preserves_id0_followup_saved_before_late_reread() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let session = "AgentDesk-claude-adk-cc-4184";
    let channel_id = 4_184_000u64;

    let mut pinned_current = fresh_idle_inflight(provider.clone(), channel_id, session, 0, 10);
    pinned_current.turn_nonce = Some("turn-4184-current".to_string());
    let captured_turn_nonce = pinned_current.turn_nonce.clone();
    crate::services::discord::inflight::save_inflight_state(&pinned_current)
        .expect("save original id-0 inflight before follow-up replacement");

    let mut late_followup = fresh_idle_inflight(provider.clone(), channel_id, session, 0, 50);
    late_followup.turn_nonce = Some("turn-4184-followup".to_string());
    crate::services::discord::inflight::save_inflight_state(&late_followup)
        .expect("save id-0 follow-up inflight before late re-read");

    let late_reread_identity =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(&late_followup);
    let outcome =
        crate::services::discord::inflight::clear_inflight_state_if_matches_identity_turn_nonce(
            &provider,
            channel_id,
            &late_reread_identity,
            captured_turn_nonce.as_deref(),
        );

    assert_eq!(
        outcome,
        crate::services::discord::inflight::GuardedClearOutcome::UserMsgMismatch,
        "captured nonce from the finalizing id-0 turn must not clear a newer id-0 follow-up"
    );
    let survived = crate::services::discord::inflight::load_inflight_state(&provider, channel_id)
        .expect("follow-up inflight must survive");
    assert_eq!(survived.user_msg_id, 0);
    assert_eq!(survived.turn_start_offset, Some(50));
    assert_eq!(survived.turn_nonce.as_deref(), Some("turn-4184-followup"));
}

#[test]
fn loop_top_nonce_refresh_keeps_observed_nonce_for_id0_followup_at_current_offset() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let session = "AgentDesk-claude-adk-cc-4184-loop-top";
    let channel_id = 4_184_003u64;
    let current_offset = 50;

    let mut observed = fresh_idle_inflight(provider.clone(), channel_id, session, 0, 10);
    observed.turn_nonce = Some("turn-4184-observed".to_string());
    crate::services::discord::inflight::save_inflight_state(&observed)
        .expect("save observed id-0 turn");
    let mut watcher_identity =
        Some(crate::services::discord::inflight::InflightTurnIdentity::from_state(&observed));
    let mut captured_turn_nonce = observed.turn_nonce.clone();

    let mut followup =
        fresh_idle_inflight(provider.clone(), channel_id, session, 0, current_offset);
    followup.turn_nonce = Some("turn-4184-followup".to_string());
    crate::services::discord::inflight::save_inflight_state(&followup)
        .expect("replace with id-0 follow-up before loop-top refresh");

    refresh_watcher_turn_identity(
        &mut watcher_identity,
        &mut captured_turn_nonce,
        &provider,
        ChannelId::new(channel_id),
        session,
        current_offset,
    );

    assert_eq!(
        captured_turn_nonce.as_deref(),
        Some("turn-4184-observed"),
        "follow-up row at the current consumed offset must not replace the observed turn nonce"
    );

    let followup_identity =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(&followup);
    let outcome =
        crate::services::discord::inflight::clear_inflight_state_if_matches_identity_turn_nonce(
            &provider,
            channel_id,
            &followup_identity,
            captured_turn_nonce.as_deref(),
        );

    assert_eq!(
        outcome,
        crate::services::discord::inflight::GuardedClearOutcome::UserMsgMismatch,
        "pinned observed nonce must make the late-read follow-up identity fail the guarded clear"
    );
    let survived = crate::services::discord::inflight::load_inflight_state(&provider, channel_id)
        .expect("follow-up inflight must survive");
    assert_eq!(survived.user_msg_id, 0);
    assert_eq!(survived.turn_start_offset, Some(current_offset));
    assert_eq!(survived.turn_nonce.as_deref(), Some("turn-4184-followup"));
}

#[test]
fn committed_clear_with_captured_turn_nonce_clears_legacy_row_without_nonce() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let session = "AgentDesk-claude-adk-cc-4184-legacy";
    let channel_id = 4_184_001u64;

    let mut legacy = fresh_idle_inflight(provider.clone(), channel_id, session, 0, 10);
    legacy.turn_nonce = None;
    crate::services::discord::inflight::save_inflight_state(&legacy)
        .expect("save legacy id-0 inflight without nonce");
    let identity = crate::services::discord::inflight::InflightTurnIdentity::from_state(&legacy);

    let outcome =
        crate::services::discord::inflight::clear_inflight_state_if_matches_identity_turn_nonce(
            &provider,
            channel_id,
            &identity,
            Some("turn-4184-current"),
        );

    assert_eq!(
        outcome,
        crate::services::discord::inflight::GuardedClearOutcome::Cleared,
        "legacy rows without a nonce preserve the identity-only clear contract"
    );
    assert!(
        crate::services::discord::inflight::load_inflight_state(&provider, channel_id).is_none(),
        "legacy matching row should be cleared"
    );
}

#[test]
fn committed_clear_with_expected_none_clears_row_with_nonce() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let session = "AgentDesk-claude-adk-cc-4184-expected-none";
    let channel_id = 4_184_004u64;

    let mut current = fresh_idle_inflight(provider.clone(), channel_id, session, 0, 10);
    current.turn_nonce = Some("turn-4184-on-disk".to_string());
    crate::services::discord::inflight::save_inflight_state(&current)
        .expect("save id-0 inflight with nonce");
    let identity = crate::services::discord::inflight::InflightTurnIdentity::from_state(&current);

    let outcome =
        crate::services::discord::inflight::clear_inflight_state_if_matches_identity_turn_nonce(
            &provider, channel_id, &identity, None,
        );

    assert_eq!(
        outcome,
        crate::services::discord::inflight::GuardedClearOutcome::Cleared,
        "expected None preserves legacy identity-only clear semantics even when the row has a nonce"
    );
    assert!(
        crate::services::discord::inflight::load_inflight_state(&provider, channel_id).is_none(),
        "matching row should be cleared when expected nonce is absent"
    );
}

#[test]
fn committed_clear_filters_empty_string_turn_nonce_values() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let session = "AgentDesk-claude-adk-cc-4184-empty-nonce";
    let channel_id = 4_184_005u64;

    let mut empty_on_disk = fresh_idle_inflight(provider.clone(), channel_id, session, 0, 10);
    empty_on_disk.turn_nonce = Some(String::new());
    crate::services::discord::inflight::save_inflight_state(&empty_on_disk)
        .expect("save id-0 inflight with empty on-disk nonce");
    let empty_on_disk_identity =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(&empty_on_disk);

    let outcome =
        crate::services::discord::inflight::clear_inflight_state_if_matches_identity_turn_nonce(
            &provider,
            channel_id,
            &empty_on_disk_identity,
            Some("turn-4184-expected"),
        );
    assert_eq!(
        outcome,
        crate::services::discord::inflight::GuardedClearOutcome::Cleared,
        "empty on-disk nonce is filtered to legacy identity-only matching"
    );

    let mut empty_expected = fresh_idle_inflight(provider.clone(), channel_id, session, 0, 20);
    empty_expected.turn_nonce = Some("turn-4184-on-disk".to_string());
    crate::services::discord::inflight::save_inflight_state(&empty_expected)
        .expect("save id-0 inflight for empty expected nonce case");
    let empty_expected_identity =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(&empty_expected);

    let outcome =
        crate::services::discord::inflight::clear_inflight_state_if_matches_identity_turn_nonce(
            &provider,
            channel_id,
            &empty_expected_identity,
            Some(""),
        );
    assert_eq!(
        outcome,
        crate::services::discord::inflight::GuardedClearOutcome::Cleared,
        "empty expected nonce is filtered to legacy identity-only matching"
    );
    assert!(
        crate::services::discord::inflight::load_inflight_state(&provider, channel_id).is_none(),
        "matching row should be cleared after empty nonce filtering"
    );
}

#[test]
fn committed_clear_with_captured_turn_nonce_clears_matching_same_turn_row() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let session = "AgentDesk-claude-adk-cc-4184-current";
    let channel_id = 4_184_002u64;

    let mut current = fresh_idle_inflight(provider.clone(), channel_id, session, 0, 10);
    current.turn_nonce = Some("turn-4184-current".to_string());
    crate::services::discord::inflight::save_inflight_state(&current)
        .expect("save current id-0 inflight with nonce");
    let identity = crate::services::discord::inflight::InflightTurnIdentity::from_state(&current);

    let outcome =
        crate::services::discord::inflight::clear_inflight_state_if_matches_identity_turn_nonce(
            &provider,
            channel_id,
            &identity,
            current.turn_nonce.as_deref(),
        );

    assert_eq!(
        outcome,
        crate::services::discord::inflight::GuardedClearOutcome::Cleared,
        "matching nonce should clear the same turn normally"
    );
    assert!(
        crate::services::discord::inflight::load_inflight_state(&provider, channel_id).is_none(),
        "matching current row should be cleared"
    );
}

#[test]
fn fresh_idle_clear_uses_pinned_nonce_to_preserve_id0_full_identity_alias_followup() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );

    let provider = ProviderKind::Claude;
    let session = "AgentDesk-claude-adk-cc-4273-fresh-idle";
    let channel_id = 4_273_000u64;

    let mut pinned_current = fresh_idle_inflight(provider.clone(), channel_id, session, 0, 50);
    pinned_current.started_at = "2026-07-08 01:33:00".to_string();
    pinned_current.turn_nonce = Some("turn-4273-pinned".to_string());
    crate::services::discord::inflight::save_inflight_state(&pinned_current)
        .expect("save pinned pre-cleanup id-0 inflight");

    let pinned_pre_cleanup_inflight =
        crate::services::discord::inflight::load_inflight_state(&provider, channel_id)
            .expect("load pinned pre-cleanup snapshot");
    let pinned_clear_identity = InflightTurnIdentity::from_state(&pinned_pre_cleanup_inflight);
    let pinned_clear_turn_nonce = pinned_pre_cleanup_inflight.turn_nonce.as_deref();

    let mut alias_followup = fresh_idle_inflight(provider.clone(), channel_id, session, 0, 50);
    alias_followup.started_at = pinned_pre_cleanup_inflight.started_at.clone();
    alias_followup.turn_nonce = Some("turn-4273-followup".to_string());
    assert!(
        pinned_clear_identity.matches_state(&alias_followup),
        "test fixture must alias the pinned full identity"
    );
    assert_ne!(
        pinned_clear_turn_nonce,
        alias_followup.turn_nonce.as_deref(),
        "only the observed-turn nonce distinguishes the follow-up"
    );
    crate::services::discord::inflight::save_inflight_state(&alias_followup)
        .expect("save id-0 full-identity-alias follow-up");

    let outcome =
        crate::services::discord::inflight::clear_inflight_state_if_matches_identity_turn_nonce(
            &provider,
            channel_id,
            &pinned_clear_identity,
            pinned_clear_turn_nonce,
        );
    assert_eq!(
        outcome,
        crate::services::discord::inflight::GuardedClearOutcome::UserMsgMismatch,
        "fresh-idle clear must not delete an id-0 full-identity alias with a different nonce"
    );
    let survived = crate::services::discord::inflight::load_inflight_state(&provider, channel_id)
        .expect("different-nonce follow-up must survive");
    assert_eq!(survived.user_msg_id, 0);
    assert_eq!(survived.started_at, pinned_pre_cleanup_inflight.started_at);
    assert_eq!(survived.turn_start_offset, Some(50));
    assert_eq!(survived.turn_nonce.as_deref(), Some("turn-4273-followup"));

    let mut same_nonce_row = fresh_idle_inflight(provider.clone(), channel_id, session, 0, 50);
    same_nonce_row.started_at = pinned_current.started_at.clone();
    same_nonce_row.turn_nonce = Some("turn-4273-pinned".to_string());
    assert!(
        pinned_clear_identity.matches_state(&same_nonce_row),
        "same-nonce fixture must still match the pinned full identity"
    );
    crate::services::discord::inflight::save_inflight_state(&same_nonce_row)
        .expect("save id-0 full-identity row with matching nonce");

    let outcome =
        crate::services::discord::inflight::clear_inflight_state_if_matches_identity_turn_nonce(
            &provider,
            channel_id,
            &pinned_clear_identity,
            pinned_clear_turn_nonce,
        );
    assert_eq!(
        outcome,
        crate::services::discord::inflight::GuardedClearOutcome::Cleared,
        "fresh-idle clear should still delete the same id-0 row when the nonce matches"
    );
    assert!(
        crate::services::discord::inflight::load_inflight_state(&provider, channel_id).is_none(),
        "same-nonce matching row should be cleared"
    );
}

// #3016 S3 — end-to-end through the REAL completion signal AND the REAL
// finalizer actor: a genuine empty/suppressed delegated completion whose
// on-disk transcript HAS a structural terminator (Claude `result`) finalizes
// via the structural signal even with the legacy `mailbox_finalize_owed` flag
// FALSE. This drives:
//   1. `TurnFinalizer::completion_signal_state` over a real JSONL file → Done,
//   2. `watcher_fresh_idle_finalize_decision(Done, ..)` → Finalize{real id},
//   3. `finish_restored_watcher_active_turn(.., normal_completion=true, ..)`
//      through the real actor + mailbox → the turn's token is released.
// The prior A2 FAIL was re-implementing the decision; this routes the EXACT
// production helpers.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn fresh_idle_empty_terminated_completion_finalizes_via_completion_signal_flag_false() {
    use crate::services::discord::turn_finalizer::CompletionSignal;
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(987_3103);
    let tmux_session_name = "AgentDesk-claude-adk-cc-9873103";
    let user_msg_id = 8201u64;
    let turn_start_offset = 10u64;
    let current_offset = 50u64;

    // A real on-disk JSONL transcript that ENDS with a structural terminator
    // (Claude `result`) — i.e. the turn is genuinely done, even though it
    // committed NO assistant text to relay (empty/suppressed completion).
    let transcript = tmp.path().join("out.jsonl");
    std::fs::write(
        &transcript,
        "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n\
             {\"type\":\"result\",\"result\":\"done\",\"session_id\":\"s\"}\n",
    )
    .expect("write transcript");

    // 1. The REAL structural signal over the REAL file → Done.
    let signal = shared.turn_finalizer.completion_signal_state(
        &provider,
        Some(RuntimeHandoffKind::ClaudeTui),
        transcript.as_path(),
    );
    assert_eq!(
        signal,
        CompletionSignal::Done,
        "a transcript ending in a `result` terminator is structurally Done"
    );

    // 2. The REAL decision helper for the current turn → Finalize{real id}.
    let snapshot = fresh_idle_inflight(
        provider.clone(),
        channel_id.get(),
        tmux_session_name,
        user_msg_id,
        turn_start_offset,
    );
    let finalize_id = match watcher_fresh_idle_finalize_decision(
        signal,
        true, // full_response_is_empty — empty/suppressed, but Done finalizes anyway
        false,
        false,
        Some(&snapshot),
        tmux_session_name,
        current_offset,
    ) {
        FreshIdleFinalizeDecision::Finalize { user_msg_id } => user_msg_id,
        other => panic!("empty-but-terminated current turn must Finalize, got {other:?}"),
    };
    assert_eq!(
        finalize_id, user_msg_id,
        "pinned id is the current turn's real id"
    );

    // Live active mailbox turn with the turn's real id so we can observe the
    // finalize releasing exactly THIS turn's token.
    let token = std::sync::Arc::new(CancelToken::new());
    assert!(
        mailbox_try_start_turn(
            &shared,
            channel_id,
            token,
            UserId::new(42),
            MessageId::new(user_msg_id),
        )
        .await
    );

    // 3. Production fresh-idle commit point with the legacy flag FALSE: clear
    // inflight, then drive the finalizer on the structural authority.
    crate::services::discord::inflight::clear_inflight_state(&provider, channel_id.get());
    let drove = super::finish_restored_watcher_active_turn(
        &shared,
        &provider,
        channel_id,
        finalize_id,
        false, // finish_mailbox_on_completion — fresh live watcher
        true,  // normal_completion — S3: structural-signal-driven, flag-independent
        true,
        None,
        "watcher fresh ready-for-input idle (structural completion terminator)",
    )
    .await;
    assert!(
        drove,
        "Done structural completion drives the finalizer regardless of the legacy flag"
    );
    assert!(
        mailbox_snapshot(&shared, channel_id)
            .await
            .cancel_token
            .is_none(),
        "empty/suppressed but structurally-terminated completion finalizes with the flag FALSE"
    );

    // Idempotency: a second submit for the same turn is a no-op.
    super::finish_restored_watcher_active_turn(
        &shared,
        &provider,
        channel_id,
        finalize_id,
        false,
        true,
        true,
        None,
        "watcher fresh ready-for-input idle (structural completion terminator)",
    )
    .await;
    assert!(
        mailbox_snapshot(&shared, channel_id)
            .await
            .cancel_token
            .is_none(),
        "second finalize is a no-op (AlreadyFinalized), no double-finalize"
    );
}

#[test]
fn legacy_wrapper_pane_prompt_candidates_reconstruct_wrapped_direct_input() {
    let pane = "\
▶ Ready for input (type message + Enter)
TUI-E2E-marker 한 줄로 marker를 그대로 응답하고, 'ssh
-direct' 단어도 포함해줘.
[sending...]
[session: abc]
TUI-E2E-marker ssh-direct

▶ Ready for input (type message + Enter)
";

    let candidates = legacy_wrapper_prompt_candidates_from_pane(pane);

    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.contains("'ssh-direct'")),
        "wrapped terminal prompt should have a compact candidate for pending-prompt matching"
    );
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.contains("'ssh -direct'")),
        "wrapped terminal prompt should keep a spaced candidate for readable direct observation"
    );
}

#[test]
fn legacy_wrapper_prompt_observation_requires_response_batch() {
    assert!(!watcher_batch_contains_relayable_response(
        br#"{"provider":"codex","type":"ready_for_input"}"#
    ));
    assert!(watcher_batch_contains_relayable_response(
        br#"{"type":"assistant","message":{"content":[{"text":"ok"}]}}"#
    ));
    assert!(watcher_batch_contains_relayable_response(
        br#"{"type":"result","result":"ok"}"#
    ));
}

#[test]
fn legacy_wrapper_prompt_observation_accepts_spaced_json_type_forms() {
    assert!(watcher_batch_contains_relayable_response(
        br#"{"type": "assistant","message":{"content":[{"text":"ok"}]}}"#
    ));
    assert!(watcher_batch_contains_relayable_response(
        br#"{"type": "result","result":"ok"}"#
    ));
}

#[test]
fn post_terminal_continuation_probe_ignores_result_only_batches() {
    assert!(!watcher_batch_contains_assistant_event(
        br#"{"provider":"codex","type":"ready_for_input"}"#
    ));
    assert!(watcher_batch_contains_assistant_event(
        br#"{"type":"assistant","message":{"content":[{"type":"tool_use"}]}}"#
    ));
    assert!(!watcher_batch_contains_assistant_event(
        br#"{"type":"result","result":"duplicate terminal text"}"#
    ));
}

#[test]
fn no_inflight_terminal_response_does_not_reuse_previous_placeholder() {
    assert!(watcher_should_clear_stale_terminal_message_ids(
        false,
        true,
        Some(MessageId::new(42))
    ));
    assert!(!watcher_should_clear_stale_terminal_message_ids(
        true,
        true,
        Some(MessageId::new(42))
    ));
    assert!(!watcher_should_clear_stale_terminal_message_ids(
        false,
        false,
        Some(MessageId::new(42))
    ));
    assert!(!watcher_should_clear_stale_terminal_message_ids(
        false, true, None
    ));
}

/// #3351: orphan-reclaim decision for the same turn's relay placeholder.
#[test]
fn orphan_turn_placeholder_reclaim_decision() {
    let id = Some(MessageId::new(42));
    // The leaked-spinner case from the issue: reclaim.
    assert!(watcher_should_reclaim_orphan_turn_placeholder(
        true,
        id,
        false,
        "⠸ 계속 처리 중"
    ));
    // Empty body = still-placeholder (sweeper semantics inherited).
    assert!(watcher_should_reclaim_orphan_turn_placeholder(
        true, id, false, ""
    ));
    // Already edited into a real response body: never delete.
    assert!(!watcher_should_reclaim_orphan_turn_placeholder(
        true,
        id,
        false,
        "실제 응답 본문"
    ));
    // Turn produced assistant text: owned by the existing arms.
    assert!(!watcher_should_reclaim_orphan_turn_placeholder(
        true,
        id,
        true,
        "⠸ 계속 처리 중"
    ));
    // Bridge-owned turn: hands off.
    assert!(!watcher_should_reclaim_orphan_turn_placeholder(
        false,
        id,
        false,
        "⠸ 계속 처리 중"
    ));
    assert!(!watcher_should_reclaim_orphan_turn_placeholder(
        true,
        None,
        false,
        "⠸ 계속 처리 중"
    ));
}

#[test]
fn no_inflight_terminal_response_drops_restored_response_seed() {
    let restored = "previous turn";
    let mut full_response = "previous turnfresh turn".to_string();
    let mut response_sent_offset = 0;
    let mut last_edit_text = "previous turn".to_string();

    assert!(
        discard_restored_response_seed_before_no_inflight_terminal_relay(
            &mut full_response,
            &mut response_sent_offset,
            &mut last_edit_text,
            restored,
            false,
            true,
            false,
            false,
        )
    );
    assert_eq!(full_response, "fresh turn");
    assert_eq!(response_sent_offset, 0);
    assert!(last_edit_text.is_empty());
}

#[test]
fn restored_response_seed_is_kept_for_managed_inflight() {
    let restored = "previous turn";
    let mut full_response = "previous turnfresh turn".to_string();
    let mut response_sent_offset = restored.len();
    let mut last_edit_text = "previous turn".to_string();

    assert!(
        !discard_restored_response_seed_before_no_inflight_terminal_relay(
            &mut full_response,
            &mut response_sent_offset,
            &mut last_edit_text,
            restored,
            true,
            true,
            false,
            false,
        )
    );
    assert_eq!(full_response, "previous turnfresh turn");
    assert_eq!(response_sent_offset, restored.len());
}

#[test]
fn no_inflight_user_boundary_without_fresh_text_drops_already_delivered_restored_response_seed() {
    let restored = "previous turn";
    let mut full_response = "previous turn".to_string();
    let mut response_sent_offset = restored.len();
    let mut last_edit_text = "previous turn".to_string();

    assert!(
        discard_restored_response_seed_before_no_inflight_terminal_relay(
            &mut full_response,
            &mut response_sent_offset,
            &mut last_edit_text,
            restored,
            false,
            false,
            false,
            false,
        )
    );
    assert_eq!(full_response, "");
    assert_eq!(response_sent_offset, 0);
    assert!(last_edit_text.is_empty());
}

#[test]
fn no_inflight_user_boundary_without_fresh_text_preserves_body_bearing_seed_for_relay() {
    let restored = "undelivered body";
    let mut full_response = restored.to_string();
    let mut response_sent_offset = 0;
    let mut last_edit_text = String::new();

    assert!(
        !discard_restored_response_seed_before_no_inflight_terminal_relay(
            &mut full_response,
            &mut response_sent_offset,
            &mut last_edit_text,
            restored,
            false,
            false,
            false,
            false,
        )
    );
    assert_eq!(full_response, restored);
    assert_eq!(response_sent_offset, 0);
    assert!(last_edit_text.is_empty());

    let has_assistant_response = !full_response.trim().is_empty();
    let current_response = full_response.get(response_sent_offset..).unwrap_or("");
    let has_current_response = !current_response.trim().is_empty();
    let relay_decision = terminal_relay_decision(has_assistant_response, None, true);
    let watcher_direct_send = watcher_should_direct_send_after_session_bound_ack(
        relay_decision.should_direct_send,
        // #3579: the non-attempt sentinel folds to `Unknown` exactly like the
        // old `MissingTarget` init, so the watcher-direct gate is unchanged.
        SessionBoundRelayAckOutcome::NotAttempted,
        false,
    );

    assert!(has_assistant_response);
    assert!(has_current_response);
    assert!(relay_decision.should_direct_send);
    assert!(watcher_direct_send);
    assert_eq!(
        watcher_terminal_response_for_direct_send(&full_response, response_sent_offset, false),
        restored
    );
}

#[test]
fn placeholderless_full_send_failure_rewinds_then_retries_body_once_4115() {
    struct FailOnceSender {
        attempts: usize,
        delivered: Vec<String>,
    }

    impl FailOnceSender {
        fn send(&mut self, body: &str) -> Result<(), &'static str> {
            self.attempts += 1;
            if self.attempts == 1 {
                Err("discord 5xx")
            } else {
                self.delivered.push(body.to_string());
                Ok(())
            }
        }
    }

    let turn_data_start_offset = 128;
    let turn_end_offset = 384;
    let body = "placeholder-less terminal body";
    let mut next_read_offset = turn_data_start_offset;
    let mut sender = FailOnceSender {
        attempts: 0,
        delivered: Vec::new(),
    };

    for _ in 0..2 {
        let mut current_offset = turn_end_offset;
        let mut all_data = b"{\"type\":\"result\",\"result\":\"body\"}\n".to_vec();
        let mut all_data_start_offset = next_read_offset;
        assert_eq!(
            all_data_start_offset, turn_data_start_offset,
            "retry pass must re-read the same turn body"
        );
        assert_eq!(current_offset, turn_end_offset);
        assert!(!all_data.is_empty());

        if sender.send(body).is_ok() {
            break;
        }

        let plan = watcher_full_send_failure_retry_plan();
        assert!(!plan.relay_ok);
        assert!(plan.retry_offset);
        current_offset = turn_data_start_offset;
        all_data.clear();
        assert!(all_data.is_empty());
        all_data_start_offset = current_offset;
        next_read_offset = all_data_start_offset;
    }

    assert_eq!(sender.attempts, 2);
    assert_eq!(sender.delivered, vec![body.to_string()]);
}

#[test]
fn placeholderless_rollback_sender_cleans_prefix_before_rewind_4154() {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    runtime.block_on(async {
        let http = Http::new("test-token");
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(9_154_154);
        let anchor_id = MessageId::new(41_540);
        let active_messages: Arc<Mutex<Vec<(MessageId, String)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let send_calls = Arc::new(AtomicUsize::new(0));
        let delete_calls = Arc::new(AtomicUsize::new(0));
        let next_message_id = Arc::new(AtomicU64::new(1000));

        let send_active = Arc::clone(&active_messages);
        let send_count = Arc::clone(&send_calls);
        let send_next_id = Arc::clone(&next_message_id);
        let delete_active = Arc::clone(&active_messages);
        let delete_count = Arc::clone(&delete_calls);
        let _hook = crate::services::discord::formatting::rollback_transport_test_hook::install(
            Box::new(
                move |seen_channel, content, _reference, _nonce, _enforce_nonce| {
                    if seen_channel != channel_id {
                        return None;
                    }
                    let call = send_count.fetch_add(1, Ordering::SeqCst) + 1;
                    if call == 2 {
                        return Some(Err("Error while sending HTTP request.".to_string()));
                    }
                    let id = MessageId::new(send_next_id.fetch_add(1, Ordering::SeqCst));
                    send_active
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .push((id, content.to_string()));
                    Some(Ok(id))
                },
            ),
            Box::new(move |seen_channel, message_id| {
                if seen_channel != channel_id {
                    return None;
                }
                delete_count.fetch_add(1, Ordering::SeqCst);
                let mut active = delete_active
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                active.retain(|(id, _)| *id != message_id);
                Some(Ok(()))
            }),
        );

        let body = "x".repeat(crate::services::discord::DISCORD_MSG_LIMIT + 32);
        assert!(
            body.len() > crate::services::discord::DISCORD_MSG_LIMIT,
            "test body must force multi-chunk send"
        );

        let first =
            crate::services::discord::formatting::send_long_message_raw_with_reference_rollback(
                &http, channel_id, anchor_id, &body, &shared, None,
            )
            .await;
        let first_err = first.expect_err("first pass fails on chunk 2");
        assert_eq!(
            classify_watcher_send_failure_message(&first_err.to_string()),
            WatcherSendFailureClass::Transient,
            "rollback sender must carry the retryable class through its flattened error"
        );
        assert_eq!(
            delete_calls.load(Ordering::SeqCst),
            1,
            "rollback deletes the already-posted prefix"
        );
        assert!(
            active_messages
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .is_empty(),
            "failed rollback pass must leave no delivered prefix behind"
        );

        let delivered =
            crate::services::discord::formatting::send_long_message_raw_with_reference_rollback(
                &http, channel_id, anchor_id, &body, &shared, None,
            )
            .await
            .expect("second pass delivers");
        assert_eq!(delivered.len(), 2, "body should split into two chunks");
        assert_eq!(send_calls.load(Ordering::SeqCst), 4);

        let active = active_messages
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        assert_eq!(active.len(), 2);
        let joined = active
            .iter()
            .map(|(_, content)| content.as_str())
            .collect::<String>();
        assert_eq!(
            joined, body,
            "retry should deliver exactly one copy of the multi-chunk body"
        );
    });
}

#[test]
fn rollback_sender_marks_serenity_5xx_html_decode_failure_transient_4154() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    runtime.block_on(async {
        let http = Http::new("test-token");
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(9_154_155);
        let anchor_id = MessageId::new(41_541);
        let _hook = crate::services::discord::formatting::rollback_transport_test_hook::install(
            Box::new(
                move |seen_channel, _content, _reference, _nonce, _enforce_nonce| {
                    if seen_channel != channel_id {
                        return None;
                    }
                    Some(Err(
                    "[Serenity] Could not decode json when receiving error response from discord:"
                        .to_string(),
                ))
                },
            ),
            Box::new(move |_seen_channel, _message_id| Some(Ok(()))),
        );

        let err =
            crate::services::discord::formatting::send_long_message_raw_with_reference_rollback(
                &http, channel_id, anchor_id, "body", &shared, None,
            )
            .await
            .expect_err("5xx HTML decode failure should fail the pass");
        let class = classify_watcher_send_failure_message(&err.to_string());
        assert_eq!(class, WatcherSendFailureClass::Transient);
        let plan = watcher_send_failure_retry_plan(class);
        assert!(
            plan.retry_offset,
            "structured transient class should rewind"
        );
    });
}

#[test]
fn watcher_full_send_permanent_error_does_not_rewind_4154() {
    let class = classify_watcher_send_failure_message("403 Forbidden (Missing Access)");
    assert_eq!(class, WatcherSendFailureClass::Permanent);
    let plan = watcher_send_failure_retry_plan(class);
    assert!(!plan.relay_ok);
    assert!(
        !plan.retry_offset,
        "permanent Discord errors fall through without rewind"
    );
}

#[test]
fn watcher_full_send_rewind_cap_degrades_after_three_attempts_4154() {
    for attempts in [1, 2, 3] {
        assert_eq!(
            watcher_rewind_attempt_disposition(attempts),
            WatcherRewindAttemptDisposition::Retry
        );
    }
    assert_eq!(
        watcher_rewind_attempt_disposition(4),
        WatcherRewindAttemptDisposition::GiveUp
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn relay_emission_timeout_rewinds_and_releases_slot_4194() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    let slot = Arc::new(AtomicU64::new(4_194));
    let slot_for_task = slot.clone();
    let timeout_task = tokio::spawn(async move {
        let mut slot_guard = RelaySlotGuard::new(slot_for_task);
        let result = watcher_relay_emission_with_timeout(std::future::pending::<()>()).await;
        assert!(
            result.is_err(),
            "never-resolving emission must trip the watchdog timeout"
        );

        let plan = watcher_relay_emission_timeout_failure_plan(
            &ProviderKind::Claude,
            ChannelId::new(4_194),
            "AgentDesk-claude-adk-cc",
            128,
            256,
        );
        assert!(!plan.relay_ok, "timeout is never treated as delivered");
        assert!(
            plan.retry_offset,
            "timeout must enter the rewind retry path"
        );

        let placeholder = MessageId::new(41_940);
        let status_panel = MessageId::new(41_941);
        let seed = watcher_terminal_rewind_seed(WatcherTerminalRewindSeedInput {
            placeholder_msg_id: Some(placeholder),
            status_panel_msg_id: Some(status_panel),
            response_sent_offset: 64,
            last_edit_text: "streamed prefix before hung emission",
            task_notification_kind: None,
            finish_mailbox_on_completion: true,
            injected_prompt_message_id: Some(7),
            streaming_rollover_frozen_msg_ids: &[MessageId::new(41_942)],
        })
        .expect("timeout retry must preserve the original delivery row context");
        assert_eq!(seed.current_msg_id, placeholder);
        assert_eq!(seed.status_message_id, Some(status_panel));
        assert_eq!(seed.response_sent_offset, 64);
        assert_eq!(seed.last_edit_text, "streamed prefix before hung emission");
        assert!(
            seed.same_turn_rewind,
            "timeout retry evidence must survive restored-seed guards"
        );

        slot_guard.release();
        plan
    });

    tokio::task::yield_now().await;
    assert_eq!(
        slot.load(Ordering::Acquire),
        4_194,
        "slot stays held while the emission future is hung"
    );

    tokio::time::advance(WATCHER_RELAY_EMISSION_TIMEOUT + std::time::Duration::from_millis(1))
        .await;
    tokio::task::yield_now().await;
    let plan = timeout_task.await.expect("timeout task joins");
    assert_eq!(
        plan,
        watcher_send_failure_retry_plan(WatcherSendFailureClass::Transient),
        "timeout must use the existing transient send-failure retry disposition"
    );
    assert_eq!(
        slot.load(Ordering::Acquire),
        0,
        "timeout retry branch releases the relay slot"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn relay_emission_timeout_fast_future_is_unchanged_4194() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    let slot = Arc::new(AtomicU64::new(99));
    let mut slot_guard = RelaySlotGuard::new(slot.clone());
    let result = watcher_relay_emission_with_timeout(async { "delivered" })
        .await
        .expect("fast emission should not timeout");

    assert_eq!(result, "delivered");
    assert_eq!(
        slot.load(Ordering::Acquire),
        99,
        "fast success path leaves existing slot-release timing unchanged"
    );
    slot_guard.release();
    assert_eq!(slot.load(Ordering::Acquire), 0);
}

#[test]
fn relay_emission_timeout_wiring_source_assertion_4194() {
    let module_src = include_str!("../tmux_watcher.rs");
    const BODY_WRAPPER_ANCHOR: &str =
        "let relay_ok = match watcher_relay_emission_with_timeout(async {";
    const BODY_TIMEOUT_RETRY_ANCHOR: &str = r#"Err(_) => {
                let plan = watcher_relay_emission_timeout_failure_plan(
                    &watcher_provider,
                    channel_id,
                    &tmux_session_name,
                    data_start_offset,
                    current_offset,
                );
                retry_terminal_delivery_from_offset = plan.retry_offset;
                plan.relay_ok
            }"#;

    assert!(
        module_src.contains(BODY_WRAPPER_ANCHOR),
        "terminal relay expression must remain enclosed by watcher_relay_emission_with_timeout"
    );
    assert!(
        module_src.contains(BODY_TIMEOUT_RETRY_ANCHOR),
        "body timeout Err arm must keep failed-undelivered rewind semantics"
    );
}

#[test]
fn completion_chrome_timeout_wiring_source_assertion_4194() {
    let module_src = include_str!("../tmux_watcher.rs");
    let helper_src = include_str!("session_bound_ack.rs");
    const COMPLETION_TIMEOUT_CONST_ANCHOR: &str =
        "pub(super) const WATCHER_RELAY_COMPLETION_CHROME_TIMEOUT: std::time::Duration";
    const COMPLETION_TIMEOUT_HELPER_ANCHOR: &str =
        "pub(super) async fn watcher_completion_chrome_with_timeout<T>";
    const COMPLETION_TIMEOUT_WARN_ANCHOR: &str = "watcher: completion chrome step timed out after terminal body commit; skipping remaining chrome without rewind";

    assert!(helper_src.contains(COMPLETION_TIMEOUT_CONST_ANCHOR));
    assert!(helper_src.contains(COMPLETION_TIMEOUT_HELPER_ANCHOR));
    assert!(helper_src.contains(COMPLETION_TIMEOUT_WARN_ANCHOR));

    let completion_start = module_src
        .find("let mut completion_chrome_timed_out = false;")
        .expect("completion chrome skip flag must exist");
    let release_rel = module_src[completion_start..]
        .find("// Release the emission slot regardless of success.")
        .expect("completion source slice must reach the slot release comment");
    let completion_src = &module_src[completion_start..completion_start + release_rel];

    assert!(
        !completion_src.contains("retry_terminal_delivery_from_offset"),
        "completion chrome timeouts must not request terminal-delivery rewind"
    );
    assert!(
        completion_src.contains("if !completion_chrome_timed_out"),
        "completion chrome timeout must skip remaining chrome steps before slot release"
    );
    assert_completion_chrome_wrap(
        completion_src,
        "completed_panel_usage_backfill_and_compact",
        "backfill_completed_panel_usage_and_maybe_inject_compact",
    );
    assert_completion_chrome_wrap(
        completion_src,
        "session_panel_lifecycle_refresh",
        "refresh_watcher_session_panel_from_lifecycle",
    );
    assert_completion_chrome_wrap(
        completion_src,
        "terminal_footer_or_status_panel_completion",
        "complete_watcher_terminal_footer_or_status_panel_with_sniffer",
    );
    assert_completion_chrome_wrap(
        completion_src,
        "stale_streaming_footer_reconcile",
        "crate::services::discord::http::edit_channel_message",
    );
}

fn assert_completion_chrome_wrap(source: &str, step_anchor: &str, call_anchor: &str) {
    let step_idx = source
        .find(step_anchor)
        .unwrap_or_else(|| panic!("missing completion timeout step anchor: {step_anchor}"));
    let prefix = &source[..step_idx];
    let call_idx = prefix
        .rfind(call_anchor)
        .unwrap_or_else(|| panic!("missing completion call anchor: {call_anchor}"));
    let wrapper_idx = prefix
        .rfind("watcher_completion_chrome_with_timeout(")
        .unwrap_or_else(|| panic!("missing completion timeout wrapper for: {step_anchor}"));
    assert!(
        wrapper_idx < call_idx,
        "timeout wrapper must enclose call anchor for {step_anchor}"
    );
    assert!(
        step_idx - wrapper_idx < 3_000,
        "timeout warning anchor drifted too far from wrapper for {step_anchor}"
    );
}

#[test]
fn edit_arm_rewind_seed_preserves_placeholder_context_4154() {
    let placeholder = MessageId::new(5154);
    let status_panel = MessageId::new(5155);
    let frozen = vec![MessageId::new(1), MessageId::new(2)];
    let seed = watcher_terminal_rewind_seed(WatcherTerminalRewindSeedInput {
        placeholder_msg_id: Some(placeholder),
        status_panel_msg_id: Some(status_panel),
        response_sent_offset: 128,
        last_edit_text: "streamed prefix",
        task_notification_kind: None,
        finish_mailbox_on_completion: true,
        injected_prompt_message_id: Some(99),
        streaming_rollover_frozen_msg_ids: &frozen,
    })
    .expect("placeholder context should seed retry");

    assert_eq!(seed.current_msg_id, placeholder);
    assert_eq!(seed.status_message_id, Some(status_panel));
    assert_eq!(seed.response_sent_offset, 128);
    assert!(
        seed.full_response.is_empty(),
        "retry re-reads JSONL bytes; carrying parsed response would double-append"
    );
    assert!(
        seed.same_turn_rewind,
        "rewind seeds bypass restart stale-seed discard"
    );
    assert_eq!(seed.last_edit_text, "streamed prefix");
    assert_eq!(seed.streaming_rollover_frozen_msg_ids, frozen);
}

#[test]
fn tui_direct_rewind_seed_with_prompt_anchor_reuses_placeholder_4115() {
    let placeholder = MessageId::new(6_115);
    let seed = watcher_terminal_rewind_seed(WatcherTerminalRewindSeedInput {
        placeholder_msg_id: Some(placeholder),
        status_panel_msg_id: None,
        response_sent_offset: 64,
        last_edit_text: "streamed prefix",
        task_notification_kind: None,
        finish_mailbox_on_completion: false,
        injected_prompt_message_id: Some(6_114),
        streaming_rollover_frozen_msg_ids: &[],
    })
    .expect("placeholder context should seed retry");

    let discard = super::super::should_discard_restored_seed_for_idle_direct_prompt(
        true,
        true,
        false,
        seed.same_turn_rewind,
        false,
    );
    assert!(
        !discard,
        "same-turn rewind seed must survive the idle direct-prompt anchor guard"
    );

    let stream_seed = super::super::watcher_stream_seed(Some(seed));
    assert_eq!(
        stream_seed.placeholder_msg_id,
        Some(placeholder),
        "retry pass edits the original placeholder instead of POSTing a second one"
    );
}

#[test]
fn cross_turn_watcher_reuse_discards_restored_seed_through_watcher_wiring_4105() {
    let seed_identity = InflightTurnIdentity {
        user_msg_id: 0,
        started_at: "2026-07-07T01:00:00Z".to_string(),
        tmux_session_name: Some("AgentDesk-claude-adk".to_string()),
        turn_start_offset: Some(100),
    };
    let current_identity = InflightTurnIdentity {
        started_at: "2026-07-07T01:00:10Z".to_string(),
        turn_start_offset: Some(240),
        ..seed_identity.clone()
    };
    let restored = RestoredWatcherTurn {
        current_msg_id: MessageId::new(4105),
        status_message_id: None,
        response_sent_offset: 0,
        full_response: "WARMUP".to_string(),
        last_edit_text: String::new(),
        task_notification_kind: None,
        finish_mailbox_on_completion: false,
        injected_prompt_message_id: Some(9001),
        turn_identity: Some(seed_identity),
        streaming_rollover_frozen_msg_ids: Vec::new(),
        same_turn_rewind: false,
    };

    let disposition = watcher_stream_seed_after_restored_seed_discard(
        Some(restored),
        Some(&current_identity),
        Some(9001),
    );

    assert!(disposition.seed_reassigned_to_different_turn);
    assert!(disposition.discard_restored_seed);
    assert!(disposition.stream_seed.full_response.is_empty());
    assert_eq!(disposition.stream_seed.response_sent_offset, 0);
}

#[test]
fn compact_local_only_boundary_without_seed_delivery_fingerprint_preserves_restored_seed_4096() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(987_4096_001);
    let tmux_session_name = "AgentDesk-claude-adk-cc-4096-missing-fingerprint";
    let restored = "queued turn terminal body that failed delivery";
    let mut full_response = restored.to_string();
    let mut response_sent_offset = 0;
    let mut last_edit_text = restored.to_string();
    let compact_tail =
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"/compact\"}}\n";

    let tool_state = crate::services::discord::tmux::WatcherToolState::new();
    let force_discard = local_cmd_no_output(
        compact_tail,
        Some(WatcherTerminalKind::SoftUserBoundary),
        false,
        &tool_state,
    );

    assert!(force_discard);
    crate::services::discord::outbound::delivery_record::record_delivered_content_fingerprint(
        &provider,
        channel_id,
        tmux_session_name,
        "same-session tui-direct turn that really delivered",
    );
    let restored_seed_delivery_confirmed =
        crate::services::discord::outbound::delivery_record::recent_delivered_content_matches(
            &provider,
            channel_id,
            tmux_session_name,
            restored,
        );
    assert!(
        !restored_seed_delivery_confirmed,
        "a competing same-session delivery must not authorize dropping the failed seed"
    );
    assert!(
        !discard_restored_response_seed_before_no_inflight_terminal_relay(
            &mut full_response,
            &mut response_sent_offset,
            &mut last_edit_text,
            restored,
            false,
            false,
            force_discard,
            restored_seed_delivery_confirmed,
        )
    );
    assert_eq!(full_response, restored);
    assert_eq!(response_sent_offset, 0);
    assert_eq!(last_edit_text, restored);
}

fn make_post_work_idle_inflight(
    tmux_session_name: &str,
    turn_source: TurnSource,
    relay_owner: RelayOwnerKind,
    turn_start_offset: Option<u64>,
    last_offset: u64,
) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Codex,
        987_4102_001,
        None,
        42,
        1001,
        1002,
        "external prompt".to_string(),
        None,
        Some(tmux_session_name.to_string()),
        Some("/tmp/agentdesk-4102-output.jsonl".to_string()),
        None,
        last_offset,
    );
    state.turn_source = turn_source;
    state.set_relay_owner_kind(relay_owner);
    state.turn_start_offset = turn_start_offset;
    state
}

#[test]
fn tui_direct_watcher_synthetic_post_work_idle_preserves_fresh_body_4102() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let tmux_session_name = "AgentDesk-codex-adk-cc-4102";
    let state = make_post_work_idle_inflight(
        tmux_session_name,
        TurnSource::ExternalInput,
        RelayOwnerKind::Watcher,
        Some(64),
        64,
    );

    let mut full_response = "provider body accumulated before idle timeout".to_string();
    let mut terminal_kind = None;

    assert!(watcher_handle_no_dispatch_post_work_idle_body(
        &mut full_response,
        &mut terminal_kind,
        Some(&state),
        false,
        tmux_session_name,
        true,
        128,
    ));
    assert_eq!(
        full_response,
        "provider body accumulated before idle timeout"
    );
    assert_eq!(terminal_kind, None);
}

#[test]
fn post_work_idle_synthetic_seed_without_fresh_text_is_cleared_4108() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let tmux_session_name = "AgentDesk-codex-adk-cc-4108-restored-seed";
    let state = make_post_work_idle_inflight(
        tmux_session_name,
        TurnSource::ExternalInput,
        RelayOwnerKind::Watcher,
        Some(64),
        64,
    );

    let mut full_response = "restored seed body from previous turn".to_string();
    let mut terminal_kind = Some(WatcherTerminalKind::SoftUserBoundary);

    assert!(!watcher_handle_no_dispatch_post_work_idle_body(
        &mut full_response,
        &mut terminal_kind,
        Some(&state),
        false,
        tmux_session_name,
        false,
        128,
    ));
    assert!(full_response.is_empty());
    assert_eq!(terminal_kind, Some(WatcherTerminalKind::SoftUserBoundary));
}

#[test]
fn no_dispatch_post_work_idle_clears_non_preservable_shapes_4108() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let tmux_session_name = "AgentDesk-codex-adk-cc-4108-negative";
    let mismatch_session_name = "AgentDesk-codex-adk-cc-4108-other";
    let cases = [
        (
            "external input without watcher owner",
            Some(make_post_work_idle_inflight(
                tmux_session_name,
                TurnSource::ExternalInput,
                RelayOwnerKind::None,
                Some(64),
                64,
            )),
            false,
            "fresh body",
            tmux_session_name,
        ),
        (
            "managed turn with watcher owner",
            Some(make_post_work_idle_inflight(
                tmux_session_name,
                TurnSource::Managed,
                RelayOwnerKind::Watcher,
                Some(64),
                64,
            )),
            false,
            "fresh body",
            tmux_session_name,
        ),
        (
            "dispatch id present",
            Some(make_post_work_idle_inflight(
                tmux_session_name,
                TurnSource::ExternalInput,
                RelayOwnerKind::Watcher,
                Some(64),
                64,
            )),
            true,
            "fresh body",
            tmux_session_name,
        ),
        (
            "session name mismatch",
            Some(make_post_work_idle_inflight(
                mismatch_session_name,
                TurnSource::ExternalInput,
                RelayOwnerKind::Watcher,
                Some(64),
                64,
            )),
            false,
            "fresh body",
            tmux_session_name,
        ),
        (
            "empty body",
            Some(make_post_work_idle_inflight(
                tmux_session_name,
                TurnSource::ExternalInput,
                RelayOwnerKind::Watcher,
                Some(64),
                64,
            )),
            false,
            "",
            tmux_session_name,
        ),
    ];

    for (label, state, dispatch_id_present, body, session_name) in cases {
        let mut full_response = body.to_string();
        let mut terminal_kind = None;
        assert!(
            !watcher_handle_no_dispatch_post_work_idle_body(
                &mut full_response,
                &mut terminal_kind,
                state.as_ref(),
                dispatch_id_present,
                session_name,
                true,
                128,
            ),
            "{label}"
        );
        assert!(full_response.is_empty(), "{label}");
        assert_eq!(terminal_kind, None, "{label}");
    }
}

#[test]
fn tui_direct_watcher_synthetic_newer_offset_does_not_match_4108() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let tmux_session_name = "AgentDesk-codex-adk-cc-4108-newer-offset";
    let state = make_post_work_idle_inflight(
        tmux_session_name,
        TurnSource::ExternalInput,
        RelayOwnerKind::Watcher,
        Some(256),
        256,
    );
    assert!(
        !crate::services::discord::tui_prompt_relay::tui_direct_watcher_synthetic_inflight_matches(
            Some(&state),
            tmux_session_name,
            128,
        )
    );
    let fallback_state = make_post_work_idle_inflight(
        tmux_session_name,
        TurnSource::ExternalInput,
        RelayOwnerKind::Watcher,
        None,
        256,
    );
    assert!(
        !crate::services::discord::tui_prompt_relay::tui_direct_watcher_synthetic_inflight_matches(
            Some(&fallback_state),
            tmux_session_name,
            128,
        )
    );

    let mut full_response = "future turn body must not be attributed backward".to_string();
    let mut terminal_kind = None;
    assert!(!watcher_handle_no_dispatch_post_work_idle_body(
        &mut full_response,
        &mut terminal_kind,
        Some(&state),
        false,
        tmux_session_name,
        true,
        128,
    ));
    assert!(full_response.is_empty());
}

#[test]
fn compact_local_only_boundary_without_output_drops_restored_seed_4081() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _root_guard = AgentdeskRootGuard::set(tmp.path());

    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(987_4096_002);
    let tmux_session_name = "AgentDesk-claude-adk-cc-4096-confirmed-fingerprint";
    let restored = "prior delivered response";
    let mut full_response = restored.to_string();
    let mut response_sent_offset = 0;
    let mut last_edit_text = restored.to_string();
    let compact_tail =
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"/compact\"}}\n";

    let tool_state = crate::services::discord::tmux::WatcherToolState::new();
    let force_discard = local_cmd_no_output(
        compact_tail,
        Some(WatcherTerminalKind::SoftUserBoundary),
        false,
        &tool_state,
    );

    assert!(force_discard);
    crate::services::discord::outbound::delivery_record::record_delivered_content_fingerprint(
        &provider,
        channel_id,
        tmux_session_name,
        restored,
    );
    let restored_seed_delivery_confirmed =
        crate::services::discord::outbound::delivery_record::recent_delivered_content_matches(
            &provider,
            channel_id,
            tmux_session_name,
            restored,
        );
    assert!(restored_seed_delivery_confirmed);
    assert!(
        discard_restored_response_seed_before_no_inflight_terminal_relay(
            &mut full_response,
            &mut response_sent_offset,
            &mut last_edit_text,
            restored,
            false,
            false,
            force_discard,
            restored_seed_delivery_confirmed,
        )
    );
    assert_eq!(full_response, "");
    assert_eq!(response_sent_offset, 0);
    assert!(last_edit_text.is_empty());
}

#[test]
fn orphan_reclaim_real_tail_still_preserves_body_seed_4081() {
    let restored = "undelivered orphan tail";
    let mut full_response = restored.to_string();
    let mut response_sent_offset = 0;
    let mut last_edit_text = String::new();
    let tool_state = crate::services::discord::tmux::WatcherToolState::new();
    let force_discard = local_cmd_no_output(
        "",
        Some(WatcherTerminalKind::SoftUserBoundary),
        false,
        &tool_state,
    );

    assert!(!force_discard);
    assert!(
        !discard_restored_response_seed_before_no_inflight_terminal_relay(
            &mut full_response,
            &mut response_sent_offset,
            &mut last_edit_text,
            restored,
            false,
            false,
            force_discard,
            false,
        )
    );
    assert_eq!(full_response, restored);
    assert_eq!(response_sent_offset, 0);
}

#[test]
fn tmux_dead_marker_short_circuits_liveness_interval() {
    assert!(should_probe_tmux_liveness(
        std::time::Duration::from_millis(1),
        true,
    ));
    assert!(!should_probe_tmux_liveness(
        std::time::Duration::from_millis(1),
        false,
    ));
}

#[test]
fn status_panel_v2_watcher_streaming_edit_moves_processing_footer_to_response_message() {
    let rendered = build_watcher_streaming_edit_text(
        true,
        "PIPE-E2E-CODEX OK",
        "⠙ 계속 처리 중",
        &ProviderKind::Codex,
    );

    assert_eq!(rendered, "PIPE-E2E-CODEX OK\n\n⠙ 계속 처리 중");
}

#[test]
fn legacy_watcher_streaming_edit_keeps_processing_footer() {
    let rendered = build_watcher_streaming_edit_text(
        false,
        "Partial answer",
        "⠙ 계속 처리 중",
        &ProviderKind::Codex,
    );

    assert_eq!(rendered, "Partial answer\n\n⠙ 계속 처리 중");
}

#[test]
fn watcher_streaming_suppression_stops_at_bridge_committed_frontier() {
    let committed_terminal_range = Some((100, 200));

    assert!(
        super::streaming_status_tick::watcher_should_suppress_streaming_after_bridge_delivery(
            true,
            true,
            (100, 200),
            committed_terminal_range,
        )
    );
    assert!(
        !super::streaming_status_tick::watcher_should_suppress_streaming_after_bridge_delivery(
            true,
            true,
            (200, 240),
            committed_terminal_range,
        )
    );
    assert!(
        !super::streaming_status_tick::watcher_should_suppress_streaming_after_bridge_delivery(
            true,
            true,
            (180, 240),
            committed_terminal_range,
        )
    );
    assert!(
        !super::streaming_status_tick::watcher_should_suppress_streaming_after_bridge_delivery(
            true,
            false,
            (100, 200),
            committed_terminal_range,
        )
    );
    assert!(
        !super::streaming_status_tick::watcher_should_suppress_streaming_after_bridge_delivery(
            false,
            true,
            (100, 200),
            committed_terminal_range,
        )
    );
    assert!(
        !super::streaming_status_tick::watcher_should_suppress_streaming_after_bridge_delivery(
            true,
            true,
            (100, 200),
            None,
        )
    );
}

#[test]
fn watcher_terminal_edit_detaches_placeholder_from_later_cleanup() {
    assert!(watcher_terminal_edit_consumes_placeholder(
        &ReplaceLongMessageOutcome::EditedOriginal
    ));
    assert!(!watcher_terminal_edit_consumes_placeholder(
        &ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
            edit_error: "edit failed".to_string(),
            replacement_anchor: None,
        }
    ));
}

#[test]
fn watcher_bridge_delivery_preserves_restored_inflight_placeholder() {
    assert!(!watcher_should_delete_suppressed_placeholder(true));
    assert!(watcher_should_delete_suppressed_placeholder(false));
}

#[test]
fn fallback_edit_failure_never_deletes_original_without_placeholder_probe() {
    assert!(!watcher_fallback_edit_failure_can_delete_original_placeholder(12, "partial answer"));
    assert!(!watcher_fallback_edit_failure_can_delete_original_placeholder(0, "partial answer"));
    assert!(!watcher_fallback_edit_failure_can_delete_original_placeholder(0, "⠙ Processing..."));
}

/// #3041 P1-1 (§3, codex R2 Issue-1): heartbeat-renew lifecycle for the
/// in-flight watcher delivery lease. These tests use the GATED Tokio clock
/// (`start_paused`) to drive the heartbeat's `tokio::time::interval` WITHOUT
/// real sleeps; `lease_now_ms()` is a separate real monotonic clock, so we
/// assert reclaim behaviour with EXPLICIT `now_ms` arguments anchored to the
/// observed `lease_now_ms()` baseline.
mod delivery_lease_heartbeat {
    use super::super::{
        DeliveryLeaseHeartbeat, WATCHER_DELIVERY_LEASE_DEADLINE_MS,
        WATCHER_DELIVERY_LEASE_HEARTBEAT_MS,
    };
    use crate::services::discord::turn_finalizer::TurnKey;
    use crate::services::discord::{
        DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder, LeaseOutcome, LeaseSnapshot, lease_now_ms,
    };
    use serenity::model::id::ChannelId;
    use std::sync::Arc;

    fn watcher(id: u64) -> LeaseHolder {
        LeaseHolder::Watcher { instance_id: id }
    }

    fn deadline_of(cell: &DeliveryLeaseCell) -> Option<u64> {
        match cell.read() {
            LeaseSnapshot::Leased { deadline_ms, .. } => Some(deadline_ms),
            _ => None,
        }
    }

    fn lease_turn(ch: ChannelId, user_msg_id: u64) -> DeliveryLeaseKey {
        DeliveryLeaseKey::from_turn_key(TurnKey::new(ch, user_msg_id, 0))
    }

    /// (a) A send that runs LONGER than the (short) deadline, but with the
    /// heartbeat renewing every interval, is NEVER reclaimed mid-send: the
    /// ORIGINAL holder's commit SUCCEEDS and advances the offset exactly once.
    /// We acquire with a deliberately SHORT deadline (would expire almost
    /// immediately), then let the heartbeat push it far forward, and confirm a
    /// reclaim attempt well past the original deadline is a no-op.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn long_send_heartbeat_renew_prevents_midsend_reclaim() {
        let ch = ChannelId::new(7101);
        let cell = Arc::new(DeliveryLeaseCell::new(ch));
        let turn = lease_turn(ch, 11);
        let h = watcher(1);

        // Acquire with a TINY deadline relative to lease_now_ms(): without a
        // heartbeat it would be reclaimable almost immediately.
        let acquire_now = lease_now_ms();
        let short_deadline = acquire_now.saturating_add(100);
        assert!(cell.try_acquire(turn.clone(), h, 0, 64, short_deadline));
        assert_eq!(deadline_of(&cell), Some(short_deadline));

        // Start the heartbeat (owned by this "watcher" frame).
        let hb = DeliveryLeaseHeartbeat::spawn(cell.clone(), h, turn.clone());

        // Drive the gated clock across SEVERAL heartbeat intervals — i.e. a
        // long multi-chunk send. Each crossed interval fires one renew.
        for _ in 0..6 {
            tokio::time::advance(std::time::Duration::from_millis(
                WATCHER_DELIVERY_LEASE_HEARTBEAT_MS,
            ))
            .await;
            tokio::task::yield_now().await;
        }

        // The heartbeat has pushed the deadline far beyond the original short
        // one: it is now lease_now_ms()+DEADLINE_MS (a much larger value).
        let renewed_deadline = deadline_of(&cell).expect("still Leased mid-send");
        assert!(
            renewed_deadline > short_deadline,
            "heartbeat must have renewed the deadline forward (was {short_deadline}, now {renewed_deadline})"
        );

        // A reclaim attempt at a time PAST the ORIGINAL short deadline (but
        // before the renewed one) is a no-op — the live holder is protected.
        assert!(
            !cell.reclaim_if_expired(short_deadline.saturating_add(1)),
            "a renewed (live) lease must NOT be reclaimed past its original deadline"
        );

        // Stop the heartbeat (as the watcher does before committing), then the
        // ORIGINAL holder commits successfully and advances exactly once.
        hb.stop();
        tokio::task::yield_now().await;
        assert!(
            cell.commit(h, turn, 0, 64, LeaseOutcome::Delivered),
            "the original holder's own commit must succeed (lease never lost)"
        );
        match cell.read() {
            LeaseSnapshot::Committed { outcome, end, .. } => {
                assert_eq!(outcome, LeaseOutcome::Delivered);
                assert_eq!(end, 64);
            }
            other => panic!("expected Committed, got {other:?}"),
        }
    }

    /// (b) A holder that "dies" (its heartbeat is dropped/stopped and never
    /// renews) lets the SHORT deadline elapse, so a replacement reclaims and
    /// acquires. We simulate death by dropping the heartbeat handle BEFORE the
    /// renew interval fires, then asserting a reclaim past the (un-renewed)
    /// deadline succeeds and a replacement acquires.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn dead_holder_no_renew_is_reclaimed_after_short_deadline() {
        let ch = ChannelId::new(7102);
        let cell = Arc::new(DeliveryLeaseCell::new(ch));
        let turn = lease_turn(ch, 22);
        let dead = watcher(1);

        let acquire_now = lease_now_ms();
        let deadline = acquire_now.saturating_add(WATCHER_DELIVERY_LEASE_DEADLINE_MS);
        assert!(cell.try_acquire(turn.clone(), dead, 0, 40, deadline));

        // The holder "dies": its heartbeat is dropped immediately (Drop aborts
        // it) WITHOUT ever renewing.
        let hb = DeliveryLeaseHeartbeat::spawn(cell.clone(), dead, turn);
        drop(hb);
        tokio::task::yield_now().await;

        // Before the deadline: NOT reclaimable (single-holder still honored).
        assert!(!cell.reclaim_if_expired(deadline.saturating_sub(1)));
        // Past the (un-renewed, short) deadline: a replacement reclaims it.
        assert!(
            cell.reclaim_if_expired(deadline),
            "a dead holder that stopped heartbeating is reclaimed after the short deadline"
        );
        // And a replacement (new instance, new turn) can acquire the freed cell.
        let replacement = watcher(2);
        let turn_b = lease_turn(ch, 33);
        assert!(
            cell.try_acquire(
                turn_b,
                replacement,
                40,
                72,
                deadline.saturating_add(WATCHER_DELIVERY_LEASE_DEADLINE_MS),
            ),
            "a reclaimed cell is acquirable by the replacement (no black-hole)"
        );
    }

    /// (c) `renew` by a NON-holder, or for the WRONG turn, is a no-op (false)
    /// and does NOT touch the live holder's deadline — the heartbeat of one
    /// holder can never extend (or steal) another's lease.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn renew_by_non_holder_or_wrong_turn_is_noop() {
        let ch = ChannelId::new(7103);
        let cell = DeliveryLeaseCell::new(ch);
        let turn = lease_turn(ch, 44);
        let holder = watcher(1);

        let now = lease_now_ms();
        let deadline = now.saturating_add(1_000);
        assert!(cell.try_acquire(turn.clone(), holder, 0, 16, deadline));

        // Wrong holder, correct turn → no-op.
        assert!(
            !cell.renew(watcher(2), turn.clone(), now.saturating_add(99_999)),
            "a different holder cannot renew the lease"
        );
        // Correct holder, wrong (stale) turn → no-op.
        let wrong_turn = lease_turn(ch, 45);
        assert!(
            !cell.renew(holder, wrong_turn, now.saturating_add(99_999)),
            "a stale/wrong turn cannot renew the lease"
        );
        // The deadline is UNCHANGED by the rejected renews.
        assert_eq!(
            deadline_of(&cell),
            Some(deadline),
            "rejected renews must not mutate the deadline"
        );

        // The TRUE holder/turn CAN renew → deadline extends.
        let extended = now.saturating_add(WATCHER_DELIVERY_LEASE_DEADLINE_MS);
        assert!(cell.renew(holder, turn.clone(), extended));
        assert_eq!(deadline_of(&cell), Some(extended));

        // After commit (Committed, not Leased) even the true holder's renew
        // no-ops — a late heartbeat tick after commit cannot disturb the cell.
        assert!(cell.commit(holder, turn.clone(), 0, 16, LeaseOutcome::Delivered));
        assert!(
            !cell.renew(holder, turn, extended.saturating_add(1)),
            "a renew on a Committed lease (a late tick after commit) is a no-op"
        );
    }
}

// #3089 A4 — controller-path characterization for the watcher short-replace
// cutover. Drives the REAL `toc::deliver_turn_output` + a real per-channel
// `DeliveryLeaseCell` with a fake `TurnGateway` (mirrors the A2b sink suite), so
// the ctx the production `deliver_short_replace_via_controller` builds
// (holder=Watcher, Transient, Replace{Active}, PreserveAlways, CommitOnFallback,
// identity-gated advance, heartbeat) is exercised end-to-end. Pinned inline in
// this `#[cfg(test)] mod tests` block of the FROZEN file => ZERO production LoC.
mod watcher_short_replace_controller {
    use super::super::terminal_long_chunks::{
        WatcherLongChunksLocals, apply_watcher_long_chunks_result,
        deliver_long_chunks_via_controller, remember_ordered_long_chunks_footer_target,
    };
    use super::super::terminal_send::{
        WatcherShortReplaceLocals, WatcherShortReplaceResult, apply_watcher_short_replace_result,
        deliver_short_replace_via_controller, watcher_terminal_lease_range,
    };
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::gateway::{GatewayFuture, TurnGateway};
    use crate::services::discord::inflight::RelayOwnerKind;
    use crate::services::discord::outbound::turn_output_controller as toc;
    use crate::services::discord::placeholder_controller::{
        PlaceholderController, PlaceholderKey, PlaceholderLifecycle,
    };
    use crate::services::discord::replace_outcome_policy::{
        WatcherSendFailureClass, watcher_send_failure_classified_message,
    };
    use crate::services::discord::turn_finalizer::TurnKey;
    use crate::services::discord::{
        DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder, LeaseSnapshot, lease_now_ms,
    };
    use crate::services::provider::ProviderKind;
    use serenity::all::{ChannelId, Http, MessageId};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // A fake `TurnGateway` whose `replace_message_with_outcome` returns a fixed
    // outcome (or `Err`) and counts transport calls. All other methods panic — the
    // short-replace path must touch ONLY `replace_message_with_outcome` (the
    // `Active` lifecycle keeps `post_send_finalize` a no-op, so no `edit_message`).
    struct ShortReplaceFakeGateway {
        outcome: ReplaceLongMessageOutcome,
        ok: bool,
        failure_class: WatcherSendFailureClass,
        replace_calls: AtomicUsize,
    }

    impl TurnGateway for ShortReplaceFakeGateway {
        fn replace_message_with_outcome<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            Box::pin(async move {
                self.replace_calls.fetch_add(1, Ordering::SeqCst);
                if self.ok {
                    Ok(self.outcome.clone())
                } else {
                    Err(watcher_send_failure_classified_message(
                        self.failure_class,
                        "fake transport failure",
                    ))
                }
            })
        }
        fn send_message<'a>(
            &'a self,
            _c: ChannelId,
            _x: &'a str,
        ) -> GatewayFuture<'a, Result<MessageId, String>> {
            panic!("short-replace never sends a new message")
        }
        fn edit_message<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _x: &'a str,
        ) -> GatewayFuture<'a, Result<(), String>> {
            panic!("Active lifecycle → post_send_finalize no-op → no edit")
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _c: ChannelId,
            _u: MessageId,
            _t: &'a str,
        ) -> GatewayFuture<'a, ()> {
            panic!("unused on the short-replace path")
        }
        fn dispatch_queued_turn<'a>(
            &'a self,
            _c: ChannelId,
            _i: &'a crate::services::discord::Intervention,
            _o: &'a str,
            _h: bool,
            _dispatch_lease: Option<
                std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>,
            >,
        ) -> GatewayFuture<'a, Result<(), String>> {
            panic!("unused on the short-replace path")
        }
        fn validate_live_routing<'a>(
            &'a self,
            _c: ChannelId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            panic!("unused on the short-replace path")
        }
        fn requester_mention(&self) -> Option<String> {
            None
        }
        fn can_chain_locally(&self) -> bool {
            false
        }
        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            None
        }
    }

    struct LongChunksFakeGateway {
        send_ok: bool,
        delete_ok: bool,
        send_calls: AtomicUsize,
        delete_calls: AtomicUsize,
        clock: AtomicUsize,
        send_step: AtomicUsize,
        delete_step: AtomicUsize,
    }

    impl TurnGateway for LongChunksFakeGateway {
        fn send_long_message_with_rollback<'a>(
            &'a self,
            _c: ChannelId,
            _a: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<Vec<MessageId>, String>> {
            Box::pin(async move {
                self.send_calls.fetch_add(1, Ordering::SeqCst);
                self.send_step
                    .store(self.clock.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
                if self.send_ok {
                    Ok(vec![MessageId::new(9100), MessageId::new(9101)])
                } else {
                    Err("chunk send failed after rollback".to_string())
                }
            })
        }
        fn delete_message<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.delete_calls.fetch_add(1, Ordering::SeqCst);
                self.delete_step
                    .store(self.clock.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
                if self.delete_ok {
                    Ok(())
                } else {
                    Err("delete failed".to_string())
                }
            })
        }
        fn replace_message_with_outcome<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            panic!("long chunks never replace")
        }
        fn send_message<'a>(
            &'a self,
            _c: ChannelId,
            _x: &'a str,
        ) -> GatewayFuture<'a, Result<MessageId, String>> {
            panic!("long chunks use send_long_message_with_rollback")
        }
        fn edit_message<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _x: &'a str,
        ) -> GatewayFuture<'a, Result<(), String>> {
            panic!("long chunks never edit")
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _c: ChannelId,
            _u: MessageId,
            _t: &'a str,
        ) -> GatewayFuture<'a, ()> {
            panic!("unused on long chunks")
        }
        fn dispatch_queued_turn<'a>(
            &'a self,
            _c: ChannelId,
            _i: &'a crate::services::discord::Intervention,
            _o: &'a str,
            _h: bool,
            _dispatch_lease: Option<
                std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>,
            >,
        ) -> GatewayFuture<'a, Result<(), String>> {
            panic!("unused on long chunks")
        }
        fn validate_live_routing<'a>(
            &'a self,
            _c: ChannelId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            panic!("unused on long chunks")
        }
        fn requester_mention(&self) -> Option<String> {
            None
        }
        fn can_chain_locally(&self) -> bool {
            true
        }
        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            None
        }
    }

    const CH: u64 = 8_141;
    const MSG: u64 = 99;
    const START: u64 = 10;
    const END: u64 = 42;
    const INSTANCE: u64 = 7;

    fn ch() -> ChannelId {
        ChannelId::new(CH)
    }
    fn turn() -> TurnKey {
        TurnKey::new(ch(), 11, 0)
    }
    fn lease_key() -> DeliveryLeaseKey {
        DeliveryLeaseKey::from_turn_key(turn())
    }
    fn watcher() -> LeaseHolder {
        LeaseHolder::Watcher {
            instance_id: INSTANCE,
        }
    }
    fn gateway(outcome: ReplaceLongMessageOutcome, ok: bool) -> ShortReplaceFakeGateway {
        gateway_with_failure_class(outcome, ok, WatcherSendFailureClass::Transient)
    }

    fn gateway_with_failure_class(
        outcome: ReplaceLongMessageOutcome,
        ok: bool,
        failure_class: WatcherSendFailureClass,
    ) -> ShortReplaceFakeGateway {
        ShortReplaceFakeGateway {
            outcome,
            ok,
            failure_class,
            replace_calls: AtomicUsize::new(0),
        }
    }

    fn long_gateway(send_ok: bool, delete_ok: bool) -> LongChunksFakeGateway {
        LongChunksFakeGateway {
            send_ok,
            delete_ok,
            send_calls: AtomicUsize::new(0),
            delete_calls: AtomicUsize::new(0),
            clock: AtomicUsize::new(1),
            send_step: AtomicUsize::new(0),
            delete_step: AtomicUsize::new(0),
        }
    }

    // Drive the REAL controller through the production helper with a fresh cell.
    // `advance_returns` is irrelevant to the PRODUCTION advance (which calls the
    // real `advance_watcher_confirmed_end`); the test cell + a make_shared driver is
    // used in the offset-advance test instead. Here we observe the result + lease.
    async fn run(
        gw: &ShortReplaceFakeGateway,
        shared: &Arc<crate::services::discord::SharedData>,
        cell: &Arc<DeliveryLeaseCell>,
    ) -> WatcherShortReplaceResult {
        deliver_short_replace_via_controller(
            gw,
            shared,
            &ProviderKind::Claude,
            ch(),
            "AgentDesk-claude-8141",
            MessageId::new(MSG),
            "answer",
            "answer",
            cell,
            turn(),
            Some(lease_key()),
            INSTANCE,
            START,
            END,
        )
        .await
    }

    async fn run_long(
        gw: &LongChunksFakeGateway,
        shared: &Arc<crate::services::discord::SharedData>,
        cell: &Arc<DeliveryLeaseCell>,
    ) -> toc::DeliveryOutcome {
        deliver_long_chunks_via_controller(
            gw,
            shared,
            &ProviderKind::Claude,
            ch(),
            "AgentDesk-claude-8141",
            MessageId::new(MSG),
            &"x".repeat(crate::services::discord::DISCORD_MSG_LIMIT + 10),
            &"x".repeat(crate::services::discord::DISCORD_MSG_LIMIT + 10),
            cell,
            turn(),
            Some(lease_key()),
            INSTANCE,
            START,
            END,
        )
        .await
    }

    fn toc_debug_outcome(outcome: &toc::DeliveryOutcome) -> &'static str {
        match outcome {
            toc::DeliveryOutcome::Delivered { .. } => "Delivered",
            toc::DeliveryOutcome::FreshDelivered { .. } => "FreshDelivered",
            toc::DeliveryOutcome::NotDelivered { .. } => "NotDelivered",
            toc::DeliveryOutcome::Transient { .. } => "Transient",
            toc::DeliveryOutcome::Unknown { .. } => "Unknown",
            toc::DeliveryOutcome::Skipped => "Skipped",
        }
    }

    // (1) lease pre-held by ANOTHER holder → controller acquire fails →
    // AcquireFailureMode::Transient → B2Skip, NO transport. Mutation:
    // `Transient`→`ProceedMarkerless` in the sibling would POST → replace_calls=1
    // and the result would not be B2Skip.
    #[tokio::test(flavor = "current_thread")]
    async fn watcher_short_replace_acquire_transient_no_send() {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let cell = Arc::new(DeliveryLeaseCell::new(ch()));
        // A DIFFERENT holder owns the exact range with a FRESH deadline → the
        // controller's `try_acquire` loses and `reclaim_if_expired` cannot free it.
        let other = LeaseHolder::Watcher { instance_id: 999 };
        assert!(cell.try_acquire(
            lease_key(),
            other,
            START,
            END,
            lease_now_ms().saturating_add(60_000),
        ));
        let gw = gateway(ReplaceLongMessageOutcome::EditedOriginal, true);
        let result = run(&gw, &shared, &cell).await;
        assert_eq!(
            result,
            WatcherShortReplaceResult::B2Skip,
            "a lost acquire is the B2-skip equivalent (Transient), not a send"
        );
        assert_eq!(
            gw.replace_calls.load(Ordering::SeqCst),
            0,
            "Transient acquire-fail MUST NOT POST (mutation to ProceedMarkerless POSTs)"
        );
    }

    // (2) confirmed `EditedOriginal` → the production advance runs the REAL
    // `advance_watcher_confirmed_end` (returns true) → Delivered AND the shared
    // `confirmed_end_offset` watermark advances to END. A mutation making the
    // advance callback unconditional would still advance here, so this test pins
    // Delivered + the real watermark move (the offset is the decisive assertion).
    #[test]
    fn watcher_short_replace_advance_identity_gate() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("runtime root tempdir failed: {error}"),
        };
        let _root = super::AgentdeskRootGuard::set(temp.path());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        runtime.block_on(async {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let cell = Arc::new(DeliveryLeaseCell::new(ch()));
            assert_eq!(shared.committed_relay_offset(ch()), 0);
            let gw = gateway(ReplaceLongMessageOutcome::EditedOriginal, true);
            let result = run(&gw, &shared, &cell).await;
            assert_eq!(result, WatcherShortReplaceResult::Delivered);
            assert_eq!(gw.replace_calls.load(Ordering::SeqCst), 1, "one POST");
            assert_eq!(
                shared.committed_relay_offset(ch()),
                END,
                "confirmed transport advances the watermark to the leased end"
            );
            assert!(
                matches!(cell.read(), LeaseSnapshot::Unleased),
                "controller committed then released the single lease (no leftover)"
            );
        });
    }

    // (3) #2757 PreserveAlways: a `SentFallbackAfterEditFailure` + post-send
    // `EditFailed` must NOT delete the original. The `Active` lifecycle keeps
    // `post_send_finalize` a no-op, so the fake's `delete_message` (default Ok)
    // would never be called regardless; the load-bearing pin is that the sibling
    // passes `PreserveAlways` — `watcher_short_replace_preserve_always` (below, the
    // controller-level test) proves a mutation to `DeleteIfProvenStale` deletes.
    #[tokio::test(flavor = "current_thread")]
    async fn watcher_short_replace_preserve_always() {
        // The watcher cutover passes a NON-terminal `Replace { Active }`, so
        // `post_send_finalize` returns before any `transition`/delete. Drive the
        // controller with a TERMINAL lifecycle + the fake's delete recorder to PROVE
        // the policy mapping: PreserveAlways → no delete; DeleteIfProvenStale →
        // delete. This is the #2757 fence the watcher relies on (its effective
        // policy is PreserveAlways because the delete predicate is const-false).
        struct DeleteRecorder {
            deletes: AtomicUsize,
            edit_fails: std::sync::atomic::AtomicBool,
        }
        impl TurnGateway for DeleteRecorder {
            fn replace_message_with_outcome<'a>(
                &'a self,
                _c: ChannelId,
                _m: MessageId,
                _x: &'a str,
            ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
                Box::pin(async move {
                    Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                        edit_error: "edit failed".to_string(),
                        replacement_anchor: None,
                    })
                })
            }
            fn edit_message<'a>(
                &'a self,
                _c: ChannelId,
                _m: MessageId,
                _x: &'a str,
            ) -> GatewayFuture<'a, Result<(), String>> {
                // The prime `ensure_active` edit must SUCCEED so the card goes Active;
                // the terminal `transition` edit then FAILS → `EditFailed` → #2757 fence.
                Box::pin(async move {
                    if self.edit_fails.load(Ordering::SeqCst) {
                        Err("patch failed".to_string())
                    } else {
                        Ok(())
                    }
                })
            }
            fn delete_message<'a>(
                &'a self,
                _c: ChannelId,
                _m: MessageId,
            ) -> GatewayFuture<'a, Result<(), String>> {
                self.deletes.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok(()) })
            }
            fn send_message<'a>(
                &'a self,
                _c: ChannelId,
                _x: &'a str,
            ) -> GatewayFuture<'a, Result<MessageId, String>> {
                panic!("unused")
            }

            fn schedule_retry_with_history<'a>(
                &'a self,
                _c: ChannelId,
                _u: MessageId,
                _t: &'a str,
            ) -> GatewayFuture<'a, ()> {
                panic!("unused")
            }
            fn dispatch_queued_turn<'a>(
                &'a self,
                _c: ChannelId,
                _i: &'a crate::services::discord::Intervention,
                _o: &'a str,
                _h: bool,
                _dispatch_lease: Option<
                    std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>,
                >,
            ) -> GatewayFuture<'a, Result<(), String>> {
                panic!("unused")
            }
            fn validate_live_routing<'a>(
                &'a self,
                _c: ChannelId,
            ) -> GatewayFuture<'a, Result<(), String>> {
                panic!("unused")
            }
            fn requester_mention(&self) -> Option<String> {
                None
            }
            fn can_chain_locally(&self) -> bool {
                false
            }
            fn bot_owner_provider(&self) -> Option<ProviderKind> {
                None
            }
        }

        async fn drive(policy: toc::EditFailPlaceholderPolicy) -> usize {
            use crate::services::discord::formatting::MonitorHandoffReason;
            use crate::services::discord::placeholder_controller::PlaceholderActiveInput;
            let cell = Arc::new(DeliveryLeaseCell::new(ch()));
            let controller = PlaceholderController::default();
            let gw = DeleteRecorder {
                deletes: AtomicUsize::new(0),
                edit_fails: std::sync::atomic::AtomicBool::new(false),
            };
            let key = PlaceholderKey {
                provider: ProviderKind::Claude,
                channel_id: ch(),
                message_id: MessageId::new(MSG),
            };
            // Prime the card Active (the prime edit succeeds), then make the terminal
            // transition's edit FAIL so `post_send_finalize` sees `EditFailed`.
            let primed = controller
                .ensure_active(
                    &gw,
                    key.clone(),
                    PlaceholderActiveInput {
                        reason: MonitorHandoffReason::ExplicitCall,
                        started_at_unix: 1_700_000_000,
                        tool_summary: None,
                        command_summary: None,
                        reason_detail: None,
                        context_line: None,
                        request_line: None,
                        progress_line: None,
                    },
                )
                .await;
            assert_eq!(
                    primed,
                    crate::services::discord::placeholder_controller::PlaceholderControllerOutcome::Edited,
                    "prime put the card Active"
                );
            gw.edit_fails.store(true, Ordering::SeqCst);
            let advance = |_r: (u64, u64)| -> bool { true };
            let _ = toc::deliver_turn_output(
                &gw,
                toc::TurnOutputCtx {
                    turn: turn(),
                    lease_key: Some(crate::services::discord::DeliveryLeaseKey::from_turn_key(
                        turn(),
                    )),
                    owner: RelayOwnerKind::Watcher,
                    holder: watcher(),
                    lease: &*cell,
                    channel_id: ch(),
                    placeholder_controller: &controller,
                    placeholder: toc::PlaceholderSlot::Active {
                        message_id: MessageId::new(MSG),
                        key: key.clone(),
                    },
                    body: "answer",
                    send_range: (START, END),
                    // TERMINAL lifecycle so `post_send_finalize` runs the transition →
                    // EditFailed → engages the #2757 fence.
                    plan: toc::OutputPlan::Replace {
                        lifecycle: PlaceholderLifecycle::Completed,
                    },
                    edit_fail_policy: policy,
                    fallback_commit_policy: toc::FallbackCommitPolicy::CommitOnFallback,
                    acquire_failure_mode: toc::AcquireFailureMode::Transient,
                    advance: Some(&advance),
                    heartbeat: None,
                },
            )
            .await;
            gw.deletes.load(Ordering::SeqCst)
        }

        // The watcher's policy (PreserveAlways) NEVER deletes on EditFailed (#2757).
        assert_eq!(
            drive(toc::EditFailPlaceholderPolicy::PreserveAlways).await,
            0,
            "PreserveAlways (the watcher's effective policy) must not delete the original"
        );
        // The mutation (DeleteIfProvenStale) DOES delete → proves the mapping is
        // load-bearing.
        assert_eq!(
            drive(toc::EditFailPlaceholderPolicy::DeleteIfProvenStale).await,
            1,
            "DeleteIfProvenStale deletes — so passing PreserveAlways is load-bearing"
        );
    }

    // (4) #3151: a slow transport renews the lease during the POST and the heartbeat
    // is stopped BEFORE the inline commit. Reuse the cell-clock pattern from
    // `delivery_lease_heartbeat`: acquire with a TINY deadline, let a renew push it
    // forward mid-POST, confirm a reclaim past the original deadline is a no-op, then
    // the commit succeeds. (The controller drives its own heartbeat via the sibling's
    // `WatcherPostHeartbeat`; here we assert the renew-before-commit ordering on the
    // SAME cell the controller commits to — Delivered with the lease released.)
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watcher_short_replace_heartbeat_before_commit() {
        use super::super::WATCHER_DELIVERY_LEASE_HEARTBEAT_MS;
        use crate::services::discord::DeliveryLeaseHeartbeat;
        let cell = Arc::new(DeliveryLeaseCell::new(ch()));
        let short = lease_now_ms().saturating_add(100);
        assert!(cell.try_acquire(lease_key(), watcher(), START, END, short));
        let hb = DeliveryLeaseHeartbeat::spawn(cell.clone(), watcher(), lease_key());
        for _ in 0..3 {
            tokio::time::advance(std::time::Duration::from_millis(
                WATCHER_DELIVERY_LEASE_HEARTBEAT_MS,
            ))
            .await;
            tokio::task::yield_now().await;
        }
        let renewed = match cell.read() {
            LeaseSnapshot::Leased { deadline_ms, .. } => deadline_ms,
            other => panic!("still Leased mid-POST, got {other:?}"),
        };
        assert!(renewed > short, "heartbeat renewed the deadline forward");
        assert!(
            !cell.reclaim_if_expired(short.saturating_add(1)),
            "a renewed (live) lease is NOT reclaimed mid-POST (#3151)"
        );
        // STOP before commit (the controller drops the heartbeat guard before the
        // inline commit), then the commit succeeds — ordering held.
        hb.stop();
        tokio::task::yield_now().await;
        assert!(
            cell.commit(
                watcher(),
                lease_key(),
                START,
                END,
                crate::services::discord::LeaseOutcome::Delivered
            ),
            "the original holder's own commit succeeds after heartbeat-stop"
        );
    }

    // (5) FallbackCommitPolicy: `SentFallbackAfterEditFailure` + CommitOnFallback →
    // DeliveredFallback (advance, but carries the replace identity + `edit_error`
    // so the write-back mirrors the legacy fallback arm — #3089 A4 r2);
    // `PartialContinuationFailure` → Unknown → PartialFailureRetry (no advance,
    // I2). The offset must NOT advance on the partial path but MUST on fallback.
    #[test]
    fn watcher_short_replace_fallback_commit_policy() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("runtime root tempdir failed: {error}"),
        };
        let _root = super::AgentdeskRootGuard::set(temp.path());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        runtime.block_on(async {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let cell = Arc::new(DeliveryLeaseCell::new(ch()));
            let gw = gateway(
                ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                    edit_error: "edit failed".to_string(),
                    replacement_anchor: None,
                },
                true,
            );
            assert_eq!(
                run(&gw, &shared, &cell).await,
                WatcherShortReplaceResult::DeliveredFallback {
                    edit_error: "edit failed".to_string(),
                    replacement_anchor: None,
                },
                "CommitOnFallback maps SentFallbackAfterEditFailure → DeliveredFallback \
                     (advances, surfaces the replace identity + edit_error)"
            );
            assert_eq!(shared.committed_relay_offset(ch()), END);

            let shared2 = crate::services::discord::make_shared_data_for_tests();
            let cell2 = Arc::new(DeliveryLeaseCell::new(ch()));
            let gw2 = gateway(
                ReplaceLongMessageOutcome::PartialContinuationFailure {
                    sent_chunks: 1,
                    total_chunks: 2,
                    failed_chunk_index: 1,
                    sent_continuation_message_ids: vec![1],
                    cleanup_errors: vec![],
                    error: "mid-stream".to_string(),
                },
                true,
            );
            assert_eq!(
                run(&gw2, &shared2, &cell2).await,
                WatcherShortReplaceResult::PartialFailureRetry,
                "PartialContinuationFailure → Unknown → PartialFailureRetry (I2)"
            );
            assert_eq!(
                shared2.committed_relay_offset(ch()),
                0,
                "I2: a partial/ambiguous result NEVER advances the offset"
            );
        });
    }

    #[tokio::test(flavor = "current_thread")]
    async fn watcher_short_replace_transport_failure_requests_offset_retry_4115() {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let cell = Arc::new(DeliveryLeaseCell::new(ch()));
        let gw = gateway(ReplaceLongMessageOutcome::EditedOriginal, false);
        let result = run(&gw, &shared, &cell).await;
        assert_eq!(
            result,
            WatcherShortReplaceResult::PartialFailureRetry,
            "transport failure means no confirmed body; watcher must retry the same range"
        );
        assert_eq!(gw.replace_calls.load(Ordering::SeqCst), 1);
        assert_eq!(shared.committed_relay_offset(ch()), 0);

        let mut relay_ok = true;
        let mut direct_send_delivered = false;
        let mut tui_direct_anchor_terminal_body_visible = false;
        let mut external_input_lease_consumed_by_relay = false;
        let mut placeholder_msg_id = Some(MessageId::new(MSG));
        let mut placeholder_from_restored_inflight = true;
        let mut last_edit_text = "streamed body".to_string();
        let mut completion_footer_terminal_target = None;
        let mut retry_terminal_delivery_from_offset = false;
        apply_watcher_short_replace_result(
            result,
            &shared,
            &ProviderKind::Claude,
            ch(),
            "AgentDesk-claude-8141",
            MessageId::new(MSG),
            "answer",
            false,
            None,
            WatcherShortReplaceLocals {
                relay_ok: &mut relay_ok,
                direct_send_delivered: &mut direct_send_delivered,
                tui_direct_anchor_terminal_body_visible:
                    &mut tui_direct_anchor_terminal_body_visible,
                external_input_lease_consumed_by_relay: &mut external_input_lease_consumed_by_relay,
                placeholder_msg_id: &mut placeholder_msg_id,
                placeholder_from_restored_inflight: &mut placeholder_from_restored_inflight,
                last_edit_text: &mut last_edit_text,
                completion_footer_terminal_target: &mut completion_footer_terminal_target,
                retry_terminal_delivery_from_offset: &mut retry_terminal_delivery_from_offset,
            },
        );
        assert!(!relay_ok);
        assert!(retry_terminal_delivery_from_offset);
        assert!(!direct_send_delivered);
        assert_eq!(placeholder_msg_id, Some(MessageId::new(MSG)));
        assert!(placeholder_from_restored_inflight);
        assert_eq!(last_edit_text, "streamed body");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn watcher_short_replace_permanent_transport_failure_does_not_rewind_4154() {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let cell = Arc::new(DeliveryLeaseCell::new(ch()));
        let gw = gateway_with_failure_class(
            ReplaceLongMessageOutcome::EditedOriginal,
            false,
            WatcherSendFailureClass::Permanent,
        );
        let result = run(&gw, &shared, &cell).await;
        assert_eq!(
            result,
            WatcherShortReplaceResult::Skipped,
            "permanent controller transport failure must be non-retry"
        );
        assert_eq!(gw.replace_calls.load(Ordering::SeqCst), 1);
        assert_eq!(shared.committed_relay_offset(ch()), 0);

        let mut relay_ok = true;
        let mut direct_send_delivered = false;
        let mut tui_direct_anchor_terminal_body_visible = false;
        let mut external_input_lease_consumed_by_relay = false;
        let mut placeholder_msg_id = Some(MessageId::new(MSG));
        let mut placeholder_from_restored_inflight = true;
        let mut last_edit_text = "streamed body".to_string();
        let mut completion_footer_terminal_target = None;
        let mut retry_terminal_delivery_from_offset = false;
        apply_watcher_short_replace_result(
            result,
            &shared,
            &ProviderKind::Claude,
            ch(),
            "AgentDesk-claude-8141",
            MessageId::new(MSG),
            "answer",
            false,
            None,
            WatcherShortReplaceLocals {
                relay_ok: &mut relay_ok,
                direct_send_delivered: &mut direct_send_delivered,
                tui_direct_anchor_terminal_body_visible:
                    &mut tui_direct_anchor_terminal_body_visible,
                external_input_lease_consumed_by_relay: &mut external_input_lease_consumed_by_relay,
                placeholder_msg_id: &mut placeholder_msg_id,
                placeholder_from_restored_inflight: &mut placeholder_from_restored_inflight,
                last_edit_text: &mut last_edit_text,
                completion_footer_terminal_target: &mut completion_footer_terminal_target,
                retry_terminal_delivery_from_offset: &mut retry_terminal_delivery_from_offset,
            },
        );
        assert!(!relay_ok);
        assert!(
            !retry_terminal_delivery_from_offset,
            "permanent controller failure must not request rewind"
        );
        assert!(!direct_send_delivered);
        assert_eq!(placeholder_msg_id, Some(MessageId::new(MSG)));
        assert!(placeholder_from_restored_inflight);
        assert_eq!(last_edit_text, "streamed body");
    }

    #[test]
    fn watcher_long_chunks_controller_delivered_deletes_anchor_and_advances() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("runtime root tempdir failed: {error}"),
        };
        let _root = super::AgentdeskRootGuard::set(temp.path());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        runtime.block_on(async {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let cell = Arc::new(DeliveryLeaseCell::new(ch()));
            let gw = long_gateway(true, true);
            let outcome = run_long(&gw, &shared, &cell).await;
            match outcome {
                toc::DeliveryOutcome::Delivered {
                    new_chunks: Some(chunks),
                    ..
                } => {
                    assert_eq!(chunks.first_message_id, Some(MessageId::new(9100)));
                    assert_eq!(chunks.tail_message_id, Some(MessageId::new(9101)));
                    assert_eq!(chunks.anchor_delete_error, None);
                }
                other => panic!("expected Delivered, got {}", toc_debug_outcome(&other)),
            }
            assert_eq!(gw.send_calls.load(Ordering::SeqCst), 1);
            assert_eq!(gw.delete_calls.load(Ordering::SeqCst), 1);
            assert!(
                gw.send_step.load(Ordering::SeqCst) < gw.delete_step.load(Ordering::SeqCst),
                "placeholder delete must run after the full chunk send"
            );
            assert_eq!(shared.committed_relay_offset(ch()), END);
            assert!(matches!(cell.read(), LeaseSnapshot::Unleased));
        });
    }

    #[test]
    fn ordered_long_chunks_footer_target_uses_tail_message_and_text_4822() {
        let mut footer_target = None;
        let relay_text = format!("{}tail", "a".repeat(2_000));
        let expected_tail = crate::services::discord::formatting::split_message(&relay_text)
            .pop()
            .expect("long response has a tail chunk");

        remember_ordered_long_chunks_footer_target(
            true,
            &mut footer_target,
            Some(MessageId::new(9101)),
            &relay_text,
        );

        let target = footer_target.expect("ordered chunks must register footer tail");
        assert_eq!(target.msg_id, MessageId::new(9101));
        assert_eq!(target.text, expected_tail);
        assert!(target.text.ends_with("tail"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn watcher_long_chunks_controller_registers_tail_footer_target_4822() {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let http = Arc::new(Http::new("test-token"));
        let mut relay_ok = true;
        let mut direct = false;
        let mut visible = false;
        let mut external = false;
        let mut placeholder = Some(MessageId::new(MSG));
        let mut restored = true;
        let mut last_edit = String::from("streamed");
        let mut frozen = Vec::new();
        let mut footer_target = None;
        let relay_text = format!("{}tail", "a".repeat(2_000));
        let outcome = toc::DeliveryOutcome::Delivered {
            committed_to: END,
            replace_kind: None,
            new_chunks: Some(toc::NewChunksDelivery {
                first_message_id: Some(MessageId::new(9100)),
                tail_message_id: Some(MessageId::new(9101)),
                anchor_delete_error: None,
            }),
        };

        apply_watcher_long_chunks_result(
            outcome,
            &http,
            &shared,
            &ProviderKind::Claude,
            ch(),
            "AgentDesk-claude-8141",
            MessageId::new(MSG),
            &relay_text,
            true,
            &mut frozen,
            None,
            WatcherLongChunksLocals {
                relay_ok: &mut relay_ok,
                direct_send_delivered: &mut direct,
                tui_direct_anchor_terminal_body_visible: &mut visible,
                external_input_lease_consumed_by_relay: &mut external,
                placeholder_msg_id: &mut placeholder,
                placeholder_from_restored_inflight: &mut restored,
                last_edit_text: &mut last_edit,
                single_message_panel_footer_mode: true,
                completion_footer_terminal_target: &mut footer_target,
            },
        )
        .await;

        assert!(
            footer_target.is_some(),
            "ordered chunks must register footer tail"
        );
    }

    #[test]
    fn watcher_long_chunks_delete_failure_still_delivers() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("runtime root tempdir failed: {error}"),
        };
        let _root = super::AgentdeskRootGuard::set(temp.path());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        runtime.block_on(async {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let cell = Arc::new(DeliveryLeaseCell::new(ch()));
            let gw = long_gateway(true, false);
            let outcome = run_long(&gw, &shared, &cell).await;
            match outcome {
                toc::DeliveryOutcome::Delivered {
                    new_chunks: Some(chunks),
                    ..
                } => assert_eq!(chunks.anchor_delete_error.as_deref(), Some("delete failed")),
                other => panic!(
                    "delete failure should still be Delivered, got {}",
                    toc_debug_outcome(&other)
                ),
            }
            assert_eq!(shared.committed_relay_offset(ch()), END);
        });
    }

    #[tokio::test(flavor = "current_thread")]
    async fn watcher_long_chunks_send_failure_not_delivered_and_preserved() {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let cell = Arc::new(DeliveryLeaseCell::new(ch()));
        let gw = long_gateway(false, true);
        let outcome = run_long(&gw, &shared, &cell).await;
        assert!(
            matches!(outcome, toc::DeliveryOutcome::NotDelivered { .. }),
            "send failure maps to NotDelivered"
        );
        assert_eq!(gw.delete_calls.load(Ordering::SeqCst), 0);
        assert_eq!(shared.committed_relay_offset(ch()), 0);

        let http = Arc::new(Http::new("test-token"));
        let mut relay_ok = true;
        let mut direct = false;
        let mut visible = false;
        let mut external = false;
        let mut placeholder = Some(MessageId::new(MSG));
        let mut restored = true;
        let mut last_edit = String::from("streamed");
        let mut frozen = Vec::new();
        apply_watcher_long_chunks_result(
            outcome,
            &http,
            &shared,
            &ProviderKind::Claude,
            ch(),
            "AgentDesk-claude-8141",
            MessageId::new(MSG),
            "ordered response",
            true,
            &mut frozen,
            None,
            WatcherLongChunksLocals {
                relay_ok: &mut relay_ok,
                direct_send_delivered: &mut direct,
                tui_direct_anchor_terminal_body_visible: &mut visible,
                external_input_lease_consumed_by_relay: &mut external,
                placeholder_msg_id: &mut placeholder,
                placeholder_from_restored_inflight: &mut restored,
                last_edit_text: &mut last_edit,
                single_message_panel_footer_mode: false,
                completion_footer_terminal_target: &mut None,
            },
        )
        .await;
        assert!(
            !relay_ok,
            "failure maps back to retry-preserving relay_ok=false"
        );
        assert_eq!(placeholder, Some(MessageId::new(MSG)));
        assert!(restored);
        assert_eq!(last_edit, "streamed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn watcher_long_chunks_acquire_transient_no_send() {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let cell = Arc::new(DeliveryLeaseCell::new(ch()));
        assert!(cell.try_acquire(
            lease_key(),
            LeaseHolder::Watcher { instance_id: 999 },
            START,
            END,
            lease_now_ms().saturating_add(60_000),
        ));
        let gw = long_gateway(true, true);
        let outcome = run_long(&gw, &shared, &cell).await;
        assert!(matches!(outcome, toc::DeliveryOutcome::Transient { .. }));
        assert_eq!(gw.send_calls.load(Ordering::SeqCst), 0);
        assert_eq!(shared.committed_relay_offset(ch()), 0);
    }

    // (6) the cut-over turn must SKIP the watcher's own lease acquire (the controller
    // owns the single lease). The pure `watcher_terminal_lease_range` returns None for
    // any cut-over turn; dropping `!cutover_short_replace` (the guard-skip mutation)
    // makes it return `Some(..)` and fails here.
    #[test]
    fn cutover_skips_watcher_lease_acquire() {
        assert_eq!(
            watcher_terminal_lease_range(Some((START, END)), true),
            None,
            "a cut-over turn must NOT acquire the watcher's own lease (no double-acquire)"
        );
        assert_eq!(
            watcher_terminal_lease_range(Some((START, END)), false),
            Some((START, END)),
            "the legacy (non-cutover) path still acquires over the ordered range"
        );
        assert_eq!(watcher_terminal_lease_range(None, false), None);
        assert_eq!(watcher_terminal_lease_range(None, true), None);
    }

    // (7) pure cut-over predicate gate: mutation-pin each load-bearing term.
    #[test]
    fn watcher_terminal_lease_range_pins_cutover() {
        use super::super::terminal_send::watcher_short_replace_cutover as cut;
        // All gate terms satisfied (will-send, ordered, placeholder,
        // not-ordered-chunks, non-empty, not-tui-gated) → cut over.
        assert!(cut(true, true, true, false, false, false));
        // No placeholder → legacy (the prompt-anchor reference-send branch).
        assert!(!cut(true, true, false, false, false, false));
        // should_send_ordered_new_chunks → controller long-chunk route.
        assert!(cut(true, true, true, true, false, false));
        // empty formatted body → legacy (controller would Skip; legacy advances).
        assert!(!cut(true, true, true, false, true, false));
        // TUI-completion-gate required → legacy (post-send lifecycle_stage_paused).
        assert!(!cut(true, true, true, false, false, true));
        // not will-direct-send / not ordered range → legacy.
        assert!(!cut(false, true, true, false, false, false));
        assert!(!cut(true, false, true, false, false, false));
    }

    // (9) #3089 A4 r2 (codex r1 [High]): the controller write-back MUST mirror
    // the legacy per-variant cleanup. `DeliveredFallback`
    // (`SentFallbackAfterEditFailure`) must NOT register the original `msg_id` as
    // the completion-footer target, must record `Failed(edit_error)` cleanup, and
    // must preserve the original placeholder — the OPPOSITE of `EditedOriginal`,
    // which DOES register the footer target + `Succeeded`. Collapsing the
    // FreshFallback branch back to the `EditedOriginal` side-effects (the r1 bug)
    // fails this test on the footer-target + cleanup-outcome assertions.
    //
    // #3089 A4 r3 (codex r2 [Medium]): also PIN the preserve else-branch locals
    // the fallback arm clears. The completion footer at
    // `single_message_footer.rs:399-405` resolves its edit target as
    // `terminal_target.or(fallback_target)` where `fallback_target` is built FROM
    // `placeholder_msg_id`. So the fallback arm not registering a `terminal_target`
    // is NOT sufficient on its own: it MUST ALSO clear `placeholder_msg_id` to None
    // (legacy fallback `else`, tmux_watcher.rs:6356), or the footer's
    // placeholder-derived fallback target would STILL edit the preserved original.
    // We surface the post-apply preserve-branch locals and assert the fallback arm
    // cleared `placeholder_msg_id` (load-bearing), reset
    // `placeholder_from_restored_inflight`, and cleared `last_edit_text`
    // (tmux_watcher.rs:6356-6358), and that the resolved footer target
    // (`terminal_target.or(placeholder_msg_id→fallback)`) is therefore None — the
    // second footer-target path can never reach the preserved original. The
    // `EditedOriginal` arm DOES resolve a footer target (its `terminal_target` is
    // registered, tmux_watcher.rs:6256), so the resolved-target assertion
    // DISCRIMINATES the two arms.
    #[test]
    fn watcher_short_replace_fallback_mirrors_legacy() {
        use super::super::single_message_footer::WatcherCompletionFooterTerminalTarget;
        use super::super::terminal_send::{
            WatcherShortReplaceLocals, WatcherShortReplaceResult,
            apply_watcher_short_replace_result,
        };

        // Post-apply observation of `apply_watcher_short_replace_result`: the
        // footer-target registration, the cleanup record outcome, AND the
        // preserve-branch locals (the #3089 A4 r3 pin). `footer_target_resolves`
        // replays the `single_message_footer.rs:405` resolution
        // (`terminal_target.or(placeholder_msg_id→fallback)`) so we observe whether
        // ANY footer-edit path could still reach the original `msg_id`.
        struct Observed {
            footer_registered: bool,
            /// `single_message_footer.rs:405`: `terminal_target.or(fallback)` where
            /// `fallback = placeholder_msg_id.map(..)`. True iff some path resolves.
            footer_target_resolves: bool,
            committed: bool,
            retry_pending: bool,
            placeholder_msg_id_cleared: bool,
            placeholder_from_restored_inflight_reset: bool,
            last_edit_text_cleared: bool,
        }

        // Drive `apply_watcher_short_replace_result` with `result`.
        // `single_message_panel_footer_mode = true` so an `EditedOriginal`
        // registration is observable (the footer remember is gated on it).
        fn run(result: WatcherShortReplaceResult) -> Observed {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let mut relay_ok = true;
            let mut direct_send_delivered = false;
            let mut tui_direct_anchor_terminal_body_visible = false;
            let mut external_input_lease_consumed_by_relay = false;
            let mut placeholder_msg_id: Option<MessageId> = Some(MessageId::new(MSG));
            let mut placeholder_from_restored_inflight = true;
            let mut last_edit_text = String::from("streamed body");
            let mut completion_footer_terminal_target: Option<
                WatcherCompletionFooterTerminalTarget,
            > = None;
            let mut retry_terminal_delivery_from_offset = false;
            apply_watcher_short_replace_result(
                result,
                &shared,
                &ProviderKind::Claude,
                ch(),
                "AgentDesk-claude-8141",
                MessageId::new(MSG),
                "answer",
                true,
                None,
                WatcherShortReplaceLocals {
                    relay_ok: &mut relay_ok,
                    direct_send_delivered: &mut direct_send_delivered,
                    tui_direct_anchor_terminal_body_visible:
                        &mut tui_direct_anchor_terminal_body_visible,
                    external_input_lease_consumed_by_relay:
                        &mut external_input_lease_consumed_by_relay,
                    placeholder_msg_id: &mut placeholder_msg_id,
                    placeholder_from_restored_inflight: &mut placeholder_from_restored_inflight,
                    last_edit_text: &mut last_edit_text,
                    completion_footer_terminal_target: &mut completion_footer_terminal_target,
                    retry_terminal_delivery_from_offset: &mut retry_terminal_delivery_from_offset,
                },
            );
            let footer_registered = completion_footer_terminal_target.is_some();
            // Replay `single_message_footer.rs:399-405`: in footer mode the target
            // is `terminal_target.or(fallback)` where the fallback is derived FROM
            // `placeholder_msg_id`. A resolved target is what the footer would edit;
            // for the preserved-original fallback case it MUST resolve to None.
            let footer_target_resolves =
                completion_footer_terminal_target.is_some() || placeholder_msg_id.is_some();
            let committed = shared.ui.placeholder_cleanup.terminal_cleanup_committed(
                &ProviderKind::Claude,
                ch(),
                MessageId::new(MSG),
            );
            let retry_pending = shared
                .ui
                .placeholder_cleanup
                .terminal_cleanup_retry_pending(&ProviderKind::Claude, ch(), MessageId::new(MSG));
            // Both arms mark the body delivered (advance already happened).
            assert!(direct_send_delivered, "the body landed → delivered");
            assert!(tui_direct_anchor_terminal_body_visible);
            Observed {
                footer_registered,
                footer_target_resolves,
                committed,
                retry_pending,
                placeholder_msg_id_cleared: placeholder_msg_id.is_none(),
                placeholder_from_restored_inflight_reset: !placeholder_from_restored_inflight,
                last_edit_text_cleared: last_edit_text.is_empty(),
            }
        }

        // FreshFallback: NO footer target, cleanup `Failed` (retry_pending), NOT
        // committed — the legacy fallback arm (tmux_watcher.rs:6289-6372).
        let fb = run(WatcherShortReplaceResult::DeliveredFallback {
            edit_error: "edit failed".to_string(),
            replacement_anchor: None,
        });
        assert!(
            !fb.footer_registered,
            "fallback without a replacement anchor cannot register the original (#2757)"
        );

        let anchored = run(WatcherShortReplaceResult::DeliveredFallback {
            edit_error: "edit failed".to_string(),
            replacement_anchor: Some(MessageId::new(4_822_001)),
        });
        assert!(
            anchored.footer_registered,
            "fresh fallback must register its delivered replacement anchor"
        );
        assert!(
            !fb.committed,
            "fallback records Failed(edit_error), so the cleanup is NOT committed (Succeeded)"
        );
        assert!(
            fb.retry_pending,
            "fallback records a Failed cleanup → terminal_cleanup_retry_pending"
        );
        // #3089 A4 r3 (codex r2 [Medium]): pin the preserve else-branch locals the
        // legacy fallback `else` clears (tmux_watcher.rs:6356-6358). The load-bearing
        // one is `placeholder_msg_id`: the footer's fallback target is built from it
        // (single_message_footer.rs:401), so leaving it SET would let the completion
        // footer edit the PRESERVED original even though no `terminal_target` was
        // registered.
        assert!(
            fb.placeholder_msg_id_cleared,
            "fallback MUST clear placeholder_msg_id (tmux_watcher.rs:6356) — else the \
                 footer's placeholder-derived fallback target (single_message_footer.rs:401) \
                 would edit the preserved original"
        );
        assert!(
            fb.placeholder_from_restored_inflight_reset,
            "fallback resets placeholder_from_restored_inflight to false (tmux_watcher.rs:6357)"
        );
        assert!(
            fb.last_edit_text_cleared,
            "fallback clears last_edit_text (tmux_watcher.rs:6358)"
        );
        assert!(
            !fb.footer_target_resolves,
            "fallback: with no terminal_target AND placeholder_msg_id cleared, the \
                 completion footer (single_message_footer.rs:405) resolves NO edit target \
                 → it can never reach the preserved original"
        );

        // EditedOriginal: footer target REGISTERED, cleanup `Succeeded` — the
        // legacy edit arm (tmux_watcher.rs:6247-6288).
        let eo = run(WatcherShortReplaceResult::Delivered);
        assert!(
            eo.footer_registered,
            "EditedOriginal registers the original as the completion-footer target"
        );
        assert!(
            eo.committed,
            "EditedOriginal records EditTerminal/Succeeded → cleanup committed"
        );
        assert!(
            !eo.retry_pending,
            "EditedOriginal cleanup Succeeded, so no retry is pending"
        );
        // DISCRIMINATION: the edit arm DOES resolve a footer target (its
        // `terminal_target` is registered, tmux_watcher.rs:6256) so the footer edits
        // the original ON PURPOSE — the opposite of the fallback arm. This is what
        // makes `footer_target_resolves` a discriminating assertion: the fallback arm
        // resolves NO target only because it cleared `placeholder_msg_id`; if it
        // stopped clearing it, the fallback target would resolve here too.
        assert!(
            eo.footer_target_resolves,
            "EditedOriginal: the registered terminal_target resolves → footer edits the \
                 original deliberately (discriminates the fallback preserve arm)"
        );
    }
}

// #3089 A0 — characterization of the watcher terminal-fallback
// should-send-new-chunks predicate (design §5 A0 item 1). Its gate is
// `session_bound_fallback_uses_full_body && text.len() > DISCORD_MSG_LIMIT`.
// (The #2757 watcher edit-fail delete policy — the other watcher A0 datum —
// is already pinned above by
// `fallback_edit_failure_never_deletes_original_without_placeholder_probe`;
// A0 does not duplicate it.) Pinned inline in this `#[cfg(test)] mod tests`
// block of the FROZEN (#3016, baseline 8223) file => ZERO production LoC.
mod a0_characterization_tests {
    use super::super::watcher_should_send_ordered_new_chunks_for_terminal_fallback as should_send;
    use crate::services::discord::DISCORD_MSG_LIMIT;

    #[test]
    fn a0_watcher_fallback_predicate_gates_on_full_body_and_over_limit() {
        let over = "y".repeat(DISCORD_MSG_LIMIT + 1); // 2001 bytes
        let at_limit = "y".repeat(DISCORD_MSG_LIMIT); // exactly 2000 bytes

        // Both required: fallback uses the FULL body AND len > 2000.
        assert!(
            should_send(true, &over),
            "full-body fallback AND over-limit => send ordered new chunks"
        );
        assert!(
            !should_send(false, &over),
            "a non-full-body fallback never sends new chunks, even over-limit"
        );
        assert!(
            !should_send(true, &at_limit),
            "exactly 2000 is NOT over-limit (strict >)"
        );
        assert!(
            !should_send(false, &at_limit),
            "neither condition => no new chunks"
        );
    }

    #[test]
    fn a0_watcher_fallback_predicate_boundary_is_strictly_greater_than_2000() {
        assert!(!should_send(true, &"a".repeat(2000)));
        assert!(should_send(true, &"a".repeat(2001)));
    }
}
