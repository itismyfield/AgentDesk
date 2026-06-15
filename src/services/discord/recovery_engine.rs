use super::gateway::DiscordGateway;
use super::inflight::optional_message_id;
use super::recovery_paths::restart::dispose_recovery_relay_outcome;
use super::recovery_paths::shared::RecoveryRelayOutcome;
use super::settings::{
    load_last_remote_profile, load_last_session_path, resolve_role_binding,
    validate_bot_channel_routing_with_provider_channel,
};
use super::turn_bridge::stale_inflight_message;
use super::*;
use crate::db::turns::TurnTokenUsage;
use crate::services::agent_protocol::{RuntimeHandoff, RuntimeHandoffKind, StreamMessage};
use crate::services::git::GitCommand;
#[cfg(unix)]
use crate::services::platform::binary_resolver;
#[cfg(unix)]
use crate::services::tmux_common::tmux_exact_target;
#[cfg(unix)]
use crate::services::tmux_diagnostics::{build_tmux_death_diagnostic, tmux_session_has_live_pane};
use crate::utils::format::tail_with_ellipsis;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::Command;

#[path = "recovery_engine/status_panel.rs"]
mod recovery_status_panel;

// #3479 r8: behavior-preserving extraction of pure clusters into leaf modules.
#[path = "recovery_engine/jsonl_extract.rs"]
mod jsonl_extract;
#[path = "recovery_engine/output_path_detect.rs"]
mod output_path_detect;
#[path = "recovery_engine/phase_policy.rs"]
mod phase_policy;

// Re-import moved items so existing call sites stay byte-identical.
use self::jsonl_extract::{extract_response_from_output, success_result_end_offset_after_offset};
#[cfg(unix)]
use self::output_path_detect::{
    DetectedRebindOutputPath, StaleOutputCandidate, detect_rebind_output_path_from_candidates,
    parse_lsof_output_candidates,
};
use self::phase_policy::{
    can_fast_path_captured_full_response, recovery_has_post_work_ready_evidence,
    recovery_phase_after_output_scan, recovery_phase_after_tmux_probe,
    recovery_phase_for_existing_inflight_rebind, recovery_ready_without_output_already_delivered,
    recovery_ready_without_output_has_captured_response,
    recovery_terminal_delivery_already_committed,
};
// `extract_response_from_output_pub` is re-exported (not just re-imported) so the
// `recovery_engine::extract_response_from_output_pub` path stays valid for the
// turn_bridge / tmux_restart_handoff external callers.
pub(super) use self::jsonl_extract::extract_response_from_output_pub;

#[cfg(not(unix))]
fn tmux_session_has_live_pane(_name: &str) -> bool {
    false
}

/// #2428 H5: exponential backoff (+ jitter) for the 3-attempt recovery retry
/// loops in this module. Budget contract (Codex pass-1 review): the old fixed
/// schedule waited 1000+1000 = 2000ms total; the new gap schedule
/// `[700, 1300, 2000]`ms + 0..=100ms jitter preserves that wall-clock budget
/// (callers sleep on attempts 1 and 2: 700+1300 = 2000ms, jitter only adds)
/// while waking ~300ms earlier on average for sub-second transients. The
/// third slot is reachable only if a future change adds attempts; it caps the
/// per-gap wait. `attempt` is 1-indexed = the attempt that *just failed*;
/// call only when another attempt will actually run (`if attempt < 3`).
pub(super) fn recovery_retry_backoff(attempt: u32) -> std::time::Duration {
    // Gap schedule between attempts 1→2, 2→3, 3→4, …: 700ms, 1300ms, 2000ms.
    // The 700 + 1300 = 2000ms sum is what makes the 3-attempt total grace
    // window equal to the old fixed-1s × 3 budget. Do not adjust either of
    // the first two values without also reviewing every caller and updating
    // the budget contract above.
    const SCHEDULE_MS: [u64; 3] = [700, 1300, 2000];
    const MAX_BASE_MS: u64 = 2000;
    let idx = attempt.saturating_sub(1) as usize;
    let base_ms = SCHEDULE_MS
        .get(idx)
        .copied()
        .unwrap_or(MAX_BASE_MS)
        .min(MAX_BASE_MS);
    // Add 0..=100ms uniform jitter so simultaneous retries (e.g. two
    // channels recovering at once) do not lock-step into the same wakeup.
    use rand::Rng;
    let jitter_ms = rand::thread_rng().gen_range(0..=100);
    std::time::Duration::from_millis(base_ms + jitter_ms)
}

#[cfg(test)]
mod recovery_retry_backoff_tests {
    use super::recovery_retry_backoff;
    use std::time::Duration;

    #[test]
    fn backoff_attempt_1_is_in_700_to_800_ms() {
        let d = recovery_retry_backoff(1);
        assert!(d >= Duration::from_millis(700), "got {d:?}");
        assert!(d <= Duration::from_millis(800), "got {d:?}");
    }

    #[test]
    fn backoff_attempt_2_is_in_1300_to_1400_ms() {
        let d = recovery_retry_backoff(2);
        assert!(d >= Duration::from_millis(1300), "got {d:?}");
        assert!(d <= Duration::from_millis(1400), "got {d:?}");
    }

    #[test]
    fn backoff_attempt_3_is_in_2000_to_2100_ms() {
        let d = recovery_retry_backoff(3);
        assert!(d >= Duration::from_millis(2000), "got {d:?}");
        assert!(d <= Duration::from_millis(2100), "got {d:?}");
    }

    #[test]
    fn backoff_clamps_attempts_beyond_schedule() {
        // Even if we ever extend the loop past 3, the wait must not exceed
        // the documented cap.
        let d = recovery_retry_backoff(7);
        assert!(d <= Duration::from_millis(2100), "got {d:?}");
    }

    #[test]
    fn backoff_attempt_zero_is_treated_as_first() {
        // Defensive: a caller passing 0 should not get a divide-by-zero or
        // a tiny instant-retry; behave like attempt 1.
        let d = recovery_retry_backoff(0);
        assert!(d >= Duration::from_millis(700), "got {d:?}");
        assert!(d <= Duration::from_millis(800), "got {d:?}");
    }

    #[test]
    fn backoff_preserves_3_attempt_total_budget() {
        // Budget contract: 3-attempt loop with sleeps on attempts 1 and 2
        // must equal the old fixed-1s × 3 budget (= 2000ms wait time)
        // within the jitter envelope. This is the regression the Codex
        // pass-1 review flagged.
        let total = recovery_retry_backoff(1) + recovery_retry_backoff(2);
        // Lower bound: 700 + 1300 = 2000ms with zero jitter on both calls.
        assert!(total >= Duration::from_millis(2000), "got {total:?}");
        // Upper bound: 800 + 1400 = 2200ms with max jitter on both calls.
        assert!(total <= Duration::from_millis(2200), "got {total:?}");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryPhase {
    Pending,
    WatcherReattach,
    InflightRestore,
    Done,
}

impl RecoveryPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::WatcherReattach => "watcher_reattach",
            Self::InflightRestore => "inflight_restore",
            Self::Done => "done",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "watcher_reattach" => Some(Self::WatcherReattach),
            "inflight_restore" => Some(Self::InflightRestore),
            "done" => Some(Self::Done),
            _ => None,
        }
    }

    pub fn from_optional_str(value: Option<&str>) -> Option<Self> {
        value.and_then(Self::from_str)
    }
}

fn recovery_input_fifo_for_runtime(
    runtime_kind: RuntimeHandoffKind,
    input_fifo_path: Option<String>,
) -> Result<Option<String>, &'static str> {
    if runtime_kind.requires_input_fifo() {
        input_fifo_path
            .filter(|path| !path.is_empty())
            .map(Some)
            .ok_or("input fifo path missing during recovery")
    } else {
        Ok(input_fifo_path.filter(|path| !path.is_empty()))
    }
}

fn runtime_handoff_for_recovery(
    runtime_kind: RuntimeHandoffKind,
    output_path: String,
    input_fifo_path: Option<String>,
    tmux_session_name: String,
    session_id: Option<String>,
    last_offset: u64,
) -> RuntimeHandoff {
    match runtime_kind {
        RuntimeHandoffKind::LegacyTmuxWrapper => RuntimeHandoff::LegacyTmuxWrapper {
            output_path,
            input_fifo_path: input_fifo_path.unwrap_or_default(),
            tmux_session_name,
            last_offset,
        },
        RuntimeHandoffKind::ClaudeTui => RuntimeHandoff::ClaudeTui {
            transcript_path: output_path,
            tmux_session_name,
            last_offset,
        },
        RuntimeHandoffKind::CodexTui => RuntimeHandoff::CodexTui {
            rollout_path: output_path,
            thread_id: session_id,
            tmux_session_name,
            last_offset,
        },
        RuntimeHandoffKind::ProcessBackend => RuntimeHandoff::ProcessBackend {
            output_path,
            session_name: tmux_session_name,
            last_offset,
        },
        RuntimeHandoffKind::ClaudeEAdapter => RuntimeHandoff::ClaudeEAdapter {
            output_path,
            session_name: tmux_session_name,
            last_offset,
        },
    }
}

fn emit_recovery_quality_event(
    provider: &ProviderKind,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    reason: &str,
) {
    crate::services::observability::emit_agent_quality_event(
        crate::services::observability::AgentQualityEvent {
            source_event_id: session_key
                .map(str::to_string)
                .or_else(|| dispatch_id.map(str::to_string)),
            correlation_id: dispatch_id
                .map(str::to_string)
                .or_else(|| session_key.map(str::to_string)),
            agent_id: None,
            provider: Some(provider.as_str().to_string()),
            channel_id: Some(channel_id.to_string()),
            card_id: None,
            dispatch_id: dispatch_id.map(str::to_string),
            event_type: "recovery_fired".to_string(),
            payload: serde_json::json!({
                "reason": reason,
                "session_key": session_key,
            }),
        },
    );
}

fn should_advance_recovery_dispatch_after_relay(relay_ok: bool) -> bool {
    relay_ok
}

fn forget_completion_footer_for_recovery_takeover(channel_id: ChannelId) {
    super::single_message_panel::completion_footer_forget_registered_target(channel_id);
}

async fn relay_recovery_terminal_notice(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    state: &super::inflight::InflightTurnState,
    text: &str,
) -> RecoveryRelayOutcome {
    relay_recovered_terminal_text_to_placeholder(
        http,
        shared,
        ChannelId::new(state.channel_id),
        super::inflight::optional_message_id(state.current_msg_id),
        text,
    )
    .await
}

/// Deliver the recovered terminal text to Discord: edit the placeholder in
/// place when one was anchored, else (`placeholder == None`, e.g. TUI-direct —
/// `MessageId::new(0)` would panic) send a NEW message. Only `Delivered` lets
/// callers advance recovery (Codex P1); #3293 classifies failures so a
/// permanent Discord rejection (404/403/410) can end the retry loop. #3297
/// finding 2: the anchored path flattens errors into Strings, so a transient
/// classification gets a second opinion from an active channel probe.
async fn relay_recovered_terminal_text_to_placeholder(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    placeholder: Option<MessageId>,
    text: &str,
) -> RecoveryRelayOutcome {
    forget_completion_footer_for_recovery_takeover(channel_id);
    let delivery = match placeholder {
        Some(placeholder) => {
            use super::recovery_paths::controller_cutover as cc;
            // #3089 A6a: anchored short-replace via the unified controller behind a flag
            // (default OFF); the adapter maps the verdict to `RecoveryRelayOutcome` AND
            // re-runs the #3297 probe, returning the legacy path's equal. OFF / None / empty
            // → verbatim legacy. Provider is cosmetic on the markerless `NoLease` path.
            if cc::recovery_short_replace_should_cutover(
                cc::recovery_relay_controller_enabled(),
                true,
                text,
            ) {
                let gateway =
                    DiscordGateway::new(http.clone(), shared.clone(), ProviderKind::Claude, None);
                return cc::deliver_recovery_replace_via_controller(
                    &gateway,
                    shared,
                    &ProviderKind::Claude,
                    http,
                    channel_id,
                    placeholder,
                    text,
                )
                .await;
            }
            super::formatting::replace_long_message_raw(http, channel_id, placeholder, text, shared)
                .await
        }
        None => super::formatting::send_long_message_raw(http, channel_id, text, shared).await,
    };
    match delivery {
        Ok(()) => RecoveryRelayOutcome::Delivered,
        Err(error) => {
            let classified =
                super::recovery_paths::shared::classify_recovery_relay_error(error.as_ref());
            super::recovery_paths::shared::escalate_transient_relay_outcome_with_probe(
                classified,
                || super::recovery_paths::restart::probe_channel_liveness(http, channel_id),
            )
            .await
        }
    }
}

/// Outcome of `complete_recovery_visible_turn` exposed to callers so they can
/// tell whether the visible completion UI was emitted. A TUI quiescence timeout
/// may suppress that UI, but once recovery has terminal delivery evidence it
/// must still release mailbox/inflight ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecoveryCompletionOutcome {
    /// Visible completion emitted (or status-panel-v2 was disabled / no
    /// status message id was wired). Callers may proceed with downstream
    /// dispatch / analytics / mailbox finalization as before.
    Emitted,
    /// Terminal response delivery is authoritative, but the visible completion
    /// status/reaction was suppressed because the TUI quiescence probe timed
    /// out. Callers still proceed with cleanup.
    VisibleCompletionSuppressed,
}

impl RecoveryCompletionOutcome {
    /// `true` when callers should proceed with downstream side effects.
    /// Visible completion suppression is not a mailbox correctness primitive.
    pub(super) fn should_proceed(self) -> bool {
        matches!(
            self,
            RecoveryCompletionOutcome::Emitted
                | RecoveryCompletionOutcome::VisibleCompletionSuppressed
        )
    }
}

/// #3099: a TUI-injected external-input (task-notification) turn can complete
/// via recovery with `user_msg_id == 0`. The `⏳ → ✅` reaction step is then
/// skipped (there is no anchored Discord user message), but the hourglass was
/// added to a real notify-bot message tracked by the prompt anchor. Such a turn
/// needs the anchor-lifecycle cleanup so the `⏳` is removed from the injected
/// message instead of going stale.
fn recovery_inflight_needs_anchor_lifecycle_cleanup(
    state: &super::inflight::InflightTurnState,
) -> bool {
    state.user_msg_id == 0
        && state.tmux_session_name.is_some()
        && matches!(
            state.turn_source,
            super::inflight::TurnSource::ExternalInput
                | super::inflight::TurnSource::ExternalAdopted
        )
}

async fn complete_recovery_visible_turn(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &super::inflight::InflightTurnState,
    background: bool,
    source: &'static str,
) -> RecoveryCompletionOutcome {
    let channel_id = ChannelId::new(state.channel_id);
    // A recovery/orphan turn may carry no user message (user_msg_id == 0,
    // e.g. a TUI-direct turn). There is then no user message to react against,
    // so the ⏳→✅ reaction step is skipped while the quiescence gate and
    // status-panel completion still run. `MessageId::new(0)` would panic.
    let user_msg_id = super::inflight::optional_message_id(state.user_msg_id);

    // #2161 (Codex round-2 M1): recovery completes a turn based on JSONL
    // `result` + output-file drain, not tmux pane readiness. For ClaudeTui
    // sessions the same premature-completion bug applies — gate the
    // user-visible `응답 완료` emit on quiescence, and on timeout skip
    // the emit so the next watcher pass / placeholder sweeper reconciles.
    // The gate lives in the `tmux` module (`#[cfg(unix)]`); on non-unix
    // targets we skip it and emit completion as normal.
    //
    // #2293 H3 — the gate is hoisted ABOVE the ⏳ → ✅ reaction so a
    // TimedOut outcome ALSO suppresses the reaction (was: reaction ran
    // before the gate, lying about completion to the user). Recovery's
    // visible side effects now follow the same ordering as the bridge and
    // watcher paths.
    #[cfg(unix)]
    if let Some(tmux_session_name) = state.tmux_session_name.as_deref() {
        let outcome = super::tmux::run_tui_completion_gate(
            provider,
            channel_id,
            tmux_session_name,
            state.task_notification_kind,
        )
        .await;
        if !outcome.should_emit_completion() {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                source = source,
                "[{ts}] ⚠ #2935 recovery visible completion suppressed — TUI quiescence gate timed out; continuing dispatch / analytics / mailbox cleanup because recovery already has terminal response delivery evidence"
            );
            return RecoveryCompletionOutcome::VisibleCompletionSuppressed;
        }
    }

    if let Some(user_msg_id) = user_msg_id {
        super::formatting::remove_reaction_raw(http, channel_id, user_msg_id, '⏳').await;
        super::formatting::add_reaction_raw(http, channel_id, user_msg_id, '✅').await;
    } else if recovery_inflight_needs_anchor_lifecycle_cleanup(state) {
        // #3099: a TUI-injected task-notification turn can complete via recovery
        // with `user_msg_id == 0` (no anchored Discord user message), so the
        // `⏳ → ✅` step above is skipped. The hourglass, however, was added to a
        // real notify-bot message, so clean it off that exact injected message
        // instead of leaving it stale next to the `✅` the completion path applies
        // elsewhere.
        //
        // #3099 codex re-review (P2): target THIS turn's pinned
        // `injected_prompt_message_id` instead of re-reading the single shared
        // prompt-anchor slot, which a later injection may already own.
        if let Some(tmux_session_name) = state.tmux_session_name.as_deref() {
            let _ = super::tui_prompt_relay::complete_tui_direct_anchor_lifecycle_for_inflight(
                http,
                provider.as_str(),
                tmux_session_name,
                channel_id,
                state.injected_prompt_message_id,
                "recovery_task_notification_anchor_cleanup_user_msg_zero",
            )
            .await;
        }
    }

    if !shared.ui.status_panel_v2_enabled {
        return RecoveryCompletionOutcome::Emitted;
    }
    // #2427 D wire: explicit completion signal — most recovery paths
    // already call `clear_inflight_state` unconditionally; this is a
    // safety net for any branch that emits TurnCompleted without doing
    // so. user_msg_id guard defeats Pitfall #1 (next turn race).
    #[cfg(unix)]
    super::tmux::emit_explicit_inflight_cleanup_signal(
        provider,
        channel_id,
        state.user_msg_id,
        "turn_completed_recovery",
    );
    let started_at_unix = super::inflight::parse_started_at_unix(&state.started_at)
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    let Some(status_msg_id) = recovery_status_panel::completion_target(
        shared.ui.status_panel_v2_enabled,
        state,
        provider,
        channel_id,
    ) else {
        return RecoveryCompletionOutcome::Emitted;
    };

    // EPIC #3078 PR-2 — route recovery completion through the
    // `StatusPanelController` behind a parity check (shadow mode). The
    // controller adopts the recovered panel id and reports the id it WOULD
    // finalize; it must equal the legacy `status_msg_id`. The legacy
    // `complete_status_panel_v2_with_http` below still executes the actual
    // Discord edit/delete, so behaviour is verifiably unchanged — only the
    // (shadow) controller decision is observed. The real cutover (controller
    // executes the IO) lands in a later PR. The controller actor is only
    // spawned when v2 is enabled (we are already inside the v2-enabled branch),
    // so this is inert and untouched when v2 is off.
    let controller_id = shared
        .status_panel_controller
        .recovery_completion_parity_id(
            super::turn_finalizer::TurnKey::new(
                channel_id,
                state.user_msg_id,
                super::runtime_store::load_generation(),
            ),
            provider.clone(),
            status_msg_id,
        )
        .await;
    assert_recovery_completion_parity(controller_id, status_msg_id, channel_id, source);

    let mut last_status_panel_text = String::new();
    let _committed = super::turn_bridge::complete_status_panel_v2_with_http(
        shared,
        http,
        channel_id,
        status_msg_id,
        provider,
        started_at_unix,
        &mut last_status_panel_text,
        background,
        source,
        Some(state.user_msg_id),
    )
    .await;
    RecoveryCompletionOutcome::Emitted
}

