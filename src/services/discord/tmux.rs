use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};
use crate::db::turns::TurnTokenUsage;
use crate::services::agent_protocol::{StatusEvent, TaskNotificationKind};
use crate::services::message_outbox::{
    LIFECYCLE_NOTIFIER_SOURCE, OutboxMessage, enqueue_lifecycle_notification_best_effort,
    enqueue_outbox_best_effort,
};
use crate::services::observability::turn_lifecycle::TurnEvent;
use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};
use crate::services::session_backend::{
    StreamLineState, classify_task_notification_kind, observe_stream_context,
};
use crate::services::tmux_diagnostics::{
    build_tmux_death_diagnostic, read_tmux_exit_reason, record_tmux_exit_reason,
};

use super::formatting::{
    ReplaceLongMessageOutcome, build_streaming_placeholder_text, format_tool_input,
    plan_streaming_rollover, truncate_str,
};
use super::placeholder_cleanup::{PlaceholderCleanupOperation, PlaceholderCleanupOutcome};
use super::placeholder_live_events::{
    RecentPlaceholderEvent, events_from_json, status_events_from_json,
};
use super::settings::{
    channel_supports_provider, load_last_session_path, resolve_role_binding,
    validate_bot_channel_routing_with_provider_channel,
};
use super::tmux_error_detect::{
    ProviderProseDiagnostic, classify_provider_prose_diagnostic, is_prompt_too_long_message,
};
use super::tmux_overload_retry::clear_provider_overload_retry_state;
use super::tmux_restart_handoff::{
    resolve_dispatched_thread_dispatch_from_db, resume_aborted_restart_turn,
};
use super::{
    SharedData, TmuxWatcherHandle, TmuxWatcherRegistry, lock_tmux_watcher_registry, rate_limit_wait,
};
#[path = "tmux/relay_decisions.rs"]
mod relay_decisions;
#[path = "tmux/turn_lifecycle.rs"]
mod turn_lifecycle;

pub(in crate::services::discord) use self::relay_decisions::sniff_background_agent_pending_for_completion;
#[cfg(test)]
use self::relay_decisions::watcher_should_yield_to_inflight_state;
pub(super) use self::relay_decisions::{
    WatcherLineOutcome, WatcherTerminalKind, should_gate_completion_for_tui_quiescence,
};
use self::relay_decisions::{
    advance_buffer_start_offset, is_terminal_finalize_stop_candidate, merge_task_notification_kind,
    record_watcher_invariant, should_suppress_relay_before_emit, stream_line_state_token_usage,
    terminal_relay_decision, tmux_death_is_normal_completion,
    watcher_ready_for_input_turn_completed, watcher_should_yield_to_active_bridge_turn,
    watcher_stop_decision_after_terminal_finalize,
};
pub(in crate::services::discord) use self::turn_lifecycle::build_monitor_triggered_inflight_state;
use self::turn_lifecycle::{
    MonitorAutoTurnStart, WatcherStreamSeed, enqueue_monitor_auto_turn_deferred_notification,
    finish_monitor_auto_turn_if_claimed, next_monitor_auto_turn_ledger_generation,
    normalize_response_sent_offset, restored_seed_reassigned_to_different_turn,
    should_discard_restored_seed_for_idle_direct_prompt, watcher_stream_seed,
};
pub(super) use self::turn_lifecycle::{
    RestoredWatcherTurn, SeedBodySource, consume_monitor_auto_turn_preamble_once,
    restored_watcher_turn_from_inflight,
};

// #5071 T3-A0: `.spawn_nonce` read model, registered here so it sits beside the
// `tmux_session_files` reader it reuses and inherits this file's unix gate.
// T3-A1 made `tmux_watcher_registry` its consumer, and that module is NOT
// unix-gated, so the module is visible module-wide behind that file's own
// `cfg(unix)`/`cfg(not(unix))` shim pair.
#[path = "execution_identity.rs"]
pub(in crate::services::discord) mod execution_identity;
mod monitor_auto_turn_inflight;
#[cfg(test)]
#[path = "tmux/n1a_monitor_tests.rs"]
mod n1a_monitor_tests;
#[path = "tmux_placeholder_suppression/mod.rs"]
mod placeholder_suppression;
#[path = "tmux_reattach_offsets.rs"]
mod tmux_reattach_offsets;
#[path = "tmux_session_files.rs"]
mod tmux_session_files;
#[path = "watchers/lifecycle.rs"]
mod watcher_lifecycle;
#[cfg(all(test, unix))]
pub(crate) use watcher_lifecycle::commit_codex_watcher_restore_for_tests;
#[cfg(all(test, unix))]
pub(crate) use watcher_lifecycle::commit_codex_watcher_restore_to_empty_registry_for_tests;

#[cfg(test)]
pub(in crate::services::discord) use watcher_lifecycle::InstallFixture;
#[cfg(test)]
pub(in crate::services::discord) use watcher_lifecycle::watcher_install_fixture;

use self::monitor_auto_turn_inflight::ensure_monitor_auto_turn_inflight;
use self::placeholder_suppression::*;
use self::tmux_reattach_offsets::matching_recent_watcher_reattach_offset;
pub(in crate::services::discord) use self::tmux_session_files::committed_frontier_for_current_generation;
pub(super) use self::tmux_session_files::read_generation_file_mtime_ns;
pub(in crate::services::discord) use self::tmux_session_files::reset_relay_watermark_on_generation_change;
pub(in crate::services::discord) use self::tmux_session_files::reset_stale_relay_watermark_if_output_regressed;
pub(super) use self::tmux_session_files::session_panel_instance_key;
pub(crate) use self::tmux_session_files::stamp_spawn_markers;
#[cfg(test)]
pub(crate) use self::tmux_session_files::write_spawn_nonce;
use self::tmux_session_files::{
    preserve_session_generation_mtime_after_write,
    reset_stale_local_relay_offset_if_output_regressed, sweep_orphan_session_files,
};
#[rustfmt::skip]
#[cfg(test)]
pub(in crate::services::discord) use self::watcher_lifecycle::{CLAIM_PAUSE, claim_cross_channel_tmux_watcher_for_test, claim_or_replace_watcher, claim_or_reuse_watcher, evict_claim_before_adoption_for_test};
use self::watcher_lifecycle::*;
pub(in crate::services::discord) use self::watcher_lifecycle::{
    HostSnapshot, ThreadFollowUpParent, WatchHost, WatchWithheld, WatcherClaimIncarnation,
    claim_or_replace_watcher_for_host, claim_or_reuse_watcher_for_host,
    clear_recovery_handled_channels, fail_dispatch_for_ready_for_input_stall,
    refresh_session_heartbeat_from_tmux_output, restore_retry, restore_tmux_watchers,
    session_belongs_to_current_runtime, store_recovery_handled_channels,
    thread_follow_up_parent_channel_id, thread_follow_up_parent_from_live, watch_host_of,
};
use super::watcher_lifecycle_decision::*;
const READY_FOR_INPUT_IDLE_PROBE_INTERVAL: Duration = Duration::from_secs(2);
const SOFT_TERMINAL_DEBOUNCE: Duration = Duration::from_millis(1500);
pub(super) const WATCHER_ACTIVITY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const READY_FOR_INPUT_STUCK_LABEL: &str = "stuck_at_ready";
const READY_FOR_INPUT_STUCK_REASON: &str = "agent ended at Ready for input without commit/push";
#[path = "tmux_kill_policy.rs"]
mod tmux_kill_policy;
#[allow(unused_imports)]
pub(super) use self::tmux_kill_policy::{
    CANCEL_TEARDOWN_GRACE_BYTES, MONITOR_AUTO_TURN_DEFERRED_REASON_CODE,
    RECENT_TURN_STOP_METADATA_FALLBACK_TTL, TMUX_LIVENESS_PROBE_INTERVAL,
    cancel_induced_watcher_death, cancel_induced_watcher_death_async, recent_turn_stop_for_channel,
    recent_turn_stop_for_watcher_range, record_recent_turn_stop, tmux_output_offset,
};

