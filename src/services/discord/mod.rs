use crate::services::engine_runtime::EngineRuntime;
mod abandon_request_store;
mod adk_session;
pub(crate) mod admin_host_guard;
pub(crate) mod agent_handoff;
pub(crate) mod agentdesk_config;
mod answer_flush_barrier;
pub(crate) mod bot_role;
pub(crate) mod busy_followup_retry_store;
#[cfg(unix)]
pub(crate) mod herdr_terminate;
pub(crate) mod session_locator;
// #3479 item-2: restart-gap message recovery extracted to its catch-up sibling.
mod catch_up;
pub(crate) use adk_runtime::services::discord::codex_absence;
mod commands;
mod compact_turn_authority;
mod completion_footer_metadata;
pub(crate) mod decision_forums;
pub(crate) mod delegation_card;
mod delivery_lease_cell;
mod delivery_lease_key;
mod destructive_cancel_gate;
mod discord_io;
pub(crate) mod dispatch_policy;
pub(crate) mod e2e_control;
mod footer_retirement;
mod footer_view_reconciler;
pub(crate) mod formatting;
mod gateway;
mod gateway_queue_owner;
pub(crate) mod health;
pub(crate) mod host_defer_gate;
pub(crate) mod host_key_derivation;
pub(crate) mod host_liveness;
pub(crate) mod host_teardown_gate;
pub(crate) mod http;
mod id_bridge;
mod idle_detector;
mod idle_relay_drift;
mod inflight;
#[cfg(test)]
pub(crate) use inflight::read_only_owner_snapshot::{
    CodexEpisodeOwnerSnapshot, codex_episode_snapshot, save_actual_episode_for_tests,
};
pub(crate) use inflight::{install_codex_absence_reader, install_engine_probes};
mod inflight_heartbeat_sweeper;
pub(crate) mod inject_disposition;
pub(crate) mod internal_api;
mod jsonl_watcher;
mod mailbox_finish;
mod mailbox_probe;
mod mcp_credential_watcher;
mod metrics;
mod model_catalog;
mod model_picker_interaction;
pub(crate) mod monitoring_status;
pub(crate) use adk_runtime::services::discord::org_schema;
pub(crate) mod org_writer;
pub(crate) mod outbound;
mod placeholder_cleanup;
mod placeholder_controller;
mod placeholder_live_events;
mod placeholder_sweeper;
pub(crate) mod prompt_builder;
mod queue_dispatch;
mod queue_io;
mod queue_marker;
mod queue_overflow_dlq;
mod queue_reactions;
// #5191: catch-up recovery dedup identity set (queue + active + reservation).
mod queued_placeholders_store;
mod reaction_cleanup;
mod reaction_lifecycle;
mod readopted_mailbox_ledger;
mod recovery_known_ids;
mod relay_coord;
mod relay_health;
pub(crate) mod relay_recovery;
mod replace_outcome_policy;
pub(crate) use adk_runtime::services::discord::response_sanitizer;
// #3983 item4: one-shot top session banner emit + dual-path (sink/watcher) de-dup.
mod session_banner;
#[cfg(unix)]
mod session_relay_sink;
#[cfg(unix)]
pub(crate) use session_relay_sink::journal::append_exact_metadata;
#[cfg(all(test, unix))]
pub(crate) use session_relay_sink::journal::install_exact_append;
#[cfg(all(test, unix))]
pub(crate) use session_relay_sink::journal::{
    exact_duplicate_pg_full_fields_and_legacy_same_key_other_attempt,
    exact_namespace_pg_old_reader_and_legacy_binding_bytes_unchanged,
};
mod sidecar_interaction;
// #2011 Phase 5.3: standalone JSONL → Discord relay loop on cluster-standby nodes (leader uses tmux_watcher's relay path).
#[cfg(unix)]
mod standby_relay;
// #1074: landing zone for the future recovery-engine module split (restart / runtime / manual_rebind; see `docs/recovery-paths.md`). Named `recovery_paths` to avoid shadowing the `recovery_engine as recovery` alias until the split lands.
mod recovery_engine;
mod recovery_paths;
mod restart_mode;
// Scheduled-delivery isolation mark and outcome settlement for inflight turns.
mod scheduled_turn;
// Execution receipts of every row-removing ending, and settlement of rows kept marked.
mod receipt_settle;
pub(crate) use receipt_settle::settle_v2_drops;
// #1074: session identity parsing SSoT (legacy + namespaced session_key forms).
pub(crate) mod restart_report;
mod router;
mod runtime_bootstrap;
pub(crate) use runtime_bootstrap::{GatewayReleaseError, request_gateway_release};
pub(in crate::services::discord) mod semantic_boundaries;
mod skills_scan;
mod turn_presence;
mod turn_teardown_clearance;
// #1446 stall-deadlock recovery: shared post-clear bookkeeping for the THREAD-GUARD
// + stall-watchdog cleanup paths so neither leaks `global_active` / cancel tokens.
pub use adk_runtime::services::discord::runtime_store;
// #3646 OBSERVATION-ONLY: pure payload builders + owner-split derivation for the
// relay flight recorder's two-signal owner separation and the three terminal
// lifecycle events. No relay/cleanup behaviour lives here.
mod relay_owner_observability;
pub(crate) mod session_canonical_identity;
pub(crate) mod session_identity;
mod session_idle_cleanup;
mod session_runtime;
mod session_status_hook;
pub(super) use adk_runtime::services::discord::session_transition;
pub(crate) mod settings;
pub(crate) mod shared_memory;
// #3038 S1/S2: extracted SharedData field clusters (named sub-structs + their
// dedicated inherent impls). See `shared_state::{QueuedPlaceholderState, SessionOverrideState}`.
mod shared_state;
mod single_message_panel;
mod stall_recovery;
mod startup_reclaim;
mod status_panel_orphan_store;
mod status_panel_singleton_store;
// #4891 Task #26 Slice 1: dormant pure proofs; no production caller or authority.
pub(crate) use adk_runtime::services::discord::session_notice;
pub(crate) mod session_notice_delivery;
mod status_panel_transition_v2;
pub(in crate::services::discord) mod streaming_finalizer;
mod synthetic_headless_message_id;
mod task_notification_delivery;
pub(in crate::services::discord) mod task_supervisor;
mod terminal_coordinate;
#[cfg(test)]
mod test_fixtures;
#[cfg(unix)]
mod tmux;
#[cfg(all(test, unix))]
pub(crate) use test_fixtures::RestGuardForTest;
#[cfg(all(test, unix))]
pub(crate) use test_fixtures::claim_cross_channel_tmux_watcher_for_high_risk_test;
#[cfg(all(test, unix))]
pub(crate) use test_fixtures::gateway_context_for_test;
#[cfg(all(test, unix))]
pub(crate) use test_fixtures::input_mailbox_for_test;
#[cfg(all(test, unix))]
pub(crate) use test_fixtures::input_queue_path_for_test;
#[cfg(all(test, unix))]
pub(crate) use test_fixtures::ready_gateway_for_test;
#[cfg(test)]
pub(crate) use test_fixtures::register_resume_watcher_for_tests;
#[cfg(all(test, unix))]
pub(crate) use test_fixtures::rest_for_test;
#[cfg(test)]
pub(crate) use test_fixtures::resume_owner_channel_for_tests;
#[cfg(all(test, unix))]
pub(crate) use test_fixtures::revoke_notice_author_for_test;

