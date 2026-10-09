"""Single lexical pin for whole-file skips in the two #5071 writer gates.

The ordinary basename skips and the higher-risk resolver skips are listed
separately for review, then exposed as one immutable set.  Every exclusion is
compared with this repo-relative lexical set before either gate scans files.

The reused resolver is deliberately unchanged.  It does not guarantee at
least these seven measured Rust forms: a comment between ``#[path]`` and
``mod``; a macro-generated production ``mod``; ``cfg(not(test))`` plus
``include!``; ``cfg(any(test, feature))`` plus ``include!``;
``cfg_attr(path = ...)``; a raw-string ``#[path]``; or an ungated
``include!``.

This pin guarantees membership, not compiler-backed reachability.  In
particular, a content change can make an already pinned file production-
reachable without changing the pinned path set.  Compiler-backed reachability
is follow-up slice work.  The lexical scan root is ``src/``; call sites in files
reached by ``#[path]``/``include!`` targets resolving outside ``src/`` are not
seen.  Fail-closed handling for that boundary is follow-up slice work.  Symlink
rejection is lexical rather than atomic: a post-enumeration filesystem
replacement is outside the guarantee; CI assumes a static checkout while a
gate runs.
"""

from __future__ import annotations

import importlib.util
import os
import sys
from collections.abc import Callable, Iterable
from pathlib import Path