/// EPIC #3078 PR-2 — the parity gate between the legacy recovery
/// status-panel-completion id and the id the `StatusPanelController` chooses for
/// the same turn. They must agree: a divergence means the controller would
/// finalize a different (or no) panel, so routing the IO through it later would
/// change behaviour. `debug_assert` so test/dev builds fail loudly; release
/// builds emit a bounded `warn!` (no `panic!`) so a never-before-seen recovery
/// shape can never crash a production restart sweep over the legacy path, which
/// continues to execute regardless.
fn assert_recovery_completion_parity(
    controller_id: Option<MessageId>,
    legacy_id: Option<MessageId>,
    channel_id: ChannelId,
    source: &'static str,
) {
    if controller_id == legacy_id {
        return;
    }
    debug_assert_eq!(
        controller_id, legacy_id,
        "#3078 PR-2 recovery status-panel completion parity mismatch (channel {channel_id}, source {source}): controller chose {controller_id:?}, legacy chose {legacy_id:?}"
    );
    tracing::warn!(
        channel = channel_id.get(),
        source = source,
        controller_id = ?controller_id,
        legacy_id = ?legacy_id,
        "#3078 PR-2 recovery status-panel completion parity mismatch — StatusPanelController chose a different completion id than the legacy path; legacy path executed (no behaviour change), divergence logged for the later controller-executes cutover"
    );
}

#[cfg(test)]
mod recovery_dispatch_gate_tests {
    #[test]
    fn recovery_dispatch_advance_requires_successful_relay() {
        assert!(super::should_advance_recovery_dispatch_after_relay(true));
        assert!(!super::should_advance_recovery_dispatch_after_relay(false));
    }
}

#[cfg(test)]
mod recovery_completion_outcome_tests {
    use super::{
        RecoveryCompletionOutcome, assert_recovery_completion_parity, recovery_status_panel,
    };
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};

    fn state_for_recovery(user_msg_id: u64) -> super::inflight::InflightTurnState {
        super::inflight::InflightTurnState::new(
            ProviderKind::Claude,
            4243,
            Some("adk-cc".to_string()),
            7,
            user_msg_id,
            user_msg_id + 1,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            None,
            0,
        )
    }

    #[test]
    fn emitted_lets_callers_proceed_with_dispatch_finalize() {
        assert!(RecoveryCompletionOutcome::Emitted.should_proceed());
    }

    #[test]
    fn visible_completion_suppression_still_allows_cleanup() {
        assert!(
            RecoveryCompletionOutcome::VisibleCompletionSuppressed.should_proceed(),
            "#2935: quiescence timeout may hide 응답 완료, but must not preserve stale active ownership"
        );
    }

    #[test]
    fn completion_prefers_guarded_persisted_fallback_message_id() {
        let mut snapshot = state_for_recovery(9101);
        snapshot.status_message_id = Some(3003);
        let mut persisted = state_for_recovery(9101);
        persisted.status_message_id = Some(4004);

        let status_msg_id =
            recovery_status_panel::message_id_for_completion(&snapshot, Some(&persisted));

        assert_eq!(status_msg_id, Some(super::MessageId::new(4004)));
    }

    #[test]
    fn completion_ignores_persisted_id_from_different_turn() {
        let mut snapshot = state_for_recovery(9101);
        snapshot.status_message_id = Some(3003);
        let mut persisted = state_for_recovery(9201);
        persisted.status_message_id = Some(4004);

        let status_msg_id =
            recovery_status_panel::message_id_for_completion(&snapshot, Some(&persisted));

        assert_eq!(status_msg_id, Some(super::MessageId::new(3003)));
    }

    #[test]
    fn footer_mode_skips_recovery_status_panel_completion_for_stale_persisted_id() {
        let mut snapshot = state_for_recovery(9101);
        snapshot.status_message_id = Some(3003);
        let mut persisted = state_for_recovery(9101);
        persisted.status_message_id = Some(4004);

        let target = recovery_status_panel::completion_target_for_flags(
            true,
            true,
            &snapshot,
            Some(&persisted),
        );

        assert_eq!(
            target, None,
            "footer mode must not edit or SendFallback a separate recovery panel; the stale id is left for sweeper reclaim"
        );
    }

    #[test]
    fn flag_off_recovery_status_panel_completion_keeps_original_target() {
        let mut snapshot = state_for_recovery(9101);
        snapshot.status_message_id = Some(3003);
        let mut persisted = state_for_recovery(9101);
        persisted.status_message_id = Some(4004);

        let target = recovery_status_panel::completion_target_for_flags(
            false,
            true,
            &snapshot,
            Some(&persisted),
        );

        assert_eq!(target, Some(Some(super::MessageId::new(4004))));
    }

    #[test]
    fn flag_off_recovery_none_target_still_requests_send_fallback() {
        let snapshot = state_for_recovery(9101);

        let target =
            recovery_status_panel::completion_target_for_flags(false, true, &snapshot, None);

        assert_eq!(
            target,
            Some(None),
            "flag-off v2 recovery preserves the SendFallback rollback behavior when no status_message_id was persisted"
        );
    }

    #[test]
    fn recovery_takeover_forgets_registered_completion_footer_target() {
        let channel_id = ChannelId::new(3_089_201);
        let shared = super::super::make_shared_data_for_tests();
        super::super::single_message_panel::completion_footer_forget_registered_target(channel_id);
        let _ = super::super::single_message_panel::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_301),
            &ProviderKind::Claude,
            1_800_000_000,
            "Final answer",
            None,
            true,
        );

        super::forget_completion_footer_for_recovery_takeover(channel_id);

        assert_eq!(
            super::super::single_message_panel::completion_footer_edit_for_registered_target_at(
                shared.as_ref(),
                channel_id,
                "⠸",
                1_800_000_005,
            ),
            None
        );
    }

    // EPIC #3078 PR-2: for representative recovery-completion inputs, the
    // `StatusPanelController`'s chosen completion id (adopt + read-back through
    // the live actor) equals the legacy
    // `recovery_status_panel_message_id_for_completion` result, so the parity
    // gate passes and `complete_recovery_visible_turn` keeps executing the
    // legacy path with no behaviour change.
    #[tokio::test(flavor = "current_thread")]
    async fn controller_chosen_completion_id_matches_legacy_for_representative_inputs() {
        use super::ChannelId;
        use super::status_panel_controller::StatusPanelController;
        use super::turn_finalizer::TurnKey;

        // Case A: real user_msg_id, persisted id from the SAME turn wins.
        let mut snapshot = state_for_recovery(9101);
        snapshot.status_message_id = Some(3003);
        let mut persisted = state_for_recovery(9101);
        persisted.status_message_id = Some(4004);
        let legacy = recovery_status_panel::message_id_for_completion(&snapshot, Some(&persisted));
        assert_eq!(legacy, Some(super::MessageId::new(4004)));

        let ctl = StatusPanelController::spawn(true);
        let key = TurnKey::new(ChannelId::new(4243), snapshot.user_msg_id, 0);
        let controller = ctl
            .recovery_completion_parity_id(key, ProviderKind::Claude, legacy)
            .await;
        assert_eq!(
            controller, legacy,
            "controller's chosen completion id must equal the legacy id (real user_msg_id case)"
        );

        // Case B: channel-only (user_msg_id == 0) recovery turn — the controller
        // collapses onto the single live entry it adopted, choosing the same id.
        let mut snapshot0 = state_for_recovery(0);
        snapshot0.status_message_id = Some(5005);
        let legacy0 = recovery_status_panel::message_id_for_completion(&snapshot0, None);
        assert_eq!(legacy0, Some(super::MessageId::new(5005)));

        let ctl0 = StatusPanelController::spawn(true);
        let key0 = TurnKey::new(ChannelId::new(7777), 0, 0);
        let controller0 = ctl0
            .recovery_completion_parity_id(key0, ProviderKind::Claude, legacy0)
            .await;
        assert_eq!(
            controller0, legacy0,
            "controller's chosen completion id must equal the legacy id (channel-only case)"
        );

        // Case C: no panel id at all (None) — both agree on None.
        let snapshot_none = state_for_recovery(9300);
        let legacy_none = recovery_status_panel::message_id_for_completion(&snapshot_none, None);
        assert_eq!(legacy_none, None);

        let ctl_none = StatusPanelController::spawn(true);
        let key_none = TurnKey::new(ChannelId::new(8888), 9300, 0);
        let controller_none = ctl_none
            .recovery_completion_parity_id(key_none, ProviderKind::Claude, legacy_none)
            .await;
        assert_eq!(controller_none, legacy_none);

        // The parity assert itself must not fire for any of these.
        assert_recovery_completion_parity(controller, legacy, ChannelId::new(4243), "test");
        assert_recovery_completion_parity(controller0, legacy0, ChannelId::new(7777), "test");
        assert_recovery_completion_parity(
            controller_none,
            legacy_none,
            ChannelId::new(8888),
            "test",
        );
    }
}

#[cfg(test)]
mod no_anchored_placeholder_recovery_tests {
    //! Regression coverage for the msgid-0 panic class: a TUI-direct / recovery
    //! inflight legitimately carries `current_msg_id == 0` (never anchored a
    //! Discord placeholder, `status_message_id = None`) and may carry
    //! `user_msg_id == 0` (no Discord user message). The startup recovery /
    //! reattach loop derives placeholder / user message ids from these fields;
    //! before the fix it called `serenity::MessageId::new(0)`, which panics
    //! ("Attempted to call MessageId::new with invalid (0) value"). Because that
    //! loop awaits inline, one such inflight aborted recovery before
    //! `reconcile_done`, leaving the provider permanently degraded. These tests
    //! assert the conversion helpers and the watcher-restore seed never panic
    //! and yield a sane "no placeholder, recover watcher/session" result.
    use super::inflight::{InflightTurnState, optional_message_id};
    use super::optional_message_id as engine_optional_message_id;
    use crate::services::provider::ProviderKind;

    fn tui_direct_no_anchor_state() -> InflightTurnState {
        // TUI-direct turn: never anchored a Discord placeholder
        // (current_msg_id == 0, status_message_id == None) and has no Discord
        // user message (user_msg_id == 0), but DOES own a live tmux session.
        InflightTurnState::new(
            ProviderKind::Claude,
            4243,
            Some("adk-cc".to_string()),
            7,
            0, // user_msg_id == 0
            0, // current_msg_id == 0
            "tui direct prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            None,
            0,
        )
    }

    // #3099: a TUI-injected task-notification turn recovered with
    // `user_msg_id == 0` skips the `⏳ → ✅` reaction step, so it must route to
    // the anchor-lifecycle cleanup that removes the hourglass from the injected
    // notify-bot message.
    #[test]
    fn recovery_external_input_zero_user_msg_needs_anchor_cleanup() {
        let mut state = tui_direct_no_anchor_state();
        state.turn_source = super::inflight::TurnSource::ExternalInput;
        assert!(super::recovery_inflight_needs_anchor_lifecycle_cleanup(
            &state
        ));

        let mut adopted = tui_direct_no_anchor_state();
        adopted.turn_source = super::inflight::TurnSource::ExternalAdopted;
        assert!(super::recovery_inflight_needs_anchor_lifecycle_cleanup(
            &adopted
        ));
    }

    // A managed turn, or an external turn with a real anchored user message id,
    // must NOT use the injected-anchor cleanup path.
    #[test]
    fn recovery_managed_or_anchored_turn_skips_anchor_cleanup() {
        // Default turn_source (Managed) with user_msg_id == 0 is not external.
        let managed = tui_direct_no_anchor_state();
        assert!(!super::recovery_inflight_needs_anchor_lifecycle_cleanup(
            &managed
        ));

        // External-input turn WITH a real anchored user message uses the normal
        // ⏳ → ✅ block, not the anchor cleanup.
        let mut anchored = tui_direct_no_anchor_state();
        anchored.turn_source = super::inflight::TurnSource::ExternalInput;
        anchored.user_msg_id = 777;
        assert!(!super::recovery_inflight_needs_anchor_lifecycle_cleanup(
            &anchored
        ));
    }

    #[test]
    fn optional_message_id_maps_zero_to_none_without_panicking() {
        assert_eq!(optional_message_id(0), None);
        assert_eq!(
            optional_message_id(123),
            Some(super::MessageId::new(123)),
            "non-zero ids must still build a real MessageId"
        );
        // Reachable via the engine re-export used across the recovery loop.
        assert_eq!(engine_optional_message_id(0), None);
    }

    #[test]
    fn recovery_loop_bindings_from_zero_state_are_none_not_panic() {
        // This mirrors the eager bindings at the top of the recovery loop body
        // (`restore_inflight_turns`) that previously panicked on a 0-valued
        // inflight before `reconcile_done` could be reached.
        let state = tui_direct_no_anchor_state();
        assert_eq!(state.current_msg_id, 0);
        assert_eq!(state.user_msg_id, 0);
        assert_eq!(state.status_message_id, None);

        let current_msg_id = optional_message_id(state.current_msg_id);
        let user_msg_id = optional_message_id(state.user_msg_id);
        assert_eq!(
            current_msg_id, None,
            "no anchored placeholder → relay step is skipped, not panicked"
        );
        assert_eq!(
            user_msg_id, None,
            "no Discord user message → reaction/analytics steps are skipped"
        );
    }

    // `super::super::tmux` is `#[cfg(unix)]`-only (the tmux relay is Unix-only),
    // so gate this test to Unix to keep the Windows cross-OS build green.
    #[cfg(unix)]
    #[test]
    fn watcher_restore_seed_skips_placeholder_but_preserves_session() {
        // The pane-alive reattach branch seeds the watcher from the inflight.
        // For a no-anchor turn there is no placeholder to restore, but the tmux
        // session/watcher must still be recovered — `restored_watcher_turn_from_inflight`
        // returns None (no placeholder) WITHOUT panicking, and the watcher is
        // spawned regardless by the caller.
        let state = tui_direct_no_anchor_state();
        let restored = super::super::tmux::restored_watcher_turn_from_inflight(
            &state,
            "AgentDesk-claude-adk-cc",
            true,
        );
        assert!(
            restored.is_none(),
            "current_msg_id == 0 yields no restored placeholder (no MessageId::new(0) panic)"
        );
    }

    #[test]
    fn recovered_transcript_turn_id_avoids_bogus_zero_key() {
        // user_msg_id == 0 must NOT key on discord:<channel>:0 (would collide /
        // overwrite across every no-user-message turn in the channel), and must
        // include a per-turn discriminator so repeated message-less turns in the
        // same session do not upsert-overwrite each other (Codex P2 r3).
        assert_eq!(
            super::recovered_transcript_turn_id(4243, 0, Some("session-abc"), Some(4096), "x"),
            "discord:4243:session:session-abc:off4096"
        );
        // No offset → fall back to the started_at timestamp discriminator.
        assert_eq!(
            super::recovered_transcript_turn_id(4243, 0, None, None, "2026-06-01 12:00:00"),
            "discord:4243:recovery:at2026-06-01-12-00-00"
        );
        // Two message-less turns in the same session must NOT collide.
        let a = super::recovered_transcript_turn_id(4243, 0, Some("s"), Some(100), "t");
        let b = super::recovered_transcript_turn_id(4243, 0, Some("s"), Some(200), "t");
        assert_ne!(a, b);
        // A real user message keeps the legacy message-keyed transcript id.
        assert_eq!(
            super::recovered_transcript_turn_id(4243, 99, Some("session-abc"), Some(4096), "x"),
            "discord:4243:99"
        );
    }
}

#[cfg(test)]
mod delivered_inflight_reregister_tests {
    use super::{inflight, recovery_terminal_delivery_already_committed};
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::provider::ProviderKind;