mod tmux_error_detect;
mod turn_completion_events;
mod turn_end_summary;
pub(in crate::services::discord) mod turn_end_wip_warning;
pub(crate) use tmux_error_detect::{ProviderProseDiagnostic, classify_provider_prose_diagnostic};
mod live_bridge;
#[cfg(unix)]
mod tmux_lifecycle;
#[cfg(unix)]
mod tmux_overload_retry;
#[cfg(unix)]
mod tmux_reaper;
#[cfg(unix)]
mod tmux_restart_handoff;
mod tmux_watcher_registry;
#[rustfmt::skip]
#[cfg(test)]
mod tmux_watcher_registry_restore_tests;
#[cfg(test)]
mod krx_claude_tests;
#[cfg(all(test, unix))]
mod krx_claude_unix_tests;
#[cfg(test)]
mod krx_config_tests;
pub(crate) mod krx_session_host_test_host;
pub(crate) mod krx_tui_o_test_host;
#[cfg(test)]
mod relay_coord_tests;
pub(crate) mod terminal_delivery_custody;
mod tui_direct_abort_marker;
mod tui_direct_pending_start;
#[cfg(any(unix, test))]
pub(crate) use tui_direct_pending_start::turn_retirement::claude::note_ended as note_takeover_ended;
#[cfg(any(unix, test))]
mod herdr_owner_port;
#[cfg(any(unix, test))]
pub(crate) use herdr_owner_port::HERDR_OWNER_PORT;
#[cfg(test)]
pub(crate) use tui_direct_pending_start::turn_retirement::claude::{
    confirmed_now as claude_takeover_confirmed_now, tests::seed_takeover as seed_claude_takeover,
};
mod tui_prompt_relay;
pub(crate) use adk_runtime::services::discord::replay_policy;
use adk_runtime::services::discord::tui_task_card;
#[cfg(test)]
pub(crate) use session_notice::input_notice::INPUT_NOTICE_RUNS;
#[cfg(test)]
pub(crate) use session_notice::input_notice::input_notice_content;
pub(crate) use session_notice::input_notice::input_notice_line;
#[cfg(all(test, unix))]
pub(crate) use tmux::commit_codex_watcher_restore_for_tests;
#[cfg(all(test, unix))]
pub(crate) use tmux::commit_codex_watcher_restore_to_empty_registry_for_tests;
#[cfg(all(test, unix))]
pub(crate) use tui_prompt_relay::run_codex_rehydrate_pass_for_tests;
mod turn_bridge;
#[allow(clippy::too_many_arguments)]
mod turn_finalizer;
pub(crate) mod turn_lease;
mod turn_view_reconciler;
#[path = "watchers/lifecycle_decision.rs"]
mod watcher_lifecycle_decision;
pub(crate) mod zombie_foreground_release; // #5176 cancel/mailbox-release authority
pub(in crate::services::discord) use delivery_lease_cell::{
    DELIVERY_LEASE_DEADLINE_MS, DELIVERY_LEASE_HEARTBEAT_MS, DeliveryLeaseCell,
    DeliveryLeaseHeartbeat, LeaseHolder, LeaseOutcome, LeaseSnapshot, lease_now_ms,
};
pub(crate) use recovery_engine::CapturedReadyDeliveryCommit;
#[cfg(unix)]
pub(crate) use recovery_engine::herdr_reader::local_reconnect_health as herdr_reconnect_health;
#[allow(unused_imports)]
pub(in crate::services) use relay_coord::TmuxRelayCoord;
#[allow(unused_imports)]
pub(in crate::services::discord) use tmux_watcher_registry::{
    TMUX_WATCHER_STALE_HEARTBEAT_MS, TmuxWatcherBinding, TmuxWatcherHandle, TmuxWatcherRegistry,
    TmuxWatcherRegistryGuard, lock_tmux_watcher_registry, tmux_watcher_now_ms,
};
pub(in crate::services::discord) use {
    delivery_lease_key::DeliveryLeaseKey,
    relay_health::{RelayFrontierMutationGuard, RelayFrontierToken},
};
// #3479 item-2: re-export the catch-up subsystem entry points referenced
// outside the extracted cluster (`maybe_schedule_catch_up_retry_after_queue_drain`
// here in mod.rs and `catch_up_missed_messages` in runtime_bootstrap recovery).
pub(in crate::services::discord) use catch_up::{
    CatchUpRetryState, catch_up_missed_messages, catch_up_missed_messages_for_retry,
    should_trigger_catch_up_retry, take_catch_up_retry_checkpoint_after_queue_drain,
};
#[cfg(test)]
pub(in crate::services::discord) use mailbox_finish::mailbox_finish_cancelled_turn;
pub(in crate::services::discord) use mailbox_finish::{
    MailboxLookup, mailbox_clear_channel, mailbox_clear_recovery_marker,
    mailbox_finish_cancelled_turn_on, mailbox_finish_judged_turn, mailbox_finish_turn,
    mailbox_finish_turn_if_matches, mailbox_finish_turn_if_matches_episode_started_before,
    unavailable_finish_turn_result,
};
#[cfg(unix)]
pub(in crate::services::discord) use mailbox_probe::{
    mailbox_blocks_session_idle_commit, mailbox_has_active_turn_or_unreachable,
};
pub(in crate::services::discord) use mailbox_probe::{
    mailbox_has_active_turn, mailbox_has_blocking_active_turn,
    mailbox_has_blocking_active_turn_or_unreachable,
};
pub(in crate::services::discord) use recovery_engine as recovery;
// #3038 S1: re-export the extracted cluster type so the `SharedData` field
// declaration and constructor literals reference it without a module-qualified
// path (surface freeze, #3294/#3295 pattern).
pub(crate) use crate::domain::lifecycle::InflightRestartMode;
pub(crate) use crate::services::cluster::intake_turn::{IntakeExecution, IntakeRequest};
pub(crate) use crate::services::turn_orchestrator::HeadlessTurnStartError;
pub(crate) use queue_io::decision_drain;
pub(crate) use queue_io::input_ingress::takes_start as owner_takes_start;
#[cfg(all(test, unix))]
pub(crate) use queue_io::input_ingress::{
    ADOPTS as OWNER_ADOPTS_FOR_TEST, Hold as OwnerHoldForTest,
    execution as owner_execution_for_test, installed as input_owner_installed_for_test,
};
#[cfg(all(test, unix))]
pub(crate) use queue_io::input_owner::claude_handoff::observe_busy as observe_owned_claude_busy_for_test;
#[cfg(all(test, unix))]
pub(crate) use queue_io::input_owner::origin::Execution as OwnerExecutionForTest;
#[cfg(all(test, unix))]
pub(crate) use queue_io::input_owner::{
    drive as owner_drive_for_test, effect as owner_effect_for_test,
    held_watch as owner_held_watch_for_test, recovery as owner_recovery_for_test,
};
#[cfg(all(test, unix))]
pub(crate) use router::TurnKind;
#[cfg(test)]
pub(crate) use router::{checkpoint_waits_for_admission, execute_intake_turn_core};
#[cfg(unix)]
pub(crate) use session_relay_sink::run_session_bound_discord_relay_supervisor;
pub(in crate::services::discord) use shared_state::{
    PlaceholderState, PolicyRuntime, QueuedPlaceholderState, RuntimeHttpCache,
};
// #3038 S2: the cluster-D members were `pub(super)` on `SharedData` (visible up
// to `crate::services`), so the group type is re-exported with that same scope.
pub(in crate::services) use shared_state::SessionOverrideState;
// #3479 Item 3: the cluster members were `pub(super)` on `SharedData` (visible
// up to `crate::services`), so the group type is re-exported with that scope.
pub(in crate::services) use shared_state::DispatchRoutingState;
// #3038 S3: same scope rationale as S2 — the cluster-E members were
// `pub(super)` on `SharedData` (visible up to `crate::services`).
pub(in crate::services) use shared_state::RestartLifecycle;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
pub(crate) use turn_bridge::TmuxCleanupPolicy;

use tokio::sync::Mutex;