#[cfg(test)]
mod restored_seed_discard_tests {
    use super::{
        RestoredWatcherTurn, restored_seed_reassigned_to_different_turn,
        restored_watcher_turn_from_inflight, should_discard_restored_seed_for_idle_direct_prompt,
        watcher_stream_seed,
    };
    use crate::services::discord::inflight::{InflightTurnIdentity, InflightTurnState};
    use crate::services::discord::outbound::delivery_record::{
        self, ConfirmedDeliveryReceipt, DeliveredCommit,
    };
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};

    fn touch_generation_marker(tmux_session_name: &str) -> i64 {
        let path = crate::services::tmux_common::session_temp_path(tmux_session_name, "generation");
        if let Some(parent) = std::path::Path::new(&path).parent() {
            std::fs::create_dir_all(parent).expect("create generation parent");
        }
        std::fs::write(&path, "phase-a-generation").expect("write generation marker");
        delivery_record::current_generation_mtime_ns(tmux_session_name)
    }

    fn restored_state_fixture(
        provider: ProviderKind,
        delivery_channel_id: u64,
        owner_channel_id: Option<u64>,
        tmux_session_name: &str,
    ) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            provider,
            delivery_channel_id,
            Some("phase-a".into()),
            7,
            49_110,
            49_111,
            "turn N".into(),
            Some("session-4911".into()),
            Some(tmux_session_name.into()),
            Some("/tmp/phase-a-4911.jsonl".into()),
            None,
            100,
        );
        state.last_offset = 200;
        state.full_response = "N delivered body".into();
        state.response_sent_offset = 0;
        state.streaming_rollover_frozen_msg_ids = vec![49_112];
        state.turn_nonce = Some("turn-nonce-N".into());
        if let Some(owner_channel_id) = owner_channel_id {
            if owner_channel_id == 0 {
                state.watcher_owner_channel_id = Some(0);
            } else {
                state.set_watcher_owner_channel_id(owner_channel_id);
            }
        }
        state
    }

    fn restored_seed_fixture(
        provider: ProviderKind,
        delivery_channel: ChannelId,
        owner_channel: ChannelId,
        tmux_session_name: &str,
    ) -> RestoredWatcherTurn {
        let state = restored_state_fixture(
            provider,
            delivery_channel.get(),
            Some(owner_channel.get()),
            tmux_session_name,
        );
        crate::services::discord::inflight::save_inflight_state(&state)
            .expect("persist receipt source row");
        let mut restored = restored_watcher_turn_from_inflight(&state, tmux_session_name, false)
            .expect("restorable turn");
        restored.last_edit_text = "stale placeholder body".into();
        restored
    }

    fn write_receipt(
        provider: &ProviderKind,
        source: delivery_record::ExactJsonlSourceIdentity,
        message_id: u64,
    ) -> Result<(), String> {
        let owner = source.offset_authority_channel_id;
        let delivery = source.delivery_channel_id;
        delivery_record::write_confirmed_delivery(
            provider,
            owner,
            DeliveredCommit {
                range: source.range,
                generation_mtime_ns: source.generation_mtime_ns,
                attempts: 1,
                panel_msg_id: Some(message_id),
                panel_channel_id: Some(delivery),
            },
            ConfirmedDeliveryReceipt {
                source,
                delivery_channel_id: delivery,
                message_id,
            },
        )
    }

    #[test]
    fn idle_direct_prompt_preserves_restored_seed_with_undelivered_body() {
        assert!(!should_discard_restored_seed_for_idle_direct_prompt(
            true, true, true, false, false,
        ));
    }

    #[test]
    fn idle_direct_prompt_still_discards_empty_restored_seed_with_anchor() {
        assert!(should_discard_restored_seed_for_idle_direct_prompt(
            true, true, false, false, false,
        ));
    }

    #[test]
    fn idle_direct_prompt_preserves_same_turn_rewind_seed_with_anchor() {
        assert!(!should_discard_restored_seed_for_idle_direct_prompt(
            true, true, false, true, true,
        ));
    }

    #[test]
    fn idle_direct_prompt_discard_still_requires_restored_turn_and_anchor() {
        assert!(!should_discard_restored_seed_for_idle_direct_prompt(
            true, false, false, false, false,
        ));
        assert!(!should_discard_restored_seed_for_idle_direct_prompt(
            true, false, true, false, false,
        ));
        assert!(!should_discard_restored_seed_for_idle_direct_prompt(
            false, true, false, false, false,
        ));
        assert!(!should_discard_restored_seed_for_idle_direct_prompt(
            false, true, true, false, false,
        ));
    }

    #[test]
    fn cross_turn_watcher_reuse_discards_restored_seed_before_terminal_commit() {
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
            delivery_source: None,
            body_source: None,
        };

        let seed_reassigned_to_different_turn = restored_seed_reassigned_to_different_turn(
            Some(&restored),
            Some(&current_identity),
            Some(9002),
        );
        assert!(seed_reassigned_to_different_turn);
        assert!(should_discard_restored_seed_for_idle_direct_prompt(
            true,
            true,
            true,
            restored.same_turn_rewind,
            seed_reassigned_to_different_turn,
        ));

        let stream_seed = watcher_stream_seed(None);
        assert!(stream_seed.full_response.is_empty());
        assert_eq!(stream_seed.response_sent_offset, 0);
    }

    #[test]
    fn stale_clone_then_bridge_receipt_then_inflight_clear_discards_all_seed_state_4911() {
        let root = tempfile::tempdir().expect("isolated root");
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let delivery_channel = ChannelId::new(49_110);
        let owner_channel = ChannelId::new(49_161);
        let tmux_session_name = "AgentDesk-claude-phase-a-4911";
        assert_ne!(touch_generation_marker(tmux_session_name), 0);

        // Clone happens first with N present and response_sent_offset still 0.
        let restored = restored_seed_fixture(
            provider.clone(),
            delivery_channel,
            owner_channel,
            tmux_session_name,
        );
        assert_eq!(restored.response_sent_offset, 0);
        assert_eq!(restored.full_response, "N delivered body");
        let source = restored.delivery_source.clone().expect("captured source");

        // The bridge then commits the exact split-channel receipt and clears the
        // inflight before the watcher consumes its stale clone.
        write_receipt(&provider, source, restored.current_msg_id.get())
            .expect("write exact confirmed receipt");
        crate::services::discord::inflight::delete_inflight_state_file(
            &provider,
            delivery_channel.get(),
        );
        assert!(restored_seed_reassigned_to_different_turn(
            Some(&restored),
            None,
            None,
        ));

        let mut next_payload = watcher_stream_seed(None);
        assert!(next_payload.full_response.is_empty());
        assert_eq!(next_payload.response_sent_offset, 0);
        assert!(next_payload.placeholder_msg_id.is_none());
        assert!(next_payload.status_panel_msg_id.is_none());
        assert!(next_payload.last_edit_text.is_empty());
        assert!(next_payload.streaming_rollover_frozen_msg_ids.is_empty());
        next_payload.full_response.push_str("N+1 after edit 404");
        assert_eq!(next_payload.full_response, "N+1 after edit 404");
        assert!(!next_payload.full_response.contains("N delivered body"));
    }

    #[test]
    fn same_body_with_different_nonce_or_range_preserves_restored_seed_4911() {
        let root = tempfile::tempdir().expect("isolated root");
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(49_120);
        let tmux_session_name = "AgentDesk-claude-phase-a-nonce-range";
        touch_generation_marker(tmux_session_name);
        let restored = restored_seed_fixture(provider.clone(), channel, channel, tmux_session_name);
        let source = restored.delivery_source.clone().expect("captured source");

        write_receipt(&provider, source.clone(), restored.current_msg_id.get() + 1)
            .expect("write destination-mismatch receipt");
        assert!(!restored_seed_reassigned_to_different_turn(
            Some(&restored),
            None,
            None,
        ));

        let mut wrong_nonce = source.clone();
        wrong_nonce.turn_nonce = "different-turn-same-body".into();
        write_receipt(&provider, wrong_nonce, restored.current_msg_id.get())
            .expect("write nonce-mismatch receipt");
        assert!(!restored_seed_reassigned_to_different_turn(
            Some(&restored),
            None,
            None,
        ));

        let mut wrong_range = source;
        wrong_range.range = (100, 201);
        write_receipt(&provider, wrong_range, restored.current_msg_id.get())
            .expect("write range-mismatch receipt");
        assert!(!restored_seed_reassigned_to_different_turn(
            Some(&restored),
            None,
            None,
        ));
        assert_eq!(
            watcher_stream_seed(Some(restored)).full_response,
            "N delivered body"
        );
    }

    #[test]
    fn stale_generation_and_legacy_frontier_preserve_restored_seed_4911() {
        let root = tempfile::tempdir().expect("isolated root");
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(49_130);
        let tmux_session_name = "AgentDesk-codex-phase-a-generation";
        touch_generation_marker(tmux_session_name);
        let mut restored =
            restored_seed_fixture(provider.clone(), channel, channel, tmux_session_name);
        let source = restored.delivery_source.clone().expect("captured source");

        // A legacy frontier with no exact receipt is never seed-removal proof.
        delivery_record::write_delivered_frontier(
            &provider,
            channel.get(),
            tmux_session_name,
            DeliveredCommit {
                range: source.range,
                generation_mtime_ns: source.generation_mtime_ns,
                attempts: 1,
                panel_msg_id: Some(restored.current_msg_id.get()),
                panel_channel_id: Some(channel.get()),
            },
        )
        .expect("write legacy frontier");
        assert!(!restored_seed_reassigned_to_different_turn(
            Some(&restored),
            None,
            None,
        ));

        // Even a receipt-shaped record from another transcript incarnation is
        // rejected against the current generation marker.
        let mut stale_source = source;
        stale_source.generation_mtime_ns = stale_source.generation_mtime_ns.saturating_add(1);
        assert!(
            write_receipt(
                &provider,
                stale_source.clone(),
                restored.current_msg_id.get(),
            )
            .is_err(),
            "the guarded writer must reject stale-incarnation authority"
        );
        restored.delivery_source = Some(stale_source);
        assert!(!restored_seed_reassigned_to_different_turn(
            Some(&restored),
            None,
            None,
        ));
        assert_eq!(
            watcher_stream_seed(Some(restored)).full_response,
            "N delivered body"
        );
    }

    #[test]
    fn zero_persisted_delivery_channel_preserves_restored_seed_4911() {
        let root = tempfile::tempdir().expect("isolated root");
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let tmux_session_name = "AgentDesk-codex-phase-a-zero-delivery";
        let generation = touch_generation_marker(tmux_session_name);
        let state = restored_state_fixture(ProviderKind::Codex, 0, Some(49_141), tmux_session_name);
        let mut restored = restored_watcher_turn_from_inflight(&state, tmux_session_name, false)
            .expect("zero destination must not erase the conservative restored body");
        assert!(restored.delivery_source.is_none());
        restored.delivery_source = Some(delivery_record::ExactJsonlSourceIdentity {
            provider: ProviderKind::Codex.as_str().to_string(),
            tmux_session_name: tmux_session_name.to_string(),
            turn_nonce: "zero-delivery".into(),
            range: (100, 200),
            generation_mtime_ns: generation,
            offset_authority_channel_id: 49_141,
            delivery_channel_id: 0,
        });
        assert!(!restored_seed_reassigned_to_different_turn(
            Some(&restored),
            None,
            None,
        ));
        assert_eq!(
            watcher_stream_seed(Some(restored)).full_response,
            "N delivered body"
        );
    }

    #[test]
    fn zero_persisted_owner_channel_preserves_restored_seed_4911() {
        let root = tempfile::tempdir().expect("isolated root");
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let tmux_session_name = "AgentDesk-codex-phase-a-zero-owner";
        let generation = touch_generation_marker(tmux_session_name);
        let state = restored_state_fixture(ProviderKind::Codex, 49_151, Some(0), tmux_session_name);
        let mut restored = restored_watcher_turn_from_inflight(&state, tmux_session_name, false)
            .expect("zero owner must not erase the conservative restored body");
        assert!(restored.delivery_source.is_none());
        restored.delivery_source = Some(delivery_record::ExactJsonlSourceIdentity {
            provider: ProviderKind::Codex.as_str().to_string(),
            tmux_session_name: tmux_session_name.to_string(),
            turn_nonce: "zero-owner".into(),
            range: (100, 200),
            generation_mtime_ns: generation,
            offset_authority_channel_id: 0,
            delivery_channel_id: 49_151,
        });
        assert!(!restored_seed_reassigned_to_different_turn(
            Some(&restored),
            None,
            None,
        ));
        assert_eq!(
            watcher_stream_seed(Some(restored)).full_response,
            "N delivered body"
        );
    }
}