    #[test]
    fn committed_terminal_delivery_is_not_recoverable_active_turn() {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            4243,
            Some("adk-cc".to_string()),
            7,
            9101,
            9102,
            "summarize recent PRs".to_string(),
            Some("session-2".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            None,
            128,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        state.full_response = "already posted response".to_string();
        state.response_sent_offset = state.full_response.len();
        state.terminal_delivery_committed = true;

        assert!(recovery_terminal_delivery_already_committed(&state));
    }

    #[test]
    fn ordinary_inflight_still_recoverable_even_with_relayed_prefix() {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            4244,
            Some("adk-cc".to_string()),
            7,
            9201,
            9202,
            "continue streaming".to_string(),
            Some("session-3".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            None,
            256,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        state.full_response = "partial response".to_string();
        state.response_sent_offset = state.full_response.len();

        assert!(!recovery_terminal_delivery_already_committed(&state));
    }
}

/// Retry-aware tmux session check for recovery after dcserver restart.
/// The first check can false-negative if tmux CLI hasn't fully initialized yet.
fn tmux_session_alive_with_retry(name: &str) -> bool {
    if tmux_session_has_live_pane(name) {
        return true;
    }
    // #2428 H5: retry up to 2 more times with exponential backoff + jitter
    // (was a fixed 1s gap; see `recovery_retry_backoff`).
    for attempt in 1..=2u32 {
        std::thread::sleep(recovery_retry_backoff(attempt));
        if tmux_session_has_live_pane(name) {
            tracing::info!(
                "  [recovery] tmux pane alive on retry {} for {}",
                attempt,
                name
            );
            return true;
        }
    }
    false
}

/// Retry-aware tmux has_session check.
fn tmux_has_session_with_retry(name: &str) -> bool {
    if crate::services::platform::tmux::has_session(name) {
        return true;
    }
    // #2428 H5: see `recovery_retry_backoff`.
    for attempt in 1..=2u32 {
        std::thread::sleep(recovery_retry_backoff(attempt));
        if crate::services::platform::tmux::has_session(name) {
            tracing::info!(
                "  [recovery] tmux session found on retry {} for {}",
                attempt,
                name
            );
            return true;
        }
    }
    false
}

#[cfg(not(unix))]
fn build_tmux_death_diagnostic(_name: &str, _output_path: Option<&str>) -> Option<String> {
    None
}

fn interrupted_recovery_message(
    state: &inflight::InflightTurnState,
    saved_response: &str,
) -> String {
    state
        .restart_mode
        .map(|mode| super::turn_bridge::handoff_interrupted_message(mode, saved_response))
        .unwrap_or_else(|| stale_inflight_message(saved_response))
}

/// WARN-only trace (160-char response tail) — writes NOTHING to disk. The
/// durable full-response artifact for force-clears is
/// `recovery_paths::restart::persist_force_clear_report` (#3297 finding 3).
pub(super) fn save_missing_session_handoff(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    best_response: &str,
) {
    let partial = best_response.trim();
    let partial_summary = if partial.is_empty() {
        "partial response unavailable".to_string()
    } else {
        tail_with_ellipsis(partial, 160)
    };
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⚠ recovery: suppressed auto post-restart handoff for channel {} (provider={}, user_msg_id={}, partial={})",
        state.channel_id,
        provider.as_str(),
        state.user_msg_id,
        partial_summary
    );
}

fn inflight_ready_for_input_without_tui_pane(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    require_consumed: bool,
) -> Option<crate::services::tui_turn_state::TuiReadyState> {
    let output_path = state
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())?;
    crate::services::tui_turn_state::jsonl_ready_for_input(
        provider,
        state.runtime_kind,
        std::path::Path::new(output_path),
        require_consumed.then_some(state.last_offset),
    )
}

fn inflight_or_legacy_tmux_ready_for_input(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    tmux_session_name: &str,
    require_consumed: bool,
) -> bool {
    inflight_ready_for_input_without_tui_pane(provider, state, require_consumed)
        .map(crate::services::tui_turn_state::TuiReadyState::is_ready)
        .unwrap_or_else(|| {
            crate::services::provider::tmux_session_ready_for_input(tmux_session_name, provider)
        })
}

fn recovery_worktree_path(state: &inflight::InflightTurnState) -> Option<&str> {
    state
        .worktree_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
}

fn recovery_worktree_branch(state: &inflight::InflightTurnState) -> Option<&str> {
    state
        .worktree_branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
}

fn recovery_dispatch_id(state: &inflight::InflightTurnState) -> Option<&str> {
    state
        .dispatch_id
        .as_deref()
        .map(str::trim)
        .filter(|dispatch_id| !dispatch_id.is_empty())
}

fn recovery_tmux_session_name(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
) -> Option<String> {
    state
        .tmux_session_name
        .as_deref()
        .or_else(|| state.channel_name.as_deref())
        .map(|name| {
            if name.starts_with(&format!(
                "{}-",
                crate::services::provider::TMUX_SESSION_PREFIX
            )) {
                name.to_string()
            } else {
                provider.build_tmux_session_name(name)
            }
        })
}

fn recovery_requires_worktree_context(state: &inflight::InflightTurnState) -> bool {
    recovery_worktree_branch(state).is_some()
        || state
            .base_commit
            .as_deref()
            .is_some_and(|commit| !commit.trim().is_empty())
}

fn recovery_git_stdout(repo_path: &str, args: &[&str]) -> Option<String> {
    let output = GitCommand::new()
        .repo(repo_path)
        .args(args)
        .run_output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return None;
    }
    Some(stdout)
}

fn recovery_worktree_original_path(worktree_path: &str) -> Option<String> {
    let git_common_dir = recovery_git_stdout(worktree_path, &["rev-parse", "--git-common-dir"])?;
    let common_dir = {
        let candidate = std::path::PathBuf::from(&git_common_dir);
        if candidate.is_absolute() {
            candidate
        } else {
            std::path::Path::new(worktree_path).join(candidate)
        }
    };
    let canonical = std::fs::canonicalize(common_dir).ok()?;
    canonical.parent()?.to_str().map(str::to_string)
}

fn recovery_worktree_info(state: &inflight::InflightTurnState) -> Option<WorktreeInfo> {
    let worktree_path = recovery_worktree_path(state)?;
    if !std::path::Path::new(worktree_path).is_dir() {
        return None;
    }

    let branch_name = recovery_worktree_branch(state)
        .map(str::to_string)
        .or_else(|| recovery_git_stdout(worktree_path, &["branch", "--show-current"]))?;
    let original_path = recovery_worktree_original_path(worktree_path)?;

    Some(WorktreeInfo {
        original_path,
        worktree_path: worktree_path.to_string(),
        branch_name,
    })
}

fn restore_recovered_session_worktree(
    session: &mut DiscordSession,
    state: &inflight::InflightTurnState,
) {
    if let Some(worktree) = recovery_worktree_info(state) {
        if session.current_path.is_none() {
            session.current_path = Some(worktree.worktree_path.clone());
        }
        session.worktree = Some(worktree);
    }
}

fn recovery_spawn_adk_cwd(
    state: &inflight::InflightTurnState,
    persisted_session_path: Option<String>,
) -> Result<Option<String>, String> {
    if let Some(worktree_path) = recovery_worktree_path(state) {
        if std::path::Path::new(worktree_path).is_dir() {
            return Ok(Some(worktree_path.to_string()));
        }
        return Err(format!(
            "recovery blocked: inflight worktree missing for channel {}: {}",
            state.channel_id, worktree_path
        ));
    }

    if recovery_requires_worktree_context(state) {
        let dispatch_suffix = recovery_dispatch_id(state)
            .map(|dispatch_id| format!(" (dispatch {dispatch_id})"))
            .unwrap_or_default();
        return Err(format!(
            "recovery blocked: inflight worktree state missing for channel {}{}",
            state.channel_id, dispatch_suffix
        ));
    }

    Ok(persisted_session_path)
}

pub(super) async fn finish_recovered_turn_mailbox(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    stop_source: &'static str,
) {
    // #3016 phase 4: route the recovery terminal through the single-authority
    // finalizer. The recovered turn is channel-scoped here (the caller did not
    // thread its real `user_msg_id`), so we submit `user_msg_id == 0` — the
    // finalizer resolves it to the channel's single live entry (or finalizes
    // the orphan directly) and runs the SAME channel-scoped `mailbox_finish_turn`
    // + gated counter decrement + watchdog-override clear + dispatch_thread_parents
    // retain + role-override cleanup + queue kickoff this code did inline. The
    // ledger phase gate keeps a racing watcher/bridge terminal exactly-once safe.
    // `FinalizeContext::monitor` reproduces the inline side-effect set (no
    // inflight clear, no completion-cleanup, no voice drain, kick off backlog).
    //
    // Recovery is single-turn-per-channel (the channel is being recovered, not
    // running a fresh turn), so id-0 here is safe: the finalizer's id-0 guard
    // makes an AMBIGUOUS submission (a recently-Finalized entry AND a different
    // live turn) a NO-OP — it never releases a newer turn's token — and the
    // unambiguous case (the recovered turn is the single live entry) finalizes
    // it exactly as the inline code did. This reproduces the prior
    // channel-scoped `mailbox_finish_turn` semantics, now ledger-gated.
    let _ = shared
        .turn_finalizer
        .submit_terminal(
            super::turn_finalizer::TurnKey::new(channel_id, 0, shared.restart.current_generation),
            provider.clone(),
            super::turn_finalizer::TerminalEvent::Complete,
            super::turn_finalizer::FinalizeContext::monitor(),
            shared.clone(),
        )
        .await;
    let _ = stop_source;
}

/// #3248 gap-1 — re-seed the single-authority finalizer ledger for a
/// watcher-owned turn re-attached by recovery after a mid-turn dcserver restart
/// (e.g. `SKIP_TURN_DRAIN=1` deploy). The in-memory ledger is empty post-restart,
/// so without this the watcher's channel-only (id-0) GateTimeout creates a
/// `RelayOwnerKind::None` entry that finalizes-as-orphan (the 8s gate-backstop
/// arms only for `relay_owner != None`; the far-backstop reconcile collects only
/// `relay_owner == Watcher` rows) — the live pane never auto-reconciles until a
/// NEW user turn. Registering with the Watcher owner reproduces the bridge
/// handoff (`turn_bridge/mod.rs` `register_start(.., Watcher, ..)`) so the
/// gate-timeout DEFERS (arms the backstop) and the reconcile can collect the row.
///
/// IDEMPOTENT: the actor's `Start` handler is `entry().and_modify().or_insert()`
/// — never resurrects a `Finalized` turn, `get_or_insert`s the deadline (never
/// pushes it forward) — so a later bridge handoff `register_start` of the SAME
/// `TurnKey` is a no-op refresh (the normal, non-restart path is unaffected).
fn reseed_watcher_owned_finalizer_ledger(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    provider: &ProviderKind,
) {
    // id-0 would key the channel-only orphan slot; the sole caller already
    // returns early on id-0, but guard here so this path only ever seeds a
    // full-identity Watcher entry.
    if user_msg_id.get() == 0 {
        return;
    }
    shared.turn_finalizer.register_start(
        super::turn_finalizer::TurnKey::new(
            channel_id,
            user_msg_id.get(),
            shared.restart.current_generation,
        ),
        provider.clone(),
        super::inflight::RelayOwnerKind::Watcher,
        shared, // #3016 phase-5a: prime the reconcile cache at register time.
    );
}