use crate::services::agent_protocol::{DEFAULT_ALLOWED_TOOLS, StreamMessage};
use crate::services::claude;
use crate::services::provider::{CancelToken, ProviderKind, ReadOutputResult};
use crate::services::turn_orchestrator::ChannelMailboxHandle;
use crate::services::turn_orchestrator::HasPendingSoftQueueResult;
use crate::ui::ai_screen::{self, HistoryItem, HistoryType};
use adk_session::{
    build_adk_session_key, derive_adk_session_info, lookup_pending_dispatch_for_thread,
    parse_dispatch_id,
};
pub(in crate::services) use compact_turn_authority::ManagedCompactTurnIdentity;
use compact_turn_authority::compact_eligible_turn_source;
use formatting::{format_for_discord, format_tool_input, send_long_message_raw, truncate_str};
use inflight::InflightTurnState;
#[cfg(test)]
pub(crate) use inflight::lock_inflight_state_path;
pub(crate) use inflight::open_tui_thread_hints;
#[cfg(test)]
use inflight::save_inflight_state;
pub(crate) use inflight::{clear_inflight_state, policy_op};
use placeholder_controller::queued_card_gate::{self, QueuedCardDisposition, QueuedCardTeardown};
use poise::serenity_prelude as serenity;
pub(in crate::services::discord) use prompt_builder::load_channel_recent_context;
use prompt_builder::{RecoveryContextManifestInput, build_system_prompt_with_manifest};
pub(in crate::services::discord) use queue_dispatch::MailboxEnqueueOutcome;
#[cfg(test)]
use queue_dispatch::mailbox_take_next_soft_intervention;
use queue_dispatch::{
    AutomaticQueueProgression, MailboxTakeNextSoftOutcome,
    automatic_progression as automatic_queue_progression,
    mailbox_abandon_unclaimed_dispatch_after_success, mailbox_requeue_intervention_front,
    mailbox_restore_dequeued_head, mailbox_take_next_automatic_intervention,
};
use recovery_engine::restore_inflight_turns;
use restart_report::flush_restart_reports;
use serenity::{ChannelId, MessageId, UserId};
#[cfg(test)]
use session_idle_cleanup::mark_session_disconnected_for_idle_cleanup;
use session_idle_cleanup::maybe_cleanup_sessions;
use session_status_hook::{
    post_canonical as post_adk_session_status_with_canonical_identity,
    post_channel_turn as post_adk_session_status_for_channel,
    post_legacy as post_adk_session_status,
};
use settings::{
    RoleBinding, channel_upload_dir, cleanup_old_uploads, load_bot_settings,
    load_last_session_path, resolve_role_binding,
    validate_bot_channel_routing_with_provider_channel,
};
pub(super) use skills_scan::scan_skills;
use skills_scan::skill_dir_fingerprint_with_projects;
#[cfg(unix)]
use tmux::restore_tmux_watchers;
#[cfg(unix)]
use tmux_reaper::{cleanup_orphan_tmux_sessions, reap_dead_tmux_sessions};
use turn_bridge::{TurnBridgeContext, spawn_turn_bridge, tmux_runtime_paths};

pub(crate) use crate::services::turn_orchestrator::has_soft_intervention_at;
pub(crate) use prompt_builder::{DispatchProfile, PromptProfiles};
pub(crate) use runtime_bootstrap::RunBotContext;
pub(crate) use runtime_bootstrap::run_bot;

#[cfg(test)]
use crate::services::turn_orchestrator::QueuePersistenceContext;
use crate::services::turn_orchestrator::{
    ActiveTurnKind, CancelActiveTurnResult, CancelQueuedMessageResult, ChannelMailboxSnapshot,
    FinishTurnResult, HydratePendingQueueResult, PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER,
    QueueExitEvent, QueueExitKind, RecoveryKickoffResult, VALVE_CLEARED_DISPATCH_MARKER_GRACE,
    load_channel_pending_dispatch_marker, load_pending_dispatch_markers, load_pending_queues,
    warn_legacy_pending_queue_files,
};
pub(super) use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, Intervention, InterventionMode, MAX_INTERVENTIONS_PER_CHANNEL,
    PendingQueueItem,
};
pub use discord_io::{
    retry_failed_dm_notifications, send_file_to_channel, send_message_to_channel,
    send_message_to_user,
};
pub(in crate::services::discord) use dispatch_policy::{
    is_allowed_turn_sender, prepend_monitor_auto_turn_origin, resolve_announce_bot_user_id,
    should_phase2_recover_message, stale_dispatch_turn_for_queued_intervention,
    stale_dispatch_turn_for_text, strip_monitor_auto_turn_origin,
};
pub use settings::{
    load_discord_bot_launch_configs, resolve_discord_bot_provider, resolve_discord_token_by_hash,
};
// #2047 Finding 5 — expose the role-map resolver so HTTP channel lookups can deny channels that are
// not registered with this AgentDesk instance.
pub(crate) use settings::resolve_role_binding as resolve_channel_role_binding;

/// Discord message length limit
pub(super) use crate::domain::message_split::DISCORD_MSG_LIMIT;

#[cfg(test)]
pub(in crate::services::discord) use synthetic_headless_message_id::SYNTHETIC_HEADLESS_MESSAGE_ID_FLOOR;
pub(in crate::services::discord) use synthetic_headless_message_id::is_synthetic_headless_message_id_raw;
pub(in crate::services::discord) use synthetic_headless_message_id::{
    INTERNAL_MESSAGE_ID_START, is_internal_message_id,
};
mod bot_runtime;
pub(super) use bot_runtime::Context;
#[cfg(unix)]
use bot_runtime::DEAD_SESSION_REAP_INTERVAL;
use bot_runtime::DEFERRED_RESTART_POLL_INTERVAL;
pub(super) use bot_runtime::Data;
pub(super) use bot_runtime::Error;
pub(super) use bot_runtime::IdleQueueKickoffChannelOutcome;
use bot_runtime::RESTART_REPORT_FLUSH_INTERVAL;
use bot_runtime::SESSION_CLEANUP_INTERVAL;
use bot_runtime::SESSION_MAX_IDLE;
use bot_runtime::UPLOAD_CLEANUP_INTERVAL;
use bot_runtime::UPLOAD_MAX_AGE;
pub(in crate::services::discord) use bot_runtime::advance_last_message_checkpoint;
#[cfg(test)]
pub(super) use bot_runtime::check_deferred_restart;
pub(crate) use bot_runtime::has_fresh_inflight_for_channel;
#[cfg(test)]
use bot_runtime::increment_counter;
pub(in crate::services::discord) use bot_runtime::increment_global_active;
pub(crate) use bot_runtime::inflight_state_allows_idle_tmux_repair_for_channel;
pub(crate) use bot_runtime::mark_reconcile_complete;
pub(in crate::services::discord) use bot_runtime::saturating_decrement_counter;
pub(in crate::services::discord) use bot_runtime::saturating_decrement_global_active;
pub(super) use bot_runtime::single_message_panel_enabled;
pub(super) use bot_runtime::status_update_interval;

pub(crate) fn clear_inflight_state_for_channel(provider: &ProviderKind, channel_id: u64) -> bool {
    inflight::clear_inflight_state(provider, channel_id)
}

pub(in crate::services::discord) use recovery_known_ids::{
    RecoveryKnownIdArm, queued_message_ids, recovery_known_arms_and_ids,
};

#[cfg(test)]
mod last_message_checkpoint_tests {
    use super::*;

    struct ScopedRuntimeRoot {
        _lock: std::sync::MutexGuard<'static, ()>,
        temp: tempfile::TempDir,
        previous: Option<std::ffi::OsString>,
    }

    impl ScopedRuntimeRoot {
        fn path(&self) -> &std::path::Path {
            self.temp.path()
        }
    }