async fn emit_context_compacted_lifecycle_from_watcher(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    model: Option<&str>,
    usage: Option<TurnTokenUsage>,
) -> bool {
    let ctx_cfg = super::adk_session::fetch_context_thresholds(shared.pg_pool.as_ref()).await;
    // Provider auto-compaction output does not expose the exact pre-compaction
    // token total, so record the configured trigger threshold as the lower-bound
    // before percentage.
    let before_pct = ctx_cfg.compact_pct_for(provider);
    let context_window = provider.resolve_context_window(model);
    let after_pct = usage.and_then(|usage| {
        let pct = super::adk_session::context_usage_percent(
            usage.context_occupancy_input_tokens(),
            context_window,
        )
        .min(before_pct);
        (pct > 0).then_some(pct)
    });
    let emitted = super::adk_session::emit_context_compacted_lifecycle_for_inflight(
        shared, channel_id, provider, before_pct, after_pct,
    )
    .await;

    if !emitted {
        let target = format!("channel:{}", channel_id.get());
        let details = super::adk_session::context_compaction_details(before_pct, after_pct);
        let content = TurnEvent::ContextCompacted(details)
            .notification_content()
            .unwrap_or_else(|| "📦 컨텍스트 자동 압축".to_string());
        enqueue_lifecycle_notification_best_effort(
            shared.pg_pool.as_ref(),
            target.as_str(),
            None,
            "lifecycle.context_compacted",
            content.as_str(),
        );
        tracing::warn!(
            channel_id = channel_id.get(),
            source = LIFECYCLE_NOTIFIER_SOURCE,
            "context compacted lifecycle emit skipped; enqueued fallback notification"
        );
    }

    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalRelayDecision {
    should_direct_send: bool,
    should_tag_monitor_origin: bool,
    should_enqueue_notify_outbox: bool,
    suppressed: bool,
}

/// #1009: System-level hint injected once per monitor auto-turn entry so the
/// agent produces a 1-line summary + next action before terminating. Returned
/// verbatim at the claim sites; callers log it and record the one-shot flag.
pub(super) const MONITOR_AUTO_TURN_PREAMBLE_HINT: &str = "[system] 모니터 자동 턴 종료 전, 이번 턴에서 확인한 상태 1줄 요약과 다음 액션을 반드시 남겨주세요.";

const MONITOR_AUTO_TURN_MISSED_SIGNAL_FALLBACK: tokio::time::Duration =
    tokio::time::Duration::from_secs(30);

async fn start_monitor_auto_turn_when_available(
    // #3016 phase-5a: `&Arc<SharedData>` (not `&SharedData`) so `register_start`
    // can downgrade a `Weak` into the `Start` message and prime the finalizer's
    // reconcile cache at register time — the watcher callers already pass
    // `&shared` (an `Arc`), so the `&SharedData` deref-coercion is simply
    // dropped here; no caller change is needed.
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    data_start_offset: u64,
    cancel: &std::sync::atomic::AtomicBool,
) -> MonitorAutoTurnStart {
    if crate::services::tui_o::turn_mode::transcript_turns(channel_id.get()) {
        return MonitorAutoTurnStart {
            acquired: false,
            deferred: false,
            synthetic_message_id: None,
            ledger_generation: None,
        };
    }
    let mut deferred = false;
    let synthetic_message_id = MessageId::new(data_start_offset.max(1));

    loop {
        if cancel.load(Ordering::Relaxed) || shared.restart.shutting_down.load(Ordering::Relaxed) {
            return MonitorAutoTurnStart {
                acquired: false,
                deferred,
                synthetic_message_id: None,
                ledger_generation: None,
            };
        }

        let token = Arc::new(crate::services::provider::CancelToken::new());
        // #3167 — the monitor auto-turn is a low-priority background relay; mark
        // it with the distinct monitor kind so queued external USER intervention
        // is not starved and synthetic stale-reclaim never preempts it.
        let started = super::mailbox_try_start_turn_kinded(
            shared,
            channel_id,
            token.clone(),
            UserId::new(1),
            synthetic_message_id,
            crate::services::turn_orchestrator::ActiveTurnKind::MonitorAutoTurn,
        )
        .await;
        if started {
            super::increment_global_active(shared, "tmux_monitor_auto_turn");
            shared
                .turn_start_times
                .insert(channel_id, std::time::Instant::now());
            // #3016 P1: register the monitor turn in the finalizer ledger under
            // its synthetic message id keyed by a PROCESS-MONOTONIC ledger
            // generation (the byte-offset-derived synthetic id repeats after a
            // wrapper respawn, so the generation is what guarantees distinct
            // entries for sequential monitor turns — see `MonitorAutoTurnStart`).
            let ledger_generation = next_monitor_auto_turn_ledger_generation();
            shared.turn_finalizer.register_start(
                super::turn_finalizer::TurnKey::new(
                    channel_id,
                    synthetic_message_id.get(),
                    ledger_generation,
                )
                .with_episode_nonce(token.turn_nonce()),
                provider.clone(),
                crate::services::discord::inflight::RelayOwnerKind::Watcher,
                // #3016 phase-5a: prime the reconcile cache at register time so
                // the watcher far-backstop fires even for a fresh actor whose
                // first watcher turn never submits its own terminal.
                shared,
            );
            return MonitorAutoTurnStart {
                acquired: true,
                deferred,
                synthetic_message_id: Some(synthetic_message_id),
                ledger_generation: Some(ledger_generation),
            };
        }

        if !deferred {
            deferred = true;
            let _ = enqueue_monitor_auto_turn_deferred_notification(
                shared.pg_pool.as_ref(),
                channel_id,
                data_start_offset,
            );
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🔔 Monitor auto-turn deferred until active user turn completes (channel {}, provider={})",
                channel_id.get(),
                provider.as_str()
            );
        }

        // #2424 — wait on the mailbox actor's latched turn-finished signal
        // instead of probing mailbox state every 200ms. The long timeout is
        // only a missed-signal fallback for notify/backend bugs; the next
        // loop iteration re-checks cancellation and the mailbox slot.
        let signal = shared.mailboxes.turn_finished(channel_id);
        let _ = tokio::time::timeout(MONITOR_AUTO_TURN_MISSED_SIGNAL_FALLBACK, signal.wait()).await;
    }
}