pub(super) async fn reregister_active_turn_from_inflight(
    shared: &Arc<SharedData>,
    state: &inflight::InflightTurnState,
) -> bool {
    if state.current_msg_id == 0 || state.user_msg_id == 0 || state.request_owner_user_id == 0 {
        return false;
    }

    let channel_id = ChannelId::new(state.channel_id);
    let user_msg_id = MessageId::new(state.user_msg_id);
    let snapshot = super::mailbox_snapshot(shared, channel_id).await;
    let Some(provider) = ProviderKind::from_str(&state.provider) else {
        tracing::error!(
            "inflight reregister failed: provider={} channel_id={} error=unsupported_provider",
            state.provider,
            state.channel_id
        );
        return false;
    };
    if recovery_terminal_delivery_already_committed(state) {
        tracing::warn!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            user_msg_id = state.user_msg_id,
            "inflight reregister skipped: terminal delivery already committed; clearing stale active turn state"
        );
        finish_recovered_turn_mailbox(
            shared,
            &provider,
            channel_id,
            "recovery_terminal_delivery_already_committed",
        )
        .await;
        clear_inflight_state(&provider, state.channel_id);
        return false;
    }
    if snapshot.cancel_token.is_some() {
        if let Some(token) = snapshot.cancel_token.as_ref()
            && snapshot.active_user_message_id == Some(user_msg_id)
        {
            super::ensure_cancel_token_bound_from_inflight_state(
                &provider,
                state,
                token,
                "inflight reregister existing active turn",
            );
        }
        let restored = snapshot.active_user_message_id == Some(user_msg_id);
        if restored {
            // #3248 gap-1: existing-active-turn rebind — re-seed the empty
            // post-restart ledger so the watcher gate-timeout arms its backstop.
            reseed_watcher_owned_finalizer_ledger(shared, channel_id, user_msg_id, &provider);
        }
        return restored;
    }

    let cancel_token = Arc::new(CancelToken::new());
    super::ensure_cancel_token_bound_from_inflight_state(
        &provider,
        state,
        &cancel_token,
        "inflight reregister active turn",
    );

    let started = super::mailbox_try_start_turn(
        shared,
        channel_id,
        cancel_token,
        UserId::new(state.request_owner_user_id),
        user_msg_id,
    )
    .await;
    if started {
        // #3248 gap-1: freshly re-attached active turn — seed the empty
        // post-restart ledger with the Watcher owner (idempotent vs. a later
        // bridge handoff) so the live pane auto-reconciles without a new user turn.
        reseed_watcher_owned_finalizer_ledger(shared, channel_id, user_msg_id, &provider);
    }
    started
}
#[cfg(unix)]
fn tmux_pane_pid(tmux_session_name: &str) -> Option<u32> {
    let mut cmd = Command::new("tmux");
    binary_resolver::apply_runtime_path(&mut cmd);
    let output = cmd
        .args([
            "display-message",
            "-p",
            "-t",
            &tmux_exact_target(tmux_session_name),
            "#{pane_pid}",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

#[cfg(unix)]
fn detect_live_tmux_output_path(
    tmux_session_name: &str,
    fallback_path: &str,
) -> Result<Option<DetectedRebindOutputPath>, StaleOutputCandidate> {
    let Some(pane_pid) = tmux_pane_pid(tmux_session_name) else {
        return Ok(None);
    };
    let mut cmd = Command::new("lsof");
    binary_resolver::apply_runtime_path(&mut cmd);
    let output = match cmd.args(["-Fn", "-p", &pane_pid.to_string()]).output() {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = match String::from_utf8(output.stdout) {
        Ok(stdout) => stdout,
        Err(_) => return Ok(None),
    };
    let candidates = parse_lsof_output_candidates(&stdout);
    detect_rebind_output_path_from_candidates(fallback_path, candidates)
}

fn extract_turn_analytics_from_output(
    output_path: &str,
    start_offset: u64,
) -> (Option<String>, Option<TurnTokenUsage>) {
    crate::services::session_backend::extract_turn_analytics_from_output(output_path, start_offset)
}

fn recovered_turn_duration_ms(started_at: Option<&str>) -> Option<i64> {
    let started_at = started_at?.trim();
    if started_at.is_empty() {
        return None;
    }
    let parsed = chrono::NaiveDateTime::parse_from_str(started_at, "%Y-%m-%d %H:%M:%S").ok()?;
    let elapsed = chrono::Local::now().naive_local() - parsed;
    Some(elapsed.num_milliseconds().max(0))
}

async fn lookup_turn_finished_dispatch_kind(dispatch_id: Option<&str>) -> Option<String> {
    let dispatch_id = dispatch_id?;
    let body = super::internal_api::lookup_dispatch_info(dispatch_id)
        .await
        .ok()?;
    super::turn_bridge::classify_turn_finished_dispatch_kind(
        body.get("dispatch_context")
            .and_then(|value| value.as_str()),
        body.get("dispatch_type").and_then(|value| value.as_str()),
    )
    .map(str::to_string)
}

/// Build the transcript turn id for a recovered turn. A message-less recovery
/// turn (`user_msg_id == 0`, e.g. TUI-direct) must NOT key on `discord:<ch>:0`
/// (collides across every such turn → overwrite, Codex P2 r2) NOR purely on the
/// session (repeated message-less turns upsert-overwrite, Codex P2 r3): instead
/// append a PER-TURN discriminator — the JSONL start offset, falling back to the
/// `started_at` timestamp when no offset is recorded.
fn recovered_transcript_turn_id(
    channel_id: u64,
    user_msg_id: u64,
    session_key: Option<&str>,
    turn_start_offset: Option<u64>,
    started_at: &str,
) -> String {
    if user_msg_id != 0 {
        return format!("discord:{channel_id}:{user_msg_id}");
    }
    // Per-turn discriminator: stable for THIS turn, distinct across turns.
    let turn_discriminator = match turn_start_offset {
        Some(offset) => format!("off{offset}"),
        None => format!("at{}", started_at.replace([' ', ':'], "-")),
    };
    match session_key {
        Some(key) => format!("discord:{channel_id}:session:{key}:{turn_discriminator}"),
        None => format!("discord:{channel_id}:recovery:{turn_discriminator}"),
    }
}

async fn persist_recovered_transcript(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    dispatch_id: Option<&str>,
    assistant_message: &str,
) -> bool {
    let assistant_message = assistant_message.trim();
    if assistant_message.is_empty() {
        return false;
    }

    let turn_id = recovered_transcript_turn_id(
        state.channel_id,
        state.user_msg_id,
        state.session_key.as_deref(),
        state.turn_start_offset,
        &state.started_at,
    );
    let channel_id_text = state.channel_id.to_string();
    match crate::db::session_transcripts::persist_turn_db(
        db,
        pg_pool,
        crate::db::session_transcripts::PersistSessionTranscript {
            turn_id: &turn_id,
            session_key: state.session_key.as_deref(),
            channel_id: Some(channel_id_text.as_str()),
            agent_id: None,
            provider: Some(provider.as_str()),
            dispatch_id,
            user_message: &state.user_text,
            assistant_message,
            events: &[],
            duration_ms: None,
        },
    )
    .await
    {
        Ok(_) => true,
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ recovery: failed to persist session transcript: {e}");
            false
        }
    }
}

fn output_has_bytes_after_offset(output_path: &str, start_offset: u64) -> bool {
    std::fs::metadata(output_path)
        .map(|meta| meta.len() > start_offset)
        .unwrap_or(false)
}

const TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD: std::time::Duration = std::time::Duration::from_secs(2);

fn terminal_success_watcher_stop_allowed(
    confirmed_end: u64,
    tmux_tail_offset: u64,
    quiet_for: std::time::Duration,
) -> bool {
    confirmed_end >= tmux_tail_offset && quiet_for >= TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD
}

async fn terminal_success_output_drained_for_recovery(
    output_path: &str,
    confirmed_end: u64,
    tmux_session_name: Option<&str>,
) -> bool {
    let Ok(before_meta) = std::fs::metadata(output_path) else {
        return false;
    };
    let tmux_alive = tmux_session_name
        .map(crate::services::platform::tmux::has_session)
        .unwrap_or(false);

    if !tmux_alive {
        return terminal_success_watcher_stop_allowed(
            confirmed_end,
            before_meta.len(),
            TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD,
        );
    }

    if !terminal_success_watcher_stop_allowed(
        confirmed_end,
        before_meta.len(),
        TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD,
    ) {
        return false;
    }

    // #2442 (H2) — fast-path: if the wrapper has already emitted the
    // `terminal_end` JSONL sentinel, the pane is *definitively* done
    // writing and we can graduate the 2s drain quiet-period immediately.
    // The wrapper writes the sentinel as one of its very last actions
    // before kill_child_tree/cleanup, so its presence is a strict superset
    // of the quiet-period heuristic. We still keep the legacy 2s sleep as
    // a fallback for SIGKILL paths that bypass the sentinel write.
    if jsonl_tail_contains_terminal_end_sentinel(output_path) {
        return terminal_success_watcher_stop_allowed(
            confirmed_end,
            before_meta.len(),
            TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD,
        );
    }

    tokio::time::sleep(TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD).await;

    let tail_after = std::fs::metadata(output_path)
        .map(|meta| meta.len())
        .unwrap_or(confirmed_end.saturating_add(1));
    tail_after == confirmed_end
        && terminal_success_watcher_stop_allowed(
            confirmed_end,
            tail_after,
            TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD,
        )
}

/// #2442 — peek the JSONL tail (last ~4 KiB) for the wrapper's
/// `terminal_end` sentinel. Reading the tail rather than the entire file
/// keeps this O(1) regardless of jsonl size. False negatives (no sentinel
/// detected when one is present) just fall back to the legacy 2s
/// quiet-period sleep, so a partial-line edge case is harmless.
fn jsonl_tail_contains_terminal_end_sentinel(output_path: &str) -> bool {
    use std::io::{Read, Seek, SeekFrom};

    const TAIL_WINDOW_BYTES: u64 = 4 * 1024;

    let Ok(mut file) = std::fs::File::open(output_path) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    let len = meta.len();
    if len == 0 {
        return false;
    }
    let start = len.saturating_sub(TAIL_WINDOW_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut buf = Vec::with_capacity(TAIL_WINDOW_BYTES as usize);
    if file.read_to_end(&mut buf).is_err() {
        return false;
    }
    // The sentinel is one JSONL line: {"type":"terminal_end",...}. We search the
    // literal `"type":"terminal_end"` token because the wrapper writes JSON via
    // `serde_json::Value::to_string()` (exact compact form); the contract lives in
    // `tmux_common::emit_wrapper_sentinel` (pretty-printing would need a rework).
    let needle = format!(
        "\"type\":\"{}\"",
        crate::services::tmux_common::WRAPPER_TERMINAL_END_EVENT
    );
    let haystack = String::from_utf8_lossy(&buf);
    haystack.contains(&needle)
}

fn recovery_watcher_start_offset(output_path: &str, saved_last_offset: u64) -> (u64, u64, bool) {
    let current_len = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    if current_len >= saved_last_offset {
        (saved_last_offset, current_len, false)
    } else {
        // The output file was recreated or truncated while dcserver was down.
        // Resume from the beginning of the new file so we do not skip the
        // entire restarted session output.
        (0, current_len, true)
    }
}

pub(super) async fn restore_inflight_turns(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let states = load_inflight_states(provider);
    if states.is_empty() {
        return;
    }

    let settings_snapshot = shared.settings.read().await.clone();

    for state in states {
        // #897 round-4 High: rebind_origin inflights are synthetic
        // placeholders owned by `/api/inflight/rebind` and do NOT carry
        // a real user message, dispatch context, or placeholder Discord
        // message. Restart recovery has nothing meaningful to do with
        // them — running `replace_long_message_raw(msg_id=0)`, writing
        // `discord:<channel>:0` analytics rows, or emitting reactions
        // against `MessageId::new(0)` would all produce bogus state
        // (flagged by #897 round-4 review). The operator is expected to
        // re-invoke `/api/inflight/rebind` after dcserver comes back up
        // if the orphan tmux is still alive. Clear the stale state and
        // skip further processing for this entry.
        if state.rebind_origin {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ recovery: skipping rebind-origin inflight for channel {} — operator must re-invoke /api/inflight/rebind post-restart",
                state.channel_id
            );
            clear_inflight_state(provider, state.channel_id);
            continue;
        }

        let channel_id = ChannelId::new(state.channel_id);

        // #2235: silent-skip rows whose on-disk `runtime_kind` was a
        // present-but-unknown variant string. `load_inflight_states_from_root`
        // distinguishes this from "field absent" (legacy v7 rows) via the
        // transient `runtime_kind_unknown_on_disk` flag, so the existing
        // heuristic recovery path still runs for absent-field legacy rows.
        // Belt-and-suspenders: also silent-skip when a row's persisted
        // `version` is ahead of this binary and `runtime_kind` is missing —
        // forward-marked rows authored by a newer binary should not be
        // guessed at.
        let runtime_kind_skew_detected = state.runtime_kind_unknown_on_disk
            || (state.runtime_kind.is_none()
                && state.version > super::inflight::inflight_state_version());
        if runtime_kind_skew_detected {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::debug!(
                "  [{ts}] ↩ inflight recovery silent-skip for channel {}: runtime_kind unknown/forward-marked (version={}, local={}, unknown_on_disk={})",
                state.channel_id,
                state.version,
                super::inflight::inflight_state_version(),
                state.runtime_kind_unknown_on_disk
            );
            finish_recovered_turn_mailbox(
                shared,
                provider,
                channel_id,
                "recovery_runtime_kind_unknown_skip",
            )
            .await;
            clear_inflight_state(provider, state.channel_id);
            continue;
        }
        let is_dm = matches!(
            channel_id.to_channel(http).await,
            Ok(serenity::model::channel::Channel::Private(_))
        );
        let restart_report_exists =
            super::restart_report::load_restart_report(provider, state.channel_id).is_some();
        crate::services::observability::emit_recovery_fired(
            provider.as_str(),
            state.channel_id,
            state.dispatch_id.as_deref(),
            state.session_key.as_deref(),
            if restart_report_exists {
                "restart_report"
            } else {
                "restore_inflight"
            },
        );
        emit_recovery_quality_event(
            provider,
            state.channel_id,
            state.dispatch_id.as_deref(),
            state.session_key.as_deref(),
            if restart_report_exists {
                "restart_report"
            } else {
                "restore_inflight"
            },
        );

        // No generation gate — adopt mode allows old-gen session recovery. If a
        // restart report exists, check whether the agent already finished before
        // skipping recovery: a completed result is delivered directly and clears
        // both the inflight state and the restart report, so the flush loop won't
        // overwrite the message with a generic follow-up.
        if restart_report_exists {
            let output_path_for_check: Option<String> = state
                .output_path
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    state
                        .channel_name
                        .as_ref()
                        .map(|name| tmux_runtime_paths(&provider.build_tmux_session_name(name)).0)
                });
            let restart_tmux_name = recovery_tmux_session_name(provider, &state);
            let completed_during_downtime_end = output_path_for_check
                .as_deref()
                .and_then(|path| success_result_end_offset_after_offset(path, state.last_offset));
            let completed_during_downtime = completed_during_downtime_end.is_some();
            let completed_during_downtime_drained = match (
                output_path_for_check.as_deref(),
                completed_during_downtime_end,
            ) {
                (Some(path), Some(confirmed_end)) => {
                    terminal_success_output_drained_for_recovery(
                        path,
                        confirmed_end,
                        restart_tmux_name.as_deref(),
                    )
                    .await
                }
                (None, Some(_)) => true,
                _ => false,
            };

            if completed_during_downtime && !completed_during_downtime_drained {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ recovery: terminal success observed for channel {} but tmux output has not stayed drained; reattaching watcher",
                    state.channel_id
                );
            }

            if completed_during_downtime_drained {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ✓ recovering completed turn for channel {} (restart report exists but output has result)",
                    state.channel_id
                );
                let (recovered_session_id, recovered_usage) = output_path_for_check
                    .as_deref()
                    .map(|path| {
                        extract_turn_analytics_from_output(
                            path,
                            state.turn_start_offset.unwrap_or(state.last_offset),
                        )
                    })
                    .unwrap_or((None, None));
                let extracted = output_path_for_check
                    .as_deref()
                    .map(|p| extract_response_from_output(p, state.last_offset))
                    .unwrap_or_default();
                let assistant_response = if extracted.trim().is_empty() {
                    state.full_response.clone()
                } else {
                    extracted
                };
                let final_text = if assistant_response.trim().is_empty() {
                    "(복구됨 — 응답 텍스트 없음)".to_string()
                } else {
                    super::formatting::format_for_discord_with_provider(
                        &assistant_response,
                        provider,
                    )
                };
                let channel_id = ChannelId::new(state.channel_id);
                // An un-anchored TUI-direct/recovery turn (current_msg_id == 0)
                // delivers the recovered text as a NEW channel message, not an
                // in-place edit (the helper handles both); `relay_ok` still
                // reflects actual delivery so recovery never advances without
                // posting. `MessageId::new(0)` would panic.
                let relay_ok = relay_recovered_terminal_text_to_placeholder(
                    http,
                    shared,
                    channel_id,
                    optional_message_id(state.current_msg_id),
                    &final_text,
                )
                .await
                .delivered();
                if !should_advance_recovery_dispatch_after_relay(relay_ok) {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ recovery: Discord relay failed before downtime dispatch completion — preserving inflight for retry"
                    );
                    continue;
                }
                // Mark the user message completed only after terminal delivery
                // commits (else the channel shows completion without the final
                // message). A message-less turn (user_msg_id == 0) has no analytics
                // row to key (`discord:<ch>:0` is bogus) and `MessageId::new(0)`
                // panics; the transcript persist below stays unconditional.
                let user_msg_id = optional_message_id(state.user_msg_id);
                let visible_outcome = complete_recovery_visible_turn(
                    http,
                    shared,
                    provider,
                    &state,
                    false,
                    "completed_during_downtime",
                )
                .await;
                if !visible_outcome.should_proceed() {
                    // Reserved for future non-proceeding recovery outcomes.
                    // A TUI quiescence timeout is not one: terminal delivery
                    // evidence is authoritative for mailbox/inflight cleanup.
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        provider = %provider.as_str(),
                        channel = channel_id.get(),
                        "[{ts}] ⚠ recovery (completed_during_downtime) deferred by non-proceeding visible outcome"
                    );
                    continue;
                }
                // Complete the dispatch if this was a work dispatch turn — the
                // normal completion path was lost when dcserver restarted.
                // #142: implementation/rework need explicit completion. Review
                // and review-decision stay pending until their API handlers run.
                // Parse saved DISPATCH evidence first; reused threads may
                // already have a newer pending dispatch on the same thread.
                let recovered_dispatch_id = parse_dispatch_id(&state.user_text).or(
                    lookup_pending_dispatch_for_thread(shared.api_port, state.channel_id).await,
                );
                let role_binding = resolve_role_binding(channel_id, state.channel_name.as_deref());
                let duration_ms =
                    recovered_turn_duration_ms(Some(state.started_at.as_str())).unwrap_or(0);
                let has_completion_evidence =
                    if None::<&crate::db::Db>.is_some() || shared.pg_pool.is_some() {
                        if let Some(user_msg_id) = user_msg_id {
                            super::turn_bridge::persist_turn_analytics_row_with_handles(
                                None::<&crate::db::Db>,
                                shared.pg_pool.as_ref(),
                                provider,
                                channel_id,
                                user_msg_id,
                                role_binding.as_ref(),
                                recovered_dispatch_id
                                    .as_deref()
                                    .or(state.dispatch_id.as_deref()),
                                state.session_key.as_deref(),
                                recovered_session_id
                                    .as_deref()
                                    .or(state.session_id.as_deref()),
                                &state,
                                recovered_usage.unwrap_or_default(),
                                duration_ms,
                            );
                        }
                        persist_recovered_transcript(
                            None::<&crate::db::Db>,
                            shared.pg_pool.as_ref(),
                            provider,
                            &state,
                            recovered_dispatch_id
                                .as_deref()
                                .or(state.dispatch_id.as_deref()),
                            &assistant_response,
                        )
                        .await
                    } else {
                        !assistant_response.trim().is_empty()
                    };
                let completion_context = has_completion_evidence
                    .then(|| serde_json::json!({ "agent_response_present": true }));
                let fallback_result = completion_context
                    .clone()
                    .map(|mut result| {
                        if let Some(obj) = result.as_object_mut() {
                            obj.insert(
                                "completion_source".to_string(),
                                serde_json::Value::String("recovery_db_fallback".to_string()),
                            );
                            obj.insert(
                                "needs_reconcile".to_string(),
                                serde_json::Value::Bool(true),
                            );
                        }
                        result
                    })
                    .unwrap_or_else(|| {
                        serde_json::json!({
                            "completion_source": "recovery_db_fallback",
                            "needs_reconcile": true,
                        })
                    });
                let mut dispatch_completed = recovered_dispatch_id.is_none();
                if let Some(ref did) = recovered_dispatch_id {
                    if !has_completion_evidence {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ recovery: refusing to complete work dispatch {did} without assistant response"
                        );
                    } else if let Some(engine) = &shared.policy.engine {
                        // #143: Use finalize_dispatch directly with retry.
                        for attempt in 1..=3u8 {
                            match crate::dispatch::finalize_dispatch_with_backends(
                                None::<&crate::db::Db>,
                                engine,
                                did,
                                "recovery_completed_during_downtime",
                                completion_context.as_ref(),
                            ) {
                                Ok(_) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!(
                                        "  [{ts}] ✓ recovery: completed dispatch {did} via finalize_dispatch"
                                    );
                                    let _ =
                                        super::turn_bridge::queue_dispatch_followup_with_handles(
                                            shared.pg_pool.as_ref(),
                                            did,
                                            "recovery_completed_during_downtime",
                                        )
                                        .await;
                                    dispatch_completed = true;
                                    break;
                                }
                                Err(e) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::warn!(
                                        "  [{ts}] ⚠ recovery: finalize_dispatch failed for {did} (attempt {attempt}/3): {e}"
                                    );
                                    if attempt < 3 {
                                        // #2428 H5: exponential backoff + jitter.
                                        tokio::time::sleep(recovery_retry_backoff(u32::from(
                                            attempt,
                                        )))
                                        .await;
                                    }
                                }
                            }
                        }
                        // All retries exhausted — use the canonical runtime-root
                        // Postgres fallback instead of mutating legacy SQLite state.
                        if !dispatch_completed {
                            dispatch_completed =
                                super::turn_bridge::runtime_db_fallback_complete_with_result(
                                    did,
                                    &fallback_result,
                                );
                            if dispatch_completed {
                                let _ = super::turn_bridge::queue_dispatch_followup_with_handles(
                                    shared.pg_pool.as_ref(),
                                    did,
                                    "recovery_completed_during_downtime_fallback",
                                )
                                .await;
                            }
                        }
                    } else {
                        // Db/Engine not available — fall back to direct dispatch update with retry
                        let payload = crate::services::dispatches::UpdateDispatchBody {
                                status: Some("completed".to_string()),
                                result: Some(completion_context.clone().map(|mut result| {
                                    if let Some(obj) = result.as_object_mut() {
                                        obj.insert(
                                            "completion_source".to_string(),
                                            serde_json::Value::String(
                                                "recovery_completed_during_downtime".to_string(),
                                            ),
                                        );
                                    }
                                    result
                                }).unwrap_or_else(|| {
                                    serde_json::json!({
                                        "completion_source": "recovery_completed_during_downtime"
                                    })
                                })),
                                allowed_from: None,
                            };
                        use super::internal_api::DispatchUpdateOutcome;
                        let mut already_terminal = false;
                        for attempt in 1..=3u8 {
                            match super::internal_api::update_dispatch(did, payload.clone()).await {
                                Ok(DispatchUpdateOutcome::Updated(_)) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!("  [{ts}] ✓ recovery: completed dispatch {did}");
                                    dispatch_completed = true;
                                    break;
                                }
                                Ok(DispatchUpdateOutcome::Conflict { body }) => {
                                    // #2194 follow-up: dispatch is already in a
                                    // terminal status. Treat as success — do NOT
                                    // run DB fallback, which would overwrite the
                                    // existing result.
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!(
                                        dispatch_id = %did,
                                        response = %body,
                                        "  [{ts}] ✓ recovery: dispatch {did} already terminal (409); leaving prior result intact"
                                    );
                                    dispatch_completed = true;
                                    already_terminal = true;
                                    break;
                                }
                                Err(err) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::warn!(
                                        "  [{ts}] ⚠ recovery: dispatch {did} completion failed (attempt {attempt}/3): {err}"
                                    );
                                }
                            }
                            if attempt < 3 {
                                // #2428 H5: exponential backoff + jitter.
                                tokio::time::sleep(recovery_retry_backoff(u32::from(attempt)))
                                    .await;
                            }
                        }
                        // API retries exhausted — runtime-root DB fallback.
                        // Skip when the dispatch was already terminal (409) so we
                        // don't clobber its preserved result.
                        if !dispatch_completed && !already_terminal {
                            dispatch_completed =
                                super::turn_bridge::runtime_db_fallback_complete_with_result(
                                    did,
                                    &fallback_result,
                                );
                        }
                    }
                }
                // Only clear recovery bookkeeping if dispatch was completed (or no dispatch).
                // Preserving state on failure allows the next recovery pass to retry.
                if dispatch_completed {
                    super::restart_report::clear_restart_report(provider, state.channel_id);
                    finish_recovered_turn_mailbox(
                        shared,
                        provider,
                        channel_id,
                        "recovery_completed_during_downtime",
                    )
                    .await;
                    clear_inflight_state(provider, state.channel_id);
                } else if let Some(ref did) = recovered_dispatch_id {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ❌ recovery: dispatch {did} completion failed — preserving state for next recovery pass"
                    );
                }
                continue;
            }

            // Agent may still be running.  If the tmux session is alive, clear
            // the restart report and fall through to normal recovery (which
            // re-attaches a watcher to pick up the remaining output).
            // If the session is dead, delegate to the flush loop for fallback.
            let tmux_name = restart_tmux_name;
            let session_alive = tmux_name
                .as_deref()
                .map_or(false, tmux_session_alive_with_retry);
            // Derive channel_name from tmux session name if not in inflight state.
            // Validate before mutating restart-report state so other same-provider
            // bots do not log/clear reports for channels they do not own.
            let effective_channel_name = state.channel_name.clone().or_else(|| {
                tmux_name.as_deref().and_then(|name| {
                    crate::services::provider::parse_provider_and_channel_from_tmux_name(name)
                        .map(|(_, ch)| ch)
                })
            });
            let (allowlist_channel_id, provider_channel_name) =
                if let Some((pid, pname)) = super::resolve_thread_parent(http, channel_id).await {
                    (pid, pname.or(effective_channel_name.clone()))
                } else {
                    (channel_id, effective_channel_name.clone())
                };
            if let Err(reason) = validate_bot_channel_routing_with_provider_channel(
                &settings_snapshot,
                provider,
                allowlist_channel_id,
                effective_channel_name.as_deref(),
                provider_channel_name.as_deref(),
                is_dm,
            ) {
                if !reason.is_expected_cross_bot_skip() {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ⏭ inflight recovery skip for channel {} — {reason}",
                        state.channel_id,
                    );
                }
                continue;
            }

            if session_alive {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ restart report exists but tmux session alive for channel {}: clearing report, spawning watcher immediately",
                    state.channel_id
                );
                super::restart_report::clear_restart_report(provider, state.channel_id);
                // Register session in-memory so handlers can find it.
                // Derive channel_name from tmux session name if not in inflight state.
                let effective_channel_name = state.channel_name.clone().or_else(|| {
                    tmux_name.as_deref().and_then(|name| {
                        crate::services::provider::parse_provider_and_channel_from_tmux_name(name)
                            .map(|(_, ch)| ch)
                    })
                });
                // Resolve thread parent so validation uses the same semantics
                // as normal message routing (router.rs).
                let (allowlist_channel_id, provider_channel_name) = if let Some((pid, pname)) =
                    super::resolve_thread_parent(http, channel_id).await
                {
                    (pid, pname.or(effective_channel_name.clone()))
                } else {
                    (channel_id, effective_channel_name.clone())
                };
                if let Err(reason) = validate_bot_channel_routing_with_provider_channel(
                    &settings_snapshot,
                    provider,
                    allowlist_channel_id,
                    effective_channel_name.as_deref(),
                    provider_channel_name.as_deref(),
                    is_dm,
                ) {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⏭ inflight recovery skip for channel {} — {reason}",
                        state.channel_id,
                    );
                    continue;
                }
                {
                    let mut data = shared.core.lock().await;
                    let session =
                        data.sessions
                            .entry(channel_id)
                            .or_insert_with(|| DiscordSession {
                                session_id: state.session_id.clone(),
                                memento_context_loaded: false,
                                memento_reflected: false,
                                current_path: None,
                                history: Vec::new(),
                                pending_uploads: Vec::new(),
                                cleared: false,
                                remote_profile_name: None,
                                channel_id: Some(state.channel_id),
                                channel_name: effective_channel_name.clone(),
                                category_name: None,
                                last_active: tokio::time::Instant::now(),
                                worktree: None,
                                born_generation: super::runtime_store::load_generation(),
                            });
                    session.channel_id = Some(state.channel_id);
                    session.last_active = tokio::time::Instant::now();
                    if session.channel_name.is_none() {
                        session.channel_name = effective_channel_name;
                    }
                    restore_recovered_session_worktree(session, &state);
                }

                let finish_mailbox_on_completion =
                    reregister_active_turn_from_inflight(shared, &state).await;

                // Spawn the tmux watcher immediately rather than deferring to
                // restore_tmux_watchers(): the "watcher will adopt" approach raced
                // — the session could die in the ~50s gap and lose the response.
                if let Some(ref tmux_session_name) = tmux_name {
                    let output_path =
                        crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
                    if std::fs::metadata(&output_path).is_ok() {
                        let (initial_offset, current_len, truncated) =
                            recovery_watcher_start_offset(&output_path, state.last_offset);
                        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let resume_offset = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
                        let pause_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                        let turn_delivered =
                            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let last_heartbeat_ts_ms = std::sync::Arc::new(
                            std::sync::atomic::AtomicI64::new(super::tmux_watcher_now_ms()),
                        );
                        let handle = TmuxWatcherHandle {
                            tmux_session_name: tmux_session_name.clone(),
                            output_path: output_path.clone(),
                            paused: paused.clone(),
                            resume_offset: resume_offset.clone(),
                            cancel: cancel.clone(),
                            pause_epoch: pause_epoch.clone(),
                            turn_delivered: turn_delivered.clone(),
                            last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
                        };
                        let watcher_claimed = {
                            #[cfg(unix)]
                            {
                                let claim = super::tmux::claim_or_reuse_watcher(
                                    &shared.tmux_watchers,
                                    channel_id,
                                    handle,
                                    provider,
                                    "restart_report_recovery",
                                );
                                claim.should_spawn()
                            }
                            #[cfg(not(unix))]
                            {
                                let _ = handle;
                                false
                            }
                        };
                        if watcher_claimed {
                            let ts2 = chrono::Local::now().format("%H:%M:%S");
                            if truncated {
                                tracing::info!(
                                    "  [{ts2}] ↻ recovery: output truncated for #{} (saved offset {}, file len {}), restarting watcher from 0",
                                    tmux_session_name,
                                    state.last_offset,
                                    current_len
                                );
                            }
                            tracing::info!(
                                "  [{ts2}] 👁 recovery: spawned watcher for #{} at offset {}",
                                tmux_session_name,
                                initial_offset
                            );
                            #[cfg(unix)]
                            {
                                let restored_turn =
                                    super::tmux::restored_watcher_turn_from_inflight(
                                        &state,
                                        tmux_session_name,
                                        finish_mailbox_on_completion,
                                    );
                                shared.record_tmux_watcher_reconnect(channel_id);
                                super::task_supervisor::spawn_observed_tmux_watcher(
                                    "recovery_tmux_output_watcher_with_restore",
                                    shared.clone(),
                                    tmux_session_name.clone(),
                                    cancel.clone(),
                                    super::tmux::tmux_output_watcher_with_restore(
                                        channel_id,
                                        http.clone(),
                                        shared.clone(),
                                        output_path,
                                        tmux_session_name.clone(),
                                        initial_offset,
                                        cancel,
                                        paused,
                                        resume_offset,
                                        pause_epoch,
                                        turn_delivered,
                                        last_heartbeat_ts_ms,
                                        restored_turn,
                                    ),
                                );
                            }
                        }
                    }
                }

                // Keep the inflight state until the watcher either relays the
                // final response or triggers watcher-death handoff. Clearing it
                // here breaks the handoff path if the recovered tmux session
                // dies before producing a result.
                continue;
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                if let Some(diag) = tmux_name.as_deref().and_then(|name| {
                    build_tmux_death_diagnostic(name, output_path_for_check.as_deref())
                }) {
                    tracing::info!(
                        "  [{ts}] ↻ restart report exists but tmux session is dead for channel {}: clearing report, continuing with direct fallback recovery ({diag})",
                        state.channel_id
                    );
                } else {
                    tracing::info!(
                        "  [{ts}] ↻ restart report exists but tmux session is dead for channel {}: clearing report, continuing with direct fallback recovery",
                        state.channel_id
                    );
                }
                super::restart_report::clear_restart_report(provider, state.channel_id);
            }
        }

        // current_msg_id/user_msg_id == 0 are LEGITIMATE (TUI-direct / un-anchored
        // recovery turn). `MessageId::new(0)` PANICS, and this loop runs inline at
        // startup, so one such inflight would abort it before `reconcile_done` is
        // set → provider permanently degraded. Carry both as `Option`, skip the
        // placeholder/analytics step per use site, still recover the tmux session.
        let current_msg_id = optional_message_id(state.current_msg_id);
        let user_msg_id = optional_message_id(state.user_msg_id);
        let channel_name = state.channel_name.clone();
        let tmux_session_name = state.tmux_session_name.clone().or_else(|| {
            channel_name
                .as_ref()
                .map(|name| provider.build_tmux_session_name(name))
        });
        let channel_name = channel_name.or_else(|| {
            tmux_session_name.as_deref().and_then(|name| {
                crate::services::provider::parse_provider_and_channel_from_tmux_name(name)
                    .map(|(_, ch)| ch)
            })
        });
        // Resolve thread parent so validation uses the same semantics
        // as normal message routing (router.rs).
        let (allowlist_channel_id, provider_channel_name) =
            if let Some((pid, pname)) = super::resolve_thread_parent(http, channel_id).await {
                (pid, pname.or(channel_name.clone()))
            } else {
                (channel_id, channel_name.clone())
            };
        if let Err(reason) = validate_bot_channel_routing_with_provider_channel(
            &settings_snapshot,
            provider,
            allowlist_channel_id,
            channel_name.as_deref(),
            provider_channel_name.as_deref(),
            is_dm,
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ inflight recovery skip for channel {} — {reason}",
                state.channel_id,
            );
            continue;
        }
        let (fallback_output, fallback_input) = tmux_session_name
            .as_deref()
            .map(tmux_runtime_paths)
            .unwrap_or_else(|| (String::new(), String::new()));
        let runtime_kind = state.runtime_kind_for_recovery();
        let output_path = state
            .output_path
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                if !fallback_output.is_empty() {
                    Some(fallback_output.clone())
                } else {
                    None
                }
            });
        let input_fifo_path = if runtime_kind.requires_input_fifo() {
            state
                .input_fifo_path
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    if !fallback_input.is_empty() {
                        Some(fallback_input.clone())
                    } else {
                        None
                    }
                })
        } else {
            state.input_fifo_path.clone().filter(|s| !s.is_empty())
        };
        // Check exit reason file for post-mortem diagnostics
        if let Some(ref op) = output_path {
            let exit_reason_path = format!("{}.exit_reason", op);
            if let Ok(reason) = std::fs::read_to_string(&exit_reason_path) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔍 exit_reason for channel {}: {}",
                    state.channel_id,
                    reason.trim()
                );
                // Clean up exit reason file after reading
                let _ = std::fs::remove_file(&exit_reason_path);
            }
        }

        let terminal_success_end = output_path
            .as_deref()
            .and_then(|path| success_result_end_offset_after_offset(path, state.last_offset));
        let output_already_completed = terminal_success_end.is_some();
        let terminal_success_drained = match (output_path.as_deref(), terminal_success_end) {
            (Some(path), Some(confirmed_end)) => {
                terminal_success_output_drained_for_recovery(
                    path,
                    confirmed_end,
                    tmux_session_name.as_deref(),
                )
                .await
            }
            (None, Some(_)) => true,
            _ => false,
        };
        if output_already_completed && !terminal_success_drained {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ recovery: terminal success observed for channel {} but tmux output has not stayed drained; reattaching watcher",
                state.channel_id
            );
        }
        let output_has_new_bytes = output_path
            .as_deref()
            .map(|path| output_has_bytes_after_offset(path, state.last_offset))
            .unwrap_or(false);

        if can_fast_path_captured_full_response(&state, terminal_success_drained) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ recovery fast-path: delivering captured full_response for channel {}",
                state.channel_id,
            );

            let assistant_response = state.full_response.clone();
            let final_text =
                super::formatting::format_for_discord_with_provider(&assistant_response, provider);
            let relay_ok = relay_recovered_terminal_text_to_placeholder(
                http,
                shared,
                channel_id,
                current_msg_id,
                &final_text,
            )
            .await
            .delivered();

            if !should_advance_recovery_dispatch_after_relay(relay_ok) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ recovery: Discord relay failed before dispatch completion — preserving inflight for retry"
                );
                continue;
            }
            let visible_outcome = complete_recovery_visible_turn(
                http,
                shared,
                provider,
                &state,
                false,
                "captured_full_response",
            )
            .await;
            if !visible_outcome.should_proceed() {
                // Reserved for future non-proceeding recovery outcomes.
                // A TUI quiescence timeout is not one: terminal delivery
                // evidence is authoritative for mailbox/inflight cleanup.
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    provider = %provider.as_str(),
                    channel = channel_id.get(),
                    "[{ts}] ⚠ recovery (captured_full_response) deferred by non-proceeding visible outcome"
                );
                continue;
            }

            let recovered_dispatch_id = parse_dispatch_id(&state.user_text)
                .or(lookup_pending_dispatch_for_thread(shared.api_port, state.channel_id).await);
            let role_binding = resolve_role_binding(channel_id, state.channel_name.as_deref());
            let duration_ms =
                recovered_turn_duration_ms(Some(state.started_at.as_str())).unwrap_or(0);
            let has_completion_evidence =
                if None::<&crate::db::Db>.is_some() || shared.pg_pool.is_some() {
                    // No user message (user_msg_id == 0) → no analytics row to
                    // key (`discord:<channel>:0` would be bogus); skip the
                    // analytics persist but still write the transcript.
                    if let Some(user_msg_id) = user_msg_id {
                        super::turn_bridge::persist_turn_analytics_row_with_handles(
                            None::<&crate::db::Db>,
                            shared.pg_pool.as_ref(),
                            provider,
                            channel_id,
                            user_msg_id,
                            role_binding.as_ref(),
                            recovered_dispatch_id
                                .as_deref()
                                .or(state.dispatch_id.as_deref()),
                            state.session_key.as_deref(),
                            state.session_id.as_deref(),
                            &state,
                            TurnTokenUsage::default(),
                            duration_ms,
                        );
                    }
                    persist_recovered_transcript(
                        None::<&crate::db::Db>,
                        shared.pg_pool.as_ref(),
                        provider,
                        &state,
                        recovered_dispatch_id
                            .as_deref()
                            .or(state.dispatch_id.as_deref()),
                        &assistant_response,
                    )
                    .await
                } else {
                    !assistant_response.trim().is_empty()
                };
            let completion_context = has_completion_evidence
                .then(|| serde_json::json!({ "agent_response_present": true }));
            let fallback_result = completion_context
                .clone()
                .map(|mut result| {
                    if let Some(obj) = result.as_object_mut() {
                        obj.insert(
                            "completion_source".to_string(),
                            serde_json::Value::String(
                                "recovery_captured_full_response_db_fallback".to_string(),
                            ),
                        );
                        obj.insert("needs_reconcile".to_string(), serde_json::Value::Bool(true));
                    }
                    result
                })
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "completion_source": "recovery_captured_full_response_db_fallback",
                        "needs_reconcile": true,
                    })
                });
            let mut dispatch_completed = recovered_dispatch_id.is_none();
            if let Some(ref did) = recovered_dispatch_id {
                let dispatch_type = super::internal_api::lookup_dispatch_type(did)
                    .await
                    .ok()
                    .flatten();

                match dispatch_type.as_deref() {
                    Some("implementation") | Some("rework") => {
                        if !has_completion_evidence {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ recovery: refusing to complete work dispatch {did} without assistant response"
                            );
                        } else if let Some(engine) = &shared.policy.engine {
                            for attempt in 1..=3u8 {
                                match crate::dispatch::finalize_dispatch_with_backends(
                                    None::<&crate::db::Db>,
                                    engine,
                                    did,
                                    "recovery_captured_full_response",
                                    completion_context.as_ref(),
                                ) {
                                    Ok(_) => {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::info!(
                                            "  [{ts}] ✓ recovery: completed dispatch {did} via finalize_dispatch"
                                        );
                                        let _ =
                                            super::turn_bridge::queue_dispatch_followup_with_handles(
                                                shared.pg_pool.as_ref(),
                                                did,
                                                "recovery_captured_full_response",
                                            )
                                            .await;
                                        dispatch_completed = true;
                                        break;
                                    }
                                    Err(e) => {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::warn!(
                                            "  [{ts}] ⚠ recovery: finalize_dispatch failed for {did} (attempt {attempt}/3): {e}"
                                        );
                                        if attempt < 3 {
                                            // #2428 H5: exponential backoff + jitter.
                                            tokio::time::sleep(recovery_retry_backoff(u32::from(
                                                attempt,
                                            )))
                                            .await;
                                        }
                                    }
                                }
                            }
                            if !dispatch_completed {
                                dispatch_completed =
                                    super::turn_bridge::runtime_db_fallback_complete_with_result(
                                        did,
                                        &fallback_result,
                                    );
                                if dispatch_completed {
                                    let _ =
                                        super::turn_bridge::queue_dispatch_followup_with_handles(
                                            shared.pg_pool.as_ref(),
                                            did,
                                            "recovery_captured_full_response_fallback",
                                        )
                                        .await;
                                }
                            }
                        } else {
                            dispatch_completed =
                                super::turn_bridge::runtime_db_fallback_complete_with_result(
                                    did,
                                    &fallback_result,
                                );
                            if dispatch_completed {
                                let _ = super::turn_bridge::queue_dispatch_followup_with_handles(
                                    shared.pg_pool.as_ref(),
                                    did,
                                    "recovery_captured_full_response_runtime_fallback",
                                )
                                .await;
                            }
                        }
                        if !dispatch_completed {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ❌ recovery: dispatch {did} completion failed — preserving state for retry"
                            );
                        }
                    }
                    Some(_) => {
                        dispatch_completed = true;
                    }
                    None => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ recovery: cannot determine dispatch type for {did} — preserving state"
                        );
                    }
                }
            }

            if dispatch_completed {
                finish_recovered_turn_mailbox(
                    shared,
                    provider,
                    channel_id,
                    "recovery_captured_full_response",
                )
                .await;
                clear_inflight_state(provider, state.channel_id);
            }
            continue;
        }
        if matches!(
            recovery_phase_after_output_scan(terminal_success_drained, false),
            RecoveryPhase::Done
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ✓ recovering completed turn for channel {}: output contains result after offset {}",
                state.channel_id,
                state.last_offset
            );
            let (recovered_session_id, recovered_usage) = output_path
                .as_deref()
                .map(|path| {
                    extract_turn_analytics_from_output(
                        path,
                        state.turn_start_offset.unwrap_or(state.last_offset),
                    )
                })
                .unwrap_or((None, None));
            // Deliver the result to Discord before clearing the inflight state
            let extracted = output_path
                .as_deref()
                .map(|p| extract_response_from_output(p, state.last_offset))
                .unwrap_or_default();
            let assistant_response = if extracted.trim().is_empty() {
                state.full_response.clone()
            } else {
                extracted
            };
            let final_text = if assistant_response.trim().is_empty() {
                "(복구됨 — 응답 텍스트 없음)".to_string()
            } else {
                super::formatting::format_for_discord_with_provider(&assistant_response, provider)
            };
            // #225 P1-1: Track relay success — only clear inflight if Discord delivery succeeds
            let relay_ok = relay_recovered_terminal_text_to_placeholder(
                http,
                shared,
                channel_id,
                current_msg_id,
                &final_text,
            )
            .await
            .delivered();

            if !should_advance_recovery_dispatch_after_relay(relay_ok) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ recovery: Discord relay failed before dispatch completion — preserving inflight for retry"
                );
                continue;
            }
            // Mark user message as completed only after Discord terminal delivery commits.
            let visible_outcome = complete_recovery_visible_turn(
                http,
                shared,
                provider,
                &state,
                false,
                "output_completed",
            )
            .await;
            if !visible_outcome.should_proceed() {
                // Reserved for future non-proceeding recovery outcomes.
                // A TUI quiescence timeout is not one: terminal delivery
                // evidence is authoritative for mailbox/inflight cleanup.
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    provider = %provider.as_str(),
                    channel = channel_id.get(),
                    "[{ts}] ⚠ recovery (output_completed) deferred by non-proceeding visible outcome"
                );
                continue;
            }

            // Complete the dispatch if this was an implementation/rework turn.
            // Review dispatches require the verdict flow (review_verdict.rs)
            // and must not be generically finalized here.
            // #225 P1-3: Use DB lookup for dispatch ID (text parsing fails in unified threads)
            let recovered_dispatch_id = parse_dispatch_id(&state.user_text)
                .or(lookup_pending_dispatch_for_thread(shared.api_port, state.channel_id).await);
            let role_binding = resolve_role_binding(channel_id, state.channel_name.as_deref());
            let duration_ms =
                recovered_turn_duration_ms(Some(state.started_at.as_str())).unwrap_or(0);
            let has_completion_evidence =
                if None::<&crate::db::Db>.is_some() || shared.pg_pool.is_some() {
                    // No user message (user_msg_id == 0) → no analytics row to
                    // key (`discord:<channel>:0` would be bogus); skip the
                    // analytics persist but still write the transcript.
                    if let Some(user_msg_id) = user_msg_id {
                        super::turn_bridge::persist_turn_analytics_row_with_handles(
                            None::<&crate::db::Db>,
                            shared.pg_pool.as_ref(),
                            provider,
                            channel_id,
                            user_msg_id,
                            role_binding.as_ref(),
                            recovered_dispatch_id
                                .as_deref()
                                .or(state.dispatch_id.as_deref()),
                            state.session_key.as_deref(),
                            recovered_session_id
                                .as_deref()
                                .or(state.session_id.as_deref()),
                            &state,
                            recovered_usage.unwrap_or_default(),
                            duration_ms,
                        );
                    }
                    persist_recovered_transcript(
                        None::<&crate::db::Db>,
                        shared.pg_pool.as_ref(),
                        provider,
                        &state,
                        recovered_dispatch_id
                            .as_deref()
                            .or(state.dispatch_id.as_deref()),
                        &assistant_response,
                    )
                    .await
                } else {
                    !assistant_response.trim().is_empty()
                };
            let completion_context = has_completion_evidence
                .then(|| serde_json::json!({ "agent_response_present": true }));
            let fallback_result = completion_context
                .clone()
                .map(|mut result| {
                    if let Some(obj) = result.as_object_mut() {
                        obj.insert(
                            "completion_source".to_string(),
                            serde_json::Value::String("recovery_output_db_fallback".to_string()),
                        );
                        obj.insert("needs_reconcile".to_string(), serde_json::Value::Bool(true));
                    }
                    result
                })
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "completion_source": "recovery_output_db_fallback",
                        "needs_reconcile": true,
                    })
                });
            let mut dispatch_completed = recovered_dispatch_id.is_none();
            if let Some(ref did) = recovered_dispatch_id {
                let dispatch_type = super::internal_api::lookup_dispatch_type(did)
                    .await
                    .ok()
                    .flatten();

                match dispatch_type.as_deref() {
                    Some("implementation") | Some("rework") => {
                        if !has_completion_evidence {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ recovery: refusing to complete work dispatch {did} without assistant response"
                            );
                        } else if let Some(engine) = &shared.policy.engine {
                            for attempt in 1..=3u8 {
                                match crate::dispatch::finalize_dispatch_with_backends(
                                    None::<&crate::db::Db>,
                                    engine,
                                    did,
                                    "recovery_output_completed",
                                    completion_context.as_ref(),
                                ) {
                                    Ok(_) => {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::info!(
                                            "  [{ts}] ✓ recovery: completed dispatch {did} via finalize_dispatch"
                                        );
                                        let _ =
                                            super::turn_bridge::queue_dispatch_followup_with_handles(
                                                shared.pg_pool.as_ref(),
                                                did,
                                                "recovery_output_completed",
                                            )
                                            .await;
                                        dispatch_completed = true;
                                        break;
                                    }
                                    Err(e) => {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::warn!(
                                            "  [{ts}] ⚠ recovery: finalize_dispatch failed for {did} (attempt {attempt}/3): {e}"
                                        );
                                        if attempt < 3 {
                                            // #2428 H5: exponential backoff + jitter.
                                            tokio::time::sleep(recovery_retry_backoff(u32::from(
                                                attempt,
                                            )))
                                            .await;
                                        }
                                    }
                                }
                            }
                            if !dispatch_completed {
                                dispatch_completed =
                                    super::turn_bridge::runtime_db_fallback_complete_with_result(
                                        did,
                                        &fallback_result,
                                    );
                                if dispatch_completed {
                                    let _ =
                                        super::turn_bridge::queue_dispatch_followup_with_handles(
                                            shared.pg_pool.as_ref(),
                                            did,
                                            "recovery_output_completed_fallback",
                                        )
                                        .await;
                                }
                            }
                        } else {
                            dispatch_completed =
                                super::turn_bridge::runtime_db_fallback_complete_with_result(
                                    did,
                                    &fallback_result,
                                );
                            if dispatch_completed {
                                let _ = super::turn_bridge::queue_dispatch_followup_with_handles(
                                    shared.pg_pool.as_ref(),
                                    did,
                                    "recovery_output_completed_runtime_fallback",
                                )
                                .await;
                            }
                        }
                        if !dispatch_completed {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ❌ recovery: dispatch {did} completion failed — preserving state for retry"
                            );
                        }
                    }
                    Some(_) => {
                        // Non-work dispatches (review, review-decision) need
                        // their own explicit API completion flow. Clear inflight
                        // but leave dispatch status untouched.
                        dispatch_completed = true;
                    }
                    None => {
                        // DB unavailable — cannot determine dispatch type.
                        // Preserve inflight state so the next recovery pass can retry.
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ recovery: cannot determine dispatch type for {did} — preserving state"
                        );
                    }
                }
            }

            // #225 P1-1: Only clear inflight if both dispatch completed AND relay succeeded.
            // If relay failed, preserve inflight for retry on next startup.
            if dispatch_completed {
                finish_recovered_turn_mailbox(
                    shared,
                    provider,
                    channel_id,
                    "recovery_output_completed",
                )
                .await;
                clear_inflight_state(provider, state.channel_id);
            }
            continue;
        }

        let tmux_ready_without_new_output = tmux_session_name.as_deref().map_or(false, |name| {
            !output_has_new_bytes
                && recovery_has_post_work_ready_evidence(&state)
                && inflight_or_legacy_tmux_ready_for_input(provider, &state, name, true)
        });

        if matches!(
            recovery_phase_after_output_scan(false, tmux_ready_without_new_output),
            RecoveryPhase::Done
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            // #2770: ready/idle is not terminal delivery evidence. If recovery
            // has neither captured text nor a recorded relay commit, preserve
            // the inflight so the pane-alive reattach path below can own it.
            if recovery_ready_without_output_already_delivered(&state) {
                tracing::info!(
                    "  [{ts}] ✓ clearing inflight turn for channel {}: tmux is ready for input and terminal delivery was already recorded after offset {}",
                    state.channel_id,
                    state.last_offset
                );
                finish_recovered_turn_mailbox(
                    shared,
                    provider,
                    channel_id,
                    "recovery_ready_without_output_already_delivered",
                )
                .await;
                clear_inflight_state(provider, state.channel_id);
                continue;
            }
            if recovery_ready_without_output_has_captured_response(&state) {
                tracing::info!(
                    "  [{ts}] ✓ clearing inflight turn for channel {}: tmux is ready for input and captured output is idle after offset {}",
                    state.channel_id,
                    state.last_offset
                );
                let final_text = super::formatting::format_for_discord_with_provider(
                    &state.full_response,
                    provider,
                );
                let outcome =
                    relay_recovery_terminal_notice(http, shared, &state, &final_text).await;
                // #3293: tmux_alive=true — budget force-clear forbidden here
                // (pane-alive invariant); only a permanent verdict clears.
                dispose_recovery_relay_outcome(
                    shared,
                    provider,
                    &state,
                    outcome,
                    true,
                    "recovery_ready_without_output",
                    "ready_without_output",
                    &state.full_response,
                    false,
                )
                .await;
                continue;
            }
            tracing::warn!(
                "  [{ts}] ⚠ recovery: deferring ready-without-output completion for channel {} because no captured assistant response or terminal delivery evidence exists",
                state.channel_id
            );
        }

        let can_recover = tmux_session_name
            .as_deref()
            .map_or(false, |name| tmux_has_session_with_retry(name));

        if matches!(
            recovery_phase_after_tmux_probe(can_recover, None),
            RecoveryPhase::Done
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            // Even without a live tmux session, the output file may contain
            // response data. Try extracting from the full file first, then
            // fall back to saved partial response.
            let extracted_full = output_path
                .as_deref()
                .map(|p| extract_response_from_output(p, 0))
                .unwrap_or_default();
            let best_response = if !extracted_full.trim().is_empty() {
                extracted_full
            } else {
                state.full_response.clone()
            };
            let stale_text = interrupted_recovery_message(&state, &best_response);
            let death_diag = tmux_session_name
                .as_deref()
                .and_then(|name| build_tmux_death_diagnostic(name, output_path.as_deref()));
            if let Some(ref diag) = death_diag {
                tracing::info!(
                    "  [{ts}] ⚠ cannot recover inflight turn for channel {}: tmux session missing (response len: {}, {diag})",
                    state.channel_id,
                    best_response.len()
                );
            } else {
                tracing::info!(
                    "  [{ts}] ⚠ cannot recover inflight turn for channel {}: tmux session missing (response len: {})",
                    state.channel_id,
                    best_response.len()
                );
            }
            let outcome = relay_recovery_terminal_notice(http, shared, &state, &stale_text).await;
            if let Some(ref sk) = state.session_key {
                crate::services::termination_audit::record_termination_with_handles(
                    None::<&crate::db::Db>,
                    shared.pg_pool.as_ref(),
                    sk,
                    state.dispatch_id.as_deref(),
                    "recovery",
                    "restart_session_missing",
                    Some("tmux session missing after restart"),
                    death_diag.as_deref(),
                    Some(state.last_offset),
                    Some(false),
                );
            }
            save_missing_session_handoff(provider, &state, &best_response);
            // Handoff already saved above for every outcome (last arg).
            dispose_recovery_relay_outcome(
                shared,
                provider,
                &state,
                outcome,
                false,
                "recovery_missing_tmux",
                "missing_tmux",
                &best_response,
                true,
            )
            .await;
            continue;
        }

        let Some(tmux_session_name) = tmux_session_name else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ clearing inflight turn for channel {}: tmux session name missing",
                state.channel_id
            );
            let text = stale_inflight_message("tmux session name missing during recovery");
            let outcome = relay_recovery_terminal_notice(http, shared, &state, &text).await;
            // #3297 finding 4: past the can_recover gate tmux absence is NOT
            // established — tmux_alive=true forbids budget force-clear here.
            dispose_recovery_relay_outcome(
                shared,
                provider,
                &state,
                outcome,
                true,
                "recovery_missing_tmux_name",
                "missing_tmux_name",
                &state.full_response,
                false,
            )
            .await;
            continue;
        };
        let Some(output_path) = output_path else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ clearing inflight turn for channel {}: output path missing",
                state.channel_id
            );
            let text = stale_inflight_message("output path missing during recovery");
            let outcome = relay_recovery_terminal_notice(http, shared, &state, &text).await;
            // #3297 finding 4: tmux session existence was confirmed above
            // (can_recover consumed the missing-tmux rows) — tmux_alive=true,
            // so the budget can never clear a possibly-live pane here.
            dispose_recovery_relay_outcome(
                shared,
                provider,
                &state,
                outcome,
                true,
                "recovery_missing_output_path",
                "missing_output_path",
                &state.full_response,
                false,
            )
            .await;
            continue;
        };
        let input_fifo_path = match recovery_input_fifo_for_runtime(runtime_kind, input_fifo_path) {
            Ok(path) => path,
            Err(reason) => {
                // #2235: when the inflight row was written without a stamped
                // `runtime_kind` (legacy pre-v8 row, hook-endpoint race, or a
                // future variant this binary doesn't recognize),
                // `runtime_kind_for_recovery` had to guess. If the guess
                // requires a FIFO that the row never carried, surfacing a
                // user-visible "input fifo path missing" notice misleads the
                // operator — the right thing is to skip recovery silently and
                // let the next turn re-establish state from scratch.
                let runtime_kind_was_inferred = state.runtime_kind.is_none();
                let ts = chrono::Local::now().format("%H:%M:%S");
                if runtime_kind_was_inferred {
                    tracing::debug!(
                        "  [{ts}] ↩ inflight recovery silent-skip for channel {}: runtime_kind unknown/missing on-disk, inferred {} requires FIFO but row carries none",
                        state.channel_id,
                        runtime_kind.as_str()
                    );
                    finish_recovered_turn_mailbox(
                        shared,
                        provider,
                        channel_id,
                        "recovery_runtime_kind_missing_skip",
                    )
                    .await;
                    clear_inflight_state(provider, state.channel_id);
                    continue;
                }
                tracing::info!(
                    "  [{ts}] ⚠ clearing inflight turn for channel {}: input fifo path missing (runtime={})",
                    state.channel_id,
                    runtime_kind.as_str()
                );
                let text = stale_inflight_message(reason);
                let outcome = relay_recovery_terminal_notice(http, shared, &state, &text).await;
                // #3297 finding 4: tmux existence already confirmed —
                // tmux_alive=true (budget clear forbidden; permanent only).
                dispose_recovery_relay_outcome(
                    shared,
                    provider,
                    &state,
                    outcome,
                    true,
                    "recovery_missing_input_fifo",
                    "missing_input_fifo",
                    &state.full_response,
                    false,
                )
                .await;
                continue;
            }
        };

        if recovery_terminal_delivery_already_committed(&state) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %provider.as_str(),
                channel = state.channel_id,
                user_msg_id = state.user_msg_id,
                "  [{ts}] ✓ recovery: clearing delivered inflight before watcher re-register; terminal response already reached Discord"
            );
            finish_recovered_turn_mailbox(
                shared,
                provider,
                channel_id,
                "recovery_terminal_delivery_already_committed",
            )
            .await;
            clear_inflight_state(provider, state.channel_id);
            continue;
        }

        // If the tmux pane is alive, skip the recovery reader entirely. The idle
        // session gets a watcher immediately rather than deferring to
        // restore_tmux_watchers() — that ~50s gap raced and lost the response.
        let pane_alive = tmux_session_alive_with_retry(&tmux_session_name);
        if matches!(
            recovery_phase_after_tmux_probe(true, Some(pane_alive)),
            RecoveryPhase::WatcherReattach
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ inflight recovery: pane alive for channel {}, spawning watcher immediately",
                state.channel_id
            );
            // Register session in-memory so handlers can find it.
            let effective_channel_name = channel_name.clone().or_else(|| {
                crate::services::provider::parse_provider_and_channel_from_tmux_name(
                    &tmux_session_name,
                )
                .map(|(_, ch)| ch)
            });
            {
                let persisted_session_path = load_last_session_path(
                    shared.pg_pool.as_ref(),
                    &shared.token_hash,
                    channel_id.get(),
                );
                let recovery_adk_cwd = match recovery_spawn_adk_cwd(&state, persisted_session_path)
                {
                    Ok(path) => path,
                    Err(error) => {
                        let dispatch_id = state
                            .dispatch_id
                            .clone()
                            .or_else(|| parse_dispatch_id(&state.user_text));
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::error!("  [{ts}] {error}; main-workspace fallback blocked");
                        crate::services::observability::emit_recovery_fired(
                            provider.as_str(),
                            state.channel_id,
                            dispatch_id.as_deref(),
                            state.session_key.as_deref(),
                            "worktree_missing_main_fallback_blocked",
                        );
                        emit_recovery_quality_event(
                            provider,
                            state.channel_id,
                            dispatch_id.as_deref(),
                            state.session_key.as_deref(),
                            "worktree_missing_main_fallback_blocked",
                        );
                        let relay_ok = relay_recovered_terminal_text_to_placeholder(
                            http,
                            shared,
                            channel_id,
                            current_msg_id,
                            &format!("❌ {error}\nmain workspace fallback blocked."),
                        )
                        .await
                        .delivered();
                        if should_advance_recovery_dispatch_after_relay(relay_ok) {
                            super::turn_bridge::fail_dispatch_with_retry(
                                shared.api_port,
                                dispatch_id.as_deref(),
                                &error,
                            )
                            .await;
                            super::restart_report::clear_restart_report(provider, state.channel_id);
                            clear_inflight_state(provider, state.channel_id);
                        } else {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ recovery: worktree error relay failed before dispatch failure — preserving inflight for retry"
                            );
                        }
                        continue;
                    }
                };
                let saved_remote = load_last_remote_profile(
                    shared.pg_pool.as_ref(),
                    &shared.token_hash,
                    channel_id.get(),
                );
                let mut data = shared.core.lock().await;
                let session = data
                    .sessions
                    .entry(channel_id)
                    .or_insert_with(|| DiscordSession {
                        session_id: state.session_id.clone(),
                        memento_context_loaded: false,
                        memento_reflected: false,
                        current_path: None,
                        history: Vec::new(),
                        pending_uploads: Vec::new(),
                        cleared: false,
                        remote_profile_name: saved_remote,
                        channel_id: Some(channel_id.get()),
                        channel_name: effective_channel_name.clone(),
                        category_name: None,
                        last_active: tokio::time::Instant::now(),
                        worktree: None,
                        born_generation: super::runtime_store::load_generation(),
                    });
                session.channel_id = Some(channel_id.get());
                session.last_active = tokio::time::Instant::now();
                if session.current_path.is_none() {
                    session.current_path = recovery_adk_cwd;
                }
                if session.channel_name.is_none() {
                    session.channel_name = effective_channel_name;
                }
                restore_recovered_session_worktree(session, &state);
            }

            let finish_mailbox_on_completion =
                reregister_active_turn_from_inflight(shared, &state).await;

            // #2795 — codex_tui writes its rollout transcript directly to
            // `~/.codex/sessions/...`; the inflight's stored `output_path` is
            // the AgentDesk-side relay JSONL which may not exist on disk yet
            // when dcserver quick-exits mid-turn (e.g. agent ran deploy from
            // inside its own turn). Without a falling-back lookup the
            // `metadata` check below silently fails and recovery never spawns
            // a watcher, leaving the live codex pane permanently un-relayed.
            // Resolve the actual rollout via the inflight `session_id` and
            // persist the corrected path so subsequent restarts also find it.
            let mut output_path = output_path;
            if std::fs::metadata(&output_path).is_err()
                && matches!(
                    state.runtime_kind,
                    Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui)
                )
            {
                if let Some(session_id) = state.session_id.as_deref() {
                    if let Some(rollout) =
                        crate::services::codex_tui::rollout_tail::find_rollout_by_session_id(
                            session_id,
                        )
                    {
                        let rollout_str = rollout.display().to_string();
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ↻ recovery: codex rollout fallback for channel {} — {} → {}",
                            state.channel_id,
                            output_path,
                            rollout_str
                        );
                        output_path = rollout_str.clone();
                        let mut patched = state.clone();
                        patched.output_path = Some(rollout_str);
                        let _ = inflight::save_inflight_state(&patched);
                    }
                }
            }

            // Immediately spawn watcher to avoid race condition.
            if std::fs::metadata(&output_path).is_ok() {
                let (initial_offset, current_len, truncated) =
                    recovery_watcher_start_offset(&output_path, state.last_offset);
                let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let resume_offset = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
                let pause_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                let turn_delivered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let last_heartbeat_ts_ms = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
                    super::tmux_watcher_now_ms(),
                ));
                let handle = TmuxWatcherHandle {
                    tmux_session_name: tmux_session_name.clone(),
                    output_path: output_path.clone(),
                    paused: paused.clone(),
                    resume_offset: resume_offset.clone(),
                    cancel: cancel.clone(),
                    pause_epoch: pause_epoch.clone(),
                    turn_delivered: turn_delivered.clone(),
                    last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
                };
                let watcher_claimed = {
                    #[cfg(unix)]
                    {
                        let claim = super::tmux::claim_or_reuse_watcher(
                            &shared.tmux_watchers,
                            channel_id,
                            handle,
                            provider,
                            "inflight_recovery",
                        );
                        claim.should_spawn()
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = handle;
                        false
                    }
                };
                if watcher_claimed {
                    let ts2 = chrono::Local::now().format("%H:%M:%S");
                    if truncated {
                        tracing::info!(
                            "  [{ts2}] ↻ recovery: output truncated for #{} (saved offset {}, file len {}), restarting watcher from 0",
                            tmux_session_name,
                            state.last_offset,
                            current_len
                        );
                    }
                    tracing::info!(
                        "  [{ts2}] 👁 recovery: spawned watcher for #{} at offset {}",
                        tmux_session_name,
                        initial_offset
                    );
                    #[cfg(unix)]
                    {
                        let restored_turn = super::tmux::restored_watcher_turn_from_inflight(
                            &state,
                            &tmux_session_name,
                            finish_mailbox_on_completion,
                        );
                        shared.record_tmux_watcher_reconnect(channel_id);
                        super::task_supervisor::spawn_observed_tmux_watcher(
                            "recovery_restore_inflight_tmux_output_watcher_with_restore",
                            shared.clone(),
                            tmux_session_name.clone(),
                            cancel.clone(),
                            super::tmux::tmux_output_watcher_with_restore(
                                channel_id,
                                http.clone(),
                                shared.clone(),
                                output_path.clone(),
                                tmux_session_name.clone(),
                                initial_offset,
                                cancel,
                                paused,
                                resume_offset,
                                pause_epoch,
                                turn_delivered,
                                last_heartbeat_ts_ms,
                                restored_turn,
                            ),
                        );
                    }
                }
            }

            // Keep the inflight state until the watcher either relays the final response or
            // triggers watcher-death handoff. Clearing it here breaks the handoff path if the
            // recovered tmux session dies before producing a result.
            continue;
        }

        shared
            .restart
            .recovering_channels
            .insert(channel_id, std::time::Instant::now());

        let persisted_session_path = load_last_session_path(
            shared.pg_pool.as_ref(),
            &shared.token_hash,
            channel_id.get(),
        );
        let recovery_adk_cwd = match recovery_spawn_adk_cwd(&state, persisted_session_path) {
            Ok(path) => path,
            Err(error) => {
                let dispatch_id = state
                    .dispatch_id
                    .clone()
                    .or_else(|| parse_dispatch_id(&state.user_text));
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::error!("  [{ts}] {error}; main-workspace fallback blocked");
                crate::services::observability::emit_recovery_fired(
                    provider.as_str(),
                    state.channel_id,
                    dispatch_id.as_deref(),
                    state.session_key.as_deref(),
                    "worktree_missing_main_fallback_blocked",
                );
                emit_recovery_quality_event(
                    provider,
                    state.channel_id,
                    dispatch_id.as_deref(),
                    state.session_key.as_deref(),
                    "worktree_missing_main_fallback_blocked",
                );
                let relay_ok = relay_recovered_terminal_text_to_placeholder(
                    http,
                    shared,
                    channel_id,
                    current_msg_id,
                    &format!("❌ {error}\nmain workspace fallback blocked."),
                )
                .await
                .delivered();
                if should_advance_recovery_dispatch_after_relay(relay_ok) {
                    super::turn_bridge::fail_dispatch_with_retry(
                        shared.api_port,
                        dispatch_id.as_deref(),
                        &error,
                    )
                    .await;
                    super::restart_report::clear_restart_report(provider, state.channel_id);
                    clear_inflight_state(provider, state.channel_id);
                } else {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ recovery: worktree error relay failed before dispatch failure — preserving inflight for retry"
                    );
                }
                continue;
            }
        };
        let saved_remote = load_last_remote_profile(
            shared.pg_pool.as_ref(),
            &shared.token_hash,
            channel_id.get(),
        );

        let cancel_token = Arc::new(CancelToken::new());
        super::turn_bridge::bind_cancel_token_tmux_runtime(
            provider,
            &cancel_token,
            &tmux_session_name,
            "recovery kickoff",
        );

        {
            let mut data = shared.core.lock().await;
            let session = data
                .sessions
                .entry(channel_id)
                .or_insert_with(|| DiscordSession {
                    session_id: state.session_id.clone(),
                    memento_context_loaded: false,
                    memento_reflected: false,
                    current_path: None,
                    history: Vec::new(),
                    pending_uploads: Vec::new(),
                    cleared: false,
                    remote_profile_name: saved_remote.clone(),
                    channel_id: Some(channel_id.get()),
                    channel_name: channel_name.clone(),
                    category_name: None,
                    last_active: tokio::time::Instant::now(),
                    worktree: None,

                    born_generation: super::runtime_store::load_generation(),
                });
            session.channel_id = Some(channel_id.get());
            session.last_active = tokio::time::Instant::now();
            if session.current_path.is_none() {
                session.current_path = recovery_adk_cwd.clone();
            }
            if session.channel_name.is_none() {
                session.channel_name = channel_name.clone();
            }
            if session.remote_profile_name.is_none() {
                session.remote_profile_name = saved_remote;
            }
            restore_recovered_session_worktree(session, &state);
        }

        mailbox_recovery_kickoff(
            shared,
            channel_id,
            cancel_token.clone(),
            UserId::new(state.request_owner_user_id),
            // user_msg_id == 0 (TUI-direct turn) → no active user message to
            // bind; `optional_message_id` yields None instead of panicking.
            user_msg_id,
        )
        .await;

        let adk_session_key = build_adk_session_key(shared, channel_id, provider).await;
        let adk_session_name = channel_name.clone();
        let adk_session_info = derive_adk_session_info(
            Some(&state.user_text),
            channel_name.as_deref(),
            recovery_adk_cwd.as_deref(),
        );
        let role_binding = resolve_role_binding(channel_id, channel_name.as_deref());
        let adk_thread_channel_id = adk_session_name
            .as_deref()
            .and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name);
        post_adk_session_status(
            adk_session_key.as_deref(),
            adk_session_name.as_deref(),
            Some(provider.as_str()),
            "working",
            provider,
            Some(&adk_session_info),
            None,
            recovery_adk_cwd.as_deref(),
            parse_dispatch_id(&state.user_text)
                .or(lookup_pending_dispatch_for_thread(shared.api_port, channel_id.get()).await)
                .as_deref(),
            adk_thread_channel_id,
            Some(channel_id),
            role_binding
                .as_ref()
                .map(|binding| binding.role_id.as_str()),
            shared.api_port,
        )
        .await;

        let (tx, rx) = mpsc::channel();
        let cancel_for_reader = cancel_token.clone();
        let output_for_reader = output_path.clone();
        let input_for_reader = input_fifo_path.clone();
        let tmux_for_reader = tmux_session_name.clone();
        let start_offset = state.last_offset;
        let recovery_session_id = state.session_id.clone();
        let runtime_kind_for_reader = runtime_kind;
        let retry_channel_id = channel_id.get();
        let provider_for_reader = provider.clone();
        std::thread::spawn(move || {
            match crate::services::session_backend::read_output_file_until_result(
                &output_for_reader,
                start_offset,
                tx.clone(),
                Some(cancel_for_reader),
                crate::services::provider::SessionProbe::tmux(
                    tmux_for_reader.clone(),
                    provider_for_reader,
                ),
            ) {
                Ok(ReadOutputResult::Completed { offset })
                | Ok(ReadOutputResult::Cancelled { offset }) => {
                    let _ = tx.send(StreamMessage::RuntimeReady {
                        handoff: runtime_handoff_for_recovery(
                            runtime_kind_for_reader,
                            output_for_reader,
                            input_for_reader,
                            tmux_for_reader,
                            recovery_session_id,
                            offset,
                        ),
                    });
                }
                Ok(ReadOutputResult::SessionDied { offset }) => {
                    // Check if tmux pane is actually alive — dcserver restart
                    // may cause SessionDied because no new output arrived, but
                    // the Claude CLI process could still be idle (waiting for input).
                    let pane_alive = tmux_session_alive_with_retry(&tmux_for_reader);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    if pane_alive {
                        // Session is alive but idle — hand off to watcher instead of retrying
                        tracing::warn!(
                            "  [{ts}] ↻ Recovery: session idle but pane alive — handing off to watcher (channel {})",
                            retry_channel_id
                        );
                        let _ = tx.send(StreamMessage::RuntimeReady {
                            handoff: runtime_handoff_for_recovery(
                                runtime_kind_for_reader,
                                output_for_reader,
                                input_for_reader,
                                tmux_for_reader,
                                recovery_session_id,
                                offset,
                            ),
                        });
                    } else {
                        // Session truly died during restart recovery. Fall back
                        // to the generic auto-retry path so restart handling
                        // does not get a special handoff-only branch.
                        tracing::warn!(
                            "  [{ts}] ↻ Recovery: session died, signaling generic auto-retry (channel {})",
                            retry_channel_id
                        );
                        let _ = tx.send(StreamMessage::Done {
                            result: "__session_died_retry__".to_string(),
                            session_id: recovery_session_id,
                        });
                    }
                }
                Err(e) => {
                    let _ = tx.send(StreamMessage::Error {
                        message: e,
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: None,
                    });
                }
            }
        });

        let recovery_dispatch_id = parse_dispatch_id(&state.user_text)
            .or(lookup_pending_dispatch_for_thread(shared.api_port, channel_id.get()).await);
        let recovery_dispatch_kind =
            lookup_turn_finished_dispatch_kind(recovery_dispatch_id.as_deref()).await;
        // Backfill session_key/dispatch_id on inflight state for long-turn detection ([L]).
        let mut state = state;
        state.session_key = state.session_key.or_else(|| adk_session_key.clone());
        state.dispatch_id = state.dispatch_id.or_else(|| recovery_dispatch_id.clone());
        // #3166: read the real configured thresholds (e.g.
        // `context_compact_percent_claude`) instead of `ContextThresholds::default()`
        // so the recovered turn's status panel reflects the user-set auto-compact
        // percent, matching the live launch paths (intake_turn/headless_turn).
        // This is the display value; the spawn-side `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`
        // env is exported by the launch script (claude_tui/session.rs, #3166).
        let recovery_compact_percent =
            super::adk_session::fetch_context_thresholds(shared.api_port)
                .await
                .compact_pct_for(&provider);
        spawn_turn_bridge(
            shared.clone(),
            cancel_token,
            rx,
            TurnBridgeContext {
                provider: provider.clone(),
                gateway: Arc::new(DiscordGateway::new(
                    http.clone(),
                    shared.clone(),
                    provider.clone(),
                    None,
                )),
                channel_id,
                user_msg_id,
                user_text_owned: state.user_text.clone(),
                request_owner_name: String::new(),
                role_binding,
                adk_session_key,
                adk_session_name,
                adk_session_info: Some(adk_session_info),
                adk_cwd: recovery_adk_cwd.clone(),
                dispatch_id: recovery_dispatch_id,
                dispatch_kind: recovery_dispatch_kind,
                memory_recall_usage: crate::services::memory::TokenUsage::default(),
                context_window_tokens: provider.default_context_window(),
                context_compact_percent: recovery_compact_percent,
                current_msg_id,
                response_sent_offset: state.response_sent_offset,
                full_response: state.full_response.clone(),
                tmux_last_offset: Some(state.last_offset),
                new_session_id: state.session_id.clone(),
                defer_watcher_resume: false,
                reuse_status_panel_message: true,
                completion_tx: None,
                is_external_input_tui_direct: false, // #3089 A6b: recovery is not external-input
                inflight_state: state,
            },
        );
    }
}