    impl Drop for ScopedRuntimeRoot {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                    None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
                }
            }
        }
    }

    fn scoped_runtime_root() -> ScopedRuntimeRoot {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        let temp = tempfile::tempdir().expect("last-message runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        ScopedRuntimeRoot {
            _lock: lock,
            temp,
            previous,
        }
    }

    fn last_message_path(
        root: &std::path::Path,
        provider: &ProviderKind,
        channel_id: ChannelId,
    ) -> std::path::PathBuf {
        root.join("runtime")
            .join("last_message")
            .join(provider.as_str())
            .join(format!("{}.txt", channel_id.get()))
    }

    #[test]
    fn advance_last_message_checkpoint_interleaved_advances_keep_max() {
        let root = scoped_runtime_root();
        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_162_000);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

        let low_shared = std::sync::Arc::clone(&shared);
        let low_barrier = std::sync::Arc::clone(&barrier);
        let low = std::thread::spawn(move || {
            low_barrier.wait();
            advance_last_message_checkpoint(
                &low_shared,
                &ProviderKind::Claude,
                channel_id,
                MessageId::new(90_001),
            )
        });

        let high_shared = std::sync::Arc::clone(&shared);
        let high_barrier = std::sync::Arc::clone(&barrier);
        let high = std::thread::spawn(move || {
            high_barrier.wait();
            advance_last_message_checkpoint(
                &high_shared,
                &ProviderKind::Claude,
                channel_id,
                MessageId::new(90_002),
            )
        });

        barrier.wait();
        let _ = low.join().expect("low checkpoint thread");
        let _ = high.join().expect("high checkpoint thread");

        assert_eq!(
            shared.last_message_ids.get(&channel_id).map(|entry| *entry),
            Some(90_002)
        );
        let path = last_message_path(root.path(), &provider, channel_id);
        assert_eq!(
            std::fs::read_to_string(&path)
                .expect("checkpoint file")
                .trim(),
            "90002"
        );

        let mut stale_snapshot = std::collections::HashMap::new();
        stale_snapshot.insert(channel_id.get(), 90_001);
        runtime_store::save_all_last_message_ids(provider.as_str(), &stale_snapshot);
        assert_eq!(
            std::fs::read_to_string(path)
                .expect("checkpoint file after stale full-map save")
                .trim(),
            "90002"
        );
    }
}

#[cfg(test)]
pub(crate) use crate::services::dispatches::thread_archive_defer::should_defer_thread_archive_pg;
pub(in crate::services::discord) use queue_io::{
    arm_slow_idle_queue_backstop_if_queue_nonempty, schedule_deferred_idle_queue_kickoff,
    schedule_deferred_idle_queue_kickoff_immediate, spawn_turn_completion_idle_queue_listener,
};

#[cfg(test)]
pub(crate) use router::try_intake_runtime_transition_after_redirect;
#[cfg(test)]
pub(crate) use session_runtime::resume_launch_state_for_tests;
#[cfg(all(unix, test))]
pub(crate) use tui_prompt_relay::rehydration::rehydrate_codex_tui_binding_for_tests;

#[cfg(test)]
mod global_active_counter_tests {
    use super::{increment_counter, saturating_decrement_counter};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn saturating_decrement_counter_does_not_underflow_zero() {
        let counter = AtomicUsize::new(0);

        assert!(!saturating_decrement_counter(&counter));
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn saturating_decrement_counter_decrements_positive_value() {
        let counter = AtomicUsize::new(2);

        assert!(saturating_decrement_counter(&counter));
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn increment_counter_increments_once_and_returns_new_value() {
        let counter = AtomicUsize::new(0);

        assert_eq!(increment_counter(&counter, "unit_test"), 1);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn paired_increment_then_decrement_keeps_counter_balanced() {
        let counter = AtomicUsize::new(0);

        // A single activated mailbox transition: +1 on start, -1 on finish.
        increment_counter(&counter, "mailbox_started");
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        assert!(saturating_decrement_counter(&counter));
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn increment_counter_is_strictly_additive_across_repeated_calls() {
        let counter = AtomicUsize::new(0);

        for expected in 1..=6 {
            assert_eq!(increment_counter(&counter, "repeated_site"), expected);
        }
        assert_eq!(counter.load(Ordering::Relaxed), 6);
    }
}

use session_runtime::{
    DiscordSession, RuntimeChannelBindingStatus, WorktreeInfo, auto_restore_session,
    auto_restore_session_force, auto_restore_session_with_dm_hint, bootstrap_thread_session,
    create_git_worktree, detect_worktree_conflict, provider_handles_channel,
    rebind_channel_session, resolve_channel_category, resolve_is_dm_channel,
    resolve_reusable_worktree, resolve_runtime_channel_binding_status, resolve_thread_parent,
    select_restored_session_path, synthetic_thread_channel_name, validate_live_channel_routing,
    validate_live_channel_routing_with_dm_hint,
};

mod bot_settings_state;
use bot_settings_state::CHANNEL_ROSTER_MAX_USERS;
pub(super) use bot_settings_state::DiscordBotSettings;
pub(super) use bot_settings_state::ModelPickerPendingState;
pub(super) use bot_settings_state::UserRecord;

pub(super) use crate::services::engine_runtime::session::CoreState;

mod shared_data;
pub(crate) use shared_data::SharedData;
#[cfg(test)]
pub(crate) use shared_data::make_shared_data_for_tests;
#[cfg(test)]
pub(super) use shared_data::make_shared_data_for_tests_with_barrier;
#[cfg(test)]
pub(crate) use shared_data::make_shared_data_for_tests_with_storage;

use queue_dispatch::persistence_context as queue_persistence_context;

mod mailbox_ops;
use mailbox_ops::cleanup_retry_inflight_blocks_idle_kickoff;
use mailbox_ops::ensure_cancel_token_bound_from_inflight_state;
use mailbox_ops::idle_queue_channel_has_kickable_backlog;
use mailbox_ops::idle_queue_snapshot_has_kickable_backlog;
use mailbox_ops::idle_queue_snapshot_has_pending_or_marker_backlog;
use mailbox_ops::idle_queue_take_next_soft_if_ready;
use mailbox_ops::infer_inflight_tmux_session_for_channel;
pub(in crate::services::discord) use mailbox_ops::mailbox_abandon_pending_dispatch;
use mailbox_ops::mailbox_clear_pending_dispatch_reservation;
use mailbox_ops::mailbox_enqueue_intervention;
use mailbox_ops::mailbox_has_pending_soft_queue;
use mailbox_ops::mailbox_hydrate_pending_queue_from_disk;
use mailbox_ops::mailbox_merge_restored_dispatch_marker;
use mailbox_ops::mailbox_merge_restored_queue_items;
use mailbox_ops::mailbox_queue_snapshots;
use mailbox_ops::mailbox_restart_drain_all;
use mailbox_ops::mailbox_snapshot;
use mailbox_ops::mailbox_try_start_turn;
use mailbox_ops::mailbox_try_start_turn_kinded;
use mailbox_ops::maybe_schedule_catch_up_retry_after_queue_drain;
pub(crate) use mailbox_ops::record_turn_stop_tombstone;

mod mailbox_stop;
use mailbox_stop::mailbox_cancel_active_turn;

use queue_io::mailbox_recovery_kickoff;

mod queue_exit;
use queue_exit::QueueExitVisibleCard;
use queue_exit::apply_queue_exit_feedback;
pub(in crate::services::discord) use queue_exit::drain_pending_queue_exit_placeholder_clears;
pub(in crate::services::discord) use queue_exit::drain_pending_queue_exit_placeholder_clears_with;
pub(in crate::services::discord) use queue_exit::enqueue_internal_followup;
use queue_exit::queue_exit_card_body;
#[cfg(test)]
use queue_exit::queue_exit_drain_queued_placeholders;
pub(in crate::services::discord) use queue_exit::queue_exit_feedback_emoji;

struct QueueExitPendingPlaceholderDeleter {
    http: Arc<serenity::Http>,
}

impl placeholder_controller::queued_card_gate::StalePlaceholderDeleter
    for QueueExitPendingPlaceholderDeleter
{
    fn delete<'a>(
        &'a self,
        channel_id: ChannelId,
        placeholder_msg_id: MessageId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            channel_id
                .delete_message(&self.http, placeholder_msg_id)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }
}

#[cfg(test)]
mod queue_exit_feedback_reconciler_tests {
    use super::*;

    impl SharedData {
        pub(crate) fn queue_fixture_parts(&self) -> (&ChannelMailboxRegistry, &str) {
            (&self.mailboxes, &self.token_hash)
        }
    }

    struct ScopedRuntimeRoot {
        _lock: std::sync::MutexGuard<'static, ()>,
        _temp: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }

    impl Drop for ScopedRuntimeRoot {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                    None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
                }
            }
        }
    }

    #[must_use]
    fn scoped_runtime_root() -> ScopedRuntimeRoot {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        let temp = tempfile::tempdir().expect("create temp runtime dir for feedback test");
        unsafe {
            std::env::set_var(
                "AGENTDESK_ROOT_DIR",
                temp.path().to_str().expect("temp path must be valid utf-8"),
            );
        }
        ScopedRuntimeRoot {
            _lock: lock,
            _temp: temp,
            prev,
        }
    }

