use super::*;
use std::sync::Arc;

use crate::services::discord::session_relay_sink::journal::watcher as journal_watcher;

pub(super) struct TerminalPreflightContext<'a> {
    pub(super) http: &'a Arc<serenity::Http>,
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) channel_id: serenity::ChannelId,
    pub(super) watcher_provider: &'a ProviderKind,
    pub(super) tmux_session_name: &'a String,
    pub(super) output_path: &'a String,
}

pub(super) struct TerminalPreflightLocals<'a> {
    pub(super) current_offset: u64,
    pub(super) single_message_panel_footer_mode: bool,
    pub(super) all_data: &'a String,
    pub(super) terminal_kind: Option<WatcherTerminalKind>,
    pub(super) fresh_assistant_text_seen: bool,
    pub(super) tool_state: &'a WatcherToolState,
    pub(super) restored_response_seed: &'a String,
    pub(super) turn_is_external_input_for_session: bool,
    pub(super) task_notification_kind: Option<TaskNotificationKind>,
    pub(super) data_start_offset: u64,
    pub(super) turn_identity_for_panel:
        &'a Option<crate::services::discord::inflight::InflightTurnIdentity>,
}

pub(super) struct TerminalPreflightState<'a> {
    pub(super) watcher_streaming_rollover_frozen_msg_ids: &'a mut Vec<serenity::MessageId>,
    pub(super) placeholder_msg_id: &'a mut Option<serenity::MessageId>,
    pub(super) placeholder_from_restored_inflight: &'a mut bool,
    pub(super) status_panel_msg_id: &'a mut Option<serenity::MessageId>,
    pub(super) full_response: &'a mut String,
    pub(super) response_sent_offset: &'a mut usize,
    pub(super) last_edit_text: &'a mut String,
}

pub(super) struct TerminalPreflightPrepared {
    pub(super) prompt_anchor_present_before_relay: bool,
    pub(super) external_input_lease_before_relay: bool,
    pub(super) external_input_lease_generation_before_relay: Option<u64>,
    pub(super) inflight_before_relay: Option<InflightTurnState>,
    pub(super) inflight_identity_before_relay:
        Option<crate::services::discord::inflight::InflightTurnIdentity>,
    pub(super) inflight_before_relay_is_stale_newer_turn: bool,
    pub(super) has_assistant_response: bool,
    pub(super) has_current_response: bool,
    pub(super) recent_stop_for_output: Option<super::super::tmux_kill_policy::RecentTurnStop>,
    pub(super) inflight_missing_before_relay: bool,
    pub(super) inflight_silent_turn: bool,
}

#[derive(Debug)]
pub(super) enum TerminalPreflightOutcome {
    ContinueWatcherLoop,
    Proceed(TerminalPreflightReady),
}

#[derive(Debug)]
pub(super) struct TerminalPreflightReady {
    pub(super) placeholder_from_restored_inflight: bool,
    pub(super) last_edit_text: String,
    pub(super) last_relayed_offset: Option<u64>,
    pub(super) last_observed_generation_mtime_ns: Option<i64>,
    pub(super) monitor_auto_turn_claimed: bool,
    pub(super) monitor_auto_turn_finished: bool,
    pub(super) monitor_auto_turn_synthetic_msg_id: Option<serenity::MessageId>,
    pub(super) monitor_auto_turn_ledger_generation: Option<u64>,
}

pub(super) struct TerminalPreflightSuppressionLocals<'a> {
    pub(super) current_offset: u64,
    pub(super) all_data: &'a String,
    pub(super) data_start_offset: u64,
    pub(super) turn_data_start_offset: u64,
    pub(super) has_assistant_response: bool,
    pub(super) has_current_response: bool,
    pub(super) inflight_missing_before_relay: bool,
    pub(super) inflight_silent_turn: bool,
    pub(super) recent_stop_for_output: Option<super::super::tmux_kill_policy::RecentTurnStop>,
    pub(super) placeholder_msg_id: Option<serenity::MessageId>,
    pub(super) placeholder_from_restored_inflight: bool,
    pub(super) last_edit_text: String,
    pub(super) last_relayed_offset: Option<u64>,
    pub(super) last_observed_generation_mtime_ns: Option<i64>,
    pub(super) monitor_auto_turn_claimed: bool,
    pub(super) monitor_auto_turn_finished: bool,
    pub(super) monitor_auto_turn_synthetic_msg_id: Option<serenity::MessageId>,
    pub(super) monitor_auto_turn_ledger_generation: Option<u64>,
}