/// #896: Outcome of a successful [`rebind_inflight_for_channel`] call.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RebindOutcome {
    pub tmux_session: String,
    pub channel_id: u64,
    pub initial_offset: u64,
    /// `true` when a tmux watcher was spawned by this call. On unix this is
    /// always true on success. On non-unix builds watcher spawning is a
    /// no-op, so this reads `false` even though the inflight file was
    /// written.
    pub watcher_spawned: bool,
    /// #897 P2 #2 — `true` when a pre-existing watcher handle was present
    /// for this channel and has been cancelled + replaced by the freshly
    /// spawned one. Operators use this to distinguish a clean vacant claim
    /// from a zombie-slot recovery, which is the common case where an old
    /// watcher kept its DashMap entry after its tmux exited.
    pub watcher_replaced: bool,
}

/// #896: Errors from [`rebind_inflight_for_channel`]. Map 1:1 to HTTP status
/// codes in the `/api/inflight/rebind` handler.
#[derive(Debug)]
pub enum RebindError {
    /// Target tmux session is not alive — nothing to rebind to. 404.
    TmuxNotAlive { tmux_session: String },
    /// An inflight state already exists for this channel. Caller must clear
    /// it (force-kill or natural completion) before rebinding. 409.
    InflightAlreadyExists,
    /// The tmux pane is still writing to a deleted or replaced output fd, so
    /// rebinding the pathname would silently follow the wrong file. 409.
    StaleOutputPath {
        tmux_session: String,
        output_path: String,
        live_fd: String,
        live_inode: Option<u64>,
        live_path: String,
    },
    /// Channel is not bound to the requested provider in the role-map. 400.
    ChannelNotBound,
    /// `tmux_session` not provided and no in-memory session supplies a
    /// channel_name — cannot derive the canonical tmux session name. 400.
    ChannelNameMissing,
    /// Unrecoverable internal error (inflight write, lock poisoning, etc.). 500.
    Internal(String),
}

