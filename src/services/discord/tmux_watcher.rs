use super::*;
use crate::services::discord::InflightTurnState;
use crate::services::discord::http::{edit_channel_message, send_channel_message};
use crate::services::discord::outbound::delivery_record as dr; // #3089 B2b
use crate::services::discord::replace_outcome_policy::{
    WatcherRewindAttemptDisposition, classify_watcher_send_failure,
    watcher_rewind_attempt_disposition, watcher_send_failure_retry_plan,
};

#[path = "tmux_watcher/entry.rs"]
mod entry;
pub(in crate::services::discord) use self::entry::tmux_output_watcher;

#[path = "tmux_watcher/liveness.rs"]
mod liveness;

pub(super) use self::liveness::watcher_lifecycle_terminal_delivery_observed;
use self::liveness::*;

#[path = "tmux_watcher/panel_decisions.rs"]
mod panel_decisions;

use self::panel_decisions::*;

// #3805 P2 (PR-C): two-message status-panel WATCHER creation-order parity — the
// small PURE gate/generation/completion predicates the watcher loop and its
// single_message_footer.rs completion call thread through thinly (logic here, not
// in the EXTREME giant nor the 700-capped footer sibling).
#[path = "tmux_watcher/two_message_panel.rs"]
mod two_message_panel;

use self::two_message_panel::*;

#[path = "tmux_watcher/prompt_observe.rs"]
mod prompt_observe;

use self::prompt_observe::*;

#[path = "tmux_watcher/turn_identity.rs"]
mod turn_identity;

pub(in crate::services::discord) use self::turn_identity::emit_explicit_inflight_cleanup_signal;
use self::turn_identity::*;

#[path = "tmux_watcher/completion_gate.rs"]
mod completion_gate;

use self::completion_gate::*;

#[path = "tmux_watcher/commit_decisions.rs"]
mod commit_decisions;

use self::commit_decisions::*;

#[path = "tmux_watcher/controller_heartbeat.rs"]
mod controller_heartbeat;

#[path = "tmux_watcher/placeholder_reclaim.rs"]
mod placeholder_reclaim;

#[path = "tmux_watcher/single_message_footer.rs"]
mod single_message_footer;

#[path = "tmux_watcher/completion_producer.rs"]
mod completion_producer;

#[cfg(test)]
#[path = "tmux_watcher/single_message_footer_tests.rs"]
mod single_message_footer_tests;

#[path = "tmux_watcher/terminal_send.rs"]
mod terminal_send;

#[path = "tmux_watcher/terminal_long_chunks.rs"]
mod terminal_long_chunks;

#[path = "tmux_watcher/terminal_direct_fallback.rs"]
mod terminal_direct_fallback;

#[path = "tmux_watcher/task_response_authority.rs"]
mod task_response_authority;

#[path = "tmux_watcher/discrete_trigger_marker.rs"]
mod discrete_trigger_marker;

// #3479 item-2: the watcher-direct orphan status-panel cleanup/completion/refresh
// cluster extracted to a sibling submodule (pure move, zero logic change). Items
// are `pub(super)` there and re-imported below so the watcher loop's call sites —
// and the sibling `single_message_footer.rs` completion call — stay byte-identical.
#[path = "tmux_watcher/orphan_status_panel_cleanup.rs"]
mod orphan_status_panel_cleanup;

use self::orphan_status_panel_cleanup::{
    cleanup_orphan_external_input_status_panel, complete_watcher_status_panel_v2,
    refresh_watcher_session_panel_from_lifecycle,
};

// #3479 item-2: provider-session selector resolution + persistence cluster
// extracted to a sibling submodule (pure move, zero logic change). Items are
// `pub(super)` there and re-imported here so the watcher loop's call sites stay
// byte-identical.
#[path = "tmux_watcher/provider_session_persistence.rs"]
mod provider_session_persistence;

use self::provider_session_persistence::persist_watcher_provider_session_id;

// #3479 Phase-1 rank-1: the supervisor relay-forward + session-bound terminal ACK
// cluster extracted to sibling submodules (pure move, zero logic change). Split
// into two cohesive files only to keep each within the tmux_watcher/** namespace
// LoC cap: `supervisor_relay` holds the forward half (+ the shared
// `SessionBoundRelayAckTarget` type), `session_bound_ack` holds the ACK-outcome /
// terminal-resend / emission-slot-guard half. Items are `pub(super)` there and
// re-imported here so the watcher loop's call sites stay byte-identical.
#[path = "tmux_watcher/supervisor_relay.rs"]
mod supervisor_relay;

#[path = "tmux_watcher/session_bound_ack.rs"]
mod session_bound_ack;

// #3479 Phase-1 rank-2: two more cohesive PURE clusters extracted to sibling
// submodules (pure move, zero logic change). `utf8_chunk_decoder` holds the
// streaming UTF-8 chunk decoder; `terminal_readiness` holds the synchronous
// terminal-readiness / inflight-classification predicates and the pure
// buffer/message-id reconcilers. The async `shared`-touching
// `commit_watcher_direct_terminal_session_idle` now lives in `tmux_watcher/liveness.rs`;
// items are re-imported here so the watcher loop's call sites stay byte-identical.
#[path = "tmux_watcher/terminal_readiness.rs"]
mod terminal_readiness;

#[path = "tmux_watcher/utf8_chunk_decoder.rs"]
mod utf8_chunk_decoder;

#[path = "tmux_watcher/jsonl_rotation.rs"]
mod jsonl_rotation;

#[path = "tmux_watcher/loop_poll_prologue.rs"]
mod loop_poll_prologue;

#[path = "tmux_watcher/stall_exit.rs"]
mod stall_exit;

#[path = "tmux_watcher/streaming_status_tick.rs"]
mod streaming_status_tick;

#[path = "tmux_watcher/no_result_exits.rs"]
mod no_result_exits;

#[path = "tmux_watcher/terminal_abort_exits.rs"]
mod terminal_abort_exits;

#[path = "tmux_watcher/terminal_commit_epilogue.rs"]
mod terminal_commit_epilogue;

#[path = "tmux_watcher/turn_stream_collector.rs"]
mod turn_stream_collector;

#[path = "tmux_watcher/post_stream_exit.rs"]
mod post_stream_exit;

pub(in crate::services::discord) use self::completion_gate::{
    TuiCompletionGateOutcome, run_tui_completion_gate,
};
use self::completion_producer::*;
use self::jsonl_rotation::*;
use self::loop_poll_prologue::*;
use self::no_result_exits::*;
use self::placeholder_reclaim::*;
use self::post_stream_exit::*;
use self::session_bound_ack::*;
use self::single_message_footer::*;
use self::stall_exit::*;
use self::streaming_status_tick::*;
use self::supervisor_relay::*;
use self::terminal_abort_exits::*;
use self::terminal_commit_epilogue::*;
use self::terminal_readiness::*;
use self::turn_stream_collector::*;
use self::utf8_chunk_decoder::*;

#[derive(Debug)]
struct RestoredSeedDisposition {
    stream_seed: WatcherStreamSeed,
    discard_restored_seed: bool,
    seed_reassigned_to_different_turn: bool,
    restored_seed_undelivered_body_len: usize,
    prompt_anchor_present: bool,
}

fn watcher_stream_seed_after_restored_seed_discard(
    restored_turn_seed: Option<RestoredWatcherTurn>,
    current_turn_identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
    prompt_anchor_message_id: Option<u64>,
) -> RestoredSeedDisposition {
    let seed_from_rewind = restored_seed_from_rewind(restored_turn_seed.as_ref());
    let restored_seed_undelivered_body_len = restored_turn_seed
        .as_ref()
        .and_then(|seed| seed.full_response.get(seed.response_sent_offset..))
        .map(|body| body.trim().chars().count())
        .unwrap_or(0);
    let restored_seed_has_body = restored_seed_undelivered_body_len > 0;
    let prompt_anchor_present = prompt_anchor_message_id.is_some();
    let seed_reassigned_to_different_turn = restored_seed_reassigned_to_different_turn(
        restored_turn_seed.as_ref(),
        current_turn_identity,
        prompt_anchor_message_id,
    );
    let discard_restored_seed = should_discard_restored_seed_for_idle_direct_prompt(
        restored_turn_seed.is_some(),
        prompt_anchor_present,
        restored_seed_has_body,
        seed_from_rewind,
        seed_reassigned_to_different_turn,
    );
    let stream_seed = watcher_stream_seed(if discard_restored_seed {
        None
    } else {
        restored_turn_seed
    });
    RestoredSeedDisposition {
        stream_seed,
        discard_restored_seed,
        seed_reassigned_to_different_turn,
        restored_seed_undelivered_body_len,
        prompt_anchor_present,
    }
}