# Mechanical ``tests.rs`` / ``*_tests.rs`` basename exclusions.
PINNED_BASENAME_TEST_FILES = frozenset(
    {
        "src/services/discord/health/recovery/retirement_await_tests.rs",
        "src/services/discord/placeholder_sweeper/retirement_tests.rs",
        "src/services/discord/tmux_reaper/retirement_await_tests.rs",
        "src/services/discord/tmux_restart_handoff_retirement_await_tests.rs",
        "src/services/discord/tui_direct_abort_marker/retirement_census_tests.rs",
        "src/services/discord/tui_direct_pending_start/retirement_recheck_tests.rs",
        "src/services/discord/inflight/removal/input_fence_reaper_tests.rs",
        "src/services/discord/inflight/restart_mark_fence_tests.rs",
        "src/services/discord/recovery_engine/manual_rebind/live_bridge_guard_tests.rs",
        "src/services/discord/runtime_bootstrap/queued_recovery_fence_tests.rs",
        "src/services/discord/runtime_bootstrap/shutdown_input_fence_tests.rs",
        "src/services/discord/tui_direct_pending_start/tests/input_effect_tests.rs",
        "src/services/claude_tui/hook_server/native_clear_fence_tests.rs",
        "src/services/discord/adk_session_selector_checked_tests.rs",
        "src/services/discord/commands/control/native_tests.rs",
        "src/services/discord/commands/control/native_policy_tests.rs",
        "src/services/discord/router/message_handler/intake_turn/native_fresh_prompt_tests.rs",
        "src/services/discord/router/message_handler/intake_turn/runtime_transition/native_hold_tests.rs",
        "src/services/discord/commands/control/input_clear_tests.rs",
        "src/services/discord/commands/control/codex_verified_clear_tests.rs",
        "src/services/discord/admin_host_guard_verified_reset_tests.rs",
        "src/services/auto_queue/runtime/verified_reset_pg_tests.rs",
        "src/services/discord/input_runtime/clear_tests.rs",
        "src/services/cluster/intake_worker/input_effect_tests.rs",
        "src/services/discord/input_runtime/effect_tests.rs",
        "src/services/discord/queue_dispatch/input_effect_tests.rs",
        "src/services/discord/input_transition_tests.rs",
        "src/services/discord/input_runtime/fence_tests.rs",
        "src/services/discord/input_runtime/effects_tests.rs",
        "src/services/discord/input_runtime/supervisor_tests.rs",
        "src/services/discord/input_runtime/fence/modes_tests.rs",
        "src/services/discord/input_runtime/supervisor/drive_tests.rs",
        "src/services/discord/input_runtime/supervisor/drive_entry_tests.rs",
        "src/services/turn_orchestrator/input_fence_tests.rs",
        "src/services/tui_input/transition_fixture_tests.rs",
        "src/services/tui_input/transition_tests.rs",
        "src/services/discord/live_bridge/guard_tests.rs",
        "src/services/discord/runtime_store_checkpoint_lock_tests.rs",
        "src/services/routines/reliability_pg_tests.rs",
        "src/services/discord/turn_bridge/tmux_runtime/session_stop_tests.rs",
        "src/services/discord/turn_bridge/tmux_runtime/codex_stop_delivery_tests.rs",
        "src/services/discord/turn_bridge/tmux_runtime/claude_stop_pane_tests.rs",
        "src/services/discord/tmux/n1a_monitor_tests.rs",
        "src/services/discord/tmux_watcher/n1a_turn_mode_tests.rs",
        "src/services/discord/tui_direct_pending_start/turn_retirement_tests.rs",
        "src/services/discord/tui_prompt_relay/relay_e2e/n1a_turn_mode_tests.rs",
        "src/services/tui_o/writer/clear_launch_tests.rs",
        "src/services/tui_o/writer/canary_policy_tests.rs",
        "src/services/codex_tui/verified_tail/diagnostic_tests.rs",
        "src/services/tui_o/writer/input_facts/tests.rs",
        "src/services/tui_o/writer/input_facts/resume_tests.rs",
        "src/services/discord/turn_presence/activity_tests.rs",
        "src/services/discord/turn_presence/admission_tests.rs",
        "src/services/discord/turn_presence/supervisor_tests.rs",
        "src/services/claude_tui/input/final_ready_tests.rs",
        "src/services/claude_tui/hook_server/codex_ingress_tests.rs",
        "src/services/codex_tui/rollout_tail/child_binding_tests.rs",
        "src/services/discord/tui_prompt_relay/rehydration/child_binding_tests.rs",
        "src/config/writer_channels_tests.rs",
        "src/server/routes/pipeline_stage_save_tests.rs",
        "src/services/pipeline_routes/stage_validation_tests.rs",
        "src/services/tui_o/channel_policy/tests.rs",
        "src/services/session_host/herdr_host_tests.rs",
        "src/services/claude_tui/hosting/host_draft_tests.rs",
        "src/services/session_host/herdr/transport_tests.rs",
        "src/services/session_host/herdr/provenance_tests.rs",
        "src/services/session_host/herdr/launch_host_tests.rs",
        "src/services/session_host/herdr/launch_host_real_tests.rs",
        "src/services/session_host/herdr/pane_probe_tests.rs",
        "src/services/session_host/herdr_gate_tests.rs",
        "src/services/herdr_launch_tests.rs",
        "src/services/herdr_admission_tests.rs",
        "src/services/turn_host_tests.rs",
        "src/config/session_hosts_tests.rs",
        "src/services/claude/host_gate_tests.rs",
        "src/services/discord/router/message_handler/provider_dispatch_herdr_tests.rs",
        "src/services/session_host/herdr_socket_rig_tests.rs",
        "src/services/discord/recovery_engine/restore_inflight/host_probe_tests.rs",
        "src/services/discord/tui_prompt_relay/claude_idle_tail_host_tests.rs",
        "src/services/claude/c1_teardown_tests.rs",
        "src/services/codex/c1_teardown_tests.rs",
        "src/services/codex/startup_update_tests.rs",
        "src/services/codex_tui/input/tests/startup_update_tests.rs",
        "src/services/discord/turn_teardown_clearance_tests.rs",
        "src/services/discord/host_key_derivation_tests.rs",
        "src/services/discord/host_defer_gate_tests.rs",
        "src/services/discord/admin_host_guard_tests.rs",
        "src/server/routes/agents_host_guard_tests.rs",
        "src/services/provider_teardown_tests.rs",
        "src/services/discord/placeholder_live_events/clock_tests.rs",
        "src/services/discord/execution_identity/herdr_observation_tests.rs",
        "src/services/discord/execution_identity/herdr_report_order_tests.rs",
        "src/services/discord/watchers/lifecycle/watch_host_tests.rs",
        "src/services/discord/watchers/lifecycle/claims_host_tests.rs",
        "src/services/discord/router/message_handler/watchdog_host_tests.rs",
        "src/services/discord/tmux_watcher/herdr_entry_host_tests.rs",
        "src/services/tui_o/writer/herdr_resume_tests.rs",
        "src/services/discord/turn_bridge/runtime_handoff_loop/herdr_owner_tests.rs",
        "src/services/discord/recovery_engine/manual_rebind/herdr_withheld_tests.rs",
        "src/services/discord/execution_identity/herdr_agent_hint_tests.rs",
        "src/services/discord/recovery_engine/host_reconcile_tests.rs",
        "src/services/discord/tui_prompt_relay/herdr_source_tests.rs",
        "src/services/discord/tmux_output_stream/tests/compact_summary_tests.rs",
        "src/services/discord/tmux_watcher/loop_poll_prologue/post_terminal_disposal_tests/compact_summary_tests.rs",
        "src/services/discord/tui_prompt_relay/tests/compact_summary_tests.rs",
        "src/services/discord/turn_bridge/entry_abort_mailbox_tests.rs",
        "src/services/discord/outbound/delivery_obligation/tests.rs",
        "src/services/discord/outbound/delivery_obligation/validation_tests.rs",
        "src/services/discord/outbound/delivery_obligation/reader_tests.rs",
        "src/services/discord/outbound/delivery_obligation/codec_tests.rs",
        "src/services/discord/outbound/delivery_obligation/state/proof_tests.rs",
        "src/cli/doctor/orchestrator/observation_tests.rs",
        "src/cli/migrate/apply_config_tests.rs",
        "src/services/cluster/attachment_transfer/storage_tests.rs",
        "src/services/cluster/intake_router_hook/attachment_tests.rs",
        "src/services/cluster/execution_capacity/tests.rs",
        "src/services/cluster/machine_resources/tests.rs",
        "src/db/auto_queue/tests.rs",
        "src/engine/ops/exec_ops/exec_allowlist_tests.rs",
        "src/engine/ops/exec_ops/session_liveness_tests.rs",
        "src/engine/ops/timeouts_ops/host_repair_tests.rs",
        "src/services/platform/tmux/liveness/tests.rs",
        "src/services/cluster/intake_router_hook/edge_case_tests.rs",
        "src/services/cluster/intake_router_hook/agent_execution_node_tests.rs",
        "src/services/cluster/intake_router_hook/capacity_tests.rs",
        "src/services/cluster/intake_router_hook/o_route_tests.rs",
        "src/services/cluster/intake_router_hook/home_route_tests.rs",
        "src/services/cluster/channel_home_tests.rs",
        "src/services/discord/queue_io/transport/tests.rs",
        "src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests/recovery_retry_guard_tests.rs",
        "src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests/rest_delivery_tests.rs",
        "src/db/automation_candidates/verdict_tests.rs",
        "src/db/calendar_sync/postgres_tests.rs",
        "src/db/campaigns/tests.rs",
        "src/db/dispatched_sessions/canonical_identity_pg_tests.rs",
        "src/db/dispatched_sessions/hosted_execution_tests.rs",
        "src/db/dispatched_sessions/tests.rs",
        "src/db/intake_outbox_dispatch_stamp/tests.rs",
        "src/db/o_channel_homes_tests.rs",
        "src/db/intake_outbox_home_tests.rs",
        "src/db/session_transcripts_native_clear_tests.rs",
        "src/db/prompt_manifests/tests.rs",
        "src/db/scheduled_messages/postgres_tests.rs",
        "src/dispatch/dispatch_status/terminal_timestamp_tests.rs",
        "src/github/sync/warning_tests.rs",
        "src/github/triage/warning_tests.rs",
        "src/server/database_fixture_invariant_tests.rs",
        "src/server/dashboard_auth/tests.rs",
        "src/server/web_surface/tests.rs",
        "src/server/routes/auto_queue_lifecycle_pg_tests.rs",
        "src/server/routes/dispatched_sessions_tests.rs",
        "src/server/routes/runtime_profile_tests.rs",
        "src/server/routes/scheduled_messages/postgres_tests.rs",
        "src/server/routes/session_evidence_tests.rs",
        "src/server/routes/skills_manifest_audit_tests.rs",
        "src/server/routes/tests/auto_queue_preflight_harness_tests.rs",
        "src/services/agent_recovery/durable/postgres_tests.rs",
        "src/services/agent_recovery/tests.rs",
        "src/services/auto_queue/campaign_handoff_tests.rs",
        "src/services/auto_queue/cleanup_tasks_pg_tests.rs",
        "src/services/auto_queue/runtime/clear_slot_sessions_pg_tests.rs",
        "src/services/auto_queue/runtime/slot_reset_host_pg_tests.rs",
        "src/services/automation_candidate_materializer/allowed_path_tests.rs",
        "src/services/automation_candidate_materializer/iteration_result_tests.rs",
        "src/services/claude_tui/busy_inject_tests.rs",
        "src/services/claude_tui/busy_inject/screen_tests.rs",
        "src/services/claude_tui/busy_inject/stash_tests.rs",
        "src/services/claude_tui/busy_inject/stash_tests/hold_tests.rs",
        "src/services/discord/health/turn_deliver/inject_codex_tests.rs",
        "src/services/discord/health/turn_deliver/inject_tests.rs",
        "src/services/discord/health/watcher_respawn/claude_original_tests.rs",
        "src/services/claude_tui/hook_output_guard_tests.rs",
        "src/services/claude_tui/hook_payload_fixture_tests.rs",
        "src/services/claude_tui/hook_server_memento_tests.rs",
        "src/services/claude_tui/session/auto_compact_launch_tests.rs",
        "src/services/codex_tui/busy_inject/inject_tests.rs",
        "src/services/codex_tui/session/source_observation_tests.rs",
        "src/services/kakao/transport_tests.rs",
        "src/services/cluster/attachment_transfer/tests.rs",
        "src/services/cluster/execution_requirements/tests.rs",
        "src/services/cluster/intake_router_hook/execution_requirement_tests.rs",
        "src/services/cluster/intake_worker/dispatch_stamp_tests.rs",
        "src/services/cluster/intake_worker/drain_tests.rs",
        "src/services/cluster/intake_worker/o_route_tests.rs",
        "src/services/cluster/intake_worker/home_route_tests.rs",
        "src/services/cluster/intake_worker/home_accept_tests.rs",
        "src/services/discord/commands/control/home_fence_tests.rs",
        "src/services/tui_o/writer/deliver_home_tests.rs",
        "src/services/tui_o/writer/host_home_tests.rs",
        "src/services/codex/herdr_guard_tests.rs",
        "src/services/discord/router/message_handler/provider_dispatch_codex_herdr_tests.rs",
        "src/services/discord/router/message_handler/provider_dispatch_codex_herdr_followup_tests.rs",
        "src/services/tui_o/writer/codex_herdr_drive_tests.rs",
        "src/services/cluster/channel_home_claim_tests.rs",
        "src/services/discord/router/intake_dispatch/home_order_tests.rs",
        "src/services/cluster/readiness/tests.rs",
        "src/services/cluster/stream_relay/tests/shutdown_tests.rs",
        "src/services/discord/abandon_request_store/probe_contract_tests.rs",
        "src/services/discord/catch_up/absorbed_active_tests.rs",
        "src/services/discord/catch_up/claim_cas_tests.rs",
        "src/services/discord/catch_up/classification_order_tests.rs",
        "src/services/discord/catch_up/injected_catch_up_tests.rs",
        "src/services/discord/inject_disposition_tests.rs",
        "src/services/discord/catch_up/consumed_commands_tests.rs",
        "src/services/discord/catch_up/frontier_sweep_tests.rs",
        "src/services/discord/catch_up/handled_command_tests.rs",
        "src/services/discord/catch_up/merged_alias_tests.rs",
        "src/services/discord/catch_up/too_old_drop_pg_tests.rs",
        "src/services/discord/commands/inspect/tests.rs",
        "src/services/discord/delivery_lease_cell/exact_lease/tests.rs",
        "src/services/discord/formatting/replace_long_message_tests.rs",
        "src/services/discord/formatting/status_panel_v2_formatter_tests.rs",
        "src/services/discord/health/reachability/composite_tests.rs",
        "src/services/discord/health/reachability/coverage_tests.rs",
        "src/services/discord/health/reachability/ledger_tests.rs",
        "src/services/discord/health/reachability/obligation_tests.rs",
        "src/services/discord/inflight/host_recovery_guard_keyed_tests.rs",
        "src/server/routes/health_api/host_guard_tests.rs",
        "src/services/discord/tmux_reaper/host_guard_tests.rs",
        "src/services/discord/inflight/removal/boot_custody_tests.rs",
        "src/services/discord/inflight/removal/custody_notice_tests.rs",
        "src/services/discord/inflight/rebind_reap/tests.rs",
        "src/services/discord_custody/tests.rs",
        "src/services/discord/inflight/save_store/bridge_entry_guard_tests.rs",
        "src/services/discord/inflight/save_store/identity_gate/runtime_stamp/claude_terminal_tests.rs",
        "src/services/discord/inflight/save_store/identity_gate/runtime_stamp/herdr_terminal_admission_tests.rs",
        "src/services/discord/inflight/save_store/identity_gate/runtime_stamp/herdr_terminal_restart_tests.rs",
        "src/services/discord/inflight/save_store/outcome_decomposition_tests.rs",
        "src/services/discord/inflight/save_store/post_loop_identity_guard_tests.rs",
        "src/services/discord/outbound/manual_delivery/production_nonce_tests.rs",
        "src/services/discord/outbound/turn_output_controller/fresh_send_tests.rs",
        "src/services/discord/outbound/source_registry_scan_tests.rs",
        "src/services/discord/placeholder_controller/queued_card_gate/tests.rs",
        "src/services/discord/placeholder_live_events/probe_fixtures_tests.rs",
        "src/services/discord/placeholder_live_events/tests.rs",
        "src/services/discord/prompt_builder/dispatch_contract_tests.rs",
        "src/services/discord/recovery_engine/o_recovery_cut_tests.rs",
        "src/services/discord/recovery_engine/manual_rebind/coordinate_adoption_tests.rs",
        "src/services/discord/recovery_engine/manual_rebind/post_adoption_guard_tests.rs",
        "src/services/discord/recovery_engine/manual_rebind/adoption_notice_pg_tests.rs",
        "src/services/discord/recovery_engine/restore_inflight/kickoff_identity_tests.rs",
        "src/services/discord/recovery_engine/restore_inflight/ready_without_output_tests.rs",
        "src/services/discord/recovery_engine/routing_orphan_tests.rs",
        "src/services/discord/recovery_paths/o_anchor_repost_tests.rs",
        "src/services/discord/relay_coord_tests.rs",
        "src/services/discord/relay_recovery/tests.rs",
        "src/services/discord/router/intake_dispatch/tests.rs",
        "src/services/discord/router/message_handler/intake_turn/dispatch_stamp/tests.rs",
        "src/services/discord/router/message_handler/intake_turn/race_loss/mailbox_reaction_tests.rs",
        "src/services/discord/router/message_handler/intake_turn/race_loss/requeue_tests.rs",
        "src/services/discord/router/message_handler/provider_isolation_host_tests.rs",
        "src/services/discord/router/message_handler/session_strategy_lifecycle_tests.rs",
        "src/services/discord/runtime_bootstrap/gateway_handback_breaker_tests.rs",
        "src/services/discord/runtime_bootstrap/gateway_handback_integration_tests.rs",
        "src/services/discord/runtime_bootstrap/gateway_lease_recovery_tests.rs",
        "src/services/discord/runtime_bootstrap/gateway_lease_tests.rs",
        "src/services/discord/runtime_bootstrap/intake_delivery_capability/tests.rs",
        "src/services/discord/runtime_bootstrap/intake_delivery_sweep/tests.rs",
        "src/services/discord/runtime_bootstrap/queued_placeholders/tests.rs",
        "src/services/discord/runtime_bootstrap/relay_dlq_redelivery/tests.rs",
        "src/services/discord/runtime_bootstrap/spawns_tests.rs",
        "src/services/discord/session_idle_cleanup_tests.rs",
        "src/services/discord/session_relay_sink/delivery_orchestration_tests.rs",
        "src/services/discord/session_relay_sink/o_adoption_e2e_tests.rs",
        "src/services/discord/session_relay_sink/o_delivery_e2e_tests.rs",
        "src/services/discord/session_relay_sink/tests.rs",
        "src/services/discord/session_relay_sink/turn_parser/resend_dedupe_tests.rs",
        "src/services/discord/status_panel_orphan_store_tests.rs",
        "src/services/discord/task_notification_delivery/claim_at_post_tests.rs",
        "src/services/discord/task_notification_delivery/tests.rs",
        "src/services/discord/task_supervisor/watcher_completion_tests.rs",
        "src/services/discord/terminal_delivery_custody/pg_tests.rs",
        "src/services/discord/terminal_delivery_custody/tests.rs",
        "src/services/discord/tmux/monitor_auto_turn_inflight_tests.rs",
        "src/services/discord/tmux/task_notification_kind_restart_roundtrip_tests.rs",
        "src/services/discord/tmux_output_stream/provider_output_guard_tests.rs",
        "src/services/discord/tmux_placeholder_suppression/unicode_units_tests.rs",
        "src/services/discord/tmux_watcher/cancel_handoff/interrupted_adoption_tests.rs",
        "src/services/discord/tmux_watcher/completion_gate_tests.rs",
        "src/services/discord/tmux_watcher/jsonl_rotation/backstop_tests.rs",
        "src/services/discord/tmux_watcher/loop_poll_prologue/post_terminal_disposal_tests.rs",
        "src/services/discord/tmux_watcher/o_delegated_watcher_tests.rs",
        "src/services/discord/tmux_watcher/owed_range_baseline_tests.rs",
        "src/services/discord/tmux_watcher/panel_decisions_tests.rs",
        "src/services/discord/tmux_watcher/path_b_record_tests.rs",
        "src/services/discord/tmux_watcher/post_stream_exit_host_tests.rs",
        "src/services/discord/tmux_watcher/session_bound_ack_tests.rs",
        "src/services/discord/tmux_watcher/single_message_footer_tests.rs",
        "src/services/discord/tmux_watcher/streaming_baseline_tests.rs",
        "src/services/discord/tmux_watcher/streaming_harness_tests.rs",
        "src/services/discord/tmux_watcher/streaming_status_tick/committed_progress_tests.rs",
        "src/services/discord/tmux_watcher/streaming_status_tick/native_collector_tests.rs",
        "src/services/discord/tmux_watcher/supervisor_relay_tests.rs",
        "src/services/discord/tmux_watcher/task_response_authority_tests.rs",
        "src/services/discord/tmux_watcher/terminal_direct_fallback_tests.rs",
        "src/services/discord/tmux_watcher/terminal_readiness_tests.rs",
        "src/services/discord/tmux_watcher/terminal_relay_plan_tests.rs",
        "src/services/discord/tmux_watcher/terminal_commit_epilogue/continuation_marker_tests.rs",
        "src/services/discord/tmux_watcher/terminal_commit_epilogue/synthetic_mailbox_release_tests.rs",
        "src/services/discord/tmux_watcher/tests.rs",
        "src/services/discord/tmux_watcher/turn_identity_tests.rs",
        "src/services/discord/tmux_watcher/two_message_panel_tests.rs",
        "src/services/discord/tmux_watcher/utf8_chunk_decoder_tests.rs",
        "src/services/discord/tmux_watcher_registry_restore_tests.rs",
        "src/services/discord/tui_direct_abort_marker/warning_tests.rs",
        "src/services/discord/tui_direct_pending_start/tests.rs",
        "src/services/discord/tui_direct_pending_start/tests/retire_tests.rs",
        "src/services/discord/tui_prompt_relay/rehydration/idempotency_tests.rs",
        "src/services/discord/tui_prompt_relay/tests.rs",
        "src/services/discord/tui_prompt_relay/tests/codex_direct_owner_tests.rs",
        "src/services/discord/tui_prompt_relay/tests/codex_direct_owner_tests/boundary_tests.rs",
        "src/services/discord/tui_prompt_relay/tests/codex_direct_owner_tests/boundary_tests/loss_tests.rs",
        "src/services/discord/tui_prompt_relay/tests/codex_direct_owner_tests/boundary_tests/native_turn_tests.rs",
        "src/services/discord/tui_prompt_relay/tests/fenced_admission_tests.rs",
        "src/services/discord/tui_prompt_relay/tests/o_tool_first_panel_tests.rs",
        "src/services/discord/tui_prompt_relay/tests/retired_pending_start_claim_tests.rs",
        "src/services/discord/tui_prompt_relay/tests/synthetic_bridge_handoff_pg_tests.rs",
        "src/services/discord/tui_prompt_relay/tests/synthetic_terminal_ordering_tests.rs",
        "src/services/discord/turn_bridge/body_mutation_telemetry_tests.rs",
        "src/services/discord/turn_bridge/chunk_compose_tests.rs",
        "src/services/discord/turn_bridge/completion_guard/span_tests.rs",
        "src/services/discord/turn_bridge/completion_postlude/o_panel_below_tests.rs",
        "src/services/discord/turn_bridge/headless_delivery/production_seam_tests.rs",
        "src/services/discord/turn_bridge/intake_settlement/tests.rs",
        "src/services/discord/turn_bridge/resume_pin_tests.rs",
        "src/services/discord/turn_bridge/runtime_handoff_loop/tests.rs",
        "src/services/discord/turn_bridge/status_panel_tests.rs",
        "src/services/discord/turn_bridge/stream_loop/expected_identity_tests.rs",
        "src/services/discord/turn_bridge/stream_loop/tool_arms/authority_tests.rs",
        "src/services/discord/turn_bridge/stream_tick/guarded_persist_tests.rs",
        "src/services/discord/turn_bridge/stream_tick/lock_liveness_tests.rs",
        "src/services/discord/turn_bridge/stream_tick/o_adoption_tests.rs",
        "src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests.rs",
        "src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests/rowless_receipt_tests.rs",
        "src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests/rowless_receipt_tests/pg_tests.rs",
        "src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests/rowless_receipt_tests/o_after_done_chain_tests.rs",
        "src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests/rowless_receipt_tests/preloop_cleanup_tests.rs",
        "src/services/discord/turn_bridge/voice_completion_tests.rs",
        "src/services/discord/turn_finalizer/finalize/tests/residue_tests.rs",
        "src/services/discord/turn_lease_tests.rs",
        "src/services/discord/tmux_restart_handoff_mailbox_tests.rs",
        "src/services/discord/turn_view_reconciler/tests.rs",
        "src/services/discord/voice_barge_in/tests/pcm_harness_tests.rs",
        "src/services/discord/watchers/dispatched_origin_ghost_tests.rs",
        "src/services/discord/watchers/lifecycle/liveness_tests.rs",
        "src/services/discord/watchers/lifecycle/restore_tests.rs",
        "src/services/discord/watchers/lifecycle/tests.rs",
        "src/services/observability/events/capture_stress_tests.rs",
        "src/services/observability/events/capture_tests.rs",
        "src/services/message_outbox_circuit_authority_tests.rs",
        "src/services/message_outbox_recovery_tests.rs",
        "src/services/memory/memento_writer_tests.rs",
        "src/services/dispatch_gate/auth_profiles/selection_tests.rs",
        "src/services/process/stream_child/stream_queue/tests.rs",
        "src/services/provider/provider_conformance_invariant_tests.rs",
        "src/services/provider_auth_profile/fallback/tests.rs",
        "src/services/provider_output_guard_tests.rs",
        "src/services/session_forwarding/probe/tests.rs",
        "src/services/scheduled_messages/postgres_tests.rs",
        "src/services/tui_prompt_dedupe/binding_events/codex_claim_tests.rs",
        "src/services/tui_prompt_dedupe/runtime_binding/codex_verified_tests.rs",
        "src/services/tui_prompt_dedupe/runtime_binding/codex_legacy_lock_tests.rs",
        "src/services/tui_prompt_dedupe/binding_events/lane_tests.rs",
        "src/services/tui_prompt_dedupe/pending_tests.rs",
        "src/services/tui_prompt_dedupe/pending_history_tests.rs",
        "src/services/claude_tui/hook_server/observation_ingress_tests.rs",
        "src/services/claude_tui/hook_server/rehydration_ingress_tests.rs",
        "src/services/discord/tui_prompt_relay/rehydration_pending_tests.rs",
        "src/services/discord/tui_prompt_relay/headless_tests.rs",
        "src/services/discord/tui_direct_pending_start/tests/headless_row_tests.rs",
        "src/services/claude_tui/hook_relay/ordered_queue/tests/tq_tests.rs",
        "src/services/claude_tui/hook_relay/ordered_queue/tests/session_start_retry_tests.rs",
        "src/services/tui_prompt_dedupe/injected_steer_tests.rs",
        "src/services/tui_prompt_dedupe/tests.rs",
        "src/services/turn_orchestrator/input_handback_tests.rs",
        "src/services/turn_orchestrator/mailbox_unreachable_tests.rs",
        "src/services/turn_orchestrator/recovery_kickoff_tests.rs",
        "src/services/turn_orchestrator/registry_purge/closed_gate_tests.rs",
        "src/services/turn_orchestrator/injected_inputs_tests.rs",
        "src/services/discord/mailbox_finish/closed_actor_tests.rs",
        "src/services/discord/queue_io/turn_admission_tests.rs",
        "src/services/discord/queue_io/ledger_settlement_tests.rs",
        "src/services/discord/health/relay_auto_heal/orphan_token_tests.rs",
        "src/services/discord/health/transcript_turn_tests.rs",
        "src/services/discord/placeholder_sweeper/tick_cleanup_tests.rs",
        "src/services/discord/health/recovery/stop_judgement/judged_finish_tests.rs",
        "src/services/discord/health/recovery/live_agent_recovery/judged_finish_fence_tests.rs",
        "src/server/routes/health_api/unread_tail_attribution_tests.rs",
        "src/server/routes/health_api/tui_output_readiness_tests.rs",
        "src/services/tui_o/writer/writer_tests.rs",
        "src/services/tui_o/writer/actor_tests.rs",
        "src/services/tui_o/store/ledger_lock_tests.rs",
        "src/services/tui_o/store/rotation_tests.rs",
        "src/services/tui_o/writer/rotation_tests.rs",
        "src/services/tui_o/writer/recovery_tests.rs",
        "src/services/tui_o/writer/renumber_tests.rs",
        "src/services/tui_input/actor_tests.rs",
        "src/services/tui_input/bounded_tmux_tests.rs",
        "src/services/tui_input/durability_tests.rs",
        "src/services/tui_input/rows_tests.rs",
        "src/services/tui_o/writer/switch_tests.rs",
        "src/services/tui_o/writer/retire_tests.rs",
        "src/services/tui_o/writer/fork_tests.rs",
        "src/services/tui_o/writer/host_tests.rs",
        "src/services/tui_o/writer/host_renumber_tests.rs",
        "src/services/tui_o/writer/adoption_tests.rs",
        "src/services/tui_o/writer/deferred_tests.rs",
        "src/services/tui_o/writer/stall_tests.rs",
        "src/services/tui_o/writer/reclaim_tests.rs",
        "src/services/tui_o/writer/resume_tests.rs",
        "src/services/tui_o/cutover/channel_gate/tests.rs",
        "src/services/tui_prompt_dedupe/prompt_identity_tests.rs",
        "src/services/discord/tui_prompt_relay/relay_e2e/prompt_identity_e2e_tests.rs",
        "src/services/discord/tui_prompt_relay/synthetic_start/claim_entry_tests.rs",
        "src/services/discord/turn_bridge/tmux_runtime/process_force_kill_tests.rs",
        "src/services/discord/turn_bridge/tmux_runtime/stop_host_tests.rs",
        "src/services/discord/turn_bridge/tmux_runtime/judged_stop_tests.rs",
        "src/services/discord/tmux_watcher/judged_stop_harness_tests.rs",
        "src/services/discord/router/intake_gate/stale_turn_host_tests.rs",
        "src/cli/herdr_tests.rs",
        "src/services/discord/recovery_engine/herdr_reader_tests.rs",
        "src/services/discord/router/message_handler/provider_dispatch_herdr_reconnect_tests.rs",
        "src/services/session_host/herdr_clear_adapter_tests.rs",
        "src/services/tui_o/writer/rotation_unsettled_tests.rs",
        "src/services/discord/commands/control/native_herdr_tests.rs",
        "src/services/cluster/channel_home_drain_tests.rs",
        "src/cli/channel_home_tests.rs",
        "src/services/cluster/channel_home_port_tests.rs",
        "src/services/cluster/channel_home_boot_tests.rs",
        "src/services/discord/runtime_bootstrap/channel_homes_tests.rs",
        "src/services/tui_o/writer/drain_projection_tests.rs",
        "src/services/discord/commands/control/native_herdr_e2e_tests.rs",
        "src/services/tui_o/writer/rotation_unsettled_resume_tests.rs",
        "src/services/codex_tui/rollout_tail/parser/herdr_terminal_tests.rs",
        "src/services/claude/herdr_turn/provider_terminal_tests.rs",
    }
)