/// Monotonic-CAS advance of the channel's `confirmed_end_offset` watermark to
/// `committed_end_offset`, pairing the pre-CAS `.generation` mtime on a real
/// advance and recording the `tmux_confirmed_end_monotonic` invariant. #3041
/// P1-1 makes this `pub(in crate::services::discord)`. The watcher commits its
/// delivery lease and then calls this to advance the watermark INLINE
/// (synchronously) on a `Delivered` outcome — the lease commit + this inline
/// advance are the single thing that advances the watermark for the watcher
/// terminal path (§5.2). (The `TurnFinalizer` actor's `CommitDelivery`/
/// `ReleaseDelivery` handlers are DORMANT — retained for a later phase, NOT the
/// live watcher path after the R2 revert.) The monotonic CAS keeps the advance
/// idempotent: a second `Delivered` commit of an already-confirmed range
/// observes `cur >= committed_end_offset` and does not move (and does not
/// refresh the mtime), so no double-advance.
pub(in crate::services::discord) fn advance_watcher_confirmed_end(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    committed_end_offset: u64,
    context: &'static str,
) {
    let generation_mtime_ns = read_generation_file_mtime_ns(tmux_session_name);
    let _ = advance_watcher_confirmed_end_inner(
        WatcherDeliveryTarget {
            shared,
            provider,
            channel_id,
            tmux_session_name,
        },
        committed_end_offset,
        (generation_mtime_ns != 0).then_some(generation_mtime_ns),
        context,
    );
}

/// Advance a delivery range only for the immutable source generation captured
/// with its bytes. Callers hold the relay-frontier mutation guard, so reset
/// incarnation is stable while this validates and advances.
pub(in crate::services::discord) fn advance_watcher_confirmed_end_for_generation(
    target: WatcherDeliveryTarget<'_>,
    committed_end_offset: u64,
    source_generation_mtime_ns: i64,
    context: &'static str,
) -> bool {
    if source_generation_mtime_ns == 0
        || read_generation_file_mtime_ns(target.tmux_session_name) != source_generation_mtime_ns
    {
        return false;
    }
    advance_watcher_confirmed_end_inner(
        target,
        committed_end_offset,
        Some(source_generation_mtime_ns),
        context,
    )
}

/// The `(shared state, provider, channel, tmux session)` tuple every watcher
/// delivery advance is scoped to. Bundled so the guarded funnel's helpers stay
/// within the argument-count ratchet instead of carrying an `allow`.
#[derive(Clone, Copy)]
pub(in crate::services::discord) struct WatcherDeliveryTarget<'a> {
    pub(in crate::services::discord) shared: &'a SharedData,
    pub(in crate::services::discord) provider: &'a ProviderKind,
    pub(in crate::services::discord) channel_id: ChannelId,
    pub(in crate::services::discord) tmux_session_name: &'a str,
}

fn advance_watcher_confirmed_end_inner(
    target: WatcherDeliveryTarget<'_>,
    committed_end_offset: u64,
    mtime_at_attempt: Option<i64>,
    context: &'static str,
) -> bool {
    let WatcherDeliveryTarget {
        shared,
        provider,
        channel_id,
        tmux_session_name,
    } = target;
    let relay_coord = shared.tmux_relay_coord(channel_id);
    let confirmed_end =
        relay_coord.advance_untyped_frontier(committed_end_offset, mtime_at_attempt);
    let confirmed_reached_current = confirmed_end >= committed_end_offset;
    record_watcher_invariant(
        confirmed_reached_current,
        Some(provider),
        channel_id,
        "tmux_confirmed_end_monotonic",
        context,
        "watcher confirmed_end_offset must reach the committed tmux output end",
        serde_json::json!({
            "current_offset": committed_end_offset,
            "confirmed_end": confirmed_end,
            "tmux_session_name": tmux_session_name,
        }),
    );
    debug_assert!(
        confirmed_reached_current,
        "watcher confirmed_end_offset must reach committed output end"
    );
    confirmed_reached_current
}

async fn drain_watcher_output_tail_to_eof(
    output_path: &str,
    mut current_offset: u64,
) -> Result<u64, String> {
    loop {
        let read_more = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking({
                let path = output_path.to_string();
                let offset = current_offset;
                move || -> Result<(Vec<u8>, u64), String> {
                    use std::io::{Read as _, Seek as _};

                    let mut file =
                        std::fs::File::open(&path).map_err(|e| format!("open: {}", e))?;
                    file.seek(std::io::SeekFrom::Start(offset))
                        .map_err(|e| format!("seek: {}", e))?;
                    let mut buf = vec![0u8; 16384];
                    let n = file.read(&mut buf).map_err(|e| format!("read: {}", e))?;
                    buf.truncate(n);
                    Ok((buf, offset + n as u64))
                }
            }),
        )
        .await;

        match read_more {
            Ok(Ok(Ok((chunk, off)))) if !chunk.is_empty() => {
                current_offset = off;
            }
            Ok(Ok(Ok((_, off)))) => return Ok(off),
            Ok(Ok(Err(error))) => return Err(error),
            Ok(Err(error)) => return Err(format!("join error: {error}")),
            Err(_) => return Err("timeout reading tmux output tail".to_string()),
        }
    }
}

async fn drain_missing_inflight_dead_tmux_tail_to_eof(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: &str,
    current_offset: u64,
) -> u64 {
    match drain_watcher_output_tail_to_eof(output_path, current_offset).await {
        Ok(drained_offset) => {
            if drained_offset > current_offset {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 missing-inflight dead-tmux drain advanced {} from offset {} to EOF {} before watcher shutdown",
                    tmux_session_name,
                    current_offset,
                    drained_offset
                );
            }
            advance_watcher_confirmed_end(
                shared,
                provider,
                channel_id,
                tmux_session_name,
                drained_offset,
                "src/services/discord/tmux.rs:missing_inflight_dead_tmux_tail_drain",
            );
            drained_offset
        }
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ missing-inflight dead-tmux drain failed for {} at offset {}: {}",
                tmux_session_name,
                current_offset,
                error
            );
            current_offset
        }
    }
}

#[cfg(test)]
mod terminal_finalize_liveness_tests {
    use super::{
        WATCHER_POST_TERMINAL_IDLE_WINDOW, WatcherStopDecision,
        is_terminal_finalize_stop_candidate, watcher_stop_decision_after_terminal_finalize,
    };

    #[test]
    fn terminal_finalize_stop_candidate_requires_all_terminal_flags() {
        assert!(is_terminal_finalize_stop_candidate(true, true, true));

        assert!(!is_terminal_finalize_stop_candidate(false, true, true));
        assert!(!is_terminal_finalize_stop_candidate(true, false, true));
        assert!(!is_terminal_finalize_stop_candidate(true, true, false));
    }

    #[test]
    fn terminal_finalize_stop_decision_requires_dead_tmux() {
        assert_eq!(
            watcher_stop_decision_after_terminal_finalize(
                true,
                true,
                true,
                true,
                4096,
                4096,
                Some(WATCHER_POST_TERMINAL_IDLE_WINDOW),
            ),
            WatcherStopDecision::Continue,
            "terminal finalization with a live idle tmux pane must keep the watcher attached"
        );

        assert_eq!(
            watcher_stop_decision_after_terminal_finalize(true, true, true, true, 2048, 4096, None,),
            WatcherStopDecision::PostTerminalSuccessContinuation,
            "terminal finalization with live tmux and unread tail bytes must stay attached"
        );

        assert_eq!(
            watcher_stop_decision_after_terminal_finalize(
                true, true, true, false, 4096, 4096, None,
            ),
            WatcherStopDecision::Stop,
            "terminal finalization may stop only after tmux liveness is gone"
        );

        assert_eq!(
            watcher_stop_decision_after_terminal_finalize(
                false, true, true, false, 4096, 4096, None,
            ),
            WatcherStopDecision::Continue,
            "non-committed terminal output is not a stop candidate even if liveness is gone"
        );
    }
}

#[cfg(test)]
mod monitor_auto_turn_signal_tests {
    use super::*;