impl std::fmt::Display for RebindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TmuxNotAlive { tmux_session } => {
                write!(f, "tmux session not alive: {tmux_session}")
            }
            Self::InflightAlreadyExists => {
                write!(f, "inflight state already exists for this channel")
            }
            Self::StaleOutputPath {
                tmux_session,
                output_path,
                live_fd,
                live_inode,
                live_path,
            } => {
                write!(
                    f,
                    "StaleOutputPath: tmux session {tmux_session} still writes to fd {live_fd}"
                )?;
                if let Some(inode) = live_inode {
                    write!(f, " (inode {inode})")?;
                }
                write!(
                    f,
                    " via {live_path}; refusing to rebind pathname {output_path}"
                )
            }
            Self::ChannelNotBound => write!(f, "channel is not bound for this provider"),
            Self::ChannelNameMissing => write!(
                f,
                "channel name missing — pass tmux_session or pre-register the channel"
            ),
            Self::Internal(msg) => write!(f, "internal: {msg}"),
        }
    }
}

/// #896: Rebind a live tmux session to a freshly-created inflight state and
/// (re)spawn the output watcher — recovers orphan states whose tmux is alive
/// but whose inflight JSON was cleared, leaving output with no relay path.
///
/// Preconditions (enforced, typed error on violation): tmux session alive
/// (absent ⇒ force-kill + restart instead); no existing inflight for the
/// channel (caller clears first); channel role-map-bound to the provider.
///
/// Side effects on success: writes the provider/channel inflight JSON with
/// `last_offset` = current output size (only NEW output is relayed —
/// retroactive emission is out of scope); registers/refreshes the
/// `DiscordSession`; spawns a `tmux_output_watcher` via the single-watcher
/// claim policy (an existing live owner is reused, `watcher_spawned=false`,
/// and still picks up the new inflight — not an error).
pub(crate) async fn rebind_inflight_for_channel(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_override: Option<String>,
) -> Result<RebindOutcome, RebindError> {
    let discord_channel_id = ChannelId::new(channel_id);

    // Preflight existence check — fast 409 before walking the validation /
    // tmux-liveness path. Advisory only; the AUTHORITATIVE guard is the atomic
    // `save_inflight_state_create_new` below (`O_CREAT | O_EXCL`), so a live turn
    // winning the race between here and the write cannot be clobbered.
    let existing_inflight = match super::inflight::load_inflight_state(provider, channel_id) {
        Some(existing) => match recovery_phase_for_existing_inflight_rebind(&existing) {
            RecoveryPhase::WatcherReattach => {
                super::inflight::clear_inflight_state(provider, channel_id);
                None
            }
            RecoveryPhase::InflightRestore => Some(existing),
            RecoveryPhase::Pending | RecoveryPhase::Done => {
                return Err(RebindError::InflightAlreadyExists);
            }
        },
        None => None,
    };
    let resuming_existing_inflight = existing_inflight.is_some();

    if resuming_existing_inflight {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ♻ rebind resuming existing inflight turn for channel {} without overwriting canonical state",
            channel_id
        );
    }

    let existing_session_id = existing_inflight
        .as_ref()
        .and_then(|state| state.session_id.clone());
    let existing_saved_output_path = existing_inflight
        .as_ref()
        .and_then(|state| state.output_path.clone());

    // Resolve tmux session name + channel name from the request, falling back
    // to the in-memory session map when no override is provided.
    let (tmux_session_name, channel_name) = match tmux_session_override {
        Some(name) => {
            let ch_name =
                crate::services::provider::parse_provider_and_channel_from_tmux_name(&name)
                    .map(|(_, ch)| ch);
            (name, ch_name)
        }
        None => {
            let ch_name = {
                let data = shared.core.lock().await;
                data.sessions
                    .get(&discord_channel_id)
                    .and_then(|s| s.channel_name.clone())
            };
            let ch_name = match ch_name {
                Some(n) => n,
                None => return Err(RebindError::ChannelNameMissing),
            };
            let tmux = provider.build_tmux_session_name(&ch_name);
            (tmux, Some(ch_name))
        }
    };

    if !tmux_session_alive_with_retry(&tmux_session_name) {
        return Err(RebindError::TmuxNotAlive {
            tmux_session: tmux_session_name,
        });
    }

    // Validate provider↔channel binding against the settings snapshot,
    // mirroring what `restore_inflight_turns` requires for watcher revival.
    let settings_snapshot = shared.settings.read().await.clone();
    let is_dm = matches!(
        discord_channel_id.to_channel(http).await,
        Ok(serenity::model::channel::Channel::Private(_))
    );
    let (allowlist_channel_id, provider_channel_name) =
        if let Some((pid, pname)) = super::resolve_thread_parent(http, discord_channel_id).await {
            (pid, pname.or(channel_name.clone()))
        } else {
            (discord_channel_id, channel_name.clone())
        };
    if validate_bot_channel_routing_with_provider_channel(
        &settings_snapshot,
        provider,
        allowlist_channel_id,
        channel_name.as_deref(),
        provider_channel_name.as_deref(),
        is_dm,
    )
    .is_err()
    {
        return Err(RebindError::ChannelNotBound);
    }

    let (default_output_path, input_fifo) = tmux_runtime_paths(&tmux_session_name);
    let (output_path, synthetic_initial_offset) = {
        #[cfg(unix)]
        {
            match detect_live_tmux_output_path(&tmux_session_name, &default_output_path) {
                Ok(Some(detected)) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ♻ rebind adopted live tmux output path for {}: {} -> {} (offset {})",
                        tmux_session_name,
                        default_output_path,
                        detected.path,
                        detected.initial_offset
                    );
                    (detected.path, detected.initial_offset)
                }
                Ok(None) => {
                    let synthetic_initial_offset = std::fs::metadata(&default_output_path)
                        .map(|m| m.len())
                        .unwrap_or(0);
                    (default_output_path.clone(), synthetic_initial_offset)
                }
                Err(stale) => {
                    return Err(RebindError::StaleOutputPath {
                        tmux_session: tmux_session_name.clone(),
                        output_path: default_output_path.clone(),
                        live_fd: stale.fd,
                        live_inode: stale.inode,
                        live_path: stale.raw_path,
                    });
                }
            }
        }
        #[cfg(not(unix))]
        {
            let synthetic_initial_offset = std::fs::metadata(&default_output_path)
                .map(|m| m.len())
                .unwrap_or(0);
            (default_output_path.clone(), synthetic_initial_offset)
        }
    };

    let initial_offset = if let Some(existing) = existing_inflight.as_ref() {
        let (resume_offset, current_len, truncated) =
            recovery_watcher_start_offset(&output_path, existing.last_offset);
        if truncated {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ rebind restarting existing inflight watcher from 0 for {} (saved offset {}, file len {})",
                tmux_session_name,
                existing.last_offset,
                current_len
            );
        }
        if existing_saved_output_path.as_deref() != Some(output_path.as_str()) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ♻ rebind watcher adopted live output path for existing inflight {}: {:?} -> {}",
                tmux_session_name,
                existing_saved_output_path,
                output_path
            );
        }
        resume_offset
    } else {
        synthetic_initial_offset
    };

    let recovered_state_for_session = if let Some(existing) = existing_inflight.clone() {
        existing
    } else {
        // Build and persist the new inflight state. No request_owner / msg_ids
        // apply because this recovery has no originating Discord message.
        //
        // #897 counter-model re-review (round 2): flag this as `rebind_origin`
        // so routing / persistence code that keys off "is there a live
        // foreground turn" treats it as absent. This synthetic state exists only
        // to expose a recovered tmux session through inflight APIs; it must not
        // masquerade as a user-authored Discord turn.
        let mut state = super::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            channel_name.clone(),
            0, // request_owner_user_id — no originating Discord user
            0, // user_msg_id
            0, // current_msg_id (placeholder)
            String::from("/api/inflight/rebind"),
            None, // session_id
            Some(tmux_session_name.clone()),
            Some(output_path.clone()),
            Some(input_fifo.clone()),
            initial_offset,
        );
        state.rebind_origin = true;
        // #2161 Part 2 / #2285 adoption: this synthetic inflight is born when
        // `POST /api/inflight/rebind` adopts a tmux session the operator
        // launched outside AgentDesk (e.g. `tmux new -s <expected>` + run
        // provider manually). Tag as `ExternalAdopted` so audit logs and
        // monitoring surfaces can distinguish "AgentDesk-launched" from
        // "AgentDesk-discovered" sessions. The session-bound relay (epic
        // #2285 E1–E5) routes both identically — this is pure audit
        // metadata.
        state.turn_source = super::inflight::TurnSource::ExternalAdopted;

        // Atomic create-or-fail: if a legitimate turn created its inflight file
        // between the preflight check above and this point, the write fails
        // with `AlreadyExists` and we return 409. Without this guard the
        // synthetic rebind state (user_msg_id=0, placeholder ids zeroed) would
        // overwrite the real turn's canonical state and break its completion
        // path — the exact race the #897 P2 #1 review flagged.
        match super::inflight::save_inflight_state_create_new(&state) {
            Ok(()) => {}
            Err(super::inflight::CreateNewInflightError::AlreadyExists) => {
                return Err(RebindError::InflightAlreadyExists);
            }
            Err(super::inflight::CreateNewInflightError::Internal(msg)) => {
                return Err(RebindError::Internal(msg));
            }
        }
        state
    };
    forget_completion_footer_for_recovery_takeover(discord_channel_id);

    // Register / refresh the in-memory session so downstream handlers can
    // locate this channel after the rebind.
    {
        let mut data = shared.core.lock().await;
        let session = data
            .sessions
            .entry(discord_channel_id)
            .or_insert_with(|| DiscordSession {
                session_id: existing_session_id.clone(),
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                remote_profile_name: None,
                channel_id: Some(channel_id),
                channel_name: channel_name.clone(),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: super::runtime_store::load_generation(),
            });
        session.channel_id = Some(channel_id);
        session.last_active = tokio::time::Instant::now();
        if session.channel_name.is_none() {
            session.channel_name = channel_name.clone();
        }
        restore_recovered_session_worktree(session, &recovered_state_for_session);
    }

    // #1135: claim with the single-watcher policy. A live watcher for this
    // same tmux session is reused; a cancelled same-session handle or a
    // different-session channel incumbent is replaced so recovery is not
    // blocked by stale registry state.
    let (watcher_spawned, watcher_replaced) = {
        #[cfg(unix)]
        {
            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let resume_offset = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
            let pause_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let turn_delivered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let last_heartbeat_ts_ms = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
                super::tmux_watcher_now_ms(),
            ));
            let handle = TmuxWatcherHandle {
                tmux_session_name: tmux_session_name.clone(),
                output_path: output_path.clone(),
                paused: paused.clone(),
                resume_offset: resume_offset.clone(),
                cancel: cancel.clone(),
                pause_epoch: pause_epoch.clone(),
                turn_delivered: turn_delivered.clone(),
                last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
            };
            // `claim_or_reuse_watcher` reuses a live watcher for the same
            // tmux session and only spawns when it claimed or replaced a
            // stale/different-session slot.
            let claim = super::tmux::claim_or_reuse_watcher(
                &shared.tmux_watchers,
                discord_channel_id,
                handle,
                provider,
                "recovery_restore_inflight",
            );
            if claim.should_spawn() {
                shared.record_tmux_watcher_reconnect(discord_channel_id);
                super::task_supervisor::spawn_observed_tmux_watcher(
                    "recovery_restore_inflight_tmux_output_watcher",
                    shared.clone(),
                    tmux_session_name.clone(),
                    cancel.clone(),
                    super::tmux::tmux_output_watcher(
                        discord_channel_id,
                        http.clone(),
                        shared.clone(),
                        output_path.clone(),
                        tmux_session_name.clone(),
                        initial_offset,
                        cancel,
                        paused,
                        resume_offset,
                        pause_epoch,
                        turn_delivered,
                        last_heartbeat_ts_ms,
                    ),
                );
            }
            (claim.should_spawn(), claim.replaced_existing())
        }
        #[cfg(not(unix))]
        {
            (false, false)
        }
    };

    Ok(RebindOutcome {
        tmux_session: tmux_session_name,
        channel_id,
        initial_offset,
        watcher_spawned,
        watcher_replaced,
    })
}