# Production-looking basenames classified as test-only by the shared resolver.
PINNED_RESOLVER_TEST_ONLY_FILES = frozenset(
    {
        "src/services/discord/relay_recovery/tests/retirement.rs",
        "src/services/discord/recovery_engine/o_cut_recorder.rs",
        "src/services/tui_o/channel_policy/adoption/body_check.rs",
        "src/services/discord/runtime_bootstrap/gateway_handback_mock.rs",
        "src/services/kakao/test_support.rs",
        "src/services/discord/inject_disposition_test_support.rs",
        "src/services/discord/router/intake_gate/busy_inject_test_support.rs",
        "src/config/test_env.rs",
        "src/config/test_env/teardown_probe.rs",
        "src/db/auto_queue/test_support.rs",
        "src/db/fixture_target.rs",
        "src/db/postgres/test_db_reclaim.rs",
        "src/dispatch/test_support.rs",
        "src/github/test_support.rs",
        "src/high_risk_recovery.rs",
        "src/server/routes/tests/preflight_harness/types.rs",
        "src/server/routes/tests/preflight_harness/validation.rs",
        "src/services/discord/delivery_lease_cell/exact_lease.rs",
        "src/services/discord/delivery_lease_cell/exact_lease/token.rs",
        "src/services/discord/inflight/invariant_test_capture.rs",
        "src/services/discord/inflight/stall_recovery_tests/flake_isolation_4361.rs",
        "src/services/discord/inflight/stall_recovery_tests/flake_isolation_4422.rs",
        "src/services/discord/relay_recovery/tests/input_fence.rs",
        "src/services/discord/relay_recovery/tests/circuit_breaker_apply.rs",
        "src/services/discord/relay_recovery/tests/host_deferred.rs",
        "src/services/discord/relay_recovery/tests/incarnation_follow_up.rs",
        "src/services/discord/relay_recovery/tests/orphan_token_finish.rs",
        "src/services/discord/relay_recovery/tests/unread_tail_seed.rs",
        "src/services/cluster/intake_worker/test_executor.rs",
        "src/services/discord/session_relay_sink/fixtures/o_writer.rs",
        "src/services/discord/session_relay_sink/tests/stream_frame_fixtures.rs",
        "src/services/discord/tui_prompt_relay/local_model_queue_wake_e2e.rs",
        "src/services/discord/tui_prompt_relay/relay_e2e/busy_inject_e2e_tests.rs",
        "src/services/discord/tui_prompt_relay/relay_e2e/busy_inject_thread_e2e_tests.rs",
        "src/services/discord/tui_prompt_relay/relay_e2e/catch_up_pagination_e2e.rs",
        "src/services/discord/tui_prompt_relay/relay_e2e/consumed_command_guard_e2e.rs",
        "src/services/discord/tui_prompt_relay/relay_e2e/discord_mock.rs",
        "src/services/discord/tui_prompt_relay/relay_e2e/mod.rs",
        "src/services/discord/tui_prompt_relay/relay_e2e/queue_recovery_e2e.rs",
        "src/services/discord/tui_prompt_relay/relay_e2e/registered_bootstrap_e2e.rs",
        "src/services/discord/tui_prompt_relay/relay_e2e/stale_resume_retry_e2e.rs",
        "src/services/discord/tui_prompt_relay/relay_e2e/stop_command_catch_up_e2e.rs",
        "src/services/discord/tui_prompt_relay/relay_e2e/thread_guard_host_e2e.rs",
        "src/services/discord/tui_prompt_relay/tests/scenario_census_e2e.rs",
        "src/services/observability/events/test_capture.rs",
        "src/services/observability/test_support.rs",
        "src/services/process/stream_child/stream_queue/test_delay.rs",
        "src/services/process/stream_child/test_fixture.rs",
        "src/services/provider/read_fault.rs",
        "src/services/session_host/test_support.rs",
        "src/services/discord/host_teardown_gate/test_support.rs",
        "src/services/discord/health/legacy_supervision/test_support.rs",
        "src/services/discord/turn_presence/admission.rs",
        "src/services/tmux_turn_liveness/tests_pg.rs",
        "src/test_env_panic_probe.rs",
    }
)