    fn queue_exit_intervention(message_id: MessageId) -> Intervention {
        Intervention {
            author_id: UserId::new(7),
            author_is_bot: false,
            message_id,
            queued_generation: 91,
            source_message_ids: vec![message_id],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: "queued text".to_string(),
            mode: InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
        }
    }

    #[tokio::test]
    async fn apply_queue_exit_feedback_adds_feedback_reaction_through_reconciler() {
        let _root = scoped_runtime_root();
        let shared = make_shared_data_for_tests();
        let _ = shared
            .http
            .cached_bot_token
            .set("Bot test-token".to_string());
        let channel_id = ChannelId::new(100_000_000_000_231);
        let cases = [
            (
                MessageId::new(100_000_000_000_232),
                QueueExitKind::Cancelled,
            ),
            (MessageId::new(100_000_000_000_233), QueueExitKind::Expired),
            (
                MessageId::new(100_000_000_000_234),
                QueueExitKind::Superseded,
            ),
            (MessageId::new(100_000_000_000_235), QueueExitKind::Overflow),
        ];

        for (message_id, kind) in cases {
            let event = QueueExitEvent {
                intervention: queue_exit_intervention(message_id),
                kind,
            };
            apply_queue_exit_feedback(&shared, channel_id, &[event]).await;
            let emoji = queue_exit_feedback_emoji(kind);

            assert!(
                shared.turn_view_reconciler.ops().iter().any(|op| {
                    op.target.channel_id == channel_id
                        && op.target.message_id == message_id
                        && op.add
                        && op.emoji == emoji
                }),
                "{emoji} queue-exit feedback must route through the reconciler"
            );
        }
    }
}

pub(in crate::services::discord) use busy_followup_retry_store::requeue_inflight_for_followup_retry as mailbox_requeue_inflight_for_followup_retry;

#[cfg(test)]
mod followup_retry_requeue_tests {
    use super::*;

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    struct EnvGuard {
        previous: Option<String>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.as_deref() {
                Some(value) => unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, value) },
                None => unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) },
            }
        }
    }

    fn followup_inflight(
        channel_id: ChannelId,
        user_msg_id: MessageId,
        preserve_on_cancel: bool,
    ) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            channel_id.get(),
            Some("adk-cc".to_string()),
            42,
            user_msg_id.get(),
            user_msg_id.get() + 1,
            "please continue".to_string(),
            Some("session-3752".to_string()),
            Some("AgentDesk-claude-3752".to_string()),
            Some("/tmp/agentdesk-3752.jsonl".to_string()),
            None,
            0,
        );
        state.set_followup_requeue_context(
            Some("reply context".to_string()),
            true,
            false,
            vec![
                crate::services::cluster::attachment_transfer::uploads::Upload::from(
                    "attachment-a",
                ),
                crate::services::cluster::attachment_transfer::uploads::Upload::from(
                    "attachment-b",
                ),
            ],
            preserve_on_cancel,
        );
        state
    }

    #[test]
    fn pre_submit_requeue_preserves_context_and_returns_enqueue_outcome() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard {
            previous: std::env::var(AGENTDESK_ROOT_DIR_ENV).ok(),
        };
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path()) };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let shared = make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel_id = ChannelId::new(3_752_001);
            let user_msg_id = MessageId::new(3_752_101);
            let retry_user_msg_id = MessageId::new(3_752_100);
            let mut state = followup_inflight(channel_id, user_msg_id, false);
            state.busy_followup_retry_user_msg_id = retry_user_msg_id.get();

            let outcome =
                mailbox_requeue_inflight_for_followup_retry(&shared, &provider, channel_id, &state)
                    .await;

            assert!(outcome.enqueued);
            assert!(!outcome.merged);
            assert_eq!(outcome.refusal_reason, None);
            assert_eq!(outcome.persistence_error, None);

            let snapshot = mailbox_snapshot(&shared, channel_id).await;
            assert_eq!(snapshot.intervention_queue.len(), 1);
            let intervention = &snapshot.intervention_queue[0];
            assert_eq!(intervention.author_id, UserId::new(42));
            assert_eq!(intervention.message_id, user_msg_id);
            assert_eq!(
                intervention.source_message_ids,
                vec![user_msg_id, retry_user_msg_id],
                "retry requeue must preserve the canonical source identity across another drain"
            );
            assert_eq!(intervention.text, "please continue");
            assert_eq!(intervention.reply_context.as_deref(), Some("reply context"));
            assert!(intervention.has_reply_boundary);
            assert!(!intervention.merge_consecutive);
            assert_eq!(
                intervention.pending_uploads,
                vec![
                    crate::services::cluster::attachment_transfer::uploads::Upload::from(
                        "attachment-a"
                    ),
                    crate::services::cluster::attachment_transfer::uploads::Upload::from(
                        "attachment-b"
                    )
                ]
            );
        });
    }

    /// #4247 FIX 2 (mutation-provable): a PRE-submit busy-timeout requeue of a
    /// genuine-human turn whose `followup_preserve_on_cancel` decision was
    /// stored as `true` must reconstruct a MARKED `Intervention` (non-empty
    /// `source_message_queued_generations`, `preserve_on_cancel() == true`).
    /// Mutating `mailbox_requeue_inflight_for_followup_retry` back to the
    /// unconditional `Vec::new()` this fix replaced makes this assertion fail
    /// (not a compile error).
    #[test]
    fn pre_submit_requeue_of_marked_followup_reconstructs_marked_intervention() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard {
            previous: std::env::var(AGENTDESK_ROOT_DIR_ENV).ok(),
        };
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path()) };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let shared = make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel_id = ChannelId::new(3_752_003);
            let user_msg_id = MessageId::new(3_752_103);
            let state = followup_inflight(channel_id, user_msg_id, true);

            let outcome =
                mailbox_requeue_inflight_for_followup_retry(&shared, &provider, channel_id, &state)
                    .await;
            assert!(outcome.enqueued);

            let snapshot = mailbox_snapshot(&shared, channel_id).await;
            assert_eq!(snapshot.intervention_queue.len(), 1);
            let intervention = &snapshot.intervention_queue[0];
            assert!(
                !intervention.source_message_queued_generations.is_empty(),
                "a marked followup requeue must not reconstruct an unmarked (empty) intervention"
            );
            assert!(
                intervention.preserve_on_cancel(),
                "a marked followup requeue must carry preserve_on_cancel() == true"
            );
        });
    }

    #[test]
    fn inflight_retry_restores_earlier_message_without_reversing_fifo_4797() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard {
            previous: std::env::var(AGENTDESK_ROOT_DIR_ENV).ok(),
        };
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path()) };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let shared = make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel_id = ChannelId::new(4_797_001);
            let first_id = MessageId::new(4_797_101);
            let later_id = MessageId::new(4_797_102);
            let state = followup_inflight(channel_id, first_id, false);
            let later = Intervention {
                author_id: UserId::new(43),
                author_is_bot: true,
                message_id: later_id,
                queued_generation: shared.restart.current_generation,
                source_message_ids: vec![later_id],
                source_message_queued_generations: Vec::new(),
                source_text_segments: Vec::new(),
                text: "later bot B".to_string(),
                mode: crate::services::turn_orchestrator::InterventionMode::Soft,
                created_at: std::time::Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
            };
            mailbox_enqueue_intervention(&shared, &provider, channel_id, later.clone()).await;

            let retry =
                mailbox_requeue_inflight_for_followup_retry(&shared, &provider, channel_id, &state)
                    .await;
            assert!(retry.enqueued);

            let snapshot = mailbox_snapshot(&shared, channel_id).await;
            let order: Vec<_> = snapshot
                .intervention_queue
                .iter()
                .map(|item| item.message_id)
                .collect();
            assert_eq!(order, vec![first_id, later_id]);
            assert!(!snapshot.intervention_queue[0].author_is_bot);
            assert!(snapshot.intervention_queue[1].author_is_bot);

            let duplicate =
                mailbox_requeue_inflight_for_followup_retry(&shared, &provider, channel_id, &state)
                    .await;
            assert!(!duplicate.enqueued);
            assert_eq!(
                duplicate.refusal_reason,
                Some(
                    crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdAlreadyQueued
                )
            );
            let snapshot = mailbox_snapshot(&shared, channel_id).await;
            assert_eq!(
                snapshot.intervention_queue.len(),
                2,
                "duplicate inflight retry must not create a second queued prompt"
            );
        });
    }
}