pub(super) struct TerminalPreflightSuppressionState<'a> {
    pub(super) placeholder_from_restored_inflight: &'a mut bool,
    pub(super) last_edit_text: &'a mut String,
    pub(super) last_relayed_offset: &'a mut Option<u64>,
    pub(super) last_observed_generation_mtime_ns: &'a mut Option<i64>,
    pub(super) monitor_auto_turn_claimed: &'a mut bool,
    pub(super) monitor_auto_turn_finished: &'a mut bool,
    pub(super) monitor_auto_turn_synthetic_msg_id: &'a mut Option<serenity::MessageId>,
    pub(super) monitor_auto_turn_ledger_generation: &'a mut Option<u64>,
}

fn write_terminal_preflight_suppression_state(
    state: &mut TerminalPreflightSuppressionState<'_>,
    placeholder_from_restored_inflight: bool,
    last_edit_text: String,
    last_relayed_offset: Option<u64>,
    last_observed_generation_mtime_ns: Option<i64>,
    monitor_auto_turn_claimed: bool,
    monitor_auto_turn_finished: bool,
    monitor_auto_turn_synthetic_msg_id: Option<serenity::MessageId>,
    monitor_auto_turn_ledger_generation: Option<u64>,
) {
    *state.placeholder_from_restored_inflight = placeholder_from_restored_inflight;
    *state.last_edit_text = last_edit_text;
    *state.last_relayed_offset = last_relayed_offset;
    *state.last_observed_generation_mtime_ns = last_observed_generation_mtime_ns;
    *state.monitor_auto_turn_claimed = monitor_auto_turn_claimed;
    *state.monitor_auto_turn_finished = monitor_auto_turn_finished;
    *state.monitor_auto_turn_synthetic_msg_id = monitor_auto_turn_synthetic_msg_id;
    *state.monitor_auto_turn_ledger_generation = monitor_auto_turn_ledger_generation;
}