if PINNED_BASENAME_TEST_FILES & PINNED_RESOLVER_TEST_ONLY_FILES:
    raise RuntimeError("writer-gate basename and resolver pins must be disjoint")

PINNED_TEST_ONLY_MODULE_FILES = (
    PINNED_BASENAME_TEST_FILES | PINNED_RESOLVER_TEST_ONLY_FILES
)

_UPDATE_HINT = (
    "Review basename and resolver classification, then update only "
    "scripts/test_only_module_skip_pin.py; both gates and tests derive their "
    "path/count expectation from that single file."
)

_SYMLINK_HINT = (
    "Remove the symlink or replace it with a regular in-tree file; do not add "
    "it to the writer-gate skip pin."
)


def _load_inventory_generator():
    name = "generate_inventory_docs"
    if name in sys.modules:
        return sys.modules[name]
    spec = importlib.util.spec_from_file_location(
        name, Path(__file__).resolve().parent / "generate_inventory_docs.py"
    )
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load scripts/generate_inventory_docs.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


_INVENTORY = _load_inventory_generator()


def skip_pin_drift(
    excluded_paths: Iterable[str],
    pinned_paths: Iterable[str] = PINNED_TEST_ONLY_MODULE_FILES,
) -> str | None:
    """Compare lexical repo-relative paths in both directions."""

    actual = frozenset(Path(path).as_posix() for path in excluded_paths)
    expected = frozenset(Path(path).as_posix() for path in pinned_paths)
    if actual == expected:
        return None
    lines = ["FAIL: writer-gate whole-file skip set drift."]
    scan_only = sorted(actual - expected)
    pin_only = sorted(expected - actual)
    if scan_only:
        lines.append("  scan-only (newly skipped): " + ", ".join(scan_only))
    if pin_only:
        lines.append("  pin-only (no longer skipped): " + ", ".join(pin_only))
    lines.append("  " + _UPDATE_HINT)
    return "\n".join(lines)