    struct EnvGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set_root(path: &std::path::Path) -> Self {
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
            Self { previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.as_ref() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    #[tokio::test]
    async fn monitor_auto_turn_wait_wakes_on_turn_finished_signal() {
        let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
        let channel = ChannelId::new(2_424_000);
        let signal = registry.turn_finished(channel);
        let waiter = tokio::spawn({
            let signal = signal.clone();
            async move {
                tokio::time::timeout(std::time::Duration::from_secs(1), signal.wait())
                    .await
                    .expect("turn-finished signal should wake monitor auto-turn wait");
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        signal.mark_done();
        waiter.await.expect("waiter task should not panic");
    }

    #[tokio::test]
    async fn monitor_auto_turn_inflight_persists_actor_episode_nonce() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::tempdir().expect("runtime root");
        let _env = EnvGuard::set_root(root.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_595_240);
        let synthetic_message_id = MessageId::new(4_595_340);
        let token = Arc::new(crate::services::provider::CancelToken::new());
        assert!(
            crate::services::discord::mailbox_try_start_turn_kinded(
                &shared,
                channel_id,
                token.clone(),
                UserId::new(1),
                synthetic_message_id,
                crate::services::turn_orchestrator::ActiveTurnKind::MonitorAutoTurn,
            )
            .await
        );

        let _ = ensure_monitor_auto_turn_inflight(
            &shared,
            &provider,
            channel_id,
            "AgentDesk-claude-4595",
            "/tmp/agentdesk-4595.jsonl",
            "/tmp/agentdesk-4595.fifo",
            Some("session-4595"),
            128,
            256,
        )
        .await;

        let persisted = super::super::inflight::load_inflight_state(&provider, channel_id.get())
            .expect("monitor inflight must persist");
        assert_eq!(persisted.turn_nonce.as_deref(), token.turn_nonce());
    }

    #[tokio::test]
    async fn finish_monitor_auto_turn_if_claimed_releases_mailbox_token() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::tempdir().expect("runtime root");
        let _env = EnvGuard::set_root(root.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_019_240);
        let synthetic_message_id = MessageId::new(4_019_340);
        let ledger_generation = next_monitor_auto_turn_ledger_generation();
        let token = Arc::new(crate::services::provider::CancelToken::new());
        assert!(
            crate::services::discord::mailbox_try_start_turn_kinded(
                &shared,
                channel_id,
                token.clone(),
                UserId::new(1),
                synthetic_message_id,
                crate::services::turn_orchestrator::ActiveTurnKind::MonitorAutoTurn,
            )
            .await
        );
        crate::services::discord::increment_global_active(&shared, "test_monitor_auto_turn");
        shared.turn_finalizer.register_start(
            crate::services::discord::turn_finalizer::TurnKey::new(
                channel_id,
                synthetic_message_id.get(),
                ledger_generation,
            ),
            provider.clone(),
            crate::services::discord::inflight::RelayOwnerKind::Watcher,
            &shared,
        );

        let mut claimed = true;
        let mut finished = false;
        let mut claimed_message_id = Some(synthetic_message_id);
        let mut claimed_generation = Some(ledger_generation);
        finish_monitor_auto_turn_if_claimed(
            &shared,
            &provider,
            channel_id,
            &mut claimed,
            &mut finished,
            &mut claimed_message_id,
            &mut claimed_generation,
        )
        .await;

        assert!(!claimed);
        assert!(finished);
        assert_eq!(claimed_message_id, None);
        assert_eq!(claimed_generation, None);
        assert!(token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        let snapshot = shared.mailbox(channel_id).snapshot().await;
        assert_eq!(snapshot.active_user_message_id, None);
    }
}

#[cfg(test)]
mod tui_completion_gate_tests {
    use super::should_gate_completion_for_tui_quiescence;
    use crate::services::agent_protocol::{RuntimeHandoffKind, TaskNotificationKind};

    #[test]
    fn claude_tui_managed_turn_is_gated() {
        assert!(should_gate_completion_for_tui_quiescence(
            Some(RuntimeHandoffKind::ClaudeTui),
            false,
            None,
        ));
    }

    #[test]
    fn legacy_tmux_wrapper_skips_the_gate() {
        // Legacy wrapper's `result` event coincides with the script exiting,
        // so the existing path already aligns with TUI quiescence.
        assert!(!should_gate_completion_for_tui_quiescence(
            Some(RuntimeHandoffKind::LegacyTmuxWrapper),
            false,
            None,
        ));
    }

    #[test]
    fn process_backend_skips_the_gate() {
        assert!(!should_gate_completion_for_tui_quiescence(
            Some(RuntimeHandoffKind::ProcessBackend),
            false,
            None,
        ));
    }

    #[test]
    fn codex_tui_is_excluded_for_minimal_scope() {
        // Codex TUI has its own completion contract (#2189) and is
        // intentionally outside the scope of this #2161 fix to keep the
        // change focused and reviewable.
        assert!(!should_gate_completion_for_tui_quiescence(
            Some(RuntimeHandoffKind::CodexTui),
            false,
            None,
        ));
    }

    #[test]
    fn missing_runtime_kind_skips_the_gate() {
        assert!(!should_gate_completion_for_tui_quiescence(
            None, false, None,
        ));
    }

    #[test]
    fn rebind_origin_inflight_skips_the_gate() {
        // Externally-adopted tmux sessions don't drive a Discord-origin turn;
        // the rebind path already suppresses user-visible completion markers.
        assert!(!should_gate_completion_for_tui_quiescence(
            Some(RuntimeHandoffKind::ClaudeTui),
            true,
            None,
        ));
    }

    #[test]
    fn background_and_monitor_task_notifications_still_gated_for_tui() {
        // Codex review on #2161 H3: Background and MonitorAutoTurn emit a
        // visible `백그라운드 완료` marker that the user sees on the same
        // pane, so the gate applies to them too when running on ClaudeTui.
        // task_notification_kind is intentionally not a skip condition.
        assert!(should_gate_completion_for_tui_quiescence(
            Some(RuntimeHandoffKind::ClaudeTui),
            false,
            Some(TaskNotificationKind::Background),
        ));
        assert!(should_gate_completion_for_tui_quiescence(
            Some(RuntimeHandoffKind::ClaudeTui),
            false,
            Some(TaskNotificationKind::MonitorAutoTurn),
        ));
        assert!(should_gate_completion_for_tui_quiescence(
            Some(RuntimeHandoffKind::ClaudeTui),
            false,
            Some(TaskNotificationKind::Subagent),
        ));
    }
}

#[cfg(test)]
mod tui_completion_gate_outcome_tests {
    use super::TuiCompletionGateOutcome;

    #[test]
    fn not_gated_emits_immediately() {
        assert!(TuiCompletionGateOutcome::NotGated.should_emit_completion());
    }

    #[test]
    fn confirmed_idle_emits() {
        assert!(TuiCompletionGateOutcome::ConfirmedIdle.should_emit_completion());
    }

    #[test]
    fn skipped_dead_emits() {
        assert!(TuiCompletionGateOutcome::SkippedDead.should_emit_completion());
    }

    #[test]
    fn busy_observed_still_emits() {
        assert!(TuiCompletionGateOutcome::BusyObserved.should_emit_completion());
    }
}

/// #4047 regression coverage — the old lifecycle pause derived from
/// `TuiCompletionGateOutcome` is gone. A busy pane observation is telemetry only
/// and must not suppress completion or cleanup after the finalizer authority
/// proves `Done`.
#[cfg(test)]
mod lifecycle_stage_pause_matrix_tests {
    use super::TuiCompletionGateOutcome;

    fn lifecycle_stage_paused(outcome: TuiCompletionGateOutcome) -> bool {
        let _ = outcome;
        false
    }

    #[test]
    fn gate_outcomes_never_pause_lifecycle() {
        assert!(!lifecycle_stage_paused(TuiCompletionGateOutcome::NotGated));
        assert!(!lifecycle_stage_paused(
            TuiCompletionGateOutcome::ConfirmedIdle
        ));
        assert!(!lifecycle_stage_paused(
            TuiCompletionGateOutcome::SkippedDead
        ));
        assert!(!lifecycle_stage_paused(
            TuiCompletionGateOutcome::BusyObserved
        ));
    }

    #[test]
    fn every_outcome_emits_completion() {
        for outcome in [
            TuiCompletionGateOutcome::NotGated,
            TuiCompletionGateOutcome::ConfirmedIdle,
            TuiCompletionGateOutcome::SkippedDead,
            TuiCompletionGateOutcome::BusyObserved,
        ] {
            assert!(outcome.should_emit_completion(), "{outcome:?}");
            assert!(!lifecycle_stage_paused(outcome), "{outcome:?}");
        }
    }
}

#[cfg(test)]
mod active_bridge_turn_guard_tests {
    use super::watcher_should_yield_to_inflight_state;
    use crate::services::discord::InflightRestartMode;
    use crate::services::discord::inflight::{InflightTurnState, RelayOwnerKind};
    use crate::services::provider::ProviderKind;
    use std::ffi::OsString;
    use std::path::Path;

    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set_path(key: &'static str, value: &Path) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn with_ownerless_codex_tui_state(test: impl FnOnce(InflightTurnState)) {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());

        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            1_479_671_301_387_059_200,
            Some("adk-cdx".to_string()),
            343_742_347_365_974_026,
            1_520_972_895_491_325_952,
            1_520_975_526_431_424_663,
            "deploy release".to_string(),
            Some("019f10e3-3dad-73c2-9d8c-e6188e4ccc7c".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/codex-rollout.jsonl".to_string()),
            None,
            0,
        );
        state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui);
        state.turn_start_offset = Some(0);
        state.set_relay_owner_kind(RelayOwnerKind::None);

        test(state);
    }

    #[test]
    fn planned_restart_ownerless_uncommitted_turn_does_not_yield_to_dead_bridge() {
        with_ownerless_codex_tui_state(|mut state| {
            state.set_restart_mode(InflightRestartMode::DrainRestart);

            assert!(
                !watcher_should_yield_to_inflight_state(
                    Some(&state),
                    "AgentDesk-codex-adk-cdx",
                    0,
                    2_019_364,
                ),
                "restore_inflight watcher must deliver when no terminal delivery was committed"
            );
        });
    }

    #[test]
    fn ownerless_ordinary_turn_still_yields_to_active_bridge() {
        with_ownerless_codex_tui_state(|state| {
            assert!(watcher_should_yield_to_inflight_state(
                Some(&state),
                "AgentDesk-codex-adk-cdx",
                0,
                2_019_364,
            ));
        });
    }

    #[test]
    fn planned_restart_after_terminal_commit_may_still_yield_as_duplicate() {
        with_ownerless_codex_tui_state(|mut state| {
            state.set_restart_mode(InflightRestartMode::DrainRestart);
            state.terminal_delivery_committed = true;

            assert!(watcher_should_yield_to_inflight_state(
                Some(&state),
                "AgentDesk-codex-adk-cdx",
                0,
                2_019_364,
            ));
        });
    }

    #[test]
    fn session_bound_relay_owner_still_suppresses_watcher_duplicate() {
        with_ownerless_codex_tui_state(|mut state| {
            state.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
            state.set_restart_mode(InflightRestartMode::DrainRestart);
            state.current_msg_id = 0;
            state.readopted_from_inflight = true;
            state.terminal_delivery_committed = false;

            assert!(watcher_should_yield_to_inflight_state(
                Some(&state),
                "AgentDesk-codex-adk-cdx",
                0,
                2_019_364,
            ));
        });
    }

    // #4380 reproduction: a CRASH restart (no `restart_report` → `restart_mode ==
    // None`) re-adopts a still-live real-user bridge turn (`relay_owner_kind ==
    // None`) and stamps `readopted_from_inflight`. The recovered watcher MUST resume
    // relay, not yield to the dead bridge. Before the fix this yielded → 100% silent
    // loss. Removing the `crash_readopt_live_relay_resume_required` escape branch in
    // `watcher_should_yield_to_inflight_state` makes this assert FAIL (the fn returns
    // `true` again), which is the mutation proof.
    #[test]
    fn crash_readopt_ownerless_uncommitted_turn_does_not_yield_to_dead_bridge() {
        with_ownerless_codex_tui_state(|mut state| {
            state.readopted_from_inflight = true; // restart_mode stays None (crash)

            assert!(
                !watcher_should_yield_to_inflight_state(
                    Some(&state),
                    "AgentDesk-codex-adk-cdx",
                    0,
                    2_019_364,
                ),
                "a crash-re-adopted uncommitted live turn must resume relay, not yield to the dead bridge (#4380)"
            );
        });
    }

    // The crash escape hatch is scoped to LIVE turns: once terminal delivery is
    // committed a watcher relay would be a duplicate, so a committed re-adopt still
    // yields. This guards the `!terminal_delivery_committed` clause of the predicate.
    #[test]
    fn crash_readopt_after_terminal_commit_may_still_yield_as_duplicate() {
        with_ownerless_codex_tui_state(|mut state| {
            state.readopted_from_inflight = true;
            state.terminal_delivery_committed = true;

            assert!(watcher_should_yield_to_inflight_state(
                Some(&state),
                "AgentDesk-codex-adk-cdx",
                0,
                2_019_364,
            ));
        });
    }
}

async fn reconcile_orphan_suppressed_placeholder_for_restored_watcher(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
) {
    let has_active_turn = super::mailbox_has_active_turn_or_unreachable(shared, channel_id).await;
    let Some(state) = super::inflight::load_inflight_state(provider, channel_id.get()) else {
        return;
    };
    let Some(placeholder_msg_id) = super::inflight::opt_message_id(state.current_msg_id) else {
        return;
    };
    let ctx = PlaceholderSuppressContext {
        origin: PlaceholderSuppressOrigin::OrphanRestartHandoff,
        provider,
        placeholder_msg_id: Some(placeholder_msg_id),
        response_sent_offset: state.response_sent_offset,
        last_edit_text: "",
        inflight_state: Some(&state),
        has_active_turn,
        tmux_session_name,
        task_notification_kind: None,
        reattach_offset_match: false,
    };
    let decision = decide_placeholder_suppression(&ctx);
    let is_edit = matches!(decision, PlaceholderSuppressDecision::Edit(_));
    let msg_id = placeholder_msg_id;
    apply_placeholder_suppression(
        http,
        channel_id,
        shared,
        provider,
        state.tmux_session_name.as_deref().unwrap_or("unknown"),
        ctx.placeholder_msg_id,
        ctx.origin,
        decision,
        None,
    )
    .await;
    if is_edit {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ✓ reconciled orphan suppressed placeholder for channel {} msg {}",
            channel_id.get(),
            msg_id.get()
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn persist_watcher_stream_progress(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    // #3558: the per-turn identity the watcher captured for this panel. `Some`
    // rejects a write onto a fresh row B (different turn started after this
    // frame); `None` (early frames before identity capture) falls back to the
    // historical tmux-session-only guard inside the helper.
    require_identity: Option<&super::inflight::InflightTurnIdentity>,
    current_msg_id: Option<MessageId>,
    full_response: &str,
    response_sent_offset: usize,
    current_tool_line: Option<&str>,
    prev_tool_status: Option<&str>,
    task_notification_kind: Option<TaskNotificationKind>,
    any_tool_used: bool,
    has_post_tool_text: bool,
    // #3871: the frozen streamed rollover-prefix ids accumulated this invocation,
    // persisted so a later-iteration / post-restart terminal fallback can delete them.
    streaming_rollover_frozen_msg_ids: &[MessageId],
) -> super::inflight::WatcherProgressOutcome {
    if full_response.len() < response_sent_offset {
        tracing::debug!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session = %tmux_session_name,
            response_sent_offset,
            full_response_len = full_response.len(),
            "watcher: skipping stream-progress persistence until parsed body catches up"
        );
        return super::inflight::WatcherProgressOutcome::CoordinateMismatch;
    }

    // #3558: pre-emit the in-bounds telemetry against the caller's snapshot for
    // continuity; the helper re-clamps `response_sent_offset` against the
    // freshly reloaded `full_response` under the lock, so the persisted value is
    // always in-bounds regardless of this advisory check.
    let normalized_response_sent_offset =
        normalize_response_sent_offset(full_response, response_sent_offset);
    let offset_in_bounds = normalized_response_sent_offset <= full_response.len()
        && full_response.is_char_boundary(normalized_response_sent_offset);
    record_watcher_invariant(
        offset_in_bounds,
        Some(provider),
        channel_id,
        "response_sent_offset_in_bounds",
        "src/services/discord/tmux.rs:persist_watcher_stream_progress",
        "watcher response_sent_offset must stay on a full_response boundary",
        serde_json::json!({
            "next": normalized_response_sent_offset,
            "full_response_len": full_response.len(),
            "tmux_session_name": tmux_session_name,
        }),
    );
    debug_assert!(
        offset_in_bounds,
        "watcher response_sent_offset must stay on a full_response boundary"
    );

    // #3558: single-flock read-modify-write. `last_offset` is NOT in the patch
    // — the helper preserves whatever the in-lock disk reload carries, so a
    // concurrent owner-gated `refresh_inflight_last_offset_*` advance can no
    // longer be clobbered backward by this previously-unlocked load→save TOCTOU.
    super::inflight::persist_watcher_stream_progress_locked(
        provider,
        channel_id.get(),
        require_identity,
        tmux_session_name,
        super::inflight::WatcherStreamProgressPatch {
            current_msg_id: current_msg_id.map(MessageId::get),
            full_response: full_response.to_string(),
            response_sent_offset,
            current_tool_line: current_tool_line.map(str::to_string),
            prev_tool_status: prev_tool_status.map(str::to_string),
            task_notification_kind,
            any_tool_used,
            has_post_tool_text,
            streaming_rollover_frozen_msg_ids: streaming_rollover_frozen_msg_ids
                .iter()
                .map(|id| id.get())
                .collect(),
        },
    )
}

#[allow(clippy::too_many_arguments)]
async fn finish_restored_watcher_active_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    // #3016/#3645: finalizer ledger identity captured by the caller BEFORE it
    // cleared inflight (a reload here returns `None`). Real Discord turns use
    // `user_msg_id`; id-0 synthetic turns use their persisted finalizer_turn_id.
    finalizer_turn_id: u64,
    finish_mailbox_on_completion: bool,
    // #3016 option A: the caller observed a *normal completion* (terminal output
    // committed / pane confirmed idle past the relay) and wants the single-
    // authority finalizer driven regardless of the restore flag. The finalizer
    // is idempotent — a turn already finalized by the bridge resolves to
    // `AlreadyFinalized` — so an unconditional submit at the confirmed-completion
    // point cannot over-finalize. This decoupled the watcher's normal-completion
    // finalize from `mailbox_finalize_owed` (removed in #3016 phase-5b2) /
    // `finish_mailbox_on_completion`. Restore/recovery callers that have NOT
    // confirmed a normal completion pass `false` and keep the restore-gated path.
    normal_completion: bool,
    _kickoff_queue: bool,
    // #3350 codex r1-1: the caller's PRE-CLEAR row snapshot for the #3303
    // DeferredClaim marker ensure — inflight is cleared before this helper
    // (see `finalizer_turn_id`), so a row re-load cannot authenticate the
    // watcher's synthetic turns. `None` keeps the row-reload fallback.
    claim_snapshot: Option<super::turn_finalizer::SyntheticClaimSnapshot>,
    stop_source: &'static str,
) -> bool {
    // Default context for every caller EXCEPT the #4106 post-early-release site,
    // which routes through `finish_restored_watcher_active_turn_with_ctx` directly
    // to downgrade its EXPECTED identity-guard miss from WARN to debug.
    finish_restored_watcher_active_turn_with_ctx(
        shared,
        provider,
        channel_id,
        finalizer_turn_id,
        finish_mailbox_on_completion,
        normal_completion,
        _kickoff_queue,
        claim_snapshot,
        super::turn_finalizer::FinalizeContext::watcher(),
        stop_source,
    )
    .await
}

/// #4106: inner finalize that accepts an explicit `FinalizeContext`. The thin
/// wrapper above pins `FinalizeContext::watcher()` for all legacy callers; the
/// post-early-release watcher site passes `watcher_after_pre_panel_release()` so
/// its deterministic (already-released) identity-guard miss logs at debug.
#[allow(clippy::too_many_arguments)]
async fn finish_restored_watcher_active_turn_with_ctx(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    finalizer_turn_id: u64,
    finish_mailbox_on_completion: bool,
    normal_completion: bool,
    _kickoff_queue: bool,
    claim_snapshot: Option<super::turn_finalizer::SyntheticClaimSnapshot>,
    finalize_ctx: super::turn_finalizer::FinalizeContext,
    stop_source: &'static str,
) -> bool {
    // The mailbox is cleared now when EITHER gate holds:
    //   * `normal_completion` → confirmed terminal output committed / pane idle
    //     (#3016 option A). The canonical post-phase-5b1 finalize trigger.
    //   * `finish_mailbox_on_completion` → inflight-restore semantics (a
    //     restored/recovered watcher inherits its turn from the bridge, so
    //     it owns the cancel_token when the turn ends). Pre-existing.
    //
    // #3016 phase-5b2: the third gate (`delegated_finalize_owed`, the #1452
    // `mailbox_finalize_owed` flag) is removed — both production sites pass
    // `normal_completion = true`, and the ledger's exactly-once phase gate
    // makes a double `mailbox_finish_turn` idempotent.
    //
    // `kickoff_queue` is the dispatch-lifecycle gate (codex #1670 P2):
    // cleanup must run even on a failed dispatch (orphan prevention), but
    // auto-dispatching the next queued turn must not — callers pass
    // `dispatch_ok` here.
    //
    // #3016 (codex R1): returns whether this helper actually DROVE the
    // finalize (did not early-return); the caller folds it into
    // `watcher_handled_mailbox_finish` so the post-finalize lifecycle (queue
    // kickoff suppression + terminal-stop) reflects the real finalize.
    if !normal_completion && !finish_mailbox_on_completion {
        return false;
    }

    // #3016 phase 3: route the watcher terminal through the single-authority finalizer
    // instead of calling mailbox_finish_turn + counter + side-effects inline. The ledger
    // phase gate makes this exactly-once across bridge and watcher: whichever submits
    // first finalizes; the loser gets `AlreadyFinalized` and the watcher simply skips
    // the queue kickoff (the winner already owns it). Use the finalizer id the
    // caller captured BEFORE clearing inflight (reloading inflight here returns `None`
    // because the watcher already wiped it). The exact id makes the ledger match
    // precise: a stale terminal from an already-finalized turn resolves to that turn's
    // `Finalized` entry (→ AlreadyFinalized) instead of accidentally finalizing a
    // queued follow-up. The watcher cleared inflight inline before calling this helper,
    // and never marked completion-cleanup or drained voice, so `FinalizeContext::watcher`
    // reproduces exactly that side-effect set; the kickoff stays gated on the caller's
    // `kickoff_queue`.
    let outcome = shared
        .turn_finalizer
        .submit_terminal_with_claim_snapshot(
            super::turn_finalizer::TurnKey::new(
                channel_id,
                finalizer_turn_id,
                shared.restart.current_generation,
            ),
            provider.clone(),
            super::turn_finalizer::TerminalEvent::Complete,
            finalize_ctx,
            claim_snapshot,
            shared.clone(),
        )
        .await;
    let (_mailbox_online, _has_pending) = match outcome {
        super::turn_finalizer::FinalizeOutcome::Finalized {
            has_pending,
            mailbox_online,
            ..
        } => {
            // #3016 phase-5b2: the legacy `mailbox_finalize_owed` flag that was
            // revoked here (so a later watcher swap could not run stale cleanup
            // against the NEXT active turn) has been removed entirely. The
            // ledger's exactly-once phase gate is now the sole arbiter, so there
            // is no stale flag a surviving watcher could swap.
            (mailbox_online, has_pending)
        }
        super::turn_finalizer::FinalizeOutcome::AlreadyFinalized
        | super::turn_finalizer::FinalizeOutcome::Deferred => (false, false),
    };
    let _ = stop_source;
    // Drove the finalize (reached here past the early-return gate).
    true
}

/// Background watcher that continuously tails a tmux output file.
/// When Claude produces output from terminal input (not Discord), relay it to Discord.
#[path = "tmux_watcher.rs"]
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) mod tmux_watcher;
#[cfg(test)]
pub(in crate::services::discord) use self::tmux_watcher::pinned_delivery_lease_key_for_test;
pub(super) use self::tmux_watcher::{
    TuiCompletionGateOutcome, emit_explicit_inflight_cleanup_signal, run_tui_completion_gate,
    tmux_output_watcher, tmux_output_watcher_with_restore,
};
#[path = "tmux_output_stream.rs"]
pub(super) mod tmux_output_stream;
pub(in crate::services::discord) use self::tmux_output_stream::{
    WatcherToolState, build_watcher_placeholder_status_block, flush_placeholder_live_events,
    force_next_watcher_status_update, process_watcher_lines, process_watcher_lines_for_turn,
};

#[cfg(test)]
mod buffer_offset_tests {
    use super::advance_buffer_start_offset;