use queue_dispatch::kickoff::{
    kickoff_idle_queue_channel, kickoff_idle_queues, kickoff_idle_queues_with_deps,
};

use discord_io::{check_auth, check_owner, rate_limit_wait, try_handle_pending_dm_reply};

// ─── Event handler ───────────────────────────────────────────────────────────

#[cfg(test)]
mod idle_cleanup_selector_tests {
    use super::mark_session_disconnected_for_idle_cleanup;

    struct TestPostgresDb {
        _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let lifecycle = crate::db::postgres::lock_test_lifecycle();
            let base = crate::db::postgres::postgres_test_database_url_base()
                .expect("POSTGRES_TEST_DATABASE_URL_BASE required for idle selector tests"); // agentdesk-audit: allow-unwrap — test-only fixture constructor requires an explicitly configured shared base
            let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "postgres".to_string());
            let admin_url = format!("{base}/{admin_db}");
            let database_name =
                format!("agentdesk_idle_selector_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{base}/{database_name}");
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "idle selector tests",
            )
            .await
            .expect("create idle selector postgres test db");

            Self {
                _lifecycle: lifecycle,
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "idle selector tests",
            )
            .await
            .expect("apply idle selector postgres migrations")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "idle selector tests",
            )
            .await
            .expect("drop idle selector postgres test db");
        }
    }

    #[tokio::test]
    async fn idle_cleanup_preserves_provider_selector_columns_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "host:idle-selector-preserve";

        sqlx::query(
            "INSERT INTO sessions
             (session_key, status, active_dispatch_id, claude_session_id,
              raw_provider_session_id, created_at)
             VALUES ($1, 'idle', 'dispatch-1841', 'claude-selector-1841',
                     'raw-selector-1841', NOW())",
        )
        .bind(session_key)
        .execute(&pool)
        .await
        .unwrap();

        assert!(mark_session_disconnected_for_idle_cleanup(Some(&pool), session_key).await);

        let row = sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<String>)>(
            "SELECT status, active_dispatch_id, claude_session_id, raw_provider_session_id
             FROM sessions
             WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "disconnected");
        assert_eq!(row.1, None);
        assert_eq!(row.2.as_deref(), Some("claude-selector-1841"));
        assert_eq!(row.3.as_deref(), Some("raw-selector-1841"));

        pool.close().await;
        pg_db.drop().await;
    }
}

// ─── Slash commands (extracted to commands/ module) ──────────────────────────

// Command functions removed — see commands/ submodule.
// Remaining in mod.rs: detect_worktree_conflict, create_git_worktree, cleanup_git_worktree,
// send_file_to_channel, send_message_to_channel, send_message_to_user, auto_restore_session,
// bootstrap_thread_session, resolve_channel_category, and other non-command functions.

// ─── Text message → Claude AI ───────────────────────────────────────────────