pub(super) async fn run_terminal_preflight_suppression(
    context: &TerminalPreflightContext<'_>,
    locals: TerminalPreflightSuppressionLocals<'_>,
    state: &mut TerminalPreflightSuppressionState<'_>,
) -> TerminalPreflightOutcome {
    let http = context.http;
    let shared = context.shared;
    let channel_id = context.channel_id;
    let watcher_provider = context.watcher_provider;
    let tmux_session_name = context.tmux_session_name;
    let output_path = context.output_path;
    let TerminalPreflightSuppressionLocals {
        current_offset,
        all_data,
        data_start_offset,
        turn_data_start_offset,
        has_assistant_response,
        has_current_response,
        inflight_missing_before_relay,
        inflight_silent_turn,
        recent_stop_for_output,
        placeholder_msg_id,
        mut placeholder_from_restored_inflight,
        mut last_edit_text,
        mut last_relayed_offset,
        mut last_observed_generation_mtime_ns,
        mut monitor_auto_turn_claimed,
        mut monitor_auto_turn_finished,
        mut monitor_auto_turn_synthetic_msg_id,
        mut monitor_auto_turn_ledger_generation,
    } = locals;

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
            let confirmed_end = suppressed_terminal_confirmed_end(current_offset, &all_data);
            let generation_mtime_ns = read_generation_file_mtime_ns(&tmux_session_name);
            last_relayed_offset = Some(current_offset);
            last_observed_generation_mtime_ns = Some(generation_mtime_ns);
            advance_watcher_confirmed_end(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                confirmed_end,
                "src/services/discord/tmux.rs:silent_turn_suppressed_terminal_output",
            );
            // #5071 T1 S3b: O+S only — the frontier moved on suppression, not a POST.
            journal_watcher::settle_without_transport(
                shared,
                journal_watcher::WatcherObligationCoordinates {
                    provider: &watcher_provider,
                    channel_id,
                    tmux_session_name: &tmux_session_name,
                    generation_mtime_ns,
                    range: (data_start_offset, confirmed_end),
                },
                journal_watcher::SettlementReason::SilentTurnSuppressed,
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
        write_terminal_preflight_suppression_state(
            state,
            placeholder_from_restored_inflight,
            last_edit_text,
            last_relayed_offset,
            last_observed_generation_mtime_ns,
            monitor_auto_turn_claimed,
            monitor_auto_turn_finished,
            monitor_auto_turn_synthetic_msg_id,
            monitor_auto_turn_ledger_generation,
        );
        return TerminalPreflightOutcome::ContinueWatcherLoop;
    }
    if should_suppress_terminal_output_after_recent_stop(
        has_assistant_response,
        inflight_missing_before_relay,
        recent_stop_for_output.is_some(),
    ) {
        let stop = recent_stop_for_output.expect("recent stop checked above");
        let cleanup_committed = if let Some(msg_id) = placeholder_msg_id {
            if watcher_should_delete_suppressed_placeholder(placeholder_from_restored_inflight) {
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
            let confirmed_end = suppressed_terminal_confirmed_end(current_offset, &all_data);
            // #1270 codex P2: snapshot the current `.generation` mtime so
            // the local regression check has a real baseline (see the
            // matching snapshot in the rotation path).
            let generation_mtime_ns = read_generation_file_mtime_ns(&tmux_session_name);
            last_relayed_offset = Some(current_offset);
            last_observed_generation_mtime_ns = Some(generation_mtime_ns);
            advance_watcher_confirmed_end(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                confirmed_end,
                "src/services/discord/tmux.rs:cancel_tombstone_suppressed_terminal_output",
            );
            // #5071 T1 S3b: O+S only — cancel-tombstone suppression, no POST.
            journal_watcher::settle_without_transport(
                shared,
                journal_watcher::WatcherObligationCoordinates {
                    provider: &watcher_provider,
                    channel_id,
                    tmux_session_name: &tmux_session_name,
                    generation_mtime_ns,
                    range: (data_start_offset, confirmed_end),
                },
                journal_watcher::SettlementReason::CancelTombstoneSuppressed,
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
        write_terminal_preflight_suppression_state(
            state,
            placeholder_from_restored_inflight,
            last_edit_text,
            last_relayed_offset,
            last_observed_generation_mtime_ns,
            monitor_auto_turn_claimed,
            monitor_auto_turn_finished,
            monitor_auto_turn_synthetic_msg_id,
            monitor_auto_turn_ledger_generation,
        );
        return TerminalPreflightOutcome::ContinueWatcherLoop;
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
            write_terminal_preflight_suppression_state(
                state,
                placeholder_from_restored_inflight,
                last_edit_text,
                last_relayed_offset,
                last_observed_generation_mtime_ns,
                monitor_auto_turn_claimed,
                monitor_auto_turn_finished,
                monitor_auto_turn_synthetic_msg_id,
                monitor_auto_turn_ledger_generation,
            );
            return TerminalPreflightOutcome::ContinueWatcherLoop;
        }
    }

    TerminalPreflightOutcome::Proceed(TerminalPreflightReady {
        placeholder_from_restored_inflight,
        last_edit_text,
        last_relayed_offset,
        last_observed_generation_mtime_ns,
        monitor_auto_turn_claimed,
        monitor_auto_turn_finished,
        monitor_auto_turn_synthetic_msg_id,
        monitor_auto_turn_ledger_generation,
    })
}

pub(super) async fn run_terminal_preflight_prepare(
    context: &TerminalPreflightContext<'_>,
    locals: TerminalPreflightLocals<'_>,
    state: &mut TerminalPreflightState<'_>,
) -> TerminalPreflightPrepared {
    let http = context.http;
    let shared = context.shared;
    let channel_id = context.channel_id;
    let watcher_provider = context.watcher_provider;
    let tmux_session_name = context.tmux_session_name;
    let output_path = context.output_path;
    let TerminalPreflightLocals {
        current_offset,
        single_message_panel_footer_mode,
        all_data,
        terminal_kind,
        fresh_assistant_text_seen,
        tool_state,
        restored_response_seed,
        turn_is_external_input_for_session,
        task_notification_kind,
        data_start_offset,
        turn_identity_for_panel,
    } = locals;

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
    let external_input_lease_before_relay = external_input_lease_before_relay_snapshot.is_some();
    let external_input_lease_generation_before_relay = external_input_lease_before_relay_snapshot
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
            &mut *state.watcher_streaming_rollover_frozen_msg_ids,
            Some(inflight),
            &tmux_session_name,
        );
        adopt_watcher_terminal_message_ids_from_inflight(
            &mut *state.placeholder_msg_id,
            &mut *state.placeholder_from_restored_inflight,
            &mut *state.status_panel_msg_id,
            inflight,
            &tmux_session_name,
        );
        if single_message_panel_footer_mode {
            *state.status_panel_msg_id = None;
        }
    }
    let fresh_seen = fresh_assistant_text_seen;
    let drop_seed = local_cmd_no_output(&all_data, terminal_kind, fresh_seen, &tool_state);
    let restored_seed_delivery_confirmed = drop_seed
        && restored_response_seed
            .get(*state.response_sent_offset..)
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
        &mut *state.full_response,
        &mut *state.response_sent_offset,
        &mut *state.last_edit_text,
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
            fresh_response_len = state.full_response.len(),
            "watcher: discarded restored response seed before no-inflight terminal relay"
        );
    }
    let has_assistant_response = !state.full_response.trim().is_empty();
    let current_response = state
        .full_response
        .get(*state.response_sent_offset..)
        .unwrap_or("");
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
        *state.placeholder_msg_id,
        has_assistant_response,
        &*state.last_edit_text,
    );
    let terminal_orphan_context = turn_is_external_input_for_session
        && (state.status_panel_msg_id.is_some() || terminal_placeholder_reclaim)
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
        if terminal_orphan_context && state.status_panel_msg_id.is_some() {
            cleanup_orphan_external_input_status_panel(
                &http,
                &shared,
                channel_id,
                &mut *state.status_panel_msg_id,
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
            &mut *state.placeholder_msg_id,
            &mut *state.placeholder_from_restored_inflight,
            &mut *state.last_edit_text,
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
        *state.placeholder_msg_id,
    ) {
        if let Some(stale_msg_id) = *state.placeholder_msg_id {
            tracing::info!(
                provider = %watcher_provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = %tmux_session_name,
                stale_placeholder_msg_id = stale_msg_id.get(),
                status_panel_msg_id = state.status_panel_msg_id.map(|id| id.get()).unwrap_or(0),
                "watcher: clearing stale terminal message ids before no-inflight terminal relay"
            );
        }
        *state.placeholder_msg_id = None;
        // #3003 (codex P2 r12): only drop the local panel id if the terminal
        // reclaim above actually committed its delete. When the delete failed
        // transiently the id is held for retry (the persisted id, if any, also
        // survives for the sweeper); nulling it here would strand the still-
        // visible "계속 처리 중" panel with no handle.
        if terminal_panel_reclaim_committed {
            *state.status_panel_msg_id = None;
        }
        *state.placeholder_from_restored_inflight = false;
        state.last_edit_text.clear();
    }

    TerminalPreflightPrepared {
        prompt_anchor_present_before_relay,
        external_input_lease_before_relay,
        external_input_lease_generation_before_relay,
        inflight_before_relay,
        inflight_identity_before_relay,
        inflight_before_relay_is_stale_newer_turn,
        has_assistant_response,
        has_current_response,
        recent_stop_for_output,
        inflight_missing_before_relay,
        inflight_silent_turn,
    }
}