    #[test]
    fn advance_buffer_start_offset_tracks_drained_leftover_tail() {
        assert_eq!(advance_buffer_start_offset(100, 80, 30), 150);
        assert_eq!(advance_buffer_start_offset(100, 30, 80), 100);
    }
}

#[cfg(test)]
mod watcher_placeholder_status_tests {
    use super::watcher_should_render_status_only_placeholder;
    use crate::services::agent_protocol::TaskNotificationKind;

    #[test]
    fn status_only_placeholder_requires_existing_card_or_activity() {
        assert!(!watcher_should_render_status_only_placeholder(
            false, None, None
        ));
        assert!(!watcher_should_render_status_only_placeholder(
            false,
            Some("   "),
            None
        ));
        assert!(watcher_should_render_status_only_placeholder(
            true, None, None
        ));
        assert!(watcher_should_render_status_only_placeholder(
            false,
            Some("⚙ Bash: cargo check"),
            None
        ));
        assert!(watcher_should_render_status_only_placeholder(
            false,
            None,
            Some(TaskNotificationKind::Background)
        ));
    }
}

#[cfg(test)]
mod watcher_stream_progress_tests {
    use super::persist_watcher_stream_progress;
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};

    fn streaming_tick_persist_order_guard_4104() -> &'static str {
        let source = include_str!("tmux_watcher/streaming_status_tick.rs");
        source
            .split_once("// @persist-order-guard-start #4104")
            .and_then(|(_, guarded)| {
                guarded
                    .split_once("// @persist-order-guard-end #4104")
                    .map(|(guarded, _)| guarded)
            })
            .expect("#4104 persist-order markers must delimit production code")
    }