// #3167 — a queued external USER intervention must be kickable while a
// low-priority Background turn (monitor relay / self-paced TUI loop) holds the
// active-turn slot, and the dequeue gate must cancel the background token so
// the slot is released and the user turn can claim it.
#[cfg(test)]
mod idle_queue_background_supersede_tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    struct EnvGuard;

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
        }
    }

    fn user_intervention(message_id: u64, text: &str) -> Intervention {
        Intervention {
            author_id: UserId::new(7),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            queued_generation: crate::services::discord::runtime_store::process_generation(),
            source_message_ids: vec![MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_transition_preserves_queued_head_order_until_release() {
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = crate::config::set_agentdesk_root_for_test(tmp.path());

        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_794_200);
        let first = user_intervention(4_794_201, "first");
        let second = user_intervention(4_794_202, "second");
        shared
            .mailbox(channel_id)
            .replace_queue(
                vec![first.clone(), second.clone()],
                queue_persistence_context(&shared, &provider, channel_id),
            )
            .await;

        let transition_guard = shared
            .session_transition_locks
            .lock(channel_id)
            .lock_owned()
            .await;
        let blocked = idle_queue_take_next_soft_if_ready(&shared, &provider, channel_id).await;
        assert!(
            blocked.intervention.is_none(),
            "transition ownership must defer kickoff before dequeue"
        );
        let blocked_snapshot = mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(
            blocked_snapshot
                .intervention_queue
                .iter()
                .map(|item| item.message_id)
                .collect::<Vec<_>>(),
            vec![first.message_id, second.message_id],
            "deferred kickoff must preserve FIFO without tail requeue"
        );
        assert_eq!(blocked_snapshot.pending_user_dispatch, None);

        drop(transition_guard);
        let released = idle_queue_take_next_soft_if_ready(&shared, &provider, channel_id).await;
        assert_eq!(
            released.intervention.as_ref().map(|item| item.message_id),
            Some(first.message_id),
            "the original head must dequeue first after transition release"
        );
        assert_eq!(
            mailbox_snapshot(&shared, channel_id)
                .await
                .intervention_queue
                .iter()
                .map(|item| item.message_id)
                .collect::<Vec<_>>(),
            vec![second.message_id]
        );
    }

    // SAFETY (await_holding_lock): the test-env Mutex is held across awaits to
    // serialize the process-global `AGENTDESK_ROOT_DIR` env mutation against
    // other tests in this crate. #3167 B3: this MUST be the single crate-wide
    // `test_support` lock shared with the turn_orchestrator env tests — a local
    // per-module Mutex would not serialize against them and would recreate the
    // parallel env-race. Test-only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn queued_user_message_is_kickable_under_background_turn_and_cancels_token() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(3_167_900);

        // A background turn (monitor relay / TUI loop) holds the slot.
        let background_token = Arc::new(CancelToken::new());
        assert!(
            mailbox_try_start_turn_kinded(
                &shared,
                channel_id,
                background_token.clone(),
                UserId::new(1),
                MessageId::new(1),
                ActiveTurnKind::Background,
            )
            .await
        );

        // Queue an external user intervention behind it.
        shared
            .mailbox(channel_id)
            .replace_queue(
                vec![user_intervention(900, "user reply while loop runs")],
                queue_persistence_context(&shared, &provider, channel_id),
            )
            .await;

        // #3167 — the kickoff gate must treat the background turn as
        // non-blocking and report a kickable backlog.
        let snapshot = mailbox_snapshot(&shared, channel_id).await;
        assert!(
            idle_queue_snapshot_has_kickable_backlog(&shared, &provider, channel_id, &snapshot),
            "#3167: a queued user message must be kickable under a background turn"
        );

        // The dequeue gate detects the background turn still holding the slot,
        // cancels its token, and defers (returns no intervention this pass).
        let outcome = idle_queue_take_next_soft_if_ready(&shared, &provider, channel_id).await;
        assert!(
            outcome.intervention.is_none(),
            "#3167: the supersede pass defers the dequeue until the background slot releases"
        );
        assert!(
            background_token.cancelled.load(Ordering::Relaxed),
            "#3167: the background token must be cancelled so the slot is released"
        );
    }

    #[test]
    fn stale_marker_only_dispatch_reservation_is_kickable() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let shared = make_shared_data_for_tests();
                let provider = ProviderKind::Claude;
                let channel_id = ChannelId::new(3_167_901);
                let head = user_intervention(901, "task died before claim");
                shared
                    .mailbox(channel_id)
                    .replace_queue(
                        vec![head],
                        queue_persistence_context(&shared, &provider, channel_id),
                    )
                    .await;
                let taken = shared
                    .mailbox(channel_id)
                    .take_next_soft(queue_persistence_context(&shared, &provider, channel_id))
                    .await;
                assert_eq!(
                    taken.intervention.as_ref().map(|item| item.message_id),
                    Some(MessageId::new(901))
                );
                drop(taken);
                shared
                    .mailbox(channel_id)
                    .age_inbound_waits_for_test(
                        PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER
                            + std::time::Duration::from_secs(1),
                    )
                    .await;

                let snapshot = mailbox_snapshot(&shared, channel_id).await;

                assert!(
                    idle_queue_snapshot_has_kickable_backlog(
                        &shared, &provider, channel_id, &snapshot
                    ),
                    "stale marker-only reservations must wake the drain loop so TakeNextSoft can self-heal"
                );
            });
    }

    #[test]
    fn consume_without_claim_cleanup_clears_marker_and_unblocks_next_head() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let shared = make_shared_data_for_tests();
                let provider = ProviderKind::Claude;
                let channel_id = ChannelId::new(3_167_902);
                let consumed = user_intervention(902, "/goal done");
                let next = user_intervention(903, "next queued user reply");
                shared
                    .mailbox(channel_id)
                    .replace_queue(
                        vec![consumed.clone(), next.clone()],
                        queue_persistence_context(&shared, &provider, channel_id),
                    )
                    .await;
                let mut first = shared
                    .mailbox(channel_id)
                    .take_next_soft(queue_persistence_context(&shared, &provider, channel_id))
                    .await;
                let dispatch_lease = first
                    .dispatch_lease
                    .take()
                    .expect("dequeued consumed head should carry a lease");
                assert_eq!(
                    first.intervention.as_ref().map(|item| item.message_id),
                    Some(consumed.message_id)
                );
                assert_eq!(
                    load_channel_pending_dispatch_marker(&provider, &shared.token_hash, channel_id)
                        .map(|(marker, _)| marker.message_id),
                    Some(consumed.message_id)
                );

                mailbox_abandon_unclaimed_dispatch_after_success(
                    &shared,
                    &provider,
                    channel_id,
                    consumed.message_id,
                    dispatch_lease.clone(),
                )
                .await;

                assert_eq!(
                    std::sync::Arc::strong_count(&dispatch_lease),
                    1,
                    "post-success abandon releases the actor-held lease"
                );
                let snapshot = mailbox_snapshot(&shared, channel_id).await;
                assert_eq!(snapshot.pending_user_dispatch, None);
                assert!(
                    load_channel_pending_dispatch_marker(&provider, &shared.token_hash, channel_id)
                        .is_none(),
                    "consumed-without-claim head marker must be cleared"
                );
                let hydrate = shared
                    .mailbox(channel_id)
                    .hydrate_pending_queue_from_disk(queue_persistence_context(
                        &shared, &provider, channel_id,
                    ))
                    .await;
                assert_eq!(hydrate.absorbed, 0);
                let second = shared
                    .mailbox(channel_id)
                    .take_next_soft(queue_persistence_context(&shared, &provider, channel_id))
                    .await;
                assert_eq!(
                    second.intervention.as_ref().map(|item| item.message_id),
                    Some(next.message_id),
                    "next queued head should dispatch instead of starving behind consumed head"
                );
                assert_eq!(
                    load_channel_pending_dispatch_marker(&provider, &shared.token_hash, channel_id)
                        .map(|(marker, _)| marker.message_id),
                    Some(next.message_id),
                    "next head receives the only remaining marker"
                );
            });
    }

    #[test]
    fn stale_success_cleanup_cannot_abandon_same_identity_successor_lease() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let shared = make_shared_data_for_tests();
                let provider = ProviderKind::Claude;
                let channel_id = ChannelId::new(4_797_904);
                let intervention = user_intervention(4_797_905, "same identity successor");
                let persistence = queue_persistence_context(&shared, &provider, channel_id);
                shared
                    .mailbox(channel_id)
                    .replace_queue(vec![intervention.clone()], persistence.clone())
                    .await;

                let first = shared
                    .mailbox(channel_id)
                    .take_next_soft(persistence.clone())
                    .await;
                let stale_lease = first
                    .dispatch_lease
                    .expect("first dequeue must carry lease L1");
                let restored = shared
                    .mailbox(channel_id)
                    .restore_dequeued_head(
                        first.intervention.expect("first dequeue must return head"),
                        persistence.clone(),
                        stale_lease.clone(),
                    )
                    .await;
                assert!(restored.enqueued);

                let second = shared.mailbox(channel_id).take_next_soft(persistence).await;
                let successor_lease = second
                    .dispatch_lease
                    .expect("successor dequeue must carry lease L2");
                assert_eq!(
                    second.intervention.as_ref().map(|item| item.message_id),
                    Some(intervention.message_id)
                );

                mailbox_abandon_unclaimed_dispatch_after_success(
                    &shared,
                    &provider,
                    channel_id,
                    intervention.message_id,
                    stale_lease,
                )
                .await;

                let snapshot = mailbox_snapshot(&shared, channel_id).await;
                assert_eq!(
                    snapshot.pending_user_dispatch,
                    Some(intervention.message_id)
                );
                assert_eq!(
                    load_channel_pending_dispatch_marker(&provider, &shared.token_hash, channel_id)
                        .map(|(marker, _)| marker.message_id),
                    Some(intervention.message_id),
                    "stale L1 cleanup must preserve L2's durable marker"
                );
                assert_eq!(
                    Arc::strong_count(&successor_lease),
                    2,
                    "stale L1 cleanup must preserve the actor-held L2 reservation"
                );
            });
    }
}