/// Background watcher variant used by restart recovery to continue editing an
/// existing streaming placeholder instead of creating a new one.
pub(in crate::services::discord) async fn tmux_output_watcher_with_restore(
    channel_id: ChannelId,
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    output_path: String,
    tmux_session_name: String,
    initial_offset: u64,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
    pause_epoch: Arc<std::sync::atomic::AtomicU64>,
    turn_delivered: Arc<std::sync::atomic::AtomicBool>,
    last_heartbeat_ts_ms: Arc<std::sync::atomic::AtomicI64>,
    restored_turn: Option<RestoredWatcherTurn>,
) {
    // #3041 P1-1: this watcher instance's delivery-lease holder id. Minted once
    // per spawn so a replacement watcher cannot release/commit (or be mistaken
    // for) this instance's lease across a reattach (§5.2 B2). #3277 (Defect B):
    // minted BEFORE the start log so start/stop pairs are attributable — in the
    // incident two overlapping instances' unlabeled start/stop lines were
    // misread as one watcher dying.
    let watcher_instance_id = next_watcher_instance_id();
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 👁 tmux watcher started for #{tmux_session_name} at offset {initial_offset} (instance {watcher_instance_id})"
    );

    // E5 (#2412): cache the supervisor-owned StreamRelay producer for this
    // tmux session, if the supervisor is running and has matched the
    // session. `None` covers three legitimate cases:
    //   1. `cluster.session_bound_relay_enabled = false` (supervisor never
    //      spawned, registry empty).
    //   2. SessionDiscovery hasn't yet observed this session — the cache is
    //      refreshed below per chunk-read in that case.
    //   3. This watcher attached to a session the registry doesn't know
    //      (e.g. legacy session name pattern). The watcher keeps the legacy
    //      fallback path for envelopes the supervisor-owned relay cannot own.
    let producer_registry =
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry();
    // Cached clone so we don't take the registry RwLock on every chunk. The
    // supervisor only ever publishes ONE producer per session name, but it
    // CAN republish after an Updated event (channel rebind). We refresh on
    // miss and after every send-failure (relay torn down → producer stale).
    let mut cached_relay_producer = producer_registry.get_producer(&tmux_session_name);

    // #1134: mark the attach moment so `record_first_relay` (below) can compute
    // attach→first-relay latency. Single instrumentation point covers all
    // spawn sites (recovery_engine, turn_bridge, tmux self-recovery).
    crate::services::observability::watcher_latency::record_attach(channel_id.get());

    let (watcher_provider, watcher_channel_name) =
        parse_provider_and_channel_from_tmux_name(&tmux_session_name).unwrap_or((
            crate::services::provider::ProviderKind::Claude,
            String::new(),
        ));
    let watcher_thread_channel_id =
        crate::services::discord::adk_session::parse_thread_channel_id_from_name(
            &watcher_channel_name,
        );
    let mut current_offset = initial_offset;
    let input_fifo_path =
        crate::services::discord::turn_bridge::tmux_runtime_paths(&tmux_session_name).1;
    // #1216: leftover JSONL bytes from a buffer that contained more than one
    // turn-terminating event. `process_watcher_lines` now stops at the first
    // `result`/auth/overload event and leaves the rest in the buffer; this
    // outer-scope `all_data` carries that leftover into the next watcher loop
    // iteration so the next turn does not need to wait for fresh disk reads.
    let mut all_data = String::new();
    let mut all_data_start_offset = current_offset;
    let mut all_data_fully_mirrored_to_session_relay = true;
    let mut all_data_session_bound_relay_ack: Option<SessionBoundRelayAckTarget> = None;
    let mut all_data_first_forwarded_relay_sequence: Option<u64> = None;
    let mut utf8_decoder = Utf8ChunkDecoder::default();
    let mut prompt_too_long_killed = false;
    let mut turn_result_relayed = false;
    let mut terminal_delivery_observed = false;
    let mut last_activity_heartbeat_at: Option<std::time::Instant> = None;
    // #1137: 1-shot guard so the "post-terminal-success continuation" log
    // is emitted exactly once per dispatch. Real-world traces (codex
    // G2/G3/G4 on 2026-04-22T23:34:13Z) showed multi-second continuation
    // bursts; logging every chunk would spam the timeline.
    let mut post_terminal_continuation_logged = false;
    let mut last_post_terminal_suppressed_range: Option<(u64, u64)> = None;
    // #3107: 1-shot guard so the "self-heal: re-acquired watcher-owned inflight
    // for an actively-streaming pane that lost its inflight" incident log is
    // emitted at most once per dispatch (mirrors the one-shot suppressed-range
    // logs above). The re-acquire itself is idempotent (no-op when an inflight
    // already exists), so this only bounds the log, not the heal.
    let mut active_stream_inflight_reacquire_logged = false;
    let mut completion_footer_idle = WatcherCompletionFooterIdleState::default();
    let mut completion_footer_spin_idx: usize = 0;
    let mut restored_turn = restored_turn;
    // #3107 codex re-review (P2#3, F3): the #3099 hourglass anchor
    // (`injected_prompt_message_id`) pinned by the restored turn, captured ONCE
    // up front before `restored_turn` is consumed by the streaming path's
    // `restored_turn.take()`. The streaming-interval re-acquire site fires later
    // in the same dispatch, by which point `restored_turn` is already gone — so
    // we stash the anchor here and thread it through. This keeps a
    // hourglass-anchored turn that loses its inflight MID-STREAM re-acquiring an
    // inflight that still carries the pinned message id, so the `⏳ → ✅`
    // completion cleanup can find its own message instead of orphaning it.
    let restored_injected_prompt_message_id = restored_turn
        .as_ref()
        .and_then(|turn| turn.injected_prompt_message_id);
    // Guard against duplicate relay: track the offset from which the last relay was sent.
    // If the outer loop circles back and current_offset hasn't advanced past this point,
    // the relay is suppressed.
    // Initialize from persisted inflight state so replacement watcher instances skip
    // already-delivered output (fixes double-reply on stale watcher replacement).
    // #1270: load both the persisted offset AND its matching
    // `.generation` mtime so a replacement watcher can correctly classify
    // an output regression on restored state. When we have a persisted
    // mtime, it labels the wrapper that produced the persisted offset:
    //   - matches current `.generation` mtime → same wrapper after
    //     `truncate_jsonl_head_safe` → pin to EOF (don't re-flood
    //     surviving content; codex P2 on PR #1271).
    //   - differs from current `.generation` mtime → cancel→respawn into
    //     the same session name → reset to 0 to pick up the fresh
    //     response.
    // When the persisted state predates this field (legacy `None`), we
    // fall back to "no baseline known" semantics — the regression check
    // treats it as a first observation and resets to 0, which is the
    // safer choice for not silently dropping a fresh response.
    let restored_inflight =
        parse_provider_and_channel_from_tmux_name(&tmux_session_name).and_then(|(pk, _)| {
            crate::services::discord::inflight::load_inflight_state(&pk, channel_id.get())
        });
    let mut watcher_turn_identity =
        matching_watcher_turn_identity(restored_inflight.as_ref(), &tmux_session_name);
    let mut watcher_turn_nonce =
        matching_watcher_turn_nonce(restored_inflight.as_ref(), &tmux_session_name);
    let mut last_relayed_offset: Option<u64> = restored_inflight
        .as_ref()
        .and_then(|s| s.last_watcher_relayed_offset);
    let mut last_observed_generation_mtime_ns: Option<i64> = restored_inflight
        .as_ref()
        .and_then(|s| s.last_watcher_relayed_generation_mtime_ns);
    let mut pending_terminal_rewind_seed: Option<RestoredWatcherTurn> = None;
    let mut terminal_rewind_attempt_key: Option<WatcherRewindAttemptKey> = None;
    let mut terminal_rewind_attempts: u8 = 0;
    if let Ok(meta) = std::fs::metadata(&output_path) {
        let observed_output_end = meta.len();
        reset_stale_relay_watermark_if_output_regressed(
            &shared,
            channel_id,
            &tmux_session_name,
            observed_output_end,
            "watcher_start",
        );
        reset_stale_local_relay_offset_if_output_regressed(
            &mut last_relayed_offset,
            &mut last_observed_generation_mtime_ns,
            channel_id,
            &tmux_session_name,
            observed_output_end,
            "watcher_start",
        );
    }
    let mut rotation_tick: u32 = 0;

    // #2441 (H1) — spawn a single `notify`-crate-backed JsonlWatcher
    // keyed on the session output path. Its `Notify` is awaited alongside
    // each polling `sleep()` in this function so a real wrapper write
    // wakes us immediately while the sleep still bounds the maximum
    // wake-up latency. The watcher is dropped automatically when this
    // task exits (or the wrapper rotates the file away).
    let jsonl_watcher = crate::services::discord::jsonl_watcher::JsonlWatcher::spawn(
        std::path::PathBuf::from(&output_path),
    );
    let jsonl_notify = jsonl_watcher.notify();
    let dead_marker_watcher =
        crate::services::discord::jsonl_watcher::JsonlWatcher::spawn(std::path::PathBuf::from(
            crate::services::tmux_common::session_dead_marker_path(&tmux_session_name),
        ));
    let dead_marker_notify = dead_marker_watcher.notify();
    let poll_context = PollWatcherContext {
        http: &http,
        shared: &shared,
        channel_id,
        watcher_provider: &watcher_provider,
        tmux_session_name: &tmux_session_name,
        output_path: &output_path,
        watcher_thread_channel_id,
        watcher_instance_id,
    };
    let poll_controls = PollWatcherControls {
        cancel: &cancel,
        paused: &paused,
        resume_offset: &resume_offset,
        pause_epoch: &pause_epoch,
        turn_delivered: &turn_delivered,
        last_heartbeat_ts_ms: &last_heartbeat_ts_ms,
        jsonl_notify: &jsonl_notify,
        dead_marker_notify: &dead_marker_notify,
    };

    'watcher_loop: loop {
        let (data, data_start_offset, epoch_snapshot) = {
            let mut relay_offset_state = RelayOffsetState {
                current_offset: &mut current_offset,
                terminal_delivery_observed: &mut terminal_delivery_observed,
                last_relayed_offset: &mut last_relayed_offset,
                last_observed_generation_mtime_ns: &mut last_observed_generation_mtime_ns,
                rotation_tick: &mut rotation_tick,
                watcher_turn_identity: &mut watcher_turn_identity,
                watcher_turn_nonce: &mut watcher_turn_nonce,
            };
            let mut loop_poll_state = LoopPollState {
                prompt_too_long_killed,
                all_data: &all_data,
                utf8_decoder: &mut utf8_decoder,
                completion_footer_idle: &mut completion_footer_idle,
                last_activity_heartbeat_at: &mut last_activity_heartbeat_at,
            };
            let mut post_terminal_state = PostTerminalState {
                turn_result_relayed,
                post_terminal_continuation_logged: &mut post_terminal_continuation_logged,
                last_post_terminal_suppressed_range: &mut last_post_terminal_suppressed_range,
                active_stream_inflight_reacquire_logged:
                    &mut active_stream_inflight_reacquire_logged,
                restored_turn: &restored_turn,
                restored_injected_prompt_message_id,
            };
            match poll_watcher_output_or_continue(
                &poll_context,
                &poll_controls,
                &mut relay_offset_state,
                &mut loop_poll_state,
                &mut post_terminal_state,
            )
            .await
            {
                PollOutcome::OutputReady {
                    data,
                    data_start_offset,
                    epoch_snapshot,
                } => (data, data_start_offset, epoch_snapshot),
                PollOutcome::ContinueWatcherLoop => continue,
                PollOutcome::BreakWatcherLoop => break 'watcher_loop,
            }
        };

        let mut turn_parse_state = TurnParseState {
            current_offset: &mut current_offset,
            all_data: &mut all_data,
            all_data_start_offset: &mut all_data_start_offset,
            utf8_decoder: &mut utf8_decoder,
            pending_terminal_rewind_seed: &mut pending_terminal_rewind_seed,
            restored_turn: &mut restored_turn,
            terminal_rewind_attempt_key: &mut terminal_rewind_attempt_key,
            terminal_rewind_attempts: &mut terminal_rewind_attempts,
            watcher_turn_identity: &watcher_turn_identity,
            last_activity_heartbeat_at: &mut last_activity_heartbeat_at,
            active_stream_inflight_reacquire_logged: &mut active_stream_inflight_reacquire_logged,
        };
        let mut supervisor_relay_state = SupervisorRelayState {
            producer_registry: &producer_registry,
            cached_relay_producer: &mut cached_relay_producer,
            all_data_fully_mirrored_to_session_relay: &mut all_data_fully_mirrored_to_session_relay,
            all_data_session_bound_relay_ack: &mut all_data_session_bound_relay_ack,
            all_data_first_forwarded_relay_sequence: &mut all_data_first_forwarded_relay_sequence,
        };
        let mut monitor_auto_turn_state = MonitorAutoTurnState::default();
        let mut render_seed_state = RenderSeedState::default();
        let collected_turn_stream = match collect_turn_stream_until_terminal(
            &TurnStreamCollectorContext {
                http: http.clone(),
                shared: shared.clone(),
                channel_id,
                watcher_provider: watcher_provider.clone(),
                tmux_session_name: tmux_session_name.clone(),
                output_path: output_path.clone(),
                input_fifo_path: input_fifo_path.clone(),
                watcher_thread_channel_id,
                cancel: cancel.clone(),
                paused: paused.clone(),
                pause_epoch: pause_epoch.clone(),
                turn_delivered: turn_delivered.clone(),
                last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
                jsonl_notify: jsonl_notify.clone(),
                dead_marker_notify: dead_marker_notify.clone(),
                turn_result_relayed,
                restored_injected_prompt_message_id,
            },
            TurnStreamCollectorIo {
                data,
                data_start_offset,
                epoch_snapshot,
            },
            &mut turn_parse_state,
            &mut supervisor_relay_state,
            &mut monitor_auto_turn_state,
            &mut render_seed_state,
        )
        .await
        {
            CollectOutcome::ContinueWatcherLoop => continue,
            CollectOutcome::Fallthrough(collected) => collected,
        };
        let CollectedTurnStream {
            turn_data_start_offset,
            split_trailing_turn_follows,
            state,
            restored_response_seed,
            mut full_response,
            tool_state,
            mut placeholder_msg_id,
            mut placeholder_from_restored_inflight,
            mut status_panel_msg_id,
            single_message_panel_footer_mode,
            startup_inflight_snapshot,
            this_turn_status_panel_generation,
            turn_is_external_input_for_session,
            turn_identity_for_panel,
            status_panel_started_at,
            mut last_status_panel_text,
            mut last_edit_text,
            mut response_sent_offset,
            mut watcher_streaming_rollover_frozen_msg_ids,
            finish_mailbox_on_completion,
            mut monitor_auto_turn_claimed,
            monitor_auto_turn_deferred,
            mut monitor_auto_turn_finished,
            mut monitor_auto_turn_synthetic_msg_id,
            mut monitor_auto_turn_ledger_generation,
            mut completion_footer_terminal_target,
            mut session_bound_relay_turn_fully_mirrored,
            session_bound_relay_turn_first_forwarded_sequence,
            found_result,
            terminal_kind,
            terminal_evidence_offset,
            is_prompt_too_long,
            is_auth_error,
            auth_error_message,
            is_provider_overloaded,
            provider_overload_message,
            stale_resume_detected,
            task_notification_kind,
            task_notification_context,
            assistant_text_seen,
            fresh_assistant_text_seen,
            was_paused,
            active_read_state,
        } = collected_turn_stream;

        let no_result_outcome = {
            let no_result_context = NoResultExitContext {
                http: &http,
                shared: &shared,
                channel_id,
                watcher_provider: &watcher_provider,
                tmux_session_name: &tmux_session_name,
                output_path: &output_path,
                paused: &paused,
                pause_epoch: &pause_epoch,
                cancel: &cancel,
                turn_delivered: &turn_delivered,
                watcher_instance_id,
            };
            let no_result_locals = NoResultExitLocals {
                found_result,
                was_paused,
                epoch_snapshot,
                full_response: &full_response,
                turn_is_external_input_for_session,
                finish_mailbox_on_completion,
                startup_inflight_snapshot,
                is_prompt_too_long,
                is_auth_error,
                is_provider_overloaded,
                prompt_too_long_killed,
                terminal_delivery_observed,
                active_read_state,
            };
            let mut no_result_state = NoResultExitState {
                current_offset: &mut current_offset,
                all_data: &mut all_data,
                all_data_start_offset: &mut all_data_start_offset,
                all_data_fully_mirrored_to_session_relay:
                    &mut all_data_fully_mirrored_to_session_relay,
                all_data_session_bound_relay_ack: &mut all_data_session_bound_relay_ack,
                all_data_first_forwarded_relay_sequence:
                    &mut all_data_first_forwarded_relay_sequence,
                last_relayed_offset: &mut last_relayed_offset,
                last_observed_generation_mtime_ns: &mut last_observed_generation_mtime_ns,
                placeholder_msg_id: &mut placeholder_msg_id,
                placeholder_from_restored_inflight: &mut placeholder_from_restored_inflight,
                status_panel_msg_id: &mut status_panel_msg_id,
                last_edit_text: &mut last_edit_text,
                monitor_auto_turn_claimed: &mut monitor_auto_turn_claimed,
                monitor_auto_turn_finished: &mut monitor_auto_turn_finished,
                monitor_auto_turn_synthetic_msg_id: &mut monitor_auto_turn_synthetic_msg_id,
                monitor_auto_turn_ledger_generation: &mut monitor_auto_turn_ledger_generation,
            };
            handle_no_result_exits(&no_result_context, no_result_locals, &mut no_result_state).await
        };
        match no_result_outcome {
            NoResultExitOutcome::ContinueWatcherLoop => continue,
            NoResultExitOutcome::BreakWatcherLoop => break 'watcher_loop,
            NoResultExitOutcome::Fallthrough => {}
        }

        let abort_exit_outcome = {
            let abort_exit_context = TerminalAbortExitContext {
                http: &http,
                shared: &shared,
                channel_id,
                watcher_provider: &watcher_provider,
                tmux_session_name: &tmux_session_name,
                paused: &paused,
                pause_epoch: &pause_epoch,
            };
            let abort_exit_locals = TerminalAbortExitLocals {
                was_paused,
                epoch_snapshot,
                monitor_auto_turn_deferred,
                placeholder_msg_id,
                turn_data_start_offset,
                current_offset,
                response_sent_offset,
                is_prompt_too_long,
                is_auth_error,
                auth_error_message: &auth_error_message,
                is_provider_overloaded,
                provider_overload_message: &provider_overload_message,
            };
            let mut abort_exit_state = TerminalAbortExitState {
                placeholder_from_restored_inflight: &mut placeholder_from_restored_inflight,
                last_edit_text: &mut last_edit_text,
                monitor_auto_turn_claimed: &mut monitor_auto_turn_claimed,
                monitor_auto_turn_finished: &mut monitor_auto_turn_finished,
                monitor_auto_turn_synthetic_msg_id: &mut monitor_auto_turn_synthetic_msg_id,
                monitor_auto_turn_ledger_generation: &mut monitor_auto_turn_ledger_generation,
                all_data: &mut all_data,
                all_data_start_offset: &mut all_data_start_offset,
                all_data_fully_mirrored_to_session_relay:
                    &mut all_data_fully_mirrored_to_session_relay,
                all_data_session_bound_relay_ack: &mut all_data_session_bound_relay_ack,
                all_data_first_forwarded_relay_sequence:
                    &mut all_data_first_forwarded_relay_sequence,
                prompt_too_long_killed: &mut prompt_too_long_killed,
            };
            handle_terminal_abort_exits(
                &abort_exit_context,
                abort_exit_locals,
                &mut abort_exit_state,
            )
            .await
        };
        match abort_exit_outcome {
            AbortExitOutcome::ContinueWatcherLoop => continue,
            AbortExitOutcome::Fallthrough => {}
        }

        // Final guard: re-check epoch and turn_delivered right before relay.
        // Closes the race window where a Discord turn starts between the epoch check
        // above (line 277) and this relay — the turn_bridge may have already delivered
        // the same response to its own placeholder.
        let paused_now = paused.load(Ordering::Relaxed);
        let epoch_changed_now = pause_epoch.load(Ordering::Relaxed) != epoch_snapshot;
        let turn_delivered_now = turn_delivered.load(Ordering::Relaxed);
        let deferred_monitor_ready =
            monitor_auto_turn_claimed && monitor_auto_turn_deferred && !paused_now;
        if should_suppress_relay_before_emit(
            paused_now,
            epoch_changed_now,
            turn_delivered_now,
            deferred_monitor_ready,
        ) {
            if let Some(msg_id) = placeholder_msg_id {
                let inflight_before_cleanup =
                    crate::services::discord::inflight::load_inflight_state(
                        &watcher_provider,
                        channel_id.get(),
                    );
                let _ = delete_nonterminal_placeholder_unless_delivered(
                    &http,
                    channel_id,
                    &shared,
                    &watcher_provider,
                    &tmux_session_name,
                    msg_id,
                    inflight_before_cleanup.as_ref(),
                    Some((
                        turn_data_start_offset,
                        terminal_event_consumed_offset(current_offset, &all_data),
                    )),
                    response_sent_offset,
                    &last_edit_text,
                    "watcher_late_epoch_guard_cleanup",
                )
                .await;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 Late epoch/delivered guard: suppressed duplicate relay for {}",
                tmux_session_name
            );
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
                &mut monitor_auto_turn_synthetic_msg_id,
                &mut monitor_auto_turn_ledger_generation,
            )
            .await;
            discard_watcher_pending_buffer_after_suppressed_turn(
                &mut all_data,
                &mut all_data_start_offset,
                &mut all_data_fully_mirrored_to_session_relay,
                &mut all_data_session_bound_relay_ack,
                &mut all_data_first_forwarded_relay_sequence,
                current_offset,
            );
            continue;
        }

        if watcher_should_yield_to_active_bridge_turn(
            &watcher_provider,
            channel_id,
            &tmux_session_name,
            data_start_offset,
            current_offset,
        ) {
            let matched_reattach = matching_recent_watcher_reattach_offset(
                channel_id,
                &tmux_session_name,
                data_start_offset,
            );
            let reattach_detail = matched_reattach.as_ref().map(|r| {
                format!(
                    "{} range {}..{} matches reattach at {}",
                    tmux_session_name, data_start_offset, current_offset, r.offset
                )
            });
            let ctx = PlaceholderSuppressContext {
                origin: PlaceholderSuppressOrigin::ActiveBridgeTurnGuard,
                provider: &watcher_provider,
                placeholder_msg_id,
                response_sent_offset,
                last_edit_text: &last_edit_text,
                inflight_state: None,
                has_active_turn: false,
                tmux_session_name: &tmux_session_name,
                task_notification_kind: None,
                reattach_offset_match: matched_reattach.is_some(),
            };
            apply_placeholder_suppression(
                &http,
                channel_id,
                &shared,
                &watcher_provider,
                &tmux_session_name,
                placeholder_msg_id,
                ctx.origin,
                decide_placeholder_suppression(&ctx),
                reattach_detail.as_deref(),
            )
            .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 Active bridge turn guard: suppressed duplicate relay for {} (range {}..{})",
                tmux_session_name,
                data_start_offset,
                current_offset
            );
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
                &mut monitor_auto_turn_synthetic_msg_id,
                &mut monitor_auto_turn_ledger_generation,
            )
            .await;
            discard_watcher_pending_buffer_after_suppressed_turn(
                &mut all_data,
                &mut all_data_start_offset,
                &mut all_data_fully_mirrored_to_session_relay,
                &mut all_data_session_bound_relay_ack,
                &mut all_data_first_forwarded_relay_sequence,
                current_offset,
            );
            continue;
        }

        // Duplicate-relay guard: if we already relayed from this same data
        // range, suppress. Use strict `<` so output starting exactly at the
        // previous boundary is treated as the next turn rather than a re-read.
        if let Ok(meta) = std::fs::metadata(&output_path) {
            let observed_output_end = meta.len();
            reset_stale_relay_watermark_if_output_regressed(
                &shared,
                channel_id,
                &tmux_session_name,
                observed_output_end,
                "pre_local_dedupe",
            );
            reset_stale_local_relay_offset_if_output_regressed(
                &mut last_relayed_offset,
                &mut last_observed_generation_mtime_ns,
                channel_id,
                &tmux_session_name,
                observed_output_end,
                "pre_local_dedupe",
            );
        }
        if let Some(prev_offset) = last_relayed_offset {
            if data_start_offset < prev_offset {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] 👁 Duplicate relay guard: suppressed re-relay for {} (data_start={}, last_relayed={:?})",
                    tmux_session_name,
                    data_start_offset,
                    last_relayed_offset,
                );
                if let Some(msg_id) = placeholder_msg_id {
                    let inflight_before_cleanup =
                        crate::services::discord::inflight::load_inflight_state(
                            &watcher_provider,
                            channel_id.get(),
                        );
                    let _ = delete_nonterminal_placeholder_unless_delivered(
                        &http,
                        channel_id,
                        &shared,
                        &watcher_provider,
                        &tmux_session_name,
                        msg_id,
                        inflight_before_cleanup.as_ref(),
                        Some((
                            turn_data_start_offset,
                            terminal_event_consumed_offset(current_offset, &all_data),
                        )),
                        response_sent_offset,
                        &last_edit_text,
                        "watcher_duplicate_relay_guard_cleanup",
                    )
                    .await;
                }
                finish_monitor_auto_turn_if_claimed(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &mut monitor_auto_turn_claimed,
                    &mut monitor_auto_turn_finished,
                    &mut monitor_auto_turn_synthetic_msg_id,
                    &mut monitor_auto_turn_ledger_generation,
                )
                .await;
                discard_watcher_pending_buffer_after_suppressed_turn(
                    &mut all_data,
                    &mut all_data_start_offset,
                    &mut all_data_fully_mirrored_to_session_relay,
                    &mut all_data_session_bound_relay_ack,
                    &mut all_data_first_forwarded_relay_sequence,
                    current_offset,
                );
                continue;
            }
        }

        // Detect stale session resume failure in watcher output
        let is_stale_resume = stale_resume_detected;
        if is_stale_resume {
            clear_provider_overload_retry_state(channel_id);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ Watcher detected stale session resume failure (channel {}), clearing session_id",
                channel_id
            );
            let stale_sid = {
                let mut data = shared.core.lock().await;
                let old = data
                    .sessions
                    .get(&channel_id)
                    .and_then(|s| s.session_id.clone());
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session.clear_provider_session();
                }
                old
            };
            // Clear DB session_id
            {
                let hostname = crate::services::platform::hostname_short();
                let session_key = format!("{}:{}", hostname, tmux_session_name);
                crate::services::discord::adk_session::clear_provider_session_id(
                    &session_key,
                    shared.api_port,
                )
                .await;
            }
            if let Some(ref sid) = stale_sid {
                let _ = crate::services::discord::internal_api::clear_stale_session_id(sid).await;
            }
            crate::services::termination_audit::record_termination_for_tmux(
                &tmux_session_name,
                None,
                "tmux_watcher",
                "stale_resume_retry",
                Some("stale session resume detected — forcing fresh session before auto-retry"),
                None,
            );
            record_tmux_exit_reason(
                &tmux_session_name,
                "stale session resume detected — forcing fresh session before auto-retry",
            );
            crate::services::platform::tmux::kill_session(
                &tmux_session_name,
                "stale session resume detected — forcing fresh session before auto-retry",
            );
            // Replace placeholder with recovery notice (don't delete — avoids visual gap)
            if let Some(msg_id) = placeholder_msg_id {
                let _ = crate::services::discord::http::edit_channel_message(
                    &http,
                    channel_id,
                    msg_id,
                    "↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다.",
                )
                .await;
            }
            // Auto-retry: persist Discord history for LLM injection, then queue the
            // original user message as an internal follow-up instead of self-routing
            // through /api/discord/send announce.
            //
            // #897 round-4 Medium: a `rebind_origin` inflight has no real
            // user message or text to retry with (`user_msg_id=0`,
            // user_text="/api/inflight/rebind"), so auto-retry would
            // enqueue a garbage internal follow-up. Skip the retry; the
            // operator is expected to re-invoke `/api/inflight/rebind`
            // once the tmux session is healthy again.
            match crate::services::discord::inflight::load_inflight_state(
                &watcher_provider,
                channel_id.get(),
            ) {
                Some(state) if state.rebind_origin || state.user_msg_id == 0 => {
                    // rebind_origin and user_msg_id == 0 (e.g. a TUI-direct
                    // turn) both have no anchored user message to retry against;
                    // `MessageId::new(0)` would panic.
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ Watcher auto-retry skipped for channel {} — inflight has no user message to retry",
                        channel_id
                    );
                }
                Some(state) => {
                    crate::services::discord::tmux_overload_retry::schedule_discord_retry_with_history_completion_release(
                        shared.clone(),
                        http.clone(),
                        watcher_provider.clone(),
                        channel_id,
                        serenity::MessageId::new(state.user_msg_id),
                        state.user_text,
                    );
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ↻ Watcher auto-retry queued for channel {}",
                        channel_id
                    );
                }
                None => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ Watcher auto-retry skipped: inflight state missing for channel {}",
                        channel_id
                    );
                }
            }
            // Skip normal response relay
            full_response = String::new();
        }

        let prompt_anchor_present_before_relay =
            crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
                watcher_provider.as_str(),
                &tmux_session_name,
                channel_id.get(),
            )
            .is_some();
        // #3041 P1-4 codex: snapshot the external-input lease ONCE under a single STATE
        // lock and derive BOTH the presence bool and the generation from that one atomic
        // read. Two separate accessor calls (present + generation) re-lock STATE between
        // them, so a concurrently-started turn could record a NEWER same-key lease in the
        // gap — leaving the bool reflecting turn-1 but the generation captured from
        // turn-2's lease (present/generation TOCTOU). The post-delivery clear uses this
        // generation so it only removes the EXACT lease this relay consumed; a NEWER
        // same-key lease recorded by a concurrently-started turn during the slow send
        // survives (no stale-snapshot clobber).
        let external_input_lease_before_relay_snapshot =
            crate::services::tui_prompt_dedupe::external_input_relay_lease(
                watcher_provider.as_str(),
                &tmux_session_name,
                channel_id.get(),
            );
        let external_input_lease_before_relay =
            external_input_lease_before_relay_snapshot.is_some();
        let external_input_lease_generation_before_relay =
            external_input_lease_before_relay_snapshot
                .as_ref()
                .map(|lease| lease.generation);
        let inflight_before_relay = crate::services::discord::inflight::load_inflight_state(
            &watcher_provider,
            channel_id.get(),
        );
        let inflight_identity_before_relay =
            matching_watcher_turn_identity(inflight_before_relay.as_ref(), &tmux_session_name);
        let should_adopt_inflight_terminal_message_ids = !external_input_lease_before_relay
            || watcher_inflight_represents_external_input(inflight_before_relay.as_ref());
        // #3142: skip adopting the pre-relay snapshot's terminal message ids when it
        // is a STALE NEWER follow-up turn (turn_start_offset >= current_offset) — else
        // the older range aliases the newer turn's status panel. Uses the id==0-
        // INCLUSIVE anchor variant (None 2nd arg sound: is_some_and → false) so
        // external-input turns are caught; in-range id==0 turns adopt (OFFSET-keyed).
        let inflight_before_relay_is_stale_newer_turn =
            committed_anchor_cleanup_is_stale_for_newer_turn(
                inflight_before_relay.as_ref(),
                None,
                &tmux_session_name,
                current_offset,
            );
        if should_adopt_inflight_terminal_message_ids
            && !inflight_before_relay_is_stale_newer_turn
            && let Some(inflight) = inflight_before_relay.as_ref()
        {
            merge_persisted_rollover_frozen_msg_ids(
                &mut watcher_streaming_rollover_frozen_msg_ids,
                Some(inflight),
                &tmux_session_name,
            );
            adopt_watcher_terminal_message_ids_from_inflight(
                &mut placeholder_msg_id,
                &mut placeholder_from_restored_inflight,
                &mut status_panel_msg_id,
                inflight,
                &tmux_session_name,
            );
            if single_message_panel_footer_mode {
                status_panel_msg_id = None;
            }
        }
        let fresh_seen = fresh_assistant_text_seen;
        let drop_seed = local_cmd_no_output(&all_data, terminal_kind, fresh_seen, &tool_state);
        let restored_seed_delivery_confirmed = drop_seed
            && restored_response_seed
                .get(response_sent_offset..)
                .is_some_and(|seed_body| {
                    !seed_body.trim().is_empty()
                        && crate::services::discord::outbound::delivery_record::recent_delivered_content_matches(
                            &watcher_provider,
                            channel_id,
                            &tmux_session_name,
                            seed_body,
                        )
                });
        if discard_restored_response_seed_before_no_inflight_terminal_relay(
            &mut full_response,
            &mut response_sent_offset,
            &mut last_edit_text,
            &restored_response_seed,
            inflight_before_relay.is_some(),
            fresh_assistant_text_seen,
            drop_seed,
            restored_seed_delivery_confirmed,
        ) {
            tracing::info!(
                provider = %watcher_provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = %tmux_session_name,
                restored_response_seed_len = restored_response_seed.len(),
                fresh_response_len = full_response.len(),
                "watcher: discarded restored response seed before no-inflight terminal relay"
            );
        }
        let has_assistant_response = !full_response.trim().is_empty();
        let current_response = full_response.get(response_sent_offset..).unwrap_or("");
        let has_current_response = !current_response.trim().is_empty();

        // #3041 P1-3 (Part a, B1 — FRAME-CARRIED, codex): the watcher's
        // AUTHORITATIVE consumed-terminal END is NO LONGER persisted to the inflight
        // FILE here. The old inflight-persist Part (a) was RACY (the sink read the
        // end back from the file in `deliver_response`, a separate read/write across
        // the relay's async drain). It is REPLACED by the frame-carried commit
        // fence: the RESULT-bearing `StreamFrame` itself carries `consumed_end` +
        // the pinned turn identity (forwarded during line collection above), and the
        // sink advances `confirmed_end_offset` identity-gated on its CONFIRMED POST —
        // POST + advance atomic per-frame, no file race. See
        // `watcher_terminal_commit_fence` (producer) and
        // `advance_offset_for_confirmed_delegated_terminal` (sink).

        let recent_stop_for_output =
            recent_turn_stop_for_watcher_range(channel_id, &tmux_session_name, data_start_offset);
        let inflight_missing_before_relay = inflight_before_relay.is_none();
        // #3003 single terminal chokepoint: every turn termination converges on
        // this terminal-relay block, including a fast `result` that breaks out of
        // the streaming loop before the periodic interval reclaim runs again.
        // Reclaim a watcher-created external-input panel here when the turn will
        // not finalize it — no assistant text (status-only/no-response), a recent
        // turn-stop tombstone, or a cleared inflight (stop/cancel). A turn that has
        // assistant text, is not stopped, and still has its inflight is left for
        // the committed relay path to complete (or a failed send to preserve for
        // retry). Runs before every terminal sub-path (stale-id clear, silent,
        // recent-stop suppression, no-response).
        //
        // The no-response arm excludes task-notification turns (codex P2 r15): a
        // status-only `task_notification_kind` turn is relay-suppressed-and-
        // committed below, so `complete_watcher_status_panel_v2` still finalizes
        // its panel — deleting it here would erase a panel that is about to
        // complete. Stopped/abandoned such turns are still reclaimed via the
        // abandon arm.
        // #3351: same-turn relay placeholder reclaim rides the identical orphan
        // context; gated so a placeholder already edited into a real response (or
        // a turn with assistant text — owned by the recent-stop/stale-clear arms)
        // is never deleted here.
        let terminal_placeholder_reclaim = watcher_should_reclaim_orphan_turn_placeholder(
            turn_is_external_input_for_session,
            placeholder_msg_id,
            has_assistant_response,
            &last_edit_text,
        );
        let terminal_orphan_context = turn_is_external_input_for_session
            && (status_panel_msg_id.is_some() || terminal_placeholder_reclaim)
            && ((!has_assistant_response && task_notification_kind.is_none())
                || watcher_external_input_turn_abandoned(
                    &watcher_provider,
                    channel_id,
                    &tmux_session_name,
                    &output_path,
                    data_start_offset,
                    turn_identity_for_panel.as_ref(),
                ));
        let terminal_panel_reclaim_committed =
            if terminal_orphan_context && status_panel_msg_id.is_some() {
                cleanup_orphan_external_input_status_panel(
                    &http,
                    &shared,
                    channel_id,
                    &mut status_panel_msg_id,
                    &watcher_provider,
                    &tmux_session_name,
                    turn_is_external_input_for_session,
                )
                .await
            } else {
                true
            };
        if terminal_orphan_context && terminal_placeholder_reclaim {
            reclaim_orphan_external_input_placeholder(
                &http,
                &shared,
                channel_id,
                &mut placeholder_msg_id,
                &mut placeholder_from_restored_inflight,
                &mut last_edit_text,
                &watcher_provider,
                &tmux_session_name,
            )
            .await;
        }
        let inflight_silent_turn = inflight_before_relay
            .as_ref()
            .map(|state| state.silent_turn)
            .unwrap_or(false);
        if watcher_should_clear_stale_terminal_message_ids(
            inflight_before_relay.is_some(),
            has_assistant_response,
            placeholder_msg_id,
        ) {
            if let Some(stale_msg_id) = placeholder_msg_id {
                tracing::info!(
                    provider = %watcher_provider.as_str(),
                    channel_id = channel_id.get(),
                    tmux_session = %tmux_session_name,
                    stale_placeholder_msg_id = stale_msg_id.get(),
                    status_panel_msg_id = status_panel_msg_id.map(|id| id.get()).unwrap_or(0),
                    "watcher: clearing stale terminal message ids before no-inflight terminal relay"
                );
            }
            placeholder_msg_id = None;
            // #3003 (codex P2 r12): only drop the local panel id if the terminal
            // reclaim above actually committed its delete. When the delete failed
            // transiently the id is held for retry (the persisted id, if any, also
            // survives for the sweeper); nulling it here would strand the still-
            // visible "계속 처리 중" panel with no handle.
            if terminal_panel_reclaim_committed {
                status_panel_msg_id = None;
            }
            placeholder_from_restored_inflight = false;
            last_edit_text.clear();
        }
        if inflight_silent_turn && has_assistant_response {
            // Headless silent trigger (metadata.silent=true) — suppress assistant
            // text relay to the channel entirely, but keep the watcher state
            // machine advancing so the turn finalizes normally. Lifecycle/error/
            // cancel notifications continue to post via their own paths.
            let cleanup_committed = if let Some(msg_id) = placeholder_msg_id {
                delete_nonterminal_placeholder(
                    &http,
                    channel_id,
                    &shared,
                    &watcher_provider,
                    &tmux_session_name,
                    msg_id,
                    "watcher_silent_turn_suppress_cleanup",
                )
                .await
                .is_committed()
            } else {
                true
            };
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🤫 watcher: silent_turn suppressed terminal output for channel {} (tmux={}, range {}..{})",
                channel_id.get(),
                tmux_session_name,
                data_start_offset,
                current_offset
            );
            if cleanup_committed {
                last_relayed_offset = Some(current_offset);
                last_observed_generation_mtime_ns =
                    Some(read_generation_file_mtime_ns(&tmux_session_name));
                advance_watcher_confirmed_end(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &tmux_session_name,
                    suppressed_terminal_confirmed_end(current_offset, &all_data),
                    "src/services/discord/tmux.rs:silent_turn_suppressed_terminal_output",
                );
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
                &mut monitor_auto_turn_synthetic_msg_id,
                &mut monitor_auto_turn_ledger_generation,
            )
            .await;
            continue;
        }
        if should_suppress_terminal_output_after_recent_stop(
            has_assistant_response,
            inflight_missing_before_relay,
            recent_stop_for_output.is_some(),
        ) {
            let stop = recent_stop_for_output.expect("recent stop checked above");
            let cleanup_committed = if let Some(msg_id) = placeholder_msg_id {
                if watcher_should_delete_suppressed_placeholder(placeholder_from_restored_inflight)
                {
                    let committed = delete_nonterminal_placeholder(
                        &http,
                        channel_id,
                        &shared,
                        &watcher_provider,
                        &tmux_session_name,
                        msg_id,
                        "watcher_terminal_recent_stop_cleanup",
                    )
                    .await
                    .is_committed();
                    if committed {
                        placeholder_from_restored_inflight = false;
                        last_edit_text.clear();
                    }
                    committed
                } else {
                    placeholder_from_restored_inflight = false;
                    last_edit_text.clear();
                    true
                }
            } else {
                true
            };
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 🛑 watcher: suppressed terminal output for channel {} after recent turn stop ({}, tmux={}, range {}..{})",
                channel_id.get(),
                stop.reason,
                tmux_session_name,
                data_start_offset,
                current_offset
            );
            if cleanup_committed {
                last_relayed_offset = Some(current_offset);
                // #1270 codex P2: snapshot the current `.generation` mtime so
                // the local regression check has a real baseline (see the
                // matching snapshot in the rotation path).
                last_observed_generation_mtime_ns =
                    Some(read_generation_file_mtime_ns(&tmux_session_name));
                advance_watcher_confirmed_end(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &tmux_session_name,
                    suppressed_terminal_confirmed_end(current_offset, &all_data),
                    "src/services/discord/tmux.rs:cancel_tombstone_suppressed_terminal_output",
                );
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
                &mut monitor_auto_turn_synthetic_msg_id,
                &mut monitor_auto_turn_ledger_generation,
            )
            .await;
            continue;
        }

        // #3017 single output-offset authority — cross-actor relay dedup for
        // the inflight-less wake / idle-background / monitor turn (E-13). When
        // there is NO inflight, the idle-JSONL relay
        // (`session_relay_sink::run_idle_jsonl_relay_loop`) reads the SAME
        // JSONL and can relay this exact range. If it already committed the
        // authoritative relayed offset at/past this turn's END, that range was
        // already delivered to Discord — so the watcher must SKIP to avoid the
        // duplicate `[E2E:E13:WAKE]`. This is deliberately gated on
        // `inflight_missing_before_relay`: a normal Discord-origin turn
        // (inflight present) keeps the watcher as the sole relay owner and is
        // NEVER suppressed by the shared watermark (the long-standing
        // invariant), so this only de-duplicates the un-owned wake/idle paths.
        if inflight_missing_before_relay
            && has_current_response
            && current_offset > turn_data_start_offset
        {
            // Codex P1: a stale-high `confirmed_end_offset` left by a PREVIOUS
            // wrapper (before any actor ran the regression reset) would make a
            // FRESH wake/idle response with a lower `current_offset` look already
            // delivered and get dropped. Run the SAME generation-aware
            // regression reset BEFORE reading the watermark (a truncated /
            // respawned JSONL resets it to 0 for a fresh wrapper), exactly as
            // the idle relay path does. The unconditional pre-relay reset below
            // at `pre_relay` is for the general path; this one guards the
            // no-inflight dedup read specifically.
            let output_eof_for_no_inflight_dedup =
                std::fs::metadata(&output_path).ok().map(|meta| meta.len());
            if let Some(output_eof) = output_eof_for_no_inflight_dedup {
                reset_stale_relay_watermark_if_output_regressed(
                    &shared,
                    channel_id,
                    &tmux_session_name,
                    output_eof,
                    "no_inflight_dedup",
                );
            }
            // Codex r6 P2: `reset_stale_relay_watermark_if_output_regressed` only resets when the
            // current EOF is LOWER than the stored watermark. A respawned same-named wrapper whose
            // fresh JSONL ALREADY grew PAST the prior watermark would NOT trip that EOF-regression
            // check → fresh output wrongly suppressed. Independently reset when the `.generation`
            // mtime CHANGED since commit (fresh wrapper = different byte stream). Shared with idle.
            reset_relay_watermark_on_generation_change(
                &shared,
                channel_id,
                &tmux_session_name,
                "watcher_no_inflight_dedup",
            );
            // Read-only check against the authority: if the sink (idle-JSONL relay or the watcher's
            // own session-bound delegation) already COMMITTED at/past this turn's END, that range
            // was delivered → skip the duplicate. The watcher does NOT claim here (claim + relay
            // failure would mark delivered while dropping it); it advances only on a CONFIRMED relay
            // at `advance_watcher_confirmed_end` below.
            // Codex r5 P2: compare against this TURN's consumed terminal end, NOT the whole read
            // batch end (`current_offset`) — a batch can hold a completed turn PLUS a later turn's
            // trailing JSONL; `process_watcher_lines` stops at the first result, so the turn ends at
            // `current_offset - all_data.len()` (== the normal commit path's
            // `runtime_binding_candidate_offset`). Using `current_offset` would MISS a prior commit
            // at that smaller consumed end and re-relay the already-committed terminal.
            let turn_consumed_offset = terminal_event_consumed_offset(current_offset, &all_data);
            let committed = dr::effective_committed_offset(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                output_eof_for_no_inflight_dedup,
            );
            if committed >= turn_consumed_offset && turn_consumed_offset > turn_data_start_offset {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 watcher: suppressed no-inflight terminal relay for channel {} — range {}..{} already committed by another relay actor (offset authority, committed_end={})",
                    channel_id.get(),
                    turn_data_start_offset,
                    turn_consumed_offset,
                    committed
                );
                last_relayed_offset = Some(current_offset);
                last_observed_generation_mtime_ns =
                    Some(read_generation_file_mtime_ns(&tmux_session_name));
                finish_monitor_auto_turn_if_claimed(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &mut monitor_auto_turn_claimed,
                    &mut monitor_auto_turn_finished,
                    &mut monitor_auto_turn_synthetic_msg_id,
                    &mut monitor_auto_turn_ledger_generation,
                )
                .await;
                continue;
            }
        }

        // Relay coordination is limited to serialization plus telemetry. The
        // local `last_relayed_offset` guard handles self-duplicate relays, and
        // watcher registration enforces one live owner per tmux session. Do
        // not suppress a valid owner solely because another watcher advanced
        // the shared confirmed_end watermark.
        let relay_coord = shared.tmux_relay_coord(channel_id);
        if let Ok(meta) = std::fs::metadata(&output_path) {
            reset_stale_relay_watermark_if_output_regressed(
                &shared,
                channel_id,
                &tmux_session_name,
                meta.len(),
                "pre_relay",
            );
        }
        // CAS the emission slot. `0` = free; any non-zero value = a watcher
        // is mid-emission with that start offset. `.max(1)` guarantees the
        // stored value is non-zero even when `data_start_offset == 0`.
        let slot_claim_token = data_start_offset.max(1);
        if relay_coord
            .relay_slot
            .compare_exchange(
                0,
                slot_claim_token,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 Cross-watcher serialization: slot busy, skipped relay for {} (data_start={})",
                tmux_session_name,
                data_start_offset
            );
            if let Some(msg_id) = placeholder_msg_id {
                let _ = delete_nonterminal_placeholder(
                    &http,
                    channel_id,
                    &shared,
                    &watcher_provider,
                    &tmux_session_name,
                    msg_id,
                    "watcher_cross_watcher_slot_busy_cleanup",
                )
                .await;
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
                &mut monitor_auto_turn_synthetic_msg_id,
                &mut monitor_auto_turn_ledger_generation,
            )
            .await;
            continue;
        }

        // #2840: the CAS above acquired the emission slot. Hold it via an RAII
        // guard so ANY exit from here on (early `continue`, `?`, panic, task
        // abort) frees the slot on Drop instead of wedging the channel for
        // every replacement watcher. The two intended release points below call
        // `slot_guard.release()` explicitly to preserve their timing.
        let mut slot_guard = RelaySlotGuard::new(relay_coord.relay_slot.clone());

        // Send the terminal response to Discord, or delegate it when matched
        // session-bound inflight metadata assigns delivery to the StreamRelay sink.
        let relay_decision = terminal_relay_decision(
            has_assistant_response,
            task_notification_kind,
            assistant_text_seen,
        );
        debug_assert!(
            !relay_decision.should_enqueue_notify_outbox,
            "monitor/task-notification watcher relays must not use notify-bot outbox"
        );
        let session_bound_discord_delivery_enabled =
            crate::services::discord::session_relay_sink::session_bound_discord_delivery_enabled();
        let relay_producer_session_name = cached_relay_producer
            .as_ref()
            .map(|producer| producer.session_name());
        // #3579: INIT the ack outcome to the watcher-owned NON-attempt sentinel.
        // When `session_bound_relay_should_own_terminal_delivery` returns false
        // (e.g. relay_owner=Watcher) the ack-wait block below is SKIPPED and this
        // init value is what the flight recorder logs as `frame_ack_outcome`. It
        // is BENIGN (the watcher owns terminal delivery; the sink-delegated ack
        // path is intentionally not taken) — distinct from `MissingTarget`, which
        // `wait_for_session_bound_relay_delivery_ack` returns only when the block
        // ACTUALLY RAN but had no target (a real unconfirmed). Before #3579 this
        // init was `MissingTarget`, conflating the two and inflating relay-loss
        // tallies. Behavior is unchanged: `NotAttempted` folds to the same
        // `DeliveryOutcome::Unknown` as `MissingTarget` for the resend decision.
        let mut session_bound_ack_outcome = SessionBoundRelayAckOutcome::NotAttempted;
        let session_bound_terminal_delivery_attempted =
            session_bound_relay_should_own_terminal_delivery(
                relay_decision.should_direct_send,
                session_bound_discord_delivery_enabled,
                session_bound_relay_turn_fully_mirrored,
                relay_producer_session_name,
                inflight_before_relay.as_ref(),
                &tmux_session_name,
            );
        let session_bound_relay_owns_terminal_delivery =
            if session_bound_terminal_delivery_attempted {
                let ack_outcome = wait_for_session_bound_relay_delivery_ack(
                    all_data_session_bound_relay_ack.as_ref(),
                    std::time::Duration::from_secs(10),
                )
                .await;
                let ack_outcome = session_bound_ack_outcome_after_resolve_time_mirror_check(
                    ack_outcome,
                    &mut session_bound_relay_turn_fully_mirrored,
                    all_data_session_bound_relay_ack.as_ref(),
                    session_bound_relay_turn_first_forwarded_sequence,
                );
                session_bound_ack_outcome = ack_outcome;
                let delivered = session_bound_relay_turn_fully_mirrored
                    && matches!(ack_outcome, SessionBoundRelayAckOutcome::Delivered);
                if !delivered {
                    tracing::warn!(
                        provider = watcher_provider.as_str(),
                        channel_id = channel_id.get(),
                        tmux_session = %tmux_session_name,
                        ?ack_outcome,
                        "session-bound StreamRelay terminal delivery was not acknowledged"
                    );
                }
                delivered
            } else {
                false
            };
        let prompt_anchor_present = prompt_anchor_present_before_relay;
        let ssh_direct_pending = prompt_anchor_present
            || crate::services::tui_prompt_dedupe::is_ssh_direct_observation_pending(
                watcher_provider.as_str(),
                &tmux_session_name,
            );
        let external_input_lease_present = external_input_lease_before_relay;
        let recent_stop_reason =
            recent_turn_stop_for_watcher_range(channel_id, &tmux_session_name, data_start_offset)
                .map(|stop| stop.reason);
        // #3042: an ownerless turn (`inflight_present=false` or
        // `relay_owner_kind=none`, the post-restart restore_inflight gap) has no
        // reliable terminal-commit ACK path, so a `TimedOut` there must not drive
        // the watcher-direct re-send. Mirror the relay_flight_recorder fields used
        // below so the gate sees exactly what is logged.
        let relay_owner_present = inflight_before_relay.as_ref().is_some_and(|state| {
            !matches!(
                state.effective_relay_owner_kind(),
                crate::services::discord::inflight::RelayOwnerKind::None
            )
        });
        let watcher_direct_fallback_intended = watcher_should_direct_send_after_session_bound_ack(
            relay_decision.should_direct_send,
            session_bound_ack_outcome,
            relay_owner_present,
        );
        // #3041 P1-3 (Part b, §3.2): REPLACE the blind re-send. Before re-sending the
        // terminal body after a non-`Delivered` session-bound ACK (the
        // `relay_terminal_ack_timeout` duplicate vector), reconcile against the offset
        // authority FIRST, over the SAME consumed range `[data_start_offset, terminal_event_consumed_offset(current_offset, all_data))`.
        // Part (a) advances `committed_relay_offset` to the watcher's own `end` on a
        // confirmed sink delivery, so the consult is exact: committed >= end → SKIP (sink
        // delivered; ACK lagged → no duplicate, failure-mode-①); committed < end → re-send
        // the FULL response (no black-hole). codex BLOCKER 2: NO partial-suffix send (render
        // coordinate not derivable from the JSONL byte offset), delegation all-or-nothing so
        // `committed` is never strictly between start/end. Reconcile ONLY on the session-bound re-send path; plain watcher-direct unchanged.
        let watcher_resend_range_start = data_start_offset;
        let watcher_resend_range_end = terminal_event_consumed_offset(current_offset, &all_data);
        // #3593: self-heal a stale-high watermark BEFORE the resend-dedup `committed` read (no-inflight-gate parity; generation change → committed 0 → no false skip).
        reset_relay_watermark_on_generation_change(
            &shared,
            channel_id,
            &tmux_session_name,
            "watcher_terminal_resend_dedup",
        );
        let output_eof_for_resend_dedup =
            std::fs::metadata(&output_path).ok().map(|meta| meta.len());
        let watcher_resend_committed = dr::committed_floor_for_resend_dedup(
            &shared,
            &watcher_provider,
            channel_id,
            &tmux_session_name,
            output_eof_for_resend_dedup,
        ); // #3089 B2b + #3593 (codex HIGH): in-memory committed ∪ flag-independent durable frontier
        let watcher_resend_reconciled = session_bound_terminal_delivery_attempted
            && watcher_direct_fallback_intended
            && !matches!(
                session_bound_ack_outcome,
                SessionBoundRelayAckOutcome::Delivered
            );
        let watcher_resend_action = if watcher_resend_reconciled {
            // #3593: the stale-high self-heal ran unconditionally above (codex P2).
            // #3151: gate the re-send on the in-flight sink-delivery marker BEFORE
            // the committed-offset reconciliation. The marker is a `Leased{Sink}`
            // state on the SAME per-channel `DeliveryLeaseCell` the watcher's own
            // direct-send path acquires (B2). Read a coherent snapshot, then:
            //   * Leased{Sink, fresh}  → WaitInFlight: a sink POST is in flight; do
            //     NOT re-send this pass (the slow-sink-in-flight duplicate #3151).
            //   * Leased{Sink, expired} → reclaim the dead sink's marker, then
            //     SendFull (committed<end) — the no-black-hole arm.
            //   * Committed{Sink} → reconcile vs committed offset: committed>=end → Skip
            //     (delivered), committed<end → SendFull (#3159: refused/NotDelivered re-sends).
            //   * Unleased / non-Sink holder → unchanged (defer to the existing
            //     committed-offset reconciliation).
            let gate_cell = shared.delivery_lease(channel_id);
            let snapshot = gate_cell.read();
            // #3159 BUG 1 (codex race-1): read `committed` AFTER the lease snapshot. The sink's
            // CLEAR protocol advances `committed` FIRST, THEN commits the marker (`Committed{Sink}`),
            // so observing `Committed{Sink}` happens-after the committed write → reading `committed`
            // next sees the advanced value (committed>=end for a real Delivered → Skip). Reading it
            // BEFORE the snapshot could pair a pre-advance `committed < end` with a now-Committed
            // marker → a spurious SendFull duplicate.
            let committed = dr::effective_committed_offset(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                output_eof_for_resend_dedup,
            );
            let now_ms = crate::services::discord::lease_now_ms();
            let (action, reclaim_expired_sink) = watcher_terminal_resend_action_gated(
                &snapshot,
                committed,
                watcher_resend_range_start,
                watcher_resend_range_end,
                now_ms,
            );
            if reclaim_expired_sink {
                // Force the dead sink's marker Unleased so the watcher-direct path
                // below can re-acquire and SendFull (no black-hole). Deadline-only /
                // identity-agnostic — a LIVE sink (fresh deadline) is never reached.
                gate_cell.reclaim_if_expired(now_ms);
            }
            Some(action)
        } else if watcher_direct_fallback_intended
            && dr::range_already_committed(watcher_resend_range_end, watcher_resend_committed)
        {
            // #3593: already-delivered range (`committed >= end`) on the non-reconciled
            // synthetic-resume path (the placeholder path the #3520 new-message-only floor
            // missed) → EXISTING non-destructive `SkipAlreadyCommitted` arm, which PRESERVES
            // the restored placeholder (flipping `has_direct_terminal_response`/the fallback
            // flag would delete the already-delivered body — #3520 codex BLOCKER).
            Some(WatcherTerminalResendAction::SkipAlreadyCommitted)
        } else {
            None
        };
        // #3151: WaitInFlight suppresses BOTH the re-send and the skip-log this
        // pass — the watcher's NEXT terminal pass re-evaluates (bounded by the
        // sink's lease deadline). It must NOT be treated as "send" by the fallback.
        let watcher_resend_wait_in_flight = matches!(
            watcher_resend_action,
            Some(WatcherTerminalResendAction::WaitInFlight)
        );
        if watcher_resend_wait_in_flight {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                provider = watcher_provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = %tmux_session_name,
                start = watcher_resend_range_start,
                end = watcher_resend_range_end,
                committed = watcher_resend_committed,
                ?session_bound_ack_outcome,
                "  [{ts}] 👁 #3151: deferred watcher terminal re-send — sink POST in flight (Leased{{Sink}}, fresh); will re-evaluate next pass (no duplicate)"
            );
        }
        if matches!(
            watcher_resend_action,
            Some(WatcherTerminalResendAction::SkipAlreadyCommitted)
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                provider = watcher_provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = %tmux_session_name,
                start = watcher_resend_range_start,
                end = watcher_resend_range_end,
                committed = watcher_resend_committed,
                ?session_bound_ack_outcome,
                "  [{ts}] 👁 #3041 P1-3 §3.2: skipped watcher terminal re-send — range already committed by the sink (offset authority); no duplicate"
            );
        }
        // The watcher actually direct-sends only when the reconciliation did NOT
        // skip the range AND is not WAITING on an in-flight sink POST.
        // `SkipAlreadyCommitted` suppresses the re-send (no dup); `WaitInFlight`
        // (#3151) suppresses it this pass (re-evaluated next pass); `SendFull`/the
        // non-reconciled path proceed to send.
        let watcher_direct_fallback_after_session_bound_ack = watcher_direct_fallback_intended
            && !matches!(
                watcher_resend_action,
                Some(
                    WatcherTerminalResendAction::SkipAlreadyCommitted
                        | WatcherTerminalResendAction::WaitInFlight
                )
            );
        let session_bound_fallback_uses_full_body = session_bound_terminal_delivery_attempted
            && watcher_direct_fallback_after_session_bound_ack;
        let direct_terminal_response = watcher_terminal_response_for_direct_send(
            &full_response,
            response_sent_offset,
            session_bound_fallback_uses_full_body,
        );
        let direct_terminal_response_decision = watcher_direct_terminal_response_decision(
            &watcher_provider,
            channel_id,
            shared.restart.current_generation,
            &tmux_session_name,
            inflight_before_relay.as_ref(),
            current_offset,
            fresh_assistant_text_seen,
            direct_terminal_response,
        );
        let has_direct_terminal_response = direct_terminal_response_decision.has_sendable_body();
        let direct_terminal_response_refused_duplicate =
            watcher_direct_fallback_after_session_bound_ack
                && direct_terminal_response_decision.refused_duplicate();
        // #2838/#3042 (relay-stability P0-1): count the primary duplicate-emit vector — a
        // session-bound terminal ACK that timed out while the watcher direct-sends (sink may
        // have already posted; rising counts ⇒ P1 dual-authority lease overdue). Gate on the
        // raw `TimedOut` + original `should_direct_send` intent (records even when the ownerless-timeout suppression turned the fallback off).
        if relay_decision.should_direct_send
            && matches!(
                session_bound_ack_outcome,
                SessionBoundRelayAckOutcome::TimedOut
            )
        {
            crate::services::observability::metrics::record_relay_terminal_ack_timeout(
                channel_id.get(),
                watcher_provider.as_str(),
            );
        }
        // #3646 OBSERVATION-ONLY owner split: this is the INFLIGHT-snapshot owner
        // ONLY. The collapsed `="none"` could mean either a real None-ledger turn
        // OR "bridge cleared inflight but the ledger is still Watcher/finalized" —
        // the #3607 ambiguity. The finalizer-side `finalizer_ledger_owner` event
        // (ledger entry's relay_owner, same turn_id) supplies the second signal and
        // the two JOIN in PG. Computed once so we can emit it under BOTH the new
        // `inflight_relay_owner` name AND the legacy `relay_owner_kind` alias
        // (codex review #3678: keep the old field so existing dashboards/alerts/
        // runbooks that grep `relay_owner_kind=` don't break).
        let inflight_relay_owner_kind = inflight_before_relay
            .as_ref()
            .map(|state| state.effective_relay_owner_kind().as_str())
            .unwrap_or("none");
        tracing::info!(
            target: "agentdesk::relay_flight_recorder",
            provider = watcher_provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session = %tmux_session_name,
            data_start_offset,
            current_offset,
            terminal_kind = terminal_kind.map(WatcherTerminalKind::as_str).unwrap_or("unknown"),
            full_response_len = current_response.len(),
            assistant_text_seen,
            any_tool_used = tool_state.any_tool_used,
            has_post_tool_text = tool_state.has_post_tool_text,
            inflight_present = inflight_before_relay.is_some(),
            // #3646: new disambiguated name. Field rename/add only — control flow
            // unchanged (these are tracing fields, not branches).
            inflight_relay_owner = inflight_relay_owner_kind,
            // #3646: legacy alias preserved for backward-compatible log greps.
            relay_owner_kind = inflight_relay_owner_kind,
            session_bound_enabled = session_bound_discord_delivery_enabled,
            fully_mirrored = session_bound_relay_turn_fully_mirrored,
            frame_ack = session_bound_relay_frame_ack_reached(all_data_session_bound_relay_ack.as_ref()),
            terminal_commit_ack = session_bound_relay_owns_terminal_delivery,
            route = if session_bound_relay_owns_terminal_delivery {
                "session_bound"
            } else if direct_terminal_response_refused_duplicate {
                "duplicate_guard_refused"
            } else if watcher_direct_fallback_after_session_bound_ack {
                "watcher_direct"
            } else if relay_decision.suppressed {
                "suppressed"
            } else {
                "none"
            },
            prompt_anchor_present,
            ssh_direct_pending,
            external_input_lease_present,
            recent_stop_reason = recent_stop_reason.as_deref().unwrap_or("none"),
            placeholder_msg_id = placeholder_msg_id.map(|id| id.get()).unwrap_or(0),
            status_panel_msg_id = status_panel_msg_id.map(|id| id.get()).unwrap_or(0),
            frame_ack_outcome = ?session_bound_ack_outcome,
            "relay flight recorder"
        );
        // #3041 P1-3 (codex P1-3 R7): turn-boundary ACK reset. THIS turn's terminal
        // ACK has now been waited on (`session_bound_ack_outcome` is captured) and
        // logged. If a forward on this pass SPLIT a result-bearing chunk with a
        // trailing tail, a LATER turn (B) follows in the leftover buffer. B is
        // processed on a SUBSEQUENT pass — possibly while `turn_identity_for_panel`
        // is STILL pinned to THIS turn's offset (B's inflight not yet established),
        // which would make `carry_session_bound_ack_for_turn` KEEP this turn's stale
        // ack and let this turn's `Delivered` falsely satisfy B's ACK → B
        // black-holed. RESET the stored ack to `None` HERE, AFTER this turn consumed
        // it, so B starts with NO inherited ack → MissingTarget → §3.2 reconcile
        // (committed-offset SendFull-or-Skip) → B is never black-holed (worst case a
        // duplicate, the #3151-deferred edge). This is the primary R7 guarantee and
        // is independent of whether the pinned identity refreshes.
        if split_trailing_turn_follows {
            all_data_session_bound_relay_ack = None;
        }
        let mut watcher_direct_terminal_idle_committed = false;
        let mut tui_direct_anchor_terminal_body_visible = false;
        let mut tui_direct_anchor_or_lease_present_for_lifecycle =
            prompt_anchor_present_before_relay || external_input_lease_before_relay;

        // #3041 P1-1: acquire the delivery lease BEFORE the watcher direct-sends. Lease
        // identity = the turn-pinned id (`pinned_finalizer_turn_id`, the #3141
        // id-pinning) + the byte range `[data_start_offset, terminal_event_consumed_offset)`
        // — the SAME consumed end the commit/advance uses, so acquire and commit carry one
        // identity. Acquire only on the watcher-direct path (delegation is the sink's lease
        // P1-2; suppression/no-response deliver nothing).
        //
        // B2 (single-holder, §5.2): if a DIFFERENT watcher instance holds this cell
        // (Leased) `try_acquire` fails → this watcher MUST NOT direct-send (skip arm
        // below). Acquire is the atomic fast-path (B4); commit/advance/release run INLINE
        // (preserving the pre-P1-1 advance timing, avoiding an actor-deferral duplicate).
        // The actor CommitDelivery/ReleaseDelivery messages remain dormant.
        let (watcher_lease_turn, watcher_lease_key, watcher_lease_holder) =
            pinned_watcher_delivery_lease_identity(
                channel_id,
                shared.restart.current_generation,
                watcher_instance_id,
                inflight_before_relay.as_ref(),
                &tmux_session_name,
                current_offset,
            );
        let watcher_lease_start = data_start_offset;
        let watcher_lease_end = terminal_event_consumed_offset(current_offset, &all_data);
        // #3610 PR-1d: capture the legacy long-chunk anchor here; record it only
        // after the post-advance M4 commit below.
        let mut watcher_long_chunk_anchor_msg_id: Option<MessageId> = None;
        let mut watcher_long_chunk_delivered_body: Option<String> = None;
        let mut watcher_task_response_claim = None;
        let watcher_lease_cell = shared.delivery_lease(channel_id);
        // Lease only a watcher-direct real body; zero/inverted ranges never deliver.
        let watcher_will_direct_send = watcher_direct_fallback_after_session_bound_ack
            && has_direct_terminal_response
            && !direct_terminal_response_refused_duplicate;
        // #3089/#3998: the unified controller owns one lease for eligible non-task
        // terminals. Task responses keep the watcher lease around card+reference send;
        // empty/TUI-gated and placeholderless fresh sends remain legacy.
        let cutover_short_replace = task_notification_kind.is_none()
            && terminal_send::watcher_short_replace_cutover_decision(
                shared.ui.status_panel_v2_enabled,
                relay_decision.should_tag_monitor_origin,
                &watcher_provider,
                &direct_terminal_response,
                watcher_will_direct_send,
                watcher_lease_end > watcher_lease_start,
                placeholder_msg_id.is_some(),
                session_bound_fallback_uses_full_body,
                watcher_terminal_kind_requires_tui_completion_gate(terminal_kind),
            );
        // Pure no-double-acquire gate: `None` when cut over (the controller owns the
        // lease), so the watcher's own acquire below is skipped.
        let watcher_terminal_lease_range = terminal_send::watcher_terminal_lease_range(
            (watcher_will_direct_send && watcher_lease_end > watcher_lease_start)
                .then_some((watcher_lease_start, watcher_lease_end)),
            cutover_short_replace,
        );
        let watcher_lease_acquired = watcher_terminal_lease_range.is_some()
            // #3041 B3: reclaim elapsed leases on the shared monotonic clock; a live
            // heartbeat remains protected and other watchers B2-skip.
            && try_acquire_watcher_delivery_lease(
                &watcher_lease_cell,
                watcher_lease_holder,
                &watcher_lease_key,
                watcher_lease_start,
                watcher_lease_end,
            );
        // B2: another holder means no duplicate send. Controller cutover handles its
        // own acquire failure; same-holder retries remain bounded by monotonic advance.
        let watcher_lease_b2_skip = watcher_will_direct_send
            && watcher_lease_end > watcher_lease_start
            && !watcher_lease_acquired
            && !cutover_short_replace;

        // #3041 P1-1 (codex R2 Issue-2, BLOCKER B5 — DEFERRED, NOT a regression): the
        // lease range is the FULL `[data_start_offset, consumed_end)`. A crash AFTER
        // chunk 1 but BEFORE commit lets a replacement reclaim the EXPIRED lease and
        // re-send the WHOLE range → partial DUPLICATE. Exact-once on a partial
        // multi-chunk crash needs per-message-id partial-commit state, EXPLICITLY
        // deferred to B5. NOT a regression: the heartbeat below means a LIVE holder is
        // never reclaimed mid-send, so this matches pre-P1-1 crash behaviour (no lease
        // then either); P1-1 only adds a bounded (≤ deadline) re-delivery delay.
        //
        // #3041 P1-1 (§3, codex R2 Issue-1): keep the lease alive WHILE the send is in
        // flight. The deadline is short (15s) for fast dead-holder recovery; a long
        // legitimate send (60+ rate-limited chunks past any FIXED deadline) is covered by
        // this background heartbeat `renew()`ing every 5s. `stop()`ped BEFORE the inline
        // commit (and aborts on drop), so it never races the commit. Spawned ONLY when we
        // acquired; the B2-skip / no-send / #3089-A4-cutover arms have no lease to renew.
        let watcher_lease_heartbeat = watcher_delivery_lease_heartbeat(
            watcher_lease_acquired,
            watcher_lease_cell.clone(),
            watcher_lease_holder,
            &watcher_lease_key,
        );

        let mut retry_terminal_delivery_from_offset = false;
        // #4194: bound every await in the slot-held terminal relay expression.
        // On timeout, fall into the same failed-undelivered rewind path as a
        // transient send failure; never advance watermarks from this branch.
        let relay_ok = match watcher_relay_emission_with_timeout(async {
            if session_bound_relay_owns_terminal_delivery {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Delegating terminal response to session-bound StreamRelay sink ({} chars, offset {}, task_notification_kind={})",
                current_response.len(),
                data_start_offset,
                task_notification_kind
                    .map(TaskNotificationKind::as_str)
                    .unwrap_or("none")
            );
            if has_current_response {
                tui_direct_anchor_terminal_body_visible = true;
                last_relayed_offset = Some(turn_data_start_offset);
                last_observed_generation_mtime_ns =
                    Some(read_generation_file_mtime_ns(&tmux_session_name));
                crate::services::observability::watcher_latency::record_first_relay(
                    channel_id.get(),
                );
                // #3558 (codex review follow-up): the old unlocked
                // `load_inflight_state` → mutate → `save_inflight_state(&inflight)`
                // re-wrote the WHOLE stale row (including a possibly-backward
                // `last_offset`/`response_sent_offset`), reintroducing the exact
                // backward-write TOCTOU the #3558 fix closed in the streaming /
                // terminal-commit paths. Route the relay-success watermark through
                // the single-flock RMW helper, which patches ONLY
                // `last_watcher_relayed_*` and preserves the disk watermark.
                // #3041 P1-3 (Part a, B1 — FRAME-CARRIED): the authoritative
                // consumed-terminal END is NOT written here; it rides the
                // RESULT-bearing `StreamFrame` and the sink advances
                // `confirmed_end_offset` identity-gated on its confirmed POST.
                if let Some(identity) = inflight_identity_before_relay.as_ref() {
                    let _ =
                        crate::services::discord::inflight::persist_watcher_relay_watermark_locked(
                            &watcher_provider,
                            channel_id.get(),
                            identity,
                            &tmux_session_name,
                            crate::services::discord::inflight::WatcherRelayWatermarkPatch {
                                last_watcher_relayed_offset: Some(turn_data_start_offset),
                                last_watcher_relayed_generation_mtime_ns:
                                    last_observed_generation_mtime_ns,
                            },
                        );
                }
            }
            clear_provider_overload_retry_state(channel_id);
            true
        } else if matches!(
            watcher_resend_action,
            Some(WatcherTerminalResendAction::SkipAlreadyCommitted)
        ) {
            // #3041 P1-3 (Part b, §3.2): the offset authority already covers this
            // terminal range (`committed >= end`) — the session-bound sink already
            // delivered it (the terminal-commit ACK merely lagged the 10s wait, and
            // Part (a) advanced the authority on the sink's confirmed POST). This is
            // the failure-mode-① case: re-sending would DUPLICATE. Treat it as a
            // completed delegated delivery (mirror the delegation-success arm): do NOT
            // re-send. `relay_ok = true` so the lifecycle finalizes exactly as a
            // delivered turn (the response IS on the channel, posted by the sink); the
            // offset is already at `end`, so the inline advance below is an idempotent
            // no-op.
            if has_current_response {
                tui_direct_anchor_terminal_body_visible = true;
                last_relayed_offset = Some(turn_data_start_offset);
                last_observed_generation_mtime_ns =
                    Some(read_generation_file_mtime_ns(&tmux_session_name));
            }
            // #4158: a LIVE `placeholder_msg_id` here is NOT necessarily the message
            // that holds the delivered body. When inflight vanished mid-turn the
            // streaming loop POSTed a FRESH placeholder while the sink delivered the
            // real answer through a DIFFERENT message and advanced the offset
            // authority — that fresh placeholder is stale residue and, unless
            // reconciled, is orphaned (never edited/deleted/finalized). Route it
            // through the same guarded-cleanup helper the no-response arm uses: it
            // PRESERVES the placeholder when the delivered anchor IS this message
            // (#3593 sink-delivered-body case → `Protected`) and DELETES it only when
            // the delivered anchor is a DIFFERENT message covering the same committed
            // coordinate space (#4158 residue → `Found`); a body-bearing placeholder
            // with no positive delivered-elsewhere proof is preserved fail-safe.
            if let Some(msg_id) = placeholder_msg_id {
                let outcome = delete_terminal_placeholder_unless_delivered(
                    &http,
                    channel_id,
                    &shared,
                    &watcher_provider,
                    &tmux_session_name,
                    msg_id,
                    inflight_before_relay.as_ref(),
                    Some((
                        turn_data_start_offset,
                        terminal_event_consumed_offset(current_offset, &all_data),
                    )),
                    response_sent_offset,
                    &last_edit_text,
                    // #4158: post-commit arm — the placeholder may be the sink's
                    // PlaceholderEdit target, so require positive `Found` proof to
                    // delete (see apply_terminal_committed_delete_proof_gate).
                    true,
                    "watcher_skip_already_committed_cleanup",
                )
                .await;
                if outcome.is_some_and(|outcome| outcome.is_committed()) {
                    drop_placeholder_orphan_record(&watcher_provider, &shared, channel_id, msg_id);
                    placeholder_msg_id = None;
                    placeholder_from_restored_inflight = false;
                    last_edit_text.clear();
                }
            }
            clear_provider_overload_retry_state(channel_id);
            true
        } else if matches!(
            watcher_resend_action,
            Some(WatcherTerminalResendAction::WaitInFlight)
        ) {
            // #3151: a sink POST is genuinely IN FLIGHT for this range
            // (`Leased{Sink, fresh}`). Do NOT re-send / finalize / delete the
            // placeholder (the sink is about to post into it). Return `false` so
            // `terminal_output_committed` stays false: the turn is left OPEN and
            // re-entered NEXT pass. BOUNDED by the sink's lease deadline — within one
            // `DELIVERY_LEASE_DEADLINE_MS` the sink commits+releases (→ committed>=end →
            // SkipAlreadyCommitted) or dies (→ deadline lapses → reclaim + SendFull).
            // The sole arm closing the slow-sink-in-flight duplicate (#3151).
            let plan = watcher_wait_inflight_retry_plan();
            retry_terminal_delivery_from_offset = plan.retry_offset;
            plan.relay_ok
        } else if watcher_lease_b2_skip {
            // #3041 P1-1 B2 (single-holder, §5.2): a DIFFERENT watcher instance already
            // holds the delivery lease for this channel/turn/range (mid-send or not yet
            // committed/released/reclaimed). A replacement MUST NOT re-acquire and
            // re-emit — the duplicate-send vector the lease closes. Skip the direct send;
            // `terminal_output_committed` stays false so no offset advance / lifecycle
            // side-effects run; the live holder commit-advances the offset itself.
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %watcher_provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = %tmux_session_name,
                data_start_offset = watcher_lease_start,
                lease_end = watcher_lease_end,
                "  [{ts}] 👁 #3041 B2: delivery lease held by another holder — skipped duplicate terminal send for {tmux_session_name} (range {watcher_lease_start}..{watcher_lease_end})"
            );
            false
        } else if direct_terminal_response_refused_duplicate {
            tracing::warn!(
                provider = %watcher_provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = %tmux_session_name,
                data_start_offset = watcher_lease_start,
                lease_end = watcher_lease_end,
                "watcher: refused degenerate-key duplicate terminal response without committing delivery; waiting for fresh in-range output"
            );
            false
        } else if watcher_direct_fallback_after_session_bound_ack {
            terminal_direct_fallback::apply_watcher_direct_fallback_send(
                &http,
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                direct_terminal_response,
                relay_decision.should_tag_monitor_origin,
                data_start_offset,
                current_offset,
                turn_data_start_offset,
                response_sent_offset,
                task_notification_kind,
                task_notification_context.as_ref(),
                terminal_kind,
                has_direct_terminal_response,
                session_bound_fallback_uses_full_body,
                cutover_short_replace,
                single_message_panel_footer_mode,
                &watcher_lease_cell,
                watcher_lease_turn,
                &watcher_lease_key,
                watcher_instance_id,
                watcher_lease_start,
                watcher_lease_end,
                &inflight_before_relay,
                &inflight_identity_before_relay,
                turn_identity_for_panel.as_ref(),
                external_input_lease_before_relay,
                external_input_lease_generation_before_relay,
                prompt_anchor_present_before_relay,
                ssh_direct_pending,
                terminal_direct_fallback::WatcherDirectFallbackLocals {
                    tui_direct_anchor_terminal_body_visible:
                        &mut tui_direct_anchor_terminal_body_visible,
                    placeholder_msg_id: &mut placeholder_msg_id,
                    placeholder_from_restored_inflight: &mut placeholder_from_restored_inflight,
                    last_edit_text: &mut last_edit_text,
                    watcher_streaming_rollover_frozen_msg_ids:
                        &mut watcher_streaming_rollover_frozen_msg_ids,
                    watcher_long_chunk_anchor_msg_id: &mut watcher_long_chunk_anchor_msg_id,
                    watcher_long_chunk_delivered_body: &mut watcher_long_chunk_delivered_body,
                    completion_footer_terminal_target: &mut completion_footer_terminal_target,
                    retry_terminal_delivery_from_offset: &mut retry_terminal_delivery_from_offset,
                    tui_direct_anchor_or_lease_present_for_lifecycle:
                        &mut tui_direct_anchor_or_lease_present_for_lifecycle,
                    watcher_direct_terminal_idle_committed:
                        &mut watcher_direct_terminal_idle_committed,
                    last_relayed_offset: &mut last_relayed_offset,
                    last_observed_generation_mtime_ns: &mut last_observed_generation_mtime_ns,
                    task_response_claim: &mut watcher_task_response_claim,
                },
            )
            .await
        } else if relay_decision.suppressed {
            discrete_trigger_marker::enqueue_suppressed_machine_trigger_marker(
                &shared,
                channel_id,
                &tmux_session_name,
                data_start_offset,
                task_notification_kind,
                task_notification_context.as_ref(),
                tool_state.transcript_events.len(),
            )
            .await;
            let task_notification_detail = format!(
                "{} kind={} offset={}",
                tmux_session_name,
                task_notification_kind
                    .map(TaskNotificationKind::as_str)
                    .unwrap_or("none"),
                data_start_offset,
            );
            let ctx = PlaceholderSuppressContext {
                origin: PlaceholderSuppressOrigin::TaskNotificationTerminal,
                provider: &watcher_provider,
                placeholder_msg_id,
                response_sent_offset,
                last_edit_text: &last_edit_text,
                inflight_state: None,
                has_active_turn: false,
                tmux_session_name: &tmux_session_name,
                task_notification_kind,
                reattach_offset_match: false,
            };
            let decision = decide_placeholder_suppression(&ctx);
            apply_placeholder_suppression(
                &http,
                channel_id,
                &shared,
                &watcher_provider,
                &tmux_session_name,
                placeholder_msg_id,
                ctx.origin,
                decision,
                Some(&task_notification_detail),
            )
            .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Suppressed task-notification relay for {} (kind={}, offset {})",
                tmux_session_name,
                task_notification_kind
                    .map(TaskNotificationKind::as_str)
                    .unwrap_or("none"),
                data_start_offset
            );
            clear_provider_overload_retry_state(channel_id);
            false
        } else {
            if let Some(msg_id) = placeholder_msg_id {
                let _ = delete_terminal_placeholder_unless_delivered(
                    &http,
                    channel_id,
                    &shared,
                    &watcher_provider,
                    &tmux_session_name,
                    msg_id,
                    inflight_before_relay.as_ref(),
                    Some((
                        turn_data_start_offset,
                        terminal_event_consumed_offset(current_offset, &all_data),
                    )),
                    response_sent_offset,
                    &last_edit_text,
                    // No commit on this arm → disposable chrome; keep the base table.
                    false,
                    "watcher_no_response_cleanup",
                )
                .await;
            }
            false
            }
        })
        .await
        {
            Ok(relay_ok) => relay_ok,
            Err(_) => {
                let plan = watcher_relay_emission_timeout_failure_plan(
                    &watcher_provider,
                    channel_id,
                    &tmux_session_name,
                    data_start_offset,
                    current_offset,
                );
                retry_terminal_delivery_from_offset = plan.retry_offset;
                plan.relay_ok
            }
        };
        if retry_terminal_delivery_from_offset {
            // #4115: bound the terminal-delivery rewind so a persistently failing
            // send cannot retry the SAME range forever — give up after N attempts
            // and let the turn finalize instead of looping.
            terminal_rewind_attempts = terminal_rewind_attempts.saturating_add(1);
            if matches!(
                watcher_rewind_attempt_disposition(terminal_rewind_attempts),
                WatcherRewindAttemptDisposition::GiveUp
            ) {
                warn_terminal_rewind_give_up(
                    &watcher_provider,
                    channel_id,
                    &tmux_session_name,
                    turn_data_start_offset,
                    terminal_rewind_attempts,
                );
            } else {
                pending_terminal_rewind_seed = watcher_terminal_rewind_seed_from_parts(
                    placeholder_msg_id,
                    status_panel_msg_id,
                    response_sent_offset,
                    &last_edit_text,
                    task_notification_kind,
                    finish_mailbox_on_completion,
                    restored_injected_prompt_message_id,
                    &watcher_streaming_rollover_frozen_msg_ids,
                );
                // #3041 P1-1 / #4169: a non-committing terminal delivery path
                // (partial send failure or WaitInFlight defer) must retry the SAME range
                // next loop. Leaving the lease `Leased` would make the retry's
                // `try_acquire` lose to our own held lease (B2-skip suppresses the
                // retry until the deadline reclaim), so abandon-release here
                // (Leased→Unleased). The sole non-committing abandon,
                // released on the cell directly (same-holder, no actor serialization);
                // identity-matched no-op when not acquired (#3089 A4 cutover: the
                // controller already released its own lease on the Unknown path).
                if watcher_lease_acquired {
                    watcher_lease_cell.release(
                        watcher_lease_holder,
                        watcher_lease_key.clone(),
                        watcher_lease_start,
                        watcher_lease_end,
                    );
                }
                current_offset = turn_data_start_offset;
                all_data.clear();
                all_data_start_offset = current_offset;
                all_data_fully_mirrored_to_session_relay = true;
                all_data_session_bound_relay_ack = None;
                all_data_first_forwarded_relay_sequence = None;
                // #2840: release before the backoff sleep (timing preserved);
                // the guard's Drop is the safety net for non-explicit exits.
                slot_guard.release();
                sleep_or_jsonl_event(
                    tokio::time::Duration::from_millis(500),
                    &jsonl_notify,
                    &dead_marker_notify,
                )
                .await;
                continue 'watcher_loop;
            }
        }
        let relay_suppressed = relay_decision.suppressed;
        let terminal_output_committed = relay_ok || relay_suppressed;
        if terminal_output_committed {
            terminal_delivery_observed = true;
        }
        // #3003: the no-response/stopped external-input panel reclaim runs once at
        // the terminal chokepoint near the top of this block (where
        // recent_stop_for_output / inflight_missing_before_relay are computed), so
        // no separate cleanup is needed here.
        let runtime_binding_candidate_offset = terminal_output_committed
            .then(|| terminal_event_consumed_offset(current_offset, &all_data));
        let terminal_delivery_committed = relay_ok
            && has_assistant_response
            && mark_watcher_terminal_delivery_committed(
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                inflight_identity_before_relay.as_ref(),
                &full_response,
                turn_data_start_offset,
                last_observed_generation_mtime_ns,
                runtime_binding_candidate_offset.unwrap_or(current_offset),
            );

        // #3646 OBSERVATION-ONLY (event 1/3 — terminal_body_commit): emit once the
        // body commit decision is in hand. The `if terminal_output_committed` guard
        // only GATES the emit, never the cleanup; `inflight_relay_owner` (snapshot)
        // and the finalizer-side `ledger_relay_owner` JOIN on turn_id to resolve the
        // #3607 None-ledger vs Watcher-finalize confusion. Orchestration lives in
        // relay_owner_observability (non-hot file); this is a thin pass-through.
        if terminal_output_committed {
            crate::services::discord::relay_owner_observability::emit_terminal_body_commit(
                watcher_provider.as_str(),
                channel_id.get(),
                inflight_before_relay
                    .as_ref()
                    .and_then(|s| s.dispatch_id.as_deref()),
                inflight_before_relay
                    .as_ref()
                    .and_then(|s| s.session_key.as_deref()),
                pinned_finalizer_turn_id(
                    inflight_before_relay.as_ref(),
                    &tmux_session_name,
                    current_offset,
                ),
                pinned_finalize_user_msg_id(
                    inflight_before_relay.as_ref(),
                    &tmux_session_name,
                    current_offset,
                ),
                status_panel_msg_id.map(|id| id.get()).unwrap_or(0),
                turn_data_start_offset,
                terminal_event_consumed_offset(current_offset, &all_data),
                inflight_before_relay
                    .as_ref()
                    .map(|state| state.effective_relay_owner_kind().as_str())
                    .unwrap_or("none"),
                terminal_delivery_committed,
            );
        }

        // #4047 TUI completion observation: the strict JSONL terminator is the
        // sole finalize authority. Pane quiescence is recorded for soak
        // comparison only; it must not suppress completion or lifecycle cleanup.
        let watcher_tui_gate_outcome = if terminal_output_committed
            && watcher_terminal_kind_requires_tui_completion_gate(terminal_kind)
        {
            run_tui_completion_gate(
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                task_notification_kind,
            )
            .await
        } else {
            TuiCompletionGateOutcome::NotGated
        };
        if let Some(candidate_offset) = runtime_binding_candidate_offset {
            if watcher_commit_should_advance_runtime_binding(
                terminal_output_committed,
                watcher_tui_gate_outcome,
                terminal_delivery_committed,
            ) {
                // Keep the SSH-direct replay watermark in lockstep with committed bytes
                // Busy pane observations no longer keep this a candidate.
                crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
                    &tmux_session_name,
                    &output_path,
                    candidate_offset,
                );
            }
        }
        let lifecycle_stage_paused = watcher_tui_gate_blocks_lifecycle(
            watcher_tui_gate_outcome,
            terminal_delivery_committed,
        );

        // #4106: whether the pre-panel early release below actually released THIS
        // turn's mailbox slot. When it did, the late
        // `finish_restored_watcher_active_turn` for the same pinned id is a
        // deterministic identity-guard miss; carrying this flag routes it through
        // the guard-miss-expected context so that EXPECTED no-op logs at debug
        // instead of spamming the wrong-turn WARN on every normal completion.
        let mut pre_panel_release_drove_finalize = false;
        let mut completion_chrome_timed_out = false;

        if terminal_output_committed && watcher_tui_gate_outcome.should_emit_completion() {
            // #2849: watcher-completed turns never traverse the bridge
            // StatusUpdate path, so the completed panel can lack the Context
            // line even when terminal output carried exact usage. Backfill the
            // exact final context usage onto the panel BEFORE rendering the
            // completed panel. Skip entirely when no exact usage exists or the
            // provider/model has no resolvable window — never fabricate numbers
            // and never reuse stale prior-turn usage. set_context_panel_usage is
            // also internally gated to context_window != 0. #3262: the same
            // turn-idle helper also injects `/compact` when live Claude usage
            // crosses the configured threshold (claude-only, once-per-cycle).
            if watcher_completion_chrome_with_timeout(
                crate::services::discord::adk_session::backfill_completed_panel_usage_and_maybe_inject_compact(
                    &shared, channel_id, &state, &watcher_provider, &tmux_session_name,
                ),
            )
            .await
            .is_err()
            {
                warn_watcher_completion_chrome_timeout(
                    &watcher_provider,
                    channel_id,
                    &tmux_session_name,
                    data_start_offset,
                    current_offset,
                    "completed_panel_usage_backfill_and_compact",
                );
                completion_chrome_timed_out = true;
            }
            // #2427 D wire (Codex round 2 HIGH-1): the watcher loop is not
            // turn-scoped — a new turn may have rewritten on-disk inflight by now, so
            // re-reading user_msg_id and feeding it into `clear_inflight_state_if_matches`
            // becomes self-authentication and *enables* the Pitfall #1 race the guard
            // prevents. Drop the explicit-signal hook on the watcher D wire and rely on
            // the unconditional `clear_inflight_state` at L~2996 (committed-output path).
            // The recovery_engine D wire is preserved (its `state.user_msg_id` is pinned
            // at recovery entry, not re-read at completion).
            // #3142: offset-pin the status-panel completion identity. The old
            // session-only derivation would bind the panel to a NEWER follow-up
            // snapshot (`turn_start_offset >= current_offset`) this range does NOT own,
            // aliasing completion onto the still-running newer turn. Reuse
            // `pinned_finalize_user_msg_id` (the `< current_offset` test) so the
            // identity is None for a newer snapshot, agreeing with the
            // reaction/transcript/analytics + finalize gate. The panel EDIT/finalize
            // below is ALSO gated on `!inflight_before_relay_is_stale_newer_turn` so a
            // stale newer panel is never completed (UI-only aliasing gap). In-range
            // turns are unchanged (gate false; only `expected_user_msg_id` pinned);
            // `!rebind_origin` kept for parity.
            //
            // #3142: same stale-newer predicate as the adopt site (L8328). The panel
            // can be owned by a NEWER turn with `user_msg_id == 0` (external/injected),
            // so the id==0-INCLUSIVE anchor variant is required (the id!=0 sibling would
            // MISS it). The `None` 2nd arg is sound (`is_some_and` → false); an in-range
            // id==0 watcher-direct turn is NOT flagged and STILL completes — the gate
            // keys off the OFFSET test, not `pinned == 0`.
            let inflight_before_relay_is_stale_newer_turn =
                committed_anchor_cleanup_is_stale_for_newer_turn(
                    inflight_before_relay.as_ref(),
                    None,
                    &tmux_session_name,
                    current_offset,
                );
            let pinned_status_panel_user_msg_id = pinned_finalize_user_msg_id(
                inflight_before_relay.as_ref(),
                &tmux_session_name,
                current_offset,
            );
            let status_panel_completion_user_msg_id = inflight_before_relay
                .as_ref()
                .filter(|inflight| !inflight.rebind_origin)
                .and_then(|_| {
                    (pinned_status_panel_user_msg_id != 0)
                        .then_some(pinned_status_panel_user_msg_id)
                });
            // #3055: re-derive this turn's session lifecycle panel line before
            // finalizing. The bridge does this on every status tick via
            // `refresh_session_panel_line_from_lifecycle`; the watcher-direct
            // completion path historically skipped it and so reused a stale
            // per-channel `🆕 새 세션 시작 (최근 대화 N개…)` snapshot from a prior
            // recovery/new-session turn. A watcher-direct TUI turn has
            // `user_msg_id == 0`, keying onto the `discord:<channel>:0` turn id
            // which has no session lifecycle row, so the panel is cleared and
            // the stale line is not rendered.
            let session_panel_lifecycle_user_msg_id = inflight_before_relay
                .as_ref()
                .filter(|inflight| {
                    inflight
                        .tmux_session_name
                        .as_deref()
                        .map(str::trim)
                        .is_some_and(|name| !name.is_empty() && name == tmux_session_name)
                })
                .map(|inflight| inflight.user_msg_id)
                .unwrap_or(0);
            if !completion_chrome_timed_out
                && watcher_completion_chrome_with_timeout(
                    refresh_watcher_session_panel_from_lifecycle(
                        &shared,
                        channel_id,
                        session_panel_lifecycle_user_msg_id,
                        &tmux_session_name,
                    ),
                )
                .await
                .is_err()
            {
                warn_watcher_completion_chrome_timeout(
                    &watcher_provider,
                    channel_id,
                    &tmux_session_name,
                    data_start_offset,
                    current_offset,
                    "session_panel_lifecycle_refresh",
                );
                completion_chrome_timed_out = true;
            }
            // #3142: gate the EDIT/finalize + orphan-store reconciliation on
            // `!inflight_before_relay_is_stale_newer_turn`. When the pre-relay
            // snapshot is a stale NEWER turn the older committed range must NOT touch
            // that newer turn's panel (or its orphan record). The current in-range
            // turn's own panel, if any, is created via the streaming sources and is
            // unaffected (in-range => gate false => completion fires as today).
            if !completion_chrome_timed_out && !inflight_before_relay_is_stale_newer_turn {
                // #3969 root invariant: read the CHOKEPOINT-FRESH inflight (this
                // `inflight_before_relay` is re-loaded after the synthetic row exists,
                // unlike the stale `:1017` flag) and suppress the #3089 footer for any
                // non-Managed (TUI-mirror) turn — closing the /loop self-paced leak.
                let turn_is_non_managed_tui_mirror =
                    watcher_inflight_is_non_managed_tui_mirror_for_session(
                        inflight_before_relay.as_ref(),
                        &tmux_session_name,
                    );
                // #3805 P2 (PR-C): skip the status-panel completion edit when a
                // NEWER panel epoch has superseded this stale completion for the
                // SAME owned panel (parity with the sink completion guard). Inert
                // on the default-OFF path (generation stays 0) and at PR-C (no
                // mid-turn re-anchor bumps the epoch); the re-anchor stage (PR-D)
                // makes it live.
                let two_message_status_panel_generation_superseded =
                    watcher_two_message_status_completion_superseded(
                        this_turn_status_panel_generation,
                        status_panel_msg_id,
                        inflight_before_relay.as_ref(),
                    );
                // #4106: release the exact pinned active slot before the awaited
                // status-panel edit. If a same-channel follow-up claims the mailbox
                // while Discord is round-tripping the edit, the late finalizer below
                // will identity-miss and must be an idempotent no-op decrement.
                if !lifecycle_stage_paused {
                    let pre_panel_inflight_state_for_finalize =
                        crate::services::discord::inflight::load_inflight_state(
                            &watcher_provider,
                            channel_id.get(),
                        );
                    let pre_panel_completion_is_stale_for_newer_turn =
                        committed_completion_is_stale_for_newer_turn(
                            inflight_before_relay.as_ref(),
                            pre_panel_inflight_state_for_finalize.as_ref(),
                            &tmux_session_name,
                            current_offset,
                        );
                    let pre_panel_restored_finalizer_turn_id = pinned_finalizer_turn_id(
                        inflight_before_relay.as_ref(),
                        &tmux_session_name,
                        current_offset,
                    );
                    if should_submit_restored_watcher_finalize(
                        pre_panel_completion_is_stale_for_newer_turn,
                        pre_panel_restored_finalizer_turn_id,
                    ) {
                        pre_panel_release_drove_finalize =
                            release_restored_watcher_active_turn_before_panel_edit(
                                &shared,
                                &watcher_provider,
                                channel_id,
                                pre_panel_restored_finalizer_turn_id,
                            )
                            .await;
                    }
                }
                if watcher_completion_chrome_with_timeout(
                    complete_watcher_terminal_footer_or_status_panel_with_sniffer(
                        &http,
                        &shared,
                        channel_id,
                        &watcher_provider,
                        status_panel_started_at,
                        single_message_panel_footer_mode,
                        &mut completion_footer_spin_idx,
                        completion_footer_terminal_target.clone(),
                        placeholder_msg_id,
                        &last_edit_text,
                        status_panel_msg_id,
                        &mut last_status_panel_text,
                        task_notification_kind,
                        Some(tmux_session_name.clone()),
                        |tmux_session_name| async move {
                            crate::services::discord::tmux::sniff_background_agent_pending_for_completion(
                                tmux_session_name.as_deref(),
                            )
                            .await
                        },
                        status_panel_completion_user_msg_id,
                        turn_is_external_input_for_session,
                        turn_is_non_managed_tui_mirror,
                        two_message_status_panel_generation_superseded,
                    ),
                )
                .await
                .is_err()
                {
                    warn_watcher_completion_chrome_timeout(
                        &watcher_provider,
                        channel_id,
                        &tmux_session_name,
                        data_start_offset,
                        current_offset,
                        "terminal_footer_or_status_panel_completion",
                    );
                    completion_chrome_timed_out = true;
                }
            } // #3142: end `if !inflight_before_relay_is_stale_newer_turn` (EDIT/finalize gate)
            // #3003 single-chokepoint reclaim safety: after completion the turn
            // frame ends and the next frame re-seeds `status_panel_msg_id`, so the
            // top-of-interval abandon reclaim never observes this finalized panel's
            // id again — no explicit reset needed here.
        }

        // #3646 OBSERVATION-ONLY (event 2/3 — terminal_ui_transition): label the
        // visible-UI path the watcher took. Reads the same signals the EDIT/finalize
        // block already branched on (`watcher_tui_gate_outcome` + the #3142
        // stale-newer gate) — no new decision, only RECORDS committed /
        // gate_suppressed / stale_identity. The guard gates the EMIT, not the
        // cleanup. Orchestration lives in relay_owner_observability (non-hot file).
        if terminal_output_committed {
            let ui_transition_pane_quiescent = match watcher_tui_gate_outcome {
                TuiCompletionGateOutcome::ConfirmedIdle => Some(true),
                TuiCompletionGateOutcome::BusyObserved => Some(false),
                // NotGated / SkippedDead: quiescence was not probed.
                TuiCompletionGateOutcome::NotGated | TuiCompletionGateOutcome::SkippedDead => None,
            };
            crate::services::discord::relay_owner_observability::emit_terminal_ui_transition(
                watcher_provider.as_str(),
                channel_id.get(),
                inflight_before_relay
                    .as_ref()
                    .and_then(|s| s.dispatch_id.as_deref()),
                inflight_before_relay
                    .as_ref()
                    .and_then(|s| s.session_key.as_deref()),
                pinned_finalize_user_msg_id(
                    inflight_before_relay.as_ref(),
                    &tmux_session_name,
                    current_offset,
                ),
                crate::services::discord::relay_owner_observability::TerminalUiOutcome::derive(
                    inflight_before_relay_is_stale_newer_turn,
                    watcher_tui_gate_outcome.should_emit_completion(),
                ),
                &format!("{watcher_tui_gate_outcome:?}"),
                ui_transition_pane_quiescent,
            );
        }

        // Advance the shared confirmed-delivery watermark on any committed
        // direct emission or empty-turn cleanup. CAS loop ensures we only ever move the
        // watermark FORWARD, even if some other instance has raced ahead.
        // #4047: busy pane observations do not pin the watermark; terminal
        // authority has already proven the turn is complete.
        let terminal_committed_offset = runtime_binding_candidate_offset.unwrap_or(current_offset);
        // #3041 P1-1 (§3, codex R2 Issue-1): the send completed by here. STOP the
        // heartbeat BEFORE the inline commit so the renew loop cannot race the
        // `commit`/`release`. A tick fired before `stop()` only `renew`s our OWN still-
        // `Leased` lease (no-op extension), which the commit flips to `Committed`; after
        // `stop()` no renews occur. `None` on the non-acquired arms (incl. #3089 A4
        // cutover — the controller ran its own heartbeat), so this is a no-op there.
        if let Some(hb) = watcher_lease_heartbeat {
            hb.stop();
        }
        let mut watcher_response_frontier_committed = false;
        if watcher_lease_acquired {
            // #3041 P1-1 (§5.2): commit the 3-way outcome and, on `Delivered`, advance
            // `confirmed_end_offset` — both INLINE at the pre-P1-1 timing. (#3089 A4: the
            // cut-over short-replace path is `watcher_lease_acquired == false` here — the
            // controller already committed+advanced+released its own lease.)
            //
            // WHY INLINE (not the awaited `CommitDelivery`/`ReleaseDelivery` actor): the
            // actor-commit could queue behind an awaited `Terminal` handler, keeping
            // `confirmed_end_offset` OLD across that await while `session_relay_sink`
            // (dedups on `committed_relay_offset` until P1-2) re-relays the SAME range →
            // the #3143 duplicate. Inline commit+advance keeps that consult current. The
            // cell's `commit` is an atomic CAS, so §5.2 holds without the actor;
            // ledger-coupling (§5.3) deferred (advance is a standalone monotonic CAS).
            //
            // 3-way: `Delivered` (advance to leased `end`), `NotDelivered` (clean send
            // failure), `Unknown` (TUI gate left us lifecycle-paused → ambiguous, do NOT
            // claim delivered). Advance ONLY on `Delivered`, mirroring the old
            // `!lifecycle_stage_paused` gate (leased `end` == `terminal_committed_offset`
            // on the committed path). Then release inline (same-holder) for the next turn.
            let commit_outcome = if lifecycle_stage_paused {
                crate::services::discord::LeaseOutcome::Unknown
            } else if relay_ok {
                crate::services::discord::LeaseOutcome::Delivered
            } else {
                crate::services::discord::LeaseOutcome::NotDelivered
            };
            let committed = watcher_lease_cell.commit(
                watcher_lease_holder,
                watcher_lease_key.clone(),
                watcher_lease_start,
                watcher_lease_end,
                commit_outcome,
            );
            debug_assert!(
                committed,
                "watcher must be able to commit its own freshly-acquired lease"
            );
            if committed && commit_outcome == crate::services::discord::LeaseOutcome::Delivered {
                // INLINE advance — exactly the pre-P1-1 call site/timing.
                advance_watcher_confirmed_end(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &tmux_session_name,
                    watcher_lease_end,
                    "src/services/discord/tmux_watcher.rs:watcher_lease_commit_advance",
                );
                watcher_response_frontier_committed = true;
                // #3610 PR-1d: record the durable terminal anchor for the legacy
                // long-chunk fallback arm ONLY here — gated on the SAME successful
                // commit+advance (M4) AND `Some` anchor (⇒ the long-chunk arm ran and
                // fully committed, (A)). Same-channel; logic in the sibling.
                if let Some(anchor) = watcher_long_chunk_anchor_msg_id {
                    if let Some(body) = watcher_long_chunk_delivered_body.as_deref() {
                        terminal_send::record_watcher_long_chunk_terminal_delivery(
                            &shared,
                            &watcher_provider,
                            channel_id,
                            (watcher_lease_start, watcher_lease_end),
                            Some(anchor.get()),
                            body,
                        );
                    }
                }
            }
            // Release (Unleased for the next turn). Inline same-holder compare-and-
            // release; idempotent no-op if the identity no longer matches (e.g. a dead
            // holder's lease was reclaimed after the deadline elapsed).
            let _ = watcher_lease_cell.release(
                watcher_lease_holder,
                watcher_lease_key.clone(),
                watcher_lease_start,
                watcher_lease_end,
            );
        } else if terminal_output_committed && !lifecycle_stage_paused {
            // Non-watcher-direct committed paths (relay-suppressed task notifications,
            // empty-turn cleanup, session-bound delegation that consumed the range) keep
            // the inline monotonic-CAS advance — NOT the lease-governed delivery path.
            advance_watcher_confirmed_end(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                terminal_committed_offset,
                "src/services/discord/tmux.rs:tmux_output_watcher_confirmed_end",
            );
            watcher_response_frontier_committed = true;
        }
        task_response_authority::commit_watcher_task_response_fence(
            &shared,
            &watcher_provider,
            channel_id,
            &tmux_session_name,
            watcher_response_frontier_committed,
            watcher_task_response_claim.as_ref(),
        )
        .await;
        // #3104: terminal/idle reconciliation. A turn can commit (the channel is
        // about to return to idle) without ever relaying a body onto the live
        // streaming placeholder — e.g. a session-bound/subagent-only turn whose
        // terminal output was delegated elsewhere, so `placeholder_msg_id` keeps
        // the last streaming edit it received. When that last edit still ends in
        // the transient `⠏ 계속 처리 중` footer, the message is left advertising
        // "still processing" forever (the legacy in-body footer counterpart to
        // the status-panel reclaim below). Strip the footer through the shared
        // final-output formatter so the visible message matches the idle runtime.
        //
        // Self-gated: only on genuine commit, and only when the body still ends with a footer — a
        // genuinely-still-streaming message never reaches this committed-output
        // block, and an already-finalized body is left untouched.
        if terminal_output_committed
            && !lifecycle_stage_paused
            && !completion_chrome_timed_out
            && !single_message_panel_footer_mode
            && let Some(placeholder) = placeholder_msg_id
            && let Some(finalized) = finalize_watcher_streaming_footer(
                single_message_panel_footer_mode,
                &last_edit_text,
                &watcher_provider,
            )
        {
            match watcher_completion_chrome_with_timeout(
                crate::services::discord::http::edit_channel_message(
                    &http,
                    channel_id,
                    placeholder,
                    &finalized,
                ),
            )
            .await
            {
                Ok(Ok(_)) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 👁 #3104 reconciled stale '계속 처리 중' streaming footer on channel {} msg {} at idle",
                        channel_id.get(),
                        placeholder.get()
                    );
                }
                Ok(Err(error)) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ #3104 failed to reconcile stale streaming footer on channel {} msg {}: {error}",
                        channel_id.get(),
                        placeholder.get()
                    );
                }
                Err(_) => {
                    warn_watcher_completion_chrome_timeout(
                        &watcher_provider,
                        channel_id,
                        &tmux_session_name,
                        data_start_offset,
                        current_offset,
                        "stale_streaming_footer_reconcile",
                    );
                }
            }
        }
        // Release the emission slot regardless of success. If delivery failed
        // the local `last_relayed_offset` also stayed put, so the same watcher
        // (or its replacement) can retry on the next tick without fighting
        // the slot. #2840: via the RAII guard, so a panic/abort before this
        // point also frees the slot (Drop) instead of wedging the channel.
        slot_guard.release();

        finish_monitor_auto_turn_if_claimed(
            &shared,
            &watcher_provider,
            channel_id,
            &mut monitor_auto_turn_claimed,
            &mut monitor_auto_turn_finished,
            &mut monitor_auto_turn_synthetic_msg_id,
            &mut monitor_auto_turn_ledger_generation,
        )
        .await;

        let provider_kind = watcher_provider.clone();
        let inflight_state = crate::services::discord::inflight::load_inflight_state(
            &provider_kind,
            channel_id.get(),
        );
        let watcher_session_id = state.last_session_id.clone();
        if terminal_output_committed {
            persist_watcher_provider_session_id(
                &shared,
                channel_id,
                &provider_kind,
                &tmux_session_name,
                watcher_session_id.as_deref(),
            )
            .await;
        }
        let result_usage = stream_line_state_token_usage(&state);
        if inflight_state.is_none() {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: inflight state missing for channel {} — using DB dispatch fallback",
                channel_id.get()
            );
        }

        // #3016 (codex R3): the late `inflight_state` re-read above (and the
        // pre-relay snapshot) can already hold a NEWER follow-up turn's id in the
        // R2/R3 offset-aliasing scenario — a follow-up on the SAME tmux session
        // whose `turn_start_offset >= current_offset` (it begins AFTER this
        // committed output range) does NOT make the watcher-yield guard yield, so
        // the watcher still processes this OLD range while inflight on disk
        // belongs to the newer turn. The finalize below is already safe (it uses
        // `pinned_finalize_user_msg_id`, which returns 0 for such a newer turn —
        // the EXACT complement of this gate's offset test), but the SAME block
        // also runs the `⏳ → ✅` reaction + transcript + analytics write and
        // `clear_inflight_state` on that late read. Compute the stale-range gate
        // ONCE here and skip those wrong-turn side-effects (see the two call sites
        // below). For every normal completion (inflight is THIS or an OLDER turn,
        // absent, or rebind_origin/`user_msg_id == 0`) this is FALSE → no-op.
        let completion_is_stale_for_newer_turn = committed_completion_is_stale_for_newer_turn(
            inflight_before_relay.as_ref(),
            inflight_state.as_ref(),
            &tmux_session_name,
            current_offset,
        );

        // #3142: the id==0-inclusive sibling gate for the two anchor-cleanup
        // branches below. The id!=0 `completion_is_stale_for_newer_turn` above
        // deliberately excludes `user_msg_id == 0` newer turns (to protect the
        // finalize/clear id-0 contract), but a newer external-input / injected
        // task-notification turn can have `user_msg_id == 0` while still owning a
        // real anchor (`injected_prompt_message_id` or the shared
        // `prompt_anchor_by_tmux` slot). Computing this once here keeps the late
        // re-read and the pre-relay snapshot both checked for the anchor branches.
        let anchor_cleanup_is_stale_for_newer_turn =
            committed_anchor_cleanup_is_stale_for_newer_turn(
                inflight_before_relay.as_ref(),
                inflight_state.as_ref(),
                &tmux_session_name,
                current_offset,
            );

        if !anchor_cleanup_is_stale_for_newer_turn
            && crate::services::discord::tui_prompt_relay::should_complete_tui_direct_anchor_lifecycle(
            terminal_output_committed,
            tui_direct_anchor_terminal_body_visible,
            tui_direct_anchor_or_lease_present_for_lifecycle,
            lifecycle_stage_paused,
            inflight_state.is_some(),
        ) {
            // #3350 issue-1 + codex r1-2 (lease-gated row-absent commit,
            // tombstone-BEFORE-deliver): resolve the #3303 own-pin markers for
            // the anchor we are ABOUT to ✅ — synchronously, before the Discord
            // await below. The old deliver-then-resolve order let a TTL sweep
            // firing during (or just before) the await claim the row-absent
            // marker uncovered and stack a ⚠ next to the delivered ✅. If the
            // ✅ delivery below then fails, the anchor keeps its ⏳ for retry
            // with the marker already resolved — the same residual state as
            // pre-PR (no marker existed), not a regression.
            if let Some(anchor) = crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
                watcher_provider.as_str(),
                &tmux_session_name,
                channel_id.get(),
            ) {
                crate::services::discord::tui_direct_abort_marker::resolve_own_claim_markers_for_visibly_completed_anchor(
                    watcher_provider.as_str(),
                    &tmux_session_name,
                    channel_id.get(),
                    anchor.message_id,
                );
            }
            let completed = crate::services::discord::tui_prompt_relay::complete_tui_direct_prompt_anchor_lifecycle_if_present(
                &shared,
                watcher_provider.as_str(),
                &tmux_session_name,
                channel_id,
                external_input_lease_generation_before_relay
                    .unwrap_or(shared.restart.current_generation),
                if lifecycle_stage_paused {
                    "watcher_terminal_delivery_visible_completion_suppressed"
                } else {
                    "watcher_terminal_delivery_visible_without_inflight"
                },
            )
            .await;
            // #3174: turn-identity guard on the ⏳ lifecycle vs the lease-gated
            // completion. The gate above can fire on the external-input LEASE
            // alone; a commit inside the sub-second `notify-post + ⏳-add`
            // window finds THIS turn's `record_prompt_anchor` not yet landed —
            // the completion above no-ops (`None`) and the lease clears after
            // delivery, stranding the ⏳. Record a deferred-completion marker
            // keyed to `(provider, tmux, channel)`; the SAME turn's
            // `record_prompt_anchor` (relay) drains it and finishes the swap.
            // Only when the anchor is genuinely still absent — a `None` from a
            // `create_reaction` error keeps the anchor findable and retries.
            // codex P1: stamp the gating lease's `generation`; the relay drains
            // ONLY on a matching generation, so a NEWER same-tmux turn cannot
            // complete the wrong ⏳. Anchor-only firings stay anchor-based.
            if completed.is_none()
                && let Some(turn_lease_generation) = external_input_lease_generation_before_relay
                && crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
                    watcher_provider.as_str(),
                    &tmux_session_name,
                    channel_id.get(),
                )
                .is_none()
            {
                crate::services::tui_prompt_dedupe::record_deferred_anchor_completion(
                    watcher_provider.as_str(),
                    &tmux_session_name,
                    channel_id.get(),
                    turn_lease_generation,
                );
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏳ #3174 watcher: lease-gated completion ran before anchor recorded (channel {}, tmux={}, turn_lease_generation={turn_lease_generation}) — deferred ⏳ completion to record_prompt_anchor",
                    channel_id.get(),
                    tmux_session_name
                );
            }
        } else if terminal_output_committed
            && !lifecycle_stage_paused
            && !anchor_cleanup_is_stale_for_newer_turn
            && inflight_state
                .as_ref()
                .is_some_and(watcher_inflight_needs_anchor_lifecycle_cleanup)
        {
            // #3099: the `⏳ → ✅` block below targets `state.user_msg_id`, but a
            // TUI-injected task-notification turn can complete with an inflight
            // whose `user_msg_id == 0` (no anchored Discord user message) while a
            // real notify-bot message still carries the `⏳`. The
            // `should_complete_tui_direct_anchor_lifecycle` gate above does not
            // fire here because an inflight is still present, so clean the
            // hourglass off the injected message's OWN id.
            //
            // #3099 codex re-review (P2): target THIS turn's pinned
            // `injected_prompt_message_id` rather than re-reading the single shared
            // prompt-anchor slot — under rapid/parallel injection that slot may
            // already belong to a later turn, and reading it would `✅` the wrong
            // (still-running) message.
            let pinned_injected_message_id = inflight_state
                .as_ref()
                .and_then(|state| state.injected_prompt_message_id);
            let pinned_injected_generation = inflight_state
                .as_ref()
                .map(|state| state.born_generation)
                .unwrap_or(shared.restart.current_generation);
            let _ = crate::services::discord::tui_prompt_relay::complete_tui_direct_anchor_lifecycle_for_inflight(
                &shared,
                watcher_provider.as_str(),
                &tmux_session_name,
                channel_id,
                pinned_injected_message_id,
                pinned_injected_generation,
                "watcher_task_notification_anchor_cleanup_user_msg_zero",
            )
            .await;
        }

        // Mark user message as completed: ⏳ → ✅ when inflight metadata is
        // available and terminal output is committed. #897 round-3 Medium:
        // skip the reaction + transcript + analytics block entirely for
        // `rebind_origin` inflights. Their `user_msg_id=0` points at no real
        // message, and persisting a transcript with
        // `turn_id=discord:<channel>:0` poisons session_transcripts /
        // turn_analytics. The notify-bot outbox enqueue above already
        // delivered the recovered response to the user; nothing else on the
        // success path is legitimate here.
        //
        // #2293 H2 — also skip on `lifecycle_stage_paused`. The ✅ reaction +
        // transcript row + analytics row all claim completion at this exact
        // JSONL offset; while the pane is still busy past the gate timeout
        // they would either lie about completion (✅) or write a row that
        // gets contradicted by the next pass (transcript / analytics).
        // Skip rebind_origin (synthetic) and user_msg_id == 0 (e.g. a
        // TUI-direct turn with no anchored Discord user message): there is no
        // message to react against, `discord:<channel>:0` would be a bogus
        // analytics/turn-id key, and `MessageId::new(0)` would panic. The
        // recovered response was already delivered via the notify-bot outbox
        // enqueue above, so skipping the reaction/analytics step is safe.
        //
        // #3016 (codex R3): also skip when `completion_is_stale_for_newer_turn` —
        // the late `inflight_state` belongs to a NEWER follow-up turn that began
        // AFTER this committed range. Marking it `✅` and writing its transcript /
        // analytics here would lie about a still-running turn's completion. The
        // finalize below independently refuses this turn (its
        // `pinned_finalize_user_msg_id` returns 0 via the complementary offset
        // test), so this gate keeps the reaction/transcript/analytics consistent
        // with that decision. No-op for every normal completion.
        if terminal_output_committed
            && !lifecycle_stage_paused
            && !completion_is_stale_for_newer_turn
            && let Some(state) = inflight_state
                .as_ref()
                .filter(|s| watcher_completion_lifecycle_applies(s))
        {
            let user_msg_id = serenity::MessageId::new(state.user_msg_id);
            crate::services::discord::turn_view_reconciler::note_intake_turn_completed(
                &shared,
                &http,
                channel_id,
                user_msg_id,
                state.born_generation,
                "tmux_watcher_terminal_commit",
            )
            .await;

            if has_assistant_response && shared.pg_pool.is_some() {
                let turn_id = format!("discord:{}:{}", channel_id.get(), state.user_msg_id);
                let channel_id_text = channel_id.get().to_string();
                let resolved_did = inflight_state
                    .as_ref()
                    .and_then(|s| s.dispatch_id.clone())
                    .or_else(|| {
                        crate::services::discord::adk_session::parse_dispatch_id(&state.user_text)
                    })
                    .or(
                        crate::services::discord::adk_session::lookup_pending_dispatch_for_thread(
                            shared.api_port,
                            channel_id.get(),
                        )
                        .await,
                    )
                    .or_else(|| {
                        resolve_dispatched_thread_dispatch_from_db(
                            shared.pg_pool.as_ref(),
                            channel_id.get(),
                        )
                    });
                if let Err(e) = crate::db::session_transcripts::persist_turn_db(
                    shared.pg_pool.as_ref(),
                    crate::db::session_transcripts::PersistSessionTranscript {
                        turn_id: &turn_id,
                        session_key: state.session_key.as_deref(),
                        channel_id: Some(channel_id_text.as_str()),
                        agent_id: resolve_role_binding(channel_id, state.channel_name.as_deref())
                            .as_ref()
                            .map(|binding| binding.role_id.as_str()),
                        provider: Some(provider_kind.as_str()),
                        dispatch_id: resolved_did.as_deref().or(state.dispatch_id.as_deref()),
                        user_message: &state.user_text,
                        assistant_message: &full_response,
                        events: &tool_state.transcript_events,
                        duration_ms: inflight_duration_ms(Some(state.started_at.as_str())),
                    },
                )
                .await
                {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!("  [{ts}] ⚠ watcher: failed to persist session transcript: {e}");
                }

                crate::services::discord::turn_bridge::persist_turn_analytics_row_with_handles(
                    shared.pg_pool.as_ref(),
                    &provider_kind,
                    channel_id,
                    user_msg_id,
                    resolve_role_binding(channel_id, state.channel_name.as_deref()).as_ref(),
                    resolved_did.as_deref().or(state.dispatch_id.as_deref()),
                    state.session_key.as_deref(),
                    watcher_session_id
                        .as_deref()
                        .or(state.session_id.as_deref()),
                    state,
                    result_usage.unwrap_or_default(),
                    inflight_duration_ms(Some(state.started_at.as_str())).unwrap_or(0),
                );
            }
        }

        let resolved_did = inflight_state
            .as_ref()
            .and_then(|state| state.dispatch_id.clone())
            .or_else(|| {
                inflight_state.as_ref().and_then(|state| {
                    crate::services::discord::adk_session::parse_dispatch_id(&state.user_text)
                })
            })
            .or(
                crate::services::discord::adk_session::lookup_pending_dispatch_for_thread(
                    shared.api_port,
                    channel_id.get(),
                )
                .await,
            )
            .or_else(|| {
                resolve_dispatched_thread_dispatch_from_db(
                    shared.pg_pool.as_ref(),
                    channel_id.get(),
                )
            });

        if resolved_did.is_none() && has_assistant_response {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: no dispatch id resolved for channel {} after terminal success",
                channel_id.get()
            );
        }
        let current_worktree_path = {
            let mut data = shared.core.lock().await;
            data.sessions
                .get_mut(&channel_id)
                .and_then(|session| session.validated_path(channel_id.get()))
        };

        // #2161 (Codex round-2 H1): if the TUI quiescence gate timed out
        // before terminal delivery was durably mirrored, treat the watcher
        // dispatch finalization as "preserved": don't complete the dispatch,
        // don't kick off queued work, and leave inflight alone so the next
        // watcher pass / placeholder sweeper observes the still-busy pane and
        // reconciles. Once delivery is mirrored, match the bridge path and
        // allow cleanup while still suppressing visible completion.
        let dispatch_ok = if lifecycle_stage_paused {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %watcher_provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = %tmux_session_name,
                "[{ts}] ⚠ watcher: dispatch finalization deferred — TUI quiescence gate timed out (#2161)"
            );
            false
        } else if let Some(did) = resolved_did
            .as_deref()
            .filter(|_| !completion_is_stale_for_newer_turn)
        {
            // #3142: when stale, the late `inflight_state.dispatch_id` (the first
            // fallback in `resolved_did`) belongs to the NEWER running turn;
            // completing it here with the OLDER `full_response` is wrong-turn
            // corruption. Fall through to the `else => true` no-finalize arm
            // (dispatch_ok stays true; downstream clear/finalize keep their own
            // stale gates) — the newer turn finalizes its own dispatch on its later
            // pass. FALSE in every normal case, so the common finalize is untouched.
            let finalization =
                crate::services::discord::streaming_finalizer::finalize_watcher_streaming_dispatch(
                    crate::services::discord::streaming_finalizer::WatcherStreamingFinalRequest {
                        pg_pool: shared.pg_pool.as_ref(),
                        dispatch_id: did,
                        adk_cwd: current_worktree_path.as_deref(),
                        full_response: &full_response,
                        has_assistant_response,
                    },
                )
                .await;
            if !finalization.completed {
                tracing::debug!(
                    disposition = ?finalization.disposition,
                    dispatch_type = ?finalization.dispatch_type,
                    error = ?finalization.error,
                    "watcher streaming finalizer preserved dispatch state"
                );
            }
            finalization.completed
        } else {
            true
        };

        // #4229 S7: committed-completion finalize tail moved verbatim to
        // tmux_watcher/terminal_commit_epilogue.rs (TUI history push / tombstone+drain /
        // guarded inflight clear / restored finalize / idle commit / kickoff / stop decision).
        let terminal_commit_epilogue_context = TerminalCommitEpilogueContext {
            shared: &shared,
            channel_id,
            watcher_provider: &watcher_provider,
            provider_kind: &provider_kind,
            tmux_session_name: &tmux_session_name,
            output_path: &output_path,
            relay_coord: &relay_coord,
            turn_delivered: &turn_delivered,
        };
        let terminal_commit_epilogue_locals = TerminalCommitEpilogueLocals {
            terminal_output_committed,
            lifecycle_stage_paused,
            relay_suppressed,
            has_assistant_response,
            completion_is_stale_for_newer_turn,
            anchor_cleanup_is_stale_for_newer_turn,
            inflight_state: &inflight_state,
            inflight_before_relay: &inflight_before_relay,
            full_response: &full_response,
            watcher_turn_nonce: &watcher_turn_nonce,
            resolved_did: &resolved_did,
            dispatch_ok,
            terminal_delivery_committed,
            watcher_tui_gate_outcome,
            tui_direct_anchor_terminal_body_visible,
            terminal_kind,
            terminal_evidence_offset,
            finish_mailbox_on_completion,
            pre_panel_release_drove_finalize,
            current_offset,
            data_start_offset,
        };
        let mut terminal_commit_epilogue_state = TerminalCommitEpilogueState {
            turn_result_relayed: &mut turn_result_relayed,
            watcher_direct_terminal_idle_committed: &mut watcher_direct_terminal_idle_committed,
            monitor_auto_turn_claimed: &mut monitor_auto_turn_claimed,
            monitor_auto_turn_finished: &mut monitor_auto_turn_finished,
            monitor_auto_turn_synthetic_msg_id: &mut monitor_auto_turn_synthetic_msg_id,
            monitor_auto_turn_ledger_generation: &mut monitor_auto_turn_ledger_generation,
        };
        match run_terminal_commit_epilogue(
            &terminal_commit_epilogue_context,
            terminal_commit_epilogue_locals,
            &mut terminal_commit_epilogue_state,
        )
        .await
        {
            TerminalCommitEpilogueOutcome::BreakWatcherLoop => break 'watcher_loop,
            TerminalCommitEpilogueOutcome::Fallthrough => {}
        }

        let inflight_missing_for_fallback = missing_inflight_after_session_bound_delivery(
            inflight_state.is_none(),
            session_bound_relay_owns_terminal_delivery,
        );
        let tmux_alive_for_missing_inflight =
            if inflight_missing_for_fallback && resolved_did.is_none() && terminal_output_committed
            {
                probe_tmux_session_liveness(&tmux_session_name).await
            } else {
                true
            };
        let recent_turn_stop =
            recent_turn_stop_for_watcher_range(channel_id, &tmux_session_name, data_start_offset);
        let placeholder_cleanup_committed = placeholder_msg_id.is_some_and(|msg_id| {
            shared.ui.placeholder_cleanup.terminal_cleanup_committed(
                &provider_kind,
                channel_id,
                msg_id,
            )
        });
        let missing_inflight_plan = missing_inflight_fallback_observation(
            inflight_missing_for_fallback,
            resolved_did.is_some(),
            terminal_output_committed,
            recent_turn_stop.is_some(),
            tmux_alive_for_missing_inflight,
        );
        if missing_inflight_plan.suppressed_by_recent_stop {
            if placeholder_cleanup_committed {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ watcher: missing-inflight observation suppressed for channel {} — terminal placeholder cleanup already committed",
                    channel_id.get()
                );
            } else if let Some(stop) = recent_turn_stop {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ↻ watcher: missing-inflight observation suppressed for channel {} — recent turn stop still active ({})",
                    channel_id.get(),
                    stop.reason
                );
            }
        } else if !tmux_alive_for_missing_inflight {
            let _drained_offset = drain_missing_inflight_dead_tmux_tail_to_eof(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                &output_path,
                current_offset,
            )
            .await;
            handle_tmux_watcher_observed_death(
                channel_id,
                &http,
                &shared,
                &tmux_session_name,
                &output_path,
                &watcher_provider,
                prompt_too_long_killed,
                watcher_lifecycle_terminal_delivery_observed(
                    terminal_delivery_observed,
                    turn_delivered.load(Ordering::Acquire),
                ),
            )
            .await;
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
                &mut monitor_auto_turn_synthetic_msg_id,
                &mut monitor_auto_turn_ledger_generation,
            )
            .await;
            break 'watcher_loop;
        } else if missing_inflight_plan.mark_degraded {
            crate::services::observability::metrics::record_watcher_db_fallback_resolve_failed(
                channel_id.get(),
                provider_kind.as_str(),
            );
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: missing inflight with unresolved dispatch for channel {} while tmux is still alive; keeping watcher attached without synthetic inflight (tmux={})",
                channel_id.get(),
                tmux_session_name
            );
        }

        // Update session tokens from result event and auto-compact if threshold exceeded
        if let Some(tokens) = result_usage.map(|usage| usage.context_occupancy_input_tokens()) {
            let provider = shared.settings.read().await.provider.clone();
            let session_key = crate::services::discord::adk_session::build_adk_session_key(
                &shared, channel_id, &provider, None,
            )
            .await;
            let channel_name = {
                let data = shared.core.lock().await;
                data.sessions
                    .get(&channel_id)
                    .and_then(|s| s.channel_name.clone())
            };
            let thread_channel_id = channel_name
                .as_deref()
                .and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name);
            let agent_id = resolve_role_binding(channel_id, channel_name.as_deref())
                .map(|binding| binding.role_id);
            crate::services::discord::adk_session::post_adk_session_status(
                session_key.as_deref(),
                channel_name.as_deref(),
                None,
                watcher_terminal_token_update_status(watcher_direct_terminal_idle_committed),
                &provider,
                None,
                Some(tokens),
                None,
                None,
                thread_channel_id,
                Some(channel_id),
                agent_id.as_deref(),
                shared.api_port,
            )
            .await;

            let ctx_cfg =
                crate::services::discord::adk_session::fetch_context_thresholds(shared.api_port)
                    .await;
            let pct = (tokens * 100) / ctx_cfg.context_window.max(1);
            // #227: Re-enabled with 5-min cooldown (matches turn_bridge path).
            // Without cooldown, the compact turn's own result could re-trigger compact.
            let cooldown_key = format!("auto_compact_cooldown:{}", channel_id.get());
            let cooldown_value =
                match crate::services::discord::internal_api::get_kv_value(&cooldown_key) {
                    Ok(value) => value,
                    Err(_) => {
                        if let Some(pg_pool) = shared.pg_pool.as_ref() {
                            sqlx::query_scalar::<_, Option<String>>(
                                "SELECT value
                             FROM kv_meta
                             WHERE key = $1
                               AND (expires_at IS NULL OR expires_at > NOW())
                             LIMIT 1",
                            )
                            .bind(&cooldown_key)
                            .fetch_optional(pg_pool)
                            .await
                            .ok()
                            .flatten()
                            .flatten()
                        } else {
                            None
                        }
                    }
                };
            let compact_cooldown_ok =
                cooldown_value
                    .and_then(|v| v.parse::<i64>().ok())
                    .map_or(true, |ts| {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64;
                        now - ts > 300 // 5 min cooldown
                    });
            // DISABLED — token counting still unreliable
            if false && pct >= ctx_cfg.compact_pct && !is_prompt_too_long && compact_cooldown_ok {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚡ [watcher] Auto-compact: {} at {pct}% ({tokens} tokens)",
                    tmux_session_name
                );
                let name = tmux_session_name.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    crate::services::platform::tmux::send_keys(&name, &["/compact", "Enter"])
                })
                .await;
                // Set cooldown timestamp
                let cooldown_key = format!("auto_compact_cooldown:{}", channel_id.get());
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let now_text = now.to_string();
                if crate::services::discord::internal_api::set_kv_value(&cooldown_key, &now_text)
                    .is_err()
                {
                    if let Some(pg_pool) = shared.pg_pool.as_ref() {
                        let _ = sqlx::query(
                            "INSERT INTO kv_meta (key, value, expires_at)
                             VALUES ($1, $2, NULL)
                             ON CONFLICT (key) DO UPDATE
                             SET value = EXCLUDED.value,
                                 expires_at = EXCLUDED.expires_at",
                        )
                        .bind(&cooldown_key)
                        .bind(&now_text)
                        .execute(pg_pool)
                        .await;
                    }
                }
                // Notify: auto-compact triggered
                let target = format!("channel:{}", channel_id.get());
                let content = format!("🗜️ 자동 컨텍스트 압축 (사용률: {pct}%)");
                let _ = enqueue_outbox_best_effort(
                    shared.pg_pool.as_ref(),
                    OutboxMessage {
                        target: target.as_str(),
                        content: content.as_str(),
                        bot: "notify",
                        source: "system",
                        reason_code: None,
                        session_key: None,
                    },
                )
                .await;
            }
        }
    }

    // #4229 S5: post-stream-exit finalize tail moved verbatim to tmux_watcher/post_stream_exit.rs.
    run_post_stream_exit(PostStreamExitContext {
        channel_id,
        shared,
        tmux_session_name,
        cancel,
        watcher_turn_identity,
        watcher_instance_id,
    })
    .await;
}

#[cfg(test)]
#[path = "tmux_watcher/tests.rs"]
mod tests;