def _lexical_rust_files(root: Path, scan_root: Path) -> list[Path]:
    """Enumerate every regular file and reject symlinks/non-``.rs`` files."""

    source = root / scan_root
    symlinks: list[str] = []
    non_rust: list[str] = []
    if source.is_symlink():
        symlinks.append(scan_root.as_posix())
    files: list[Path] = []
    if source.is_dir() and not symlinks:
        for directory, dirnames, filenames in os.walk(source, followlinks=False):
            directory_path = Path(directory)
            for name in sorted((*dirnames, *filenames)):
                path = directory_path / name
                if path.is_symlink():
                    symlinks.append(path.relative_to(root).as_posix())
            for name in sorted(filenames):
                path = directory_path / name
                if path.is_symlink() or not path.is_file():
                    # The ``not path.is_file()`` half is this enumerator's sole
                    # non-fail-closed branch: git checkouts cannot carry a FIFO,
                    # while the old read-text path would block on one. Symlinks
                    # were recorded above and still fail closed; only this
                    # non-regular case is silently omitted here.
                    continue
                files.append(path)
                if not name.endswith(".rs"):
                    non_rust.append(path.relative_to(root).as_posix())
    if symlinks:
        raise RuntimeError(
            "FAIL: writer gates reject file or directory symlinks under src/: "
            + ", ".join(sorted(symlinks))
            + ". "
            + _SYMLINK_HINT
        )
    if non_rust:
        raise RuntimeError(
            "FAIL: writer gates reject non-.rs regular files under src/: "
            + ", ".join(sorted(non_rust))
            + ". The writer gate cannot classify non-.rs files under src/; "
            "remove them or extend the gate policy."
        )
    return sorted(files)