#[cfg(test)]
mod post_work_evidence_tests {
    use super::*;
    use crate::services::provider::ProviderKind;

    #[test]
    fn recovery_input_fifo_requirement_is_runtime_specific() {
        assert_eq!(
            recovery_input_fifo_for_runtime(RuntimeHandoffKind::ClaudeTui, None).unwrap(),
            None
        );
        assert_eq!(
            recovery_input_fifo_for_runtime(RuntimeHandoffKind::CodexTui, None).unwrap(),
            None
        );
        assert!(
            recovery_input_fifo_for_runtime(RuntimeHandoffKind::LegacyTmuxWrapper, None).is_err()
        );
        assert_eq!(
            recovery_input_fifo_for_runtime(
                RuntimeHandoffKind::LegacyTmuxWrapper,
                Some("/tmp/session.input".to_string())
            )
            .unwrap(),
            Some("/tmp/session.input".to_string())
        );
    }

    #[test]
    fn recovery_handoff_preserves_runtime_kind() {
        let handoff = runtime_handoff_for_recovery(
            RuntimeHandoffKind::ClaudeTui,
            "/tmp/claude-transcript.jsonl".to_string(),
            None,
            "AgentDesk-claude-adk".to_string(),
            Some("session-1".to_string()),
            42,
        );

        match handoff {
            RuntimeHandoff::ClaudeTui {
                transcript_path,
                tmux_session_name,
                last_offset,
            } => {
                assert_eq!(transcript_path, "/tmp/claude-transcript.jsonl");
                assert_eq!(tmux_session_name, "AgentDesk-claude-adk");
                assert_eq!(last_offset, 42);
            }
            other => panic!("expected ClaudeTui handoff, got {other:?}"),
        }
    }