// #3038 S0 — characterization tests for the queued-placeholder cluster (cluster
// C) method surface. These fix the observable behaviour (map round-trips,
// sidecar mirroring, ownership recheck branches, and per-channel persist-lock
// identity) BEFORE the field group is extracted into `QueuedPlaceholderState`,
// so the same tests passing unchanged after the move is the equivalence proof.
// The tests call only the method surface (never the fields directly).
#[cfg(test)]
mod queued_placeholder_cluster_characterization_tests {
    use super::*;

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    struct EnvGuard;

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
        }
    }

    fn sidecar_path(
        root: &std::path::Path,
        subdir: &str,
        channel_id: ChannelId,
    ) -> std::path::PathBuf {
        // Mirrors queued_placeholders_store's
        // `<AGENTDESK_ROOT>/runtime/<subdir>/<provider>/<token_hash>/<channel>.json`
        // layout for the values `make_shared_data_for_tests` constructs
        // (`ProviderKind::Claude`, `token_hash == "test-token-hash"`).
        root.join("runtime")
            .join(subdir)
            .join("claude")
            .join("test-token-hash")
            .join(format!("{}.json", channel_id.get()))
    }

    // Build a current-thread tokio runtime so the async cluster methods can be
    // driven from a synchronous `#[test]`. Keeping the test fn synchronous means
    // the `test_support` env lock (a `std::sync::Mutex` guard) is never held
    // across an `.await` in this scope, so it needs no
    // `#[allow(clippy::await_holding_lock)]` and does not move the ratchet.
    fn test_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn insert_remove_queued_placeholder_round_trip_with_sidecar() {
        // #3167 B3: serialize the process-global `AGENTDESK_ROOT_DIR` mutation via
        // the single crate-wide `test_support` lock (no local per-module Mutex).
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let shared = make_shared_data_for_tests();
        let channel_id = ChannelId::new(3_038_100);
        let user_msg_id = MessageId::new(11);
        let placeholder_msg_id = MessageId::new(22);

        let path = sidecar_path(tmp.path(), "discord_queued_placeholders", channel_id);
        let removed = test_rt().block_on(async {
            // The locked insert variant runs under a caller-held persist lock.
            let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
            {
                let _guard = persist_lock.lock().await;
                shared.insert_queued_placeholder_locked(
                    channel_id,
                    user_msg_id,
                    placeholder_msg_id,
                );
            }

            // Memory: the mapping is owned by exactly the placeholder we inserted.
            assert!(shared.queued_placeholder_still_owned(
                channel_id,
                user_msg_id,
                placeholder_msg_id
            ));

            // Sidecar: the channel file mirrors the mapping.
            let contents = std::fs::read_to_string(&path).expect("sidecar must exist after insert");
            assert!(contents.contains("\"user_message_id\": 11"));
            assert!(contents.contains("\"placeholder_message_id\": 22"));

            // Remove (write-through) returns the placeholder id and clears memory + sidecar.
            shared
                .remove_queued_placeholder(channel_id, user_msg_id)
                .await
        });
        assert_eq!(removed, Some(placeholder_msg_id));
        assert!(!shared.queued_placeholder_still_owned(
            channel_id,
            user_msg_id,
            placeholder_msg_id
        ));
        assert!(!path.exists(), "empty channel sidecar must be removed");
    }

    #[test]
    fn queued_placeholder_still_owned_branches() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let shared = make_shared_data_for_tests();
        let channel_id = ChannelId::new(3_038_200);
        let user_msg_id = MessageId::new(31);
        let placeholder_msg_id = MessageId::new(32);
        let other_placeholder = MessageId::new(33);

        // Absent mapping → not owned.
        assert!(!shared.queued_placeholder_still_owned(
            channel_id,
            user_msg_id,
            placeholder_msg_id
        ));

        test_rt().block_on(async {
            let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
            let _guard = persist_lock.lock().await;
            shared.insert_queued_placeholder_locked(channel_id, user_msg_id, placeholder_msg_id);
        });

        // Owned by our placeholder, not by a different one.
        assert!(shared.queued_placeholder_still_owned(channel_id, user_msg_id, placeholder_msg_id));
        assert!(!shared.queued_placeholder_still_owned(channel_id, user_msg_id, other_placeholder));
    }

    #[test]
    fn queue_exit_placeholder_clears_round_trip_with_sidecar() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let shared = make_shared_data_for_tests();
        let channel_id = ChannelId::new(3_038_300);
        let user_msg_id = MessageId::new(41);
        let placeholder_msg_id = MessageId::new(42);

        let path = sidecar_path(
            tmp.path(),
            "discord_queue_exit_placeholder_clears",
            channel_id,
        );
        test_rt().block_on(async {
            shared
                .add_pending_queue_exit_placeholder_clear_one(
                    channel_id,
                    user_msg_id,
                    placeholder_msg_id,
                )
                .await;

            let pending = shared.pending_queue_exit_placeholder_clears();
            assert_eq!(pending, vec![(channel_id, user_msg_id, placeholder_msg_id)]);

            let contents = std::fs::read_to_string(&path).expect("clears sidecar must exist");
            assert!(contents.contains("\"user_message_id\": 41"));
            assert!(contents.contains("\"placeholder_message_id\": 42"));

            shared
                .remove_pending_queue_exit_placeholder_clears(
                    channel_id,
                    &[(user_msg_id, placeholder_msg_id)],
                )
                .await;
        });

        assert!(shared.pending_queue_exit_placeholder_clears().is_empty());
        assert!(!path.exists(), "empty clears sidecar must be removed");
    }

    #[test]
    fn queued_placeholders_persist_lock_identity() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let shared = make_shared_data_for_tests();
        let channel_a = ChannelId::new(3_038_400);
        let channel_b = ChannelId::new(3_038_401);

        let lock_a1 = shared.queued_placeholders_persist_lock(channel_a);
        let lock_a2 = shared.queued_placeholders_persist_lock(channel_a);
        let lock_b = shared.queued_placeholders_persist_lock(channel_b);

        assert!(
            Arc::ptr_eq(&lock_a1, &lock_a2),
            "same channel must reuse the same lock"
        );
        assert!(
            !Arc::ptr_eq(&lock_a1, &lock_b),
            "different channels must get distinct locks"
        );
    }

    // #3089 A0 — characterization of the `LeaseOutcome` failure-signal
    // representation and its I2 invariant (design §5 A0 item 3, signal #1 of 5).
    // The dormant `DeliveryLeaseCell` state machine is already pinned by
    // `turn_finalizer::tests::delivery_lease`; this pins the load-bearing I2
    // datum the controller must preserve — `commit` RECORDS the three-way
    // outcome verbatim and never collapses NotDelivered/Unknown into Delivered,
    // so the caller can refuse to advance the offset for the ambiguous arms.
    // Pinned inline in this `#[cfg(test)] mod` of the FROZEN (baseline 4944)
    // file => ZERO production LoC.
    mod a0_failure_signal_characterization_tests {
        use super::super::turn_finalizer::TurnKey;
        use super::super::{
            DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder, LeaseOutcome, LeaseSnapshot,
        };
        use serenity::model::id::ChannelId;

        fn turn() -> DeliveryLeaseKey {
            DeliveryLeaseKey::from_turn_key(TurnKey::new(ChannelId::new(7), 11, 0))
        }

        #[test]
        fn a0_lease_outcome_has_exactly_three_distinct_arms() {
            assert_ne!(LeaseOutcome::Delivered, LeaseOutcome::NotDelivered);
            assert_ne!(LeaseOutcome::Delivered, LeaseOutcome::Unknown);
            assert_ne!(LeaseOutcome::NotDelivered, LeaseOutcome::Unknown);
        }

        #[test]
        fn a0_commit_records_each_outcome_verbatim_without_collapsing() {
            for outcome in [
                LeaseOutcome::Delivered,
                LeaseOutcome::NotDelivered,
                LeaseOutcome::Unknown,
            ] {
                let cell = DeliveryLeaseCell::new(ChannelId::new(7));
                let holder = LeaseHolder::Bridge { attempt_id: 1 };
                assert!(cell.try_acquire(turn(), holder, 100, 200, 1_000));
                assert!(
                    cell.commit(holder, turn(), 100, 200, outcome),
                    "identity-matched commit of {outcome:?} succeeds"
                );
                match cell.read() {
                    LeaseSnapshot::Committed {
                        outcome: got,
                        start,
                        end,
                        ..
                    } => {
                        assert_eq!(got, outcome, "committed outcome is recorded verbatim");
                        assert_eq!((start, end), (100, 200), "range is preserved on commit");
                    }
                    other => panic!("expected Committed{{{outcome:?}}}, got {other:?}"),
                }
            }
        }

        #[test]
        fn a0_unknown_and_not_delivered_are_distinguishable_after_commit() {
            // This pins ONLY that `DeliveryLeaseCell::commit` preserves each
            // distinct outcome (so the caller can tell them apart). The I2
            // advance rule itself — committed offset advances ONLY on Delivered
            // — is characterized against the REAL production advance path in
            // `turn_bridge::terminal_delivery`'s
            // `a0_i2_advance_characterization_tests` (driving
            // `BridgeDeliveryLease::commit_and_advance`), NOT a local closure.
            let delivered = committed_outcome_of(LeaseOutcome::Delivered);
            let not_delivered = committed_outcome_of(LeaseOutcome::NotDelivered);
            let unknown = committed_outcome_of(LeaseOutcome::Unknown);

            assert_eq!(delivered, LeaseOutcome::Delivered);
            assert_eq!(not_delivered, LeaseOutcome::NotDelivered);
            assert_eq!(unknown, LeaseOutcome::Unknown);
        }

        fn committed_outcome_of(outcome: LeaseOutcome) -> LeaseOutcome {
            let cell = DeliveryLeaseCell::new(ChannelId::new(7));
            let holder = LeaseHolder::Sink;
            assert!(cell.try_acquire(turn(), holder, 0, 5, 1_000));
            assert!(cell.commit(holder, turn(), 0, 5, outcome));
            match cell.read() {
                LeaseSnapshot::Committed { outcome, .. } => outcome,
                other => panic!("expected Committed, got {other:?}"),
            }
        }
    }
}

#[cfg(test)]
pub(crate) use recovery_engine::{codex_rebind_spawn_for_tests, codex_restart_output_for_tests};
#[cfg(all(test, unix))]
pub(crate) use tmux::tmux_output_stream::{
    codex_native_read_for_tests, codex_source_witness_present_for_tests,
};

pub(super) mod routine_presentation;

#[cfg(all(test, unix))]
pub(crate) use tmux::first_terminal_plan;