def validated_scan_files(
    root: Path,
    scan_root: Path,
    is_test_file: Callable[[str], bool],
    *,
    pinned_paths: Iterable[str] = PINNED_TEST_ONLY_MODULE_FILES,
) -> tuple[list[Path], frozenset[Path]]:
    """Enumerate, classify, pin-check, and return all files plus whole skips."""

    pinned = frozenset(Path(path).as_posix() for path in pinned_paths)
    all_files = _lexical_rust_files(root, scan_root)
    basename_skips = {path for path in all_files if is_test_file(path.name)}
    production_files = [path for path in all_files if path not in basename_skips]
    if not production_files:
        if all_files:
            detail = (
                "all enumerated regular .rs files are basename-classified "
                "test-only files"
            )
        else:
            detail = "the lexical src/ enumeration found no regular .rs files"
        raise RuntimeError(
            "FAIL: writer-gate production file list is empty; the shared "
            "test-only resolver was not invoked because its empty-input "
            "fallback would scan the repository instead of this lexical src/ "
            "enumeration. "
            + detail
            + ". Restore a production .rs file under src/ or "
            "extend the gate policy."
        )
    resolved_to_lexical = {path.resolve(): path for path in all_files}
    resolver_results = _INVENTORY.test_only_module_files(
        production_files=production_files,
        all_files=all_files,
        read_text_fn=lambda path: path.read_text(encoding="utf-8"),
    )
    resolver_skips: set[Path] = set()
    unmapped: list[str] = []
    for result in resolver_results:
        lexical = resolved_to_lexical.get(result.resolve())
        if lexical is None:
            unmapped.append(result.as_posix())
        else:
            resolver_skips.add(lexical)
    if unmapped:
        raise RuntimeError(
            "FAIL: test-only resolver returned paths outside lexical src/ enumeration: "
            + ", ".join(sorted(unmapped))
            + ". "
            + _UPDATE_HINT
        )

    whole_skips = frozenset(basename_skips | resolver_skips)
    lexical_skips = {
        path.relative_to(root).as_posix() for path in whole_skips
    }
    drift = skip_pin_drift(lexical_skips, pinned)
    pinned_count = len(pinned)
    if len(whole_skips) != pinned_count:
        census = (
            "FAIL: writer-gate skipped census differs from pin count "
            f"({len(whole_skips)} skipped, {pinned_count} pinned).\n  {_UPDATE_HINT}"
        )
        raise RuntimeError(f"{drift}\n{census}" if drift else census)
    if drift:
        raise RuntimeError(drift)
    return all_files, whole_skips