    #[test]
    fn tmux_ready_completion_requires_current_turn_work_evidence() {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "background notification".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.input".to_string()),
            64,
        );
        state.task_notification_kind =
            Some(crate::services::agent_protocol::TaskNotificationKind::Background);

        assert!(
            !recovery_has_post_work_ready_evidence(&state),
            "task-notification-only inflight must not trust a stale tmux Ready for input footer"
        );

        state.full_response = "completed".to_string();
        assert!(recovery_has_post_work_ready_evidence(&state));

        state.full_response.clear();
        state.any_tool_used = true;
        assert!(recovery_has_post_work_ready_evidence(&state));

        state.any_tool_used = false;
        state.last_tool_summary = Some("Bash completed".to_string());
        assert!(recovery_has_post_work_ready_evidence(&state));
    }
}

// #3248 gap-1 — the pane-alive reattach path (`reregister_active_turn_from_inflight`)
// must re-seed the single-authority finalizer ledger with a Watcher-owned entry
// after a mid-turn dcserver restart clears the in-memory ledger. Without it the
// live pane never auto-reconciles (the watcher's id-0 gate-timeout creates a
// `relay_owner=None` orphan that finalizes immediately instead of arming the 8s
// backstop, and the far-backstop reconcile — which collects only
// `relay_owner==Watcher` rows — never catches it), so a NEW user turn is required.
#[cfg(test)]
mod reregister_ledger_reseed_tests {
    use super::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;
    use serenity::model::id::ChannelId;

    fn active_turn_state(channel_id: u64, user_msg_id: u64) -> InflightTurnState {
        // A live (not-yet-committed) ordinary turn whose ids are all non-zero, so
        // it passes the `reregister_active_turn_from_inflight` early guard and is
        // NOT short-circuited by `recovery_terminal_delivery_already_committed`.
        InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            Some("adk-cc".to_string()),
            7, // request_owner_user_id
            user_msg_id,
            user_msg_id + 1, // current_msg_id
            "live prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            None,
            0,
        )
    }

    // Gap-1: a fresh reattach (empty mailbox → mailbox turn started) re-seeds the
    // ledger so the turn is a LIVE Watcher-pending entry. This is precisely the
    // state that makes the watcher's gate-timeout arm its backstop instead of
    // finalizing-as-orphan, and makes the far-backstop reconcile able to collect
    // the row — so the live pane auto-reconciles WITHOUT a new user turn.
    #[tokio::test(flavor = "current_thread")]
    async fn reattach_reseeds_watcher_owned_ledger_entry() {
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let ch = ChannelId::new(52_481);
        let state = active_turn_state(ch.get(), 9001);

        // Pre-condition: post-restart the in-memory ledger is empty — no
        // watcher-pending entry exists for this turn yet.
        assert!(
            !shared
                .turn_finalizer
                .has_live_watcher_pending(ch, shared.restart.current_generation)
                .await,
            "ledger must start empty (simulating a post-restart in-memory ledger)"
        );

        let restored = super::reregister_active_turn_from_inflight(&shared, &state).await;
        assert!(
            restored,
            "an empty mailbox must let the reattach start the active turn"
        );

        // Post-condition: the ledger now has a LIVE Watcher-owned entry under the
        // turn's full identity + the current (restart) generation.
        assert!(
            shared
                .turn_finalizer
                .has_live_watcher_pending(ch, shared.restart.current_generation)
                .await,
            "#3248 gap-1: reattach must register_start the turn as Watcher-owned"
        );
    }

    // Idempotency: a second reattach of the SAME turn (or a later bridge handoff
    // register_start) must NOT error or duplicate/over-finalize — the actor's
    // `Start` handler is entry().and_modify().or_insert() and never resurrects a
    // finalized turn. The turn stays a single live Watcher-pending entry.
    #[tokio::test(flavor = "current_thread")]
    async fn repeated_reattach_is_idempotent_single_watcher_entry() {
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let ch = ChannelId::new(52_482);
        let state = active_turn_state(ch.get(), 9101);

        assert!(super::reregister_active_turn_from_inflight(&shared, &state).await);
        // Second call: the mailbox already holds the active turn, so this takes
        // the "existing active turn" rebind branch and re-seeds again.
        let restored_again = super::reregister_active_turn_from_inflight(&shared, &state).await;
        assert!(
            restored_again,
            "re-attaching an already-active turn re-binds (returns true) without panic"
        );

        // Still exactly one live Watcher-pending entry (idempotent re-register).
        assert!(
            shared
                .turn_finalizer
                .has_live_watcher_pending(ch, shared.restart.current_generation)
                .await,
            "repeated reattach keeps a single live Watcher-pending entry"
        );
    }

    // Safety: a recovery turn WITHOUT a real user_msg_id (== 0) must be skipped by
    // the early guard — it must NOT register a channel-only (id-0) orphan ledger
    // entry that could collide with the watcher's id-0 submissions.
    #[tokio::test(flavor = "current_thread")]
    async fn zero_user_msg_id_is_not_registered() {
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let ch = ChannelId::new(52_483);
        // user_msg_id == 0 (and thus the early guard returns false before any
        // register_start).
        let mut state = active_turn_state(ch.get(), 0);
        state.user_msg_id = 0;
        state.current_msg_id = 0;

        let restored = super::reregister_active_turn_from_inflight(&shared, &state).await;
        assert!(!restored, "a zero-id recovery turn is not re-attached");
        assert!(
            !shared
                .turn_finalizer
                .has_live_watcher_pending(ch, shared.restart.current_generation)
                .await,
            "a zero user_msg_id turn must NOT seed an orphan ledger entry"
        );
    }

    // #3089 A0 — characterization of the recovery probe-classified outcome
    // (design §5 A0 item 3, signal #5 of 5). `RecoveryCompletionOutcome` is the
    // recovery engine's terminal-completion signal; BOTH arms `should_proceed()`
    // (a suppressed visible completion is NOT a delivery failure, so callers
    // still release mailbox/inflight ownership). Pinned inline in this
    // `#[cfg(test)] mod` block of the FROZEN (baseline 4090) file => ZERO prod
    // LoC.
    mod a0_characterization_tests {
        use super::super::RecoveryCompletionOutcome;

        #[test]
        fn a0_both_recovery_outcomes_proceed_with_cleanup() {
            assert!(
                RecoveryCompletionOutcome::Emitted.should_proceed(),
                "Emitted proceeds"
            );
            assert!(
                RecoveryCompletionOutcome::VisibleCompletionSuppressed.should_proceed(),
                "VisibleCompletionSuppressed still proceeds (terminal delivery is authoritative)"
            );
        }

        #[test]
        fn a0_recovery_outcomes_are_two_distinct_arms() {
            assert_ne!(
                RecoveryCompletionOutcome::Emitted,
                RecoveryCompletionOutcome::VisibleCompletionSuppressed
            );
        }
    }
}