    #[test]
    fn streaming_tick_persists_tool_hold_without_requiring_discord_edit_4104() {
        let guarded = streaming_tick_persist_order_guard_4104();
        let hold_persist = guarded
            .find("persist_watcher_stream_progress(")
            .expect("tool-hold snapshot must be persisted before render decisions");
        let render_gate = guarded
            .find("let raw_current_portion =")
            .expect("streaming tick render gate");

        assert!(
            hold_persist < render_gate,
            "tool-hold persistence must precede the no-edit early return"
        );
    }

    #[test]
    fn streaming_tick_persists_silent_tool_hold_before_render_suppression_4104() {
        let guarded = streaming_tick_persist_order_guard_4104();
        let hold_persist = guarded
            .find("persist_watcher_stream_progress(")
            .expect("silent tool-hold snapshot must be persisted");
        let silent_gate = guarded
            .find("if streaming_silent_turn {")
            .expect("silent rendering suppression gate");

        assert!(
            hold_persist < silent_gate,
            "silent turns must persist tool-hold state before suppressing Discord rendering"
        );
    }

    #[test]
    fn persist_watcher_stream_progress_persists_tool_hold_witness() {
        // Serialize on the PROCESS-WIDE `AGENTDESK_ROOT_DIR` lock so this test
        // is mutually exclusive with every other test that mutates the runtime
        // root (standby_relay, turn_finalizer, gateway, config, …). A module-
        // local mutex would only serialize within this module, letting a
        // concurrent root-mutating test stomp our tempdir env mid-flight.
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tmp.path(),
        );
        checkpoint(&[("AGENTDESK_ROOT_DIR", tmp.path().as_os_str())]);

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1509350490461180105);
        let tmux_session_name = "AgentDesk-claude-adk-issue-2985-hold-witness";
        let mut state = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            Some("claude-pipe".to_string()),
            42,
            9100000000000000123,
            9100000000000000124,
            "emit OK, call a tool, then wait".to_string(),
            Some("session-2985".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/issue-2985-output.jsonl".to_string()),
            Some("/tmp/issue-2985-input.fifo".to_string()),
            0,
        );
        // #3558: seed a non-zero relay watermark the streaming caller must NOT
        // own — the locked RMW must preserve it verbatim.
        state.last_offset = 777;
        super::super::inflight::save_inflight_state(&state).expect("save inflight");

        persist_watcher_stream_progress(
            &provider,
            channel_id,
            tmux_session_name,
            None,
            Some(MessageId::new(9100000000000000125)),
            "[E2E:E18:OK]\n\n",
            0,
            Some("Bash: sleep 60"),
            Some("Bash"),
            None,
            true,
            false,
            &[],
        );

        let persisted = super::super::inflight::load_inflight_state(&provider, channel_id.get())
            .expect("load inflight");
        assert_eq!(persisted.full_response, "[E2E:E18:OK]\n\n");
        assert!(persisted.any_tool_used);
        assert!(
            !persisted.has_post_tool_text,
            "pre-cancel hold witness must stay durable before post-tool text"
        );
        assert_eq!(
            persisted.current_tool_line.as_deref(),
            Some("Bash: sleep 60")
        );
        assert_eq!(persisted.current_msg_id, 9100000000000000125);
        // #3558: the streaming progress write does NOT own `last_offset` — it
        // must survive untouched (the core TOCTOU fix).
        assert_eq!(
            persisted.last_offset, 777,
            "streaming progress must preserve the non-owned last_offset watermark"
        );
    }

    use crate::test_env_panic_probe::{assert_root_restored, checkpoint};

    #[test]
    fn stream_tool_hold_witness_restores_env_after_panic_present() {
        assert_root_restored(
            true,
            persist_watcher_stream_progress_persists_tool_hold_witness,
        );
    }

    #[test]
    fn stream_tool_hold_witness_restores_env_after_panic_absent() {
        assert_root_restored(
            false,
            persist_watcher_stream_progress_persists_tool_hold_witness,
        );
    }

    #[test]
    fn persist_watcher_stream_progress_skips_rewind_seed_before_body_catches_up_4115() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tmp.path(),
        );
        checkpoint(&[("AGENTDESK_ROOT_DIR", tmp.path().as_os_str())]);

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1509350490461180415);
        let tmux_session_name = "AgentDesk-claude-adk-issue-4115-rewind-progress";
        let mut state = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            Some("claude-pipe".to_string()),
            4154,
            9_415,
            9_416,
            "prompt".to_string(),
            Some("session-4115".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/agentdesk-4115.jsonl".to_string()),
            None,
            128,
        );
        state.full_response = "already persisted prefix".to_string();
        state.response_sent_offset = state.full_response.len();
        super::super::inflight::save_inflight_state(&state).expect("save inflight");

        persist_watcher_stream_progress(
            &provider,
            channel_id,
            tmux_session_name,
            None,
            Some(MessageId::new(9_416)),
            "",
            128,
            None,
            None,
            None,
            false,
            false,
            &[],
        );

        let reloaded = super::super::inflight::load_inflight_state(&provider, channel_id.get())
            .expect("reload row");
        assert_eq!(reloaded.full_response, "already persisted prefix");
        assert_eq!(reloaded.response_sent_offset, state.response_sent_offset);
    }

    #[test]
    fn stream_rewind_seed_restores_env_after_panic_present() {
        assert_root_restored(
            true,
            persist_watcher_stream_progress_skips_rewind_seed_before_body_catches_up_4115,
        );
    }

    #[test]
    fn stream_rewind_seed_restores_env_after_panic_absent() {
        assert_root_restored(
            false,
            persist_watcher_stream_progress_skips_rewind_seed_before_body_catches_up_4115,
        );
    }
}

#[cfg(test)]
mod restored_turn_injected_anchor_tests {
    mod task_notification_kind_restart_invariant_tests {
        include!("tmux/task_notification_kind_restart_roundtrip_tests.rs");
    }

    use super::restored_watcher_turn_from_inflight;
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;

    // #3107 codex re-review (P2, F3): the streaming-interval re-acquire site reads
    // the #3099 hourglass anchor (`injected_prompt_message_id`) from the restored
    // turn captured up front (before `restored_turn.take()` consumes it). This test
    // pins the source of that capture: a hourglass-anchored inflight that the
    // watcher restores must carry the anchor onto the `RestoredWatcherTurn`, so the
    // mid-stream re-acquire can re-pin it instead of orphaning the `⏳`.
    #[test]
    fn restored_turn_carries_injected_prompt_message_id_for_streaming_reacquire() {
        let provider = ProviderKind::Claude;
        let tmux_session_name = "AgentDesk-claude-adk-cc-3107-f3";
        let mut state = InflightTurnState::new(
            provider,
            123_456,
            Some("adk-cc".to_string()),
            0, // headless / synthetic user turn (task-notification auto-turn)
            0,
            55_555, // current_msg_id must be non-zero for a restorable turn
            "anchored auto-turn".to_string(),
            None,
            Some(tmux_session_name.to_string()),
            Some("/tmp/agentdesk-3107-f3.jsonl".to_string()),
            None,
            0,
        );
        state.injected_prompt_message_id = Some(424_242);

        let restored = restored_watcher_turn_from_inflight(&state, tmux_session_name, false)
            .expect("a non-rebind inflight with a real current_msg_id restores");
        assert_eq!(
            restored.injected_prompt_message_id,
            Some(424_242),
            "the restored turn must carry the #3099 hourglass anchor so the \
             streaming-interval re-acquire can re-pin it (F3 regression)"
        );
    }
}

#[cfg(test)]
mod streaming_rollover_frozen_prefix_persistence_tests {
    use super::{
        persist_watcher_stream_progress, restored_watcher_turn_from_inflight, watcher_stream_seed,
    };
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};

    // #3871: the frozen rollover-prefix ids must survive ACROSS `'watcher_loop`
    // iterations / a watcher restart. The local accumulator is `Vec::new()`'d each
    // iteration, so a terminal full-body fallback that runs in a LATER iteration
    // than the rollover-freeze (idle-split where the result JSONL lags, or a
    // watcher restart mid-turn) would — without persistence — start with an empty
    // set and leave the earlier frozen prefix UNDELETED (residual duplicate). This
    // pins the persist→restore round-trip + the monotonic union-merge so iteration
    // B still deletes the prefix iteration A froze.
    #[test]
    fn frozen_prefix_persists_across_iteration_and_restore_for_terminal_delete() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tmp.path(),
        );
        checkpoint(&[("AGENTDESK_ROOT_DIR", tmp.path().as_os_str())]);

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1_521_269_012_347_097_158);
        let tmux_session_name = "AgentDesk-claude-adk-3871-persist";
        let state = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            Some("claude-pipe".to_string()),
            42,
            7_001,
            7_002, // current_msg_id non-zero => restorable
            "long answer".to_string(),
            Some("session-3871".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/agentdesk-3871.jsonl".to_string()),
            None,
            0,
        );
        super::super::inflight::save_inflight_state(&state).expect("save inflight");

        // Iteration A froze prefix F1 mid-stream and persisted progress.
        let f1 = MessageId::new(9_001);
        persist_watcher_stream_progress(
            &provider,
            channel_id,
            tmux_session_name,
            None,
            Some(MessageId::new(9_100)),
            "first chunk frozen…",
            0,
            None,
            None,
            None,
            false,
            false,
            &[f1],
        );

        // Iteration B / a watcher restart re-enters with an EMPTY local vec and
        // SEEDS from the persisted row: the frozen prefix must come back so the
        // terminal full-body fallback can still delete it.
        let reloaded = super::super::inflight::load_inflight_state(&provider, channel_id.get())
            .expect("reload row A");
        let restored = restored_watcher_turn_from_inflight(&reloaded, tmux_session_name, false)
            .expect("a non-rebind inflight with a real current_msg_id restores");
        let seed = watcher_stream_seed(Some(restored));
        assert_eq!(
            seed.streaming_rollover_frozen_msg_ids,
            vec![f1],
            "iteration B / restart must restore the prefix frozen in iteration A"
        );
        assert_eq!(
            super::placeholder_suppression::watcher_rollover_prefixes_to_delete_on_terminal(
                true,
                &seed.streaming_rollover_frozen_msg_ids,
            ),
            vec![f1],
            "the restored frozen prefix is deleted on iteration B's full-body fallback (no residual dup)"
        );

        // A SECOND freeze (F2) in iteration B union-merges monotonically — F1 is
        // never dropped and no id is duplicated.
        let f2 = MessageId::new(9_002);
        persist_watcher_stream_progress(
            &provider,
            channel_id,
            tmux_session_name,
            None,
            Some(MessageId::new(9_101)),
            "first chunk frozen…second chunk frozen…",
            0,
            None,
            None,
            None,
            false,
            false,
            &[f1, f2],
        );
        let reloaded2 = super::super::inflight::load_inflight_state(&provider, channel_id.get())
            .expect("reload row B");
        assert_eq!(
            reloaded2.streaming_rollover_frozen_msg_ids,
            vec![f1.get(), f2.get()],
            "the persisted frozen-prefix set is monotonic (union, no dup)"
        );
    }
    use crate::test_env_panic_probe::{assert_root_restored, checkpoint};

    #[test]
    fn frozen_prefix_persistence_restores_env_after_panic_present() {
        assert_root_restored(
            true,
            frozen_prefix_persists_across_iteration_and_restore_for_terminal_delete,
        );
    }

    #[test]
    fn frozen_prefix_persistence_restores_env_after_panic_absent() {
        assert_root_restored(
            false,
            frozen_prefix_persists_across_iteration_and_restore_for_terminal_delete,
        );
    }
}
