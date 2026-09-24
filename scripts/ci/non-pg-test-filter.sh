#!/usr/bin/env bash

# Canonical libtest filter for lanes that must exclude PostgreSQL tests.
# Whether a test needs a database is a property of its body, not its name, so
# the module skips and the replay list are generated from the source
# classifier's manifest rather than a naming convention. Regenerate with:
#   python3 scripts/check_pg_test_lane_membership.py --write-non-pg-filter
# The PG lane runs on ubuntu alone, so a test this filter skips keeps no
# macOS/Windows coverage unless the replay list names it.
# The workflow shell consumes this after sourcing the file.

# shellcheck disable=SC2034
# BEGIN generated non-PG lane selection
NON_PG_SKIP_ARGS=(
  --skip _pg
  --skip pg_
  --skip postgres
  --skip db::auto_queue::entries::dispatch_failure::tests
  --skip db::auto_queue::entries::tests
  --skip db::auto_queue::phase_gates::current_batch_phase_pg_tests
  --skip db::auto_queue::phase_gates::reconcile_phase_gate_pg_tests
  --skip db::auto_queue::tests::dispatch_terminal_sync_pg_tests
  --skip db::auto_queue::tests::grouped_card_count_pg_tests
  --skip db::automation_candidates::verdict_tests
  --skip db::calendar_sync::postgres_tests
  --skip db::campaigns::tests
  --skip db::dispatched_session_canonical_identity::pg_tests
  --skip db::dispatched_session_rebind_override::tests
  --skip db::dispatched_sessions::selector_cleanup_tests::session_authority_pg_tests
  --skip db::dispatched_sessions::tests
  --skip db::dispatches::delivery_events::tests
  --skip db::dispatches::outbox::delivery::tests
  --skip db::idempotency::tests::pg_integration
  --skip db::intake_outbox::migration_pg_tests
  --skip db::intake_outbox::postgres_tests
  --skip db::intake_outbox_delivery_proof::tests
  --skip db::intake_outbox_dispatch_stamp::tests
  --skip db::intake_outbox_dispatched_audit::postgres_tests
  --skip db::postgres::test_db_reclaim::tests
  --skip db::postgres::tests
  --skip db::prompt_manifests::tests
  --skip db::relay_dead_letter::tests
  --skip db::scheduled_messages::postgres_tests
  --skip db::session_transcripts::clear_fence_pg_tests
  --skip dispatch::dispatch_cancel::pg_observability_tests
  --skip dispatch::dispatch_context::pg_rereview_tests
  --skip dispatch::dispatch_status::auto_queue_phase_gate_finalize_wrapper_tests::postgres_tests
  --skip dispatch::dispatch_status::terminal_timestamp_tests
  --skip engine::ops::auto_queue_ops::tests
  --skip engine::ops::config_ops::tests
  --skip engine::ops::db_ops::tests
  --skip engine::ops::kanban_ops::tests
  --skip engine::ops::message_ops::tests
  --skip github::sync::terminal_open_alert_tests
  --skip high_risk_recovery
  --skip reconcile::dispatch_delivery_reconcile_tests
  --skip server::issue_specs::issue_specs_pg_tests
  --skip server::message_outbox_retry_tests
  --skip server::multinode_regression::multinode_regression_pg_tests
  --skip server::resource_locks::resource_locks_pg_tests
  --skip server::routes::auto_queue_lifecycle_pg_tests::tests
  --skip server::routes::auto_queue_preflight_harness_tests
  --skip server::routes::campaigns::tests
  --skip server::routes::dispatched_sessions::tests
  --skip server::routes::dispatches::crud::tests::dispatch_api_pg_tests
  --skip server::routes::escalation::manual_decision_gate_tests
  --skip server::routes::memory_api::request_body_tests
  --skip server::routes::message_outbox::tests
  --skip server::routes::queue_api::cancel_queue_preserve_pg_tests
  --skip server::routes::scheduled_messages::postgres_tests
  --skip server::routes::stats::memento_feedback_stats_pg_tests
  --skip server::task_dispatch_claims::task_dispatch_claims_pg_tests
  --skip services::agent_quality::regression_alerts::explicit_decode_fallback_tests
  --skip services::agent_recovery::durable::postgres_tests
  --skip services::auto_queue::cancel_run::pg_tests
  --skip services::auto_queue::cleanup_tasks::cleanup_tasks_pg_tests::pg_tests
  --skip services::auto_queue::route::activate_command::tests::activate_upstream_eligibility_gate_pg_tests
  --skip services::auto_queue::route::activate_command::tests::depth_gate_activate_pg_tests
  --skip services::auto_queue::route::activate_command::tests::side_path_hijack_pg_tests
  --skip services::auto_queue::route::command::reset_run_scope_pg_tests
  --skip services::auto_queue::route::command::tests
  --skip services::auto_queue::route::fsm::tests
  --skip services::auto_queue::route::phase_gate::tests
  --skip services::auto_queue::route::planning::record_entry_dispatch_failure_tests
  --skip services::auto_queue::route::route_generate::deploy_gate_request_rejection_tests::postgres_tests
  --skip services::auto_queue::runtime::clear_slot_sessions_pg_tests::tests
  --skip services::auto_queue::tests
  --skip services::automation_candidate_materializer::iteration_result_tests
  --skip services::cluster::attachment_transfer::storage_tests
  --skip services::cluster::execution_capacity::tests
  --skip services::cluster::intake_preflight::tests
  --skip services::cluster::intake_router_hook::agent_execution_node_tests
  --skip services::cluster::intake_router_hook::attachment_tests
  --skip services::cluster::intake_router_hook::capacity_tests
  --skip services::cluster::intake_router_hook::edge_case_tests
  --skip services::cluster::intake_router_hook::execution_requirement_tests
  --skip services::cluster::intake_router_hook::owner_record::tests
  --skip services::cluster::intake_router_hook::pg_tests
  --skip services::cluster::intake_worker::dispatch_stamp_tests
  --skip services::cluster::machine_resources::store::tests
  --skip services::discord::catch_up::too_old_notice::tests
  --skip services::discord::health::recovery::stall_alert::tests
  --skip services::discord::health::recovery::stall_watchdog_auto_heal_tests
  --skip services::discord::idle_cleanup_selector_tests
  --skip services::discord::idle_recap_interaction::tests
  --skip services::discord::relay_recovery::circuit_breaker::tests
  --skip services::discord::relay_recovery::relay_recovery_circuit_alert_producer::tests
  --skip services::discord::router::intake_dispatch::tests
  --skip services::discord::router::message_handler::intake_turn::dispatch_stamp::postgres_tests
  --skip services::discord::router::message_handler::voice_announcement_route::voice_route_tests
  --skip services::discord::runtime_bootstrap::gateway_lease_recovery_tests
  --skip services::discord::runtime_bootstrap::intake_delivery_capability::postgres_tests
  --skip services::discord::runtime_bootstrap::intake_delivery_reconciler::postgres_tests
  --skip services::discord::runtime_bootstrap::intake_delivery_sweep::tests
  --skip services::discord::session_runtime::worktree_reuse_channel_isolation_tests
  --skip services::discord::task_notification_delivery::tests
  --skip services::discord::terminal_delivery_custody::pg_tests
  --skip services::discord::tmux::watcher_lifecycle::dispatched_origin_ghost_tests::dispatched_origin_ghost_order_pg_tests
  --skip services::discord::tmux::watcher_lifecycle::restore_tests::restored_session_cwd_channel_isolation_pg_tests
  --skip services::discord::tui_prompt_relay::tests::synthetic_bridge_handoff_pg_tests
  --skip services::discord::turn_bridge::completion_guard::completion_postgres::dispatch_failure_pg_tests
  --skip services::discord::turn_bridge::headless_delivery::production_seam_tests
  --skip services::discord::turn_bridge::intake_settlement::tests
  --skip services::discord::turn_bridge::recovery_text::tests
  --skip services::discord::turn_bridge::resume_pin_tests
  --skip services::discord::turn_bridge::terminal_outcome_delivery::delivery_epilogue_tests::rowless_receipt_tests::pg_tests
  --skip services::discord::turn_bridge::voice_completion::voice_completion_tests
  --skip services::dispatches::discord_delivery::guard::tests::delivery_journal_pg_tests
  --skip services::dispatches::outbox_claiming::tests::outbox_claiming_pg_tests
  --skip services::dispatches::wait_queue::tests
  --skip services::maintenance::jobs::db_retention::tests
  --skip services::maintenance::jobs::worktree_orphan_sweep::active_dispatch_worktree_keep_set_pg_tests
  --skip services::maintenance::jobs::worktree_orphan_sweep::keep_set_query_failure_fail_closed_pg_tests
  --skip services::maintenance::jobs::worktree_orphan_sweep::resumable_keep_set_query_pg_tests
  --skip services::message_outbox::postgres_held_gc_tests
  --skip services::message_outbox::postgres_source_contract_tests
  --skip services::message_outbox_circuit_authority_tests
  --skip services::message_outbox_recovery_tests
  --skip services::observability::queries::alert_authority_tests
  --skip services::observability::recovery_audit::tests::recovery_audit_pg_tests
  --skip services::observability::turn_lifecycle::tests::turn_lifecycle_pg_tests
  --skip services::pipeline_override::pipeline_override_pg_tests
  --skip services::pipeline_routes::tests
  --skip services::scheduled_messages::context_snapshot::postgres_tests
  --skip services::scheduled_messages::postgres_tests
  --skip services::session_forwarding::tests
  --skip services::session_resume::tests
  --skip services::settings::tests
  --skip services::stale_turn_reconciler::tests
  --skip services::tmux_turn_liveness::tests_pg
  --skip voice::announce_meta::tests
  --skip voice::turn_link::tests
)
NON_PG_FILTER_REPLAY=(
  cli::dcserver_pg_bootstrap::tests::backoff_delay_follows_exponential_schedule
  cli::dcserver_pg_bootstrap::tests::backoff_delay_saturates_at_cap
  cli::dcserver_pg_bootstrap::tests::connect_exhausts_budget_and_reports_last_error
  cli::dcserver_pg_bootstrap::tests::connect_retries_then_succeeds_recording_backoff
  cli::dcserver_pg_bootstrap::tests::connect_returns_immediately_on_first_success
  cli::dcserver_pg_bootstrap::tests::exhausted_ok_none_reports_required_message
  cli::dcserver_pg_bootstrap::tests::pool_timeout_diagnostic_includes_timestamp_source_and_attempt
  cli::dcserver_pg_bootstrap::tests::slow_startup_timeout_exhausts_retries_and_reports_the_exit_line
  cli::doctor::orchestrator::profile_filter_tests::postgres_checksum_mismatch_detail_includes_applied_and_resolved_hashes
  db::auto_queue::entries::tests::pinned_dispatch_identity_is_declared_before_the_stale_retry_loop
  db::auto_queue::phase_gates::reconcile_phase_gate_pg_tests::explicit_phase_gate_verdict_key_blocks_inference
  db::auto_queue::phase_gates::reconcile_phase_gate_pg_tests::non_string_explicit_verdict_preserves_checks_inference
  db::auto_queue::phase_gates::reconcile_phase_gate_pg_tests::parse_phase_gate_context_handles_string_batch_phase
  db::auto_queue::phase_gates::reconcile_phase_gate_pg_tests::parse_phase_gate_context_returns_none_without_run_id
  db::auto_queue::phase_gates::reconcile_phase_gate_pg_tests::pass_alias_matches_phase_gate_pass_verdict_when_checks_pass
  db::auto_queue::phase_gates::reconcile_phase_gate_pg_tests::whitespace_padded_check_status_is_not_inferred_as_pass
  db::automation_candidates::verdict_tests::card_update_guards_require_exactly_one_row
  db::automation_candidates::verdict_tests::crashed_always_discards
  db::automation_candidates::verdict_tests::final_iteration_boundary
  db::automation_candidates::verdict_tests::higher_metric_improvement_keeps
  db::automation_candidates::verdict_tests::lower_metric_improvement_keeps
  db::automation_candidates::verdict_tests::metric_regression_or_equal_discards
  db::automation_candidates::verdict_tests::no_metrics_discards
  db::automation_candidates::verdict_tests::parses_metric_direction_aliases
  db::automation_candidates::verdict_tests::simplification_always_keeps
  db::automation_candidates::verdict_tests::timeout_always_discards
  db::campaigns::tests::campaign_checkpoint_keeps_unchanged_node_time_and_resume_context
  db::campaigns::tests::campaign_checkpoint_normalizes_optional_groups_without_inference
  db::campaigns::tests::campaign_validation_rejects_false_completion_and_bad_identity
  db::campaigns::tests::campaign_validation_rejects_missing_duplicate_and_cyclic_dependencies
  db::dispatched_session_canonical_identity::pg_tests::canonical_identity_conflict_is_http_409_ready
  db::dispatches::delivery_events::tests::dispatch_delivery_event_serde_roundtrips_snake_case_status
  db::dispatches::metadata::tests::parse_pg_dispatch_context_ignores_empty_context
  db::dispatches::metadata::tests::parse_pg_dispatch_context_rejects_malformed_json
  db::dispatches::metadata::tests::parse_pg_dispatch_context_rejects_non_object_context
  db::intake_outbox_delivery_proof::tests::stale_reader_projects_exactly_id
  db::postgres::tests::agent_roster_sync_gated_to_leader_or_single_node
  db::postgres::tests::background_backpressure_disabled_when_reserve_zero
  db::postgres::tests::background_backpressure_saturating_boundaries
  db::postgres::tests::background_backpressure_yields_only_at_or_past_budget
  db::postgres::tests::bootstrap_migration_pool_has_longer_scoped_deadline
  db::postgres::tests::checksum_hex_formats_lowercase_byte_pairs
  db::postgres::tests::checksum_resolution_filters_down_migrations_to_avoid_false_positive
  db::postgres::tests::clamp_foreground_reserve_always_leaves_a_background_slot
  db::postgres::tests::ownership_registry_key_matches_between_admin_url_and_config_options
  db::postgres::tests::postgres_config_is_disabled_by_default
  db::postgres::tests::postgres_summary_uses_config_fields_when_enabled
  db::postgres::tests::postgres_test_sqlx_timeout_wrapper_fails_fast
  db::postgres::tests::runtime_pool_settings_enable_dead_peer_detection
  db::postgres::tests::startup_pool_settings_raise_pool_size_and_acquire_timeout
  db::prompt_manifests::tests::prompt_manifest_builder_separates_content_visibility
  db::prompt_manifests::tests::prompt_manifest_layer_truncates_adk_provided_at_byte_cap
  db::prompt_manifests::tests::prompt_manifest_layer_truncation_disabled_when_config_disabled
  db::prompt_manifests::tests::prompt_manifest_layer_truncation_handles_utf8_boundary
  db::prompt_manifests::tests::prompt_manifest_layer_zero_cap_disables_truncation_for_visibility
  db::prompt_manifests::tests::prompt_manifest_token_estimate_is_chars_div_four
  db::prompt_manifests::tests::prompt_manifest_totals_exclude_disabled_layers
  engine::ops::config_ops::tests::config_get_raw_keeps_scalar_string_semantics_and_precedence
  engine::ops::config_ops::tests::config_get_raw_never_exposes_the_reserved_runtime_blob_key
  engine::ops::config_ops::tests::config_get_raw_preserves_runtime_blob_json_types
  engine::ops::config_ops::tests::config_get_raw_queries_runtime_blob_only_for_allowlisted_scalar_misses
  engine::ops::config_ops::tests::config_get_raw_runtime_blob_fallback_is_allowlisted_and_fail_closed
  engine::ops::db_ops::tests::policy_sql_param_wire_text_preserves_json_scalar_and_container_values
  engine::ops::db_ops::tests::prepare_policy_sql_for_pg_errors_when_parameter_is_missing
  engine::ops::db_ops::tests::prepare_policy_sql_for_pg_ignores_comment_quotes_while_rewriting_rowid
  engine::ops::db_ops::tests::prepare_policy_sql_for_pg_leaves_question_marks_inside_comments
  engine::ops::db_ops::tests::prepare_policy_sql_for_pg_leaves_question_marks_inside_strings
  engine::ops::db_ops::tests::prepare_policy_sql_for_pg_preserves_escaped_quote_identifiers
  engine::ops::db_ops::tests::prepare_policy_sql_for_pg_preserves_quoted_rowid_alias
  engine::ops::db_ops::tests::prepare_policy_sql_for_pg_reuses_numbered_placeholders
  engine::ops::db_ops::tests::prepare_policy_sql_for_pg_rewrites_insert_or_ignore
  engine::ops::db_ops::tests::prepare_policy_sql_for_pg_rewrites_insert_or_replace
  engine::ops::db_ops::tests::prepare_policy_sql_for_pg_rewrites_quoted_rowid_expressions
  engine::ops::db_ops::tests::prepare_policy_sql_for_pg_rewrites_rowid_tokens
  engine::ops::db_ops::tests::prepare_policy_sql_for_pg_skips_dollar_quoted_literals_for_rowid
  engine::ops::db_ops::tests::test_translate_sqlite_rowid_aliases
  github::sync::terminal_open_alert_tests::format_terminal_open_alert_contains_actionable_context
  github::sync::terminal_open_alert_tests::parses_batched_graphql_issue_response
  github::sync::terminal_open_alert_tests::stale_candidate_selection_excludes_pipeline_terminal_statuses
  github::sync::terminal_open_alert_tests::stale_candidate_selection_rotates_past_old_open_frontier
  github::sync::terminal_open_alert_tests::stale_candidate_selection_skips_normal_fetch_window
  github::sync::terminal_open_alert_tests::stale_fetch_failure_records_metric_without_dropping_normal_issues
  high_risk_recovery::cross_channel_tmux_claim_observability_distinguishes_thread_follow_up_4984
  reconcile::dispatch_delivery_reconcile_tests::dispatch_delivery_reconcile_classifies_rows_without_postgres
  reconcile::dispatch_delivery_reconcile_tests::dispatch_delivery_reconcile_treats_all_completed_statuses_as_delivered
  server::message_outbox_retry_tests::message_outbox_failure_action_retries_then_fails
  server::message_outbox_retry_tests::outbox_alert_snippet_truncates_and_handles_empty
  server::message_outbox_retry_tests::session_release_is_limited_to_terminal_turn_delivery_sources
  server::message_outbox_retry_tests::session_release_requires_matching_terminal_delivery_outbox_marker
  server::message_outbox_retry_tests::session_release_requires_same_failed_outbox_session_when_present
  server::routes::auto_queue_preflight_harness_tests::auto_queue_preflight_detects_split_brain_completion
  server::routes::campaigns::tests::ledger_body_limit_stays_at_sixteen_mebibytes
  server::routes::campaigns::tests::ledger_put_accepts_a_document_larger_than_the_axum_default_limit
  server::routes::campaigns::tests::ledger_put_over_the_limit_reports_the_limit_and_the_remedy
  server::routes::departments::tests::ensure_pg_missing_pool_preserves_503_and_message
  server::routes::memory_api::request_body_tests::detect_memory_backend_force_local_overrides_active_memento
  server::routes::memory_api::request_body_tests::detect_memory_backend_uses_local_when_memento_runtime_unavailable
  server::routes::memory_api::request_body_tests::detect_memory_backend_uses_memento_when_runtime_config_is_active
  server::routes::memory_api::request_body_tests::memento_recall_forget_unsupported_response_is_explicit
  server::routes::memory_api::request_body_tests::memento_remember_scope_validation_rejects_conflicts
  server::routes::memory_api::request_body_tests::remember_body_deserializes_channel_scope_fields
  server::routes::message_outbox::tests::exact_id_contract_rejects_mass_and_ambiguous_inputs
  server::routes::message_outbox::tests::monitor_alert_routing_separates_actionable_and_informational_kinds
  server::routes::message_outbox::tests::protected_message_outbox_routes_are_registered_contract
  server::routes::message_outbox::tests::redrive_contract_defaults_to_dry_run_and_denies_mass_field
  server::routes::scheduled_messages::postgres_tests::scheduled_message_bot_defaults_to_non_triggering_notify
  server::routes::scheduled_messages::postgres_tests::scheduled_push_rejects_agent_only_fields_but_allows_explicit_clears
  services::agent_quality::regression_alerts::explicit_decode_fallback_tests::fails_closed_on_column_decode_error
  services::agent_quality::regression_alerts::explicit_decode_fallback_tests::fails_closed_on_other_errors
  services::agent_quality::regression_alerts::explicit_decode_fallback_tests::metric_thresholds_preserve_retired_authority_policy
  services::agent_quality::regression_alerts::explicit_decode_fallback_tests::preserves_none_for_legitimately_absent_optional_column
  services::agent_quality::regression_alerts::explicit_decode_fallback_tests::quality_channel_target_normalizes_without_implicit_fallback
  services::agent_quality::regression_alerts::explicit_decode_fallback_tests::returns_value_on_success
  services::auto_queue::route::control_routes::phase_gate_repair_route_tests::repair_caller_audit_label_marks_unverified_when_pg_unresolved
  services::auto_queue::route::control_routes::phase_gate_repair_route_tests::repair_caller_audit_label_uses_verified_principal_when_pg_resolved
  services::auto_queue::route::fsm::tests::clamp_retry_limit_bounds
  services::auto_queue::route::phase_gate::tests::sandbox_preflight_metadata_disables_external_side_effects_only_when_safe
  services::auto_queue::tests::auto_queue_status_entry_normalizes_github_repo_url
  services::auto_queue::tests::auto_queue_status_omits_diagnostics_without_slot_invariant_violation
  services::auto_queue::tests::auto_queue_status_reports_actionable_slot_invariant_diagnostics
  services::auto_queue::tests::auto_queue_status_reports_delivery_split_brain_and_timeout
  services::auto_queue::tests::auto_queue_status_surfaces_review_cycle_clock
  services::auto_queue::tests::thread_link_view_only_builds_url_for_discord_snowflakes
  services::cluster::attachment_transfer::storage_tests::attachment_upload_reference_preserves_legacy_json_and_enforces_size_limits
  services::cluster::execution_capacity::tests::execution_capacity_ranking_uses_ratio_fairness_and_preserves_legacy_selector
  services::cluster::intake_preflight::tests::claude_and_codex_emit_structured_pass_and_fail_evidence
  services::cluster::intake_preflight::tests::each_required_failure_is_independently_fail_closed
  services::cluster::intake_preflight::tests::missing_or_malformed_snapshot_fails_closed
  services::cluster::intake_preflight::tests::missing_source_expectations_fail_closed
  services::cluster::intake_preflight::tests::ready_target_requires_no_execution_callback
  services::cluster::intake_preflight::tests::unsupported_provider_fails_closed
  services::cluster::intake_router_hook::owner_record::tests::advisory_lock_key_is_stable
  services::cluster::intake_router_hook::owner_record::tests::idempotency_key_is_composed_and_normalized
  services::cluster::machine_resources::store::tests::recorder_queue_is_bounded_without_waiting_for_the_database
  services::discord::health::recovery::stall_alert::tests::owner_zero_and_tui_sentinel_never_render_mentions
  services::discord::health::recovery::stall_alert::tests::producer_liveness_suppresses_stall_page
  services::discord::health::recovery::stall_watchdog_auto_heal_tests::idle_tmux_stale_turn_clear_refusal_preserves_mailbox_and_session
  services::discord::health::recovery::stall_watchdog_auto_heal_tests::idle_tmux_stale_turn_guarded_finish_preserves_new_mailbox_claim
  services::discord::health::recovery::stall_watchdog_auto_heal_tests::idle_tmux_stale_turn_guarded_finish_preserves_new_same_id_mailbox_claim
  services::discord::health::recovery::stall_watchdog_auto_heal_tests::idle_tmux_stale_turn_tail_recheck_preserves_mailbox_after_precheck_passed
  services::discord::health::recovery::stall_watchdog_auto_heal_tests::reachability_warrant_vetoes_transport_unknown_watchdog_branch
  services::discord::health::recovery::stall_watchdog_auto_heal_tests::reuse_no_op_reattach_tick_still_skips_the_destructive_branches_5396
  services::discord::health::recovery::stall_watchdog_auto_heal_tests::stall_watchdog_cleanup_keeps_orphan_pending_token_without_measured_death
  services::discord::idle_recap_interaction::tests::claim_database_error_fails_closed_without_injection
  services::discord::idle_recap_interaction::tests::compact_uses_claimed_recap_target_and_native_prompt
  services::discord::idle_recap_interaction::tests::concurrent_compact_claims_allow_exactly_one_injection
  services::discord::idle_recap_interaction::tests::foreign_or_missing_owner_never_claims_local_host_target
  services::discord::idle_recap_interaction::tests::foreign_session_identity_preserves_pointer_without_claim_or_injection
  services::discord::idle_recap_interaction::tests::idle_recap_component_router_accepts_all_recap_actions
  services::discord::idle_recap_interaction::tests::injection_failure_is_terminal_after_claim
  services::discord::idle_recap_interaction::tests::owner_handoff_after_claim_is_fenced_immediately_before_injection
  services::discord::idle_recap_interaction::tests::rebinding_cannot_redirect_compact_to_current_channel_session
  services::discord::idle_recap_interaction::tests::recap_component_message_id_parser_rejects_zero_and_foreign_prefixes
  services::discord::idle_recap_interaction::tests::recap_prompt_route_rejects_unrelated_custom_ids
  services::discord::idle_recap_interaction::tests::recap_prompt_route_sends_suggest_to_internal_followup_handler
  services::discord::idle_recap_interaction::tests::recap_prompt_sent_ephemeral_includes_actual_prompt_text
  services::discord::relay_recovery::circuit_breaker::tests::alert_enqueue_failure_stays_pending_and_retry_marks_only_alert_flag
  services::discord::relay_recovery::circuit_breaker::tests::alert_marker_is_exact_episode_scoped
  services::discord::relay_recovery::circuit_breaker::tests::crash_after_local_alert_commit_resumes_same_held_row_without_reenqueue
  services::discord::relay_recovery::circuit_breaker::tests::exact_new_identity_resets_while_stale_identity_cannot_poison_it
  services::discord::relay_recovery::circuit_breaker::tests::manual_lane_never_uses_durable_circuit
  services::discord::relay_recovery::circuit_breaker::tests::nonzero_originating_message_rejects_a_distinct_mailbox_anchor
  services::discord::relay_recovery::circuit_breaker::tests::nonzero_originating_message_reserves_with_the_same_mailbox_anchor
  services::discord::relay_recovery::circuit_breaker::tests::old_snapshot_cannot_reset_or_spend_after_replacement_reserves
  services::discord::relay_recovery::circuit_breaker::tests::progressed_episode_retains_held_alert_until_cleanup_is_acknowledged
  services::discord::relay_recovery::circuit_breaker::tests::same_episode_is_bounded_forever_until_confirmed_frontier_progress
  services::discord::relay_recovery::circuit_breaker::tests::same_external_ids_still_pin_session_output_and_nonce_axes
  services::discord::relay_recovery::circuit_breaker::tests::stale_decision_cannot_spend_the_replacement_turn_budget
  services::discord::relay_recovery::circuit_breaker::tests::stale_stage_cancel_failure_becomes_a_durable_cleanup_obligation
  services::discord::relay_recovery::circuit_breaker::tests::zero_originating_message_reserves_with_a_distinct_mailbox_anchor
  services::discord::relay_recovery::circuit_breaker::tests::zero_originating_message_still_rejects_episode_identity_mismatches
  services::discord::restart_mode::protocol_v2::disposition::high_risk_recovery::every_disposition_owns_exact_original_bytes
  services::discord::restart_mode::protocol_v2::fs::unix::high_risk_recovery::absolute_and_relative_root_traversal_record_exact_pairs_and_identity
  services::discord::restart_mode::protocol_v2::fs::unix::high_risk_recovery::directory_mutation_records_sync_facts_and_owned_fds_outlive_parents
  services::discord::restart_mode::protocol_v2::fs::unix::high_risk_recovery::regular_open_rejects_special_nodes_and_bounded_reads_stay_bounded
  services::discord::restart_mode::protocol_v2::fs::unix::high_risk_recovery::root_traversal_and_path_replacement_keep_the_pinned_inode
  services::discord::restart_mode::protocol_v2::fs::unix::high_risk_recovery::sealed_stage_link_records_identity_and_cleanup_stays_maintenance_only
  services::discord::restart_mode::protocol_v2::fs::unsupported::high_risk_recovery::unsupported_precedes_validation_for_every_facade_operation
  services::discord::restart_mode::protocol_v2::phase::codec::high_risk_recovery::all_phase_kinds_round_trip_to_golden_canonical_wire
  services::discord::restart_mode::protocol_v2::phase::codec::high_risk_recovery::malformed_and_unknown_versions_classify_without_panicking_or_losing_raw
  services::discord::restart_mode::protocol_v2::phase::codec::high_risk_recovery::provider_and_channel_require_canonical_lower_utf8_hex
  services::discord::restart_mode::protocol_v2::phase::codec::high_risk_recovery::unknown_fields_invalid_values_and_reencoding_preserve_exact_raw
  services::discord::restart_mode::protocol_v2::phase::reducer::high_risk_recovery::nonaccepted_dispositions_propagate_exactly_and_reducer_is_total
  services::discord::restart_mode::protocol_v2::phase::reducer::high_risk_recovery::phase_order_repeats_and_post_receipt_conflict_without_over_suppression
  services::discord::restart_mode::protocol_v2::phase::reducer::high_risk_recovery::positive_prefixes_reach_all_four_states_and_keep_last_original_raw
  services::discord::restart_mode::protocol_v2::phase::reducer::high_risk_recovery::sequence_hash_and_identity_conflicts_keep_the_first_offending_raw
  services::discord::restart_mode::protocol_v2::values::high_risk_recovery::canonical_uuid_forms_only
  services::discord::restart_mode::protocol_v2::values::high_risk_recovery::identities_preserve_exact_unicode_and_reject_empty_or_nul
  services::discord::restart_mode::protocol_v2::values::high_risk_recovery::safe_relative_reference_accepts_nested_portable_components
  services::discord::restart_mode::protocol_v2::values::high_risk_recovery::safe_relative_reference_rejects_each_unsafe_class
  services::discord::restart_mode::protocol_v2::values::high_risk_recovery::safe_relative_reference_rejects_superscript_reserved_basenames
  services::discord::restart_mode::protocol_v2::values::high_risk_recovery::safe_relative_reference_rejects_windows_forbidden_characters
  services::discord::router::intake_dispatch::tests::intake_dispatch_invariant_direct_execution_body_has_no_external_producer_callsites
  services::discord::router::intake_dispatch::tests::intake_dispatch_invariant_enforce_without_postgres_blocks_owner_unknown
  services::discord::router::intake_dispatch::tests::intake_dispatch_invariant_queued_entrypoints_promote_markers_after_admission_before_finish
  services::discord::router::intake_dispatch::tests::intake_dispatch_invariant_worker_post_claim_is_the_only_router_bypass
  services::discord::router::intake_dispatch::tests::telemetry_only_unopted_live_foreign_owner_stays_fenced_5040
  services::discord::router::intake_dispatch::tests::telemetry_only_unopted_live_local_fresh_pending_route_stays_fenced_5040
  services::discord::router::intake_dispatch::tests::telemetry_only_unopted_live_local_pending_open_route_runs_locally_5040
  services::discord::router::intake_dispatch::tests::telemetry_only_unopted_local_accepted_route_stays_fenced_5040
  services::discord::router::intake_dispatch::tests::telemetry_only_unopted_unknown_owner_authority_keeps_local_fence_5040
  services::discord::router::message_handler::voice_announcement_route::voice_route_tests::voice_announcement_foreground_miss_falls_back_to_normal_turn
  services::discord::router::message_handler::voice_announcement_route::voice_route_tests::voice_announcement_foreground_response_bypasses_normal_turn
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::existing_marker_cancel_restores_promotion_fence_for_retry
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::foreign_nonce_terminal_artifact_does_not_mask_our_cancellation
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::identity_terminal_proof_commits_handoff_despite_clock_regression
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::legacy_index_persisted_suppresses_a_matching_cancellation
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::marker_creation_links_a_complete_body_under_both_names
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::nonce_charset_gate_refuses_smuggled_newlines_and_builds_no_path
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::nonce_free_existing_marker_fails_closed_instead_of_claiming_promotion
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::nonce_free_supersession_folds_to_cancelled_despite_fresh_terminal_artifact
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::nonce_reuse_is_refused_without_publishing_an_index_only_marker
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::orphan_reap_requires_named_stale_matching_worker
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::promotion_owner_recovers_all_runtimes_when_cancel_precedes_first_poll_tick
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::retired_mtime_lifetime_gate_has_no_remaining_source_references
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::sequential_requests_keep_independent_identities
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::superseded_promotion_preserves_new_owner_fence_and_flags
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::supersession_chain_keeps_owner_until_final_cancel_and_recovers_all_runtimes
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::terminal_proof_is_three_valued_and_only_identity_is_green
  services::discord::runtime_bootstrap::gateway_lease_recovery_tests::unsafe_nonce_terminal_read_is_absent_and_never_promotes_the_index
  services::discord::runtime_bootstrap::intake_delivery_sweep::tests::spawn_wiring_claims_process_latch_before_observed_task
  services::discord::runtime_bootstrap::intake_delivery_sweep::tests::sweep_cutoffs_do_not_panic_for_extreme_values
  services::discord::runtime_bootstrap::intake_delivery_sweep::tests::sweep_spawns_exactly_once_per_process
  services::discord::runtime_bootstrap::intake_delivery_sweep::tests::sweep_task_can_restart_after_task_death
  services::discord::session_relay_sink::journal::pg_store::tests::stored_journal_event_mapping_is_closed_and_fail_closed
  services::discord::session_runtime::worktree_reuse_channel_isolation_tests::reconcile_noop_when_db_matches_tmux
  services::discord::session_runtime::worktree_reuse_channel_isolation_tests::reconcile_prefers_live_tmux_over_divergent_db_cwd
  services::discord::session_runtime::worktree_reuse_channel_isolation_tests::reconcile_refused_when_tmux_cwd_not_managed_or_unusable
  services::discord::task_notification_delivery::tests::ambiguous_post_retries_same_nonce_without_second_message
  services::discord::task_notification_delivery::tests::concurrent_ensure_posts_once_and_returns_same_card
  services::discord::task_notification_delivery::tests::confirmed_missing_edit_uses_revision_nonce_for_one_replacement
  services::discord::task_notification_delivery::tests::durable_response_turn_key_uses_shared_recovery_identity_for_degenerate_turns
  services::discord::task_notification_delivery::tests::footer_background_marker_key_is_stable_across_delivery_paths
  services::discord::task_notification_delivery::tests::footer_only_marker_omits_private_task_anchors_from_rendered_preview
  services::discord::task_notification_delivery::tests::footer_only_marker_renders_background_summary_and_result_preview
  services::discord::task_notification_delivery::tests::footer_only_observation_keeps_full_card_for_later_promotion
  services::discord::task_notification_delivery::tests::footer_only_observation_posts_nothing_until_response_promotion
  services::discord::task_notification_delivery::tests::fully_unkeyed_task_event_cannot_be_deferred_to_footer
  services::discord::task_notification_delivery::tests::identity_less_subagent_prompt_and_stream_share_one_semantic_event
  services::discord::task_notification_delivery::tests::malformed_subagent_still_has_safe_durable_identity_and_card
  services::discord::task_notification_delivery::tests::memory_delivered_reconciliation_respects_channel_provider_and_session
  services::discord::task_notification_delivery::tests::missing_card_replacement_replays_same_nonce_after_post_commit_ambiguity
  services::discord::task_notification_delivery::tests::missing_required_reference_replaces_once_and_exactly_rebinds_response
  services::discord::task_notification_delivery::tests::promotion_waits_while_an_edit_owns_the_card_lease
  services::discord::task_notification_delivery::tests::recent_post_ack_loss_retries_same_nonce_without_second_physical_message
  services::discord::task_notification_delivery::tests::response_chunk_nonce_is_stable_bounded_and_distinct
  services::discord::task_notification_delivery::tests::response_turn_key_is_stable_and_separates_offsets
  services::discord::task_notification_delivery::tests::semantic_identity_separates_tasks_sessions_and_channels
  services::discord::task_notification_delivery::tests::sourced_idle_observation_and_stream_promotion_post_one_card
  services::discord::task_notification_delivery::tests::stream_context_recovers_tool_identity_from_task_start_state
  services::discord::task_notification_delivery::tests::subagent_agent_path_is_ignored_and_never_enters_identity
  services::discord::task_notification_delivery::tests::successful_send_with_failed_final_cas_surfaces_sent_but_uncommitted
  services::discord::task_notification_delivery::tests::terminal_task_card_includes_shared_completion_metadata_4806
  services::discord::task_notification_delivery::tests::transient_edit_never_falls_back_to_fresh_post
  services::discord::task_notification_delivery::tests::unavailable_pinned_bot_releases_lease_for_immediate_retry
  services::discord::task_notification_delivery::tests::xml_and_stream_json_share_semantic_key_and_nonce_is_bounded
  services::discord::tmux::watcher_lifecycle::restore_tests::restored_session_cwd_channel_isolation_pg_tests::configured_channel_binding_is_last_resort_and_provider_scoped
  services::discord::turn_bridge::completion_guard::completion_postgres::dispatch_failure_pg_tests::dispatch_failure_result_preserves_legacy_error_shape
  services::discord::turn_bridge::completion_guard::completion_postgres::dispatch_failure_pg_tests::dispatch_failure_result_uses_auth_token_expired_code
  services::discord::turn_bridge::completion_guard::completion_postgres::dispatch_failure_pg_tests::post_commit_failure_emits_result_and_quality_observability
  services::discord::turn_bridge::completion_guard::completion_postgres::dispatch_failure_pg_tests::runtime_writer_preserves_hard_error_outcome
  services::discord::turn_bridge::completion_guard::completion_postgres::runtime_completion_policy_tests::runtime_auto_queue_terminal_sync_matches_dispatch_completion_policy
  services::discord::turn_bridge::headless_delivery::outcome::tests::pg_error_cancel_suppresses_direct_fallback_and_is_cancelled
  services::discord::turn_bridge::headless_delivery::outcome::tests::pg_error_direct_fallback_failure_is_ambiguous_and_surfaces_error
  services::discord::turn_bridge::headless_delivery::outcome::tests::pg_error_direct_fallback_success_is_delivered
  services::discord::turn_bridge::headless_delivery::production_seam_tests::absent_outbox_pool_reaches_cancel_check_and_suppresses_direct_fallback
  services::discord::turn_bridge::headless_delivery::production_seam_tests::direct_fallback_notify_http_preference_is_caller_supplied_only
  services::discord::turn_bridge::headless_delivery::production_seam_tests::durable_exact_path_observes_cancellation_before_database_work
  services::discord::turn_bridge::headless_delivery::production_seam_tests::outbox_enqueue_error_reaches_cancel_check_then_direct_fallback
  services::discord::turn_bridge::intake_settlement::tests::classify_preserve_precedes_every_terminal_receipt
  services::discord::turn_bridge::intake_settlement::tests::settlement_sql_error_is_swallowed_and_counted
  services::discord::turn_bridge::intake_settlement::tests::terminal_outcome_delivery_awaits_one_settlement_call_with_branch_flags
  services::discord::turn_bridge::recovery_text::tests::direct_runtime_context_unavailable_matches_api_and_pg_errors
  services::discord::turn_bridge::recovery_text::tests::discord_recent_recovery_context_preserves_existing_format_and_limits
  services::discord::turn_bridge::recovery_text::tests::recovery_context_store_does_not_enqueue_lifecycle_notification
  services::discord::turn_bridge::resume_pin_tests::c1_actual_postlude_resume_pin_runtime_proof
  services::discord::turn_bridge::resume_pin_tests::c1_both_late_writers_consume_pin_without_registry_backfill
  services::discord::turn_bridge::resume_pin_tests::c1_cancelled_registered_pin_leaves_all_effects_untouched
  services::discord::turn_bridge::resume_pin_tests::c1_missing_and_idle_tail_mismatch_never_resume_latest
  services::discord::turn_bridge::resume_pin_tests::c1_poisoned_resume_lock_does_not_unpause
  services::discord::turn_bridge::resume_pin_tests::c1_same_synthetic_and_handoff_pin_resume_without_clearing_marker
  services::discord::turn_bridge::resume_pin_tests::c1_synthetic_and_handoff_stale_pins_leave_replacement_untouched
  services::discord::turn_bridge::resume_pin_tests::sa2_capture_hands_off_owned_provider_receiver
  services::discord::turn_bridge::voice_completion::voice_completion_tests::background_completion_target_marker_wins_over_reverse_lookup_disagreement
  services::discord::turn_bridge::voice_completion::voice_completion_tests::background_completion_target_refuses_legacy_prefix_without_marker
  services::discord::turn_bridge::voice_completion::voice_completion_tests::background_completion_target_refuses_marker_with_wrong_background_channel
  services::discord::turn_bridge::voice_completion::voice_completion_tests::background_completion_target_returns_marker_recorded_voice_channel
  services::discord::turn_bridge::voice_completion::voice_completion_tests::handoff_prompt_classification_requires_typed_marker
  services::discord::turn_bridge::voice_completion::voice_completion_tests::recognizes_voice_background_handoff_via_typed_marker
  services::discord::voice_barge_in::tests::background_handoff_refuses_publish_when_pg_reservation_fails
  services::dispatches::wait_queue::tests::wait_timeout_uses_wait_started_at
  services::dispatches::wait_queue::tests::wake_history_keeps_only_recent_entries
  services::dispatches::wait_queue::tests::wake_query_preserves_fifo_ordering
  services::health_diagnostics::tests::diagnostics_without_pg_pool_stay_safe
  services::maintenance::jobs::worktree_orphan_sweep::managed_root_recursion_tests::no_pg_is_noop_even_with_managed_orphans
  services::message_outbox::postgres_source_contract_tests::typed_outbox_core_preserves_cancel_at_the_observation_site
  services::message_outbox::postgres_source_contract_tests::typed_outbox_outcomes_fold_to_the_legacy_option_contract
  services::observability::cancellation_observability_tests::turn_cancelled_emit_records_normalized_payload_without_pg
  services::pipeline_routes::tests::invalid_backoff_is_bad_request
  services::pipeline_routes::tests::normalize_optional_blanks_to_none
  services::pipeline_routes::tests::persistence_sql_includes_backoff_column
  services::pipeline_routes::tests::stage_json_absent_backoff_is_null
  services::pipeline_routes::tests::stage_json_emits_backoff_field
  services::scheduled_messages::postgres_tests::postgres_precision_normalizes_linux_nanosecond_timestamps
  services::session_forwarding::tests::cancel_retry_accepts_ack_and_authenticated_structured_not_found
  services::session_forwarding::tests::cancel_retry_reloads_owner_only_for_conflict
  services::session_forwarding::tests::cleartext_rejection_never_yields_an_authenticated_request_target
  services::session_forwarding::tests::encode_path_segment_escapes_session_key_separators
  services::session_forwarding::tests::every_session_forwarder_uses_the_shared_trusted_request_builder
  services::session_forwarding::tests::forward_cancel_sends_auth_and_owner_headers
  services::session_forwarding::tests::forward_json_response_preserves_worker_auth_failure_status
  services::session_forwarding::tests::forwarded_header_is_detected_and_receiver_fence_is_exact
  services::session_forwarding::tests::invalid_forwarding_headers_send_no_authenticated_request
  services::session_forwarding::tests::legacy_session_key_match_is_numeric_delimiter_aware
  services::session_forwarding::tests::redirect_is_not_followed_and_bearer_never_reaches_redirect_target
  services::session_forwarding::tests::resolve_forward_target_keeps_missing_and_local_sessions_local
  services::session_forwarding::tests::resolve_forward_target_rejects_invalid_owner_and_local_instance_ids
  services::session_forwarding::tests::resolve_forward_target_returns_trusted_foreign_owner
  services::session_forwarding::tests::stale_capability_and_missing_trust_config_fail_before_forwarding
  services::session_resume::tests::completed_critical_section_cancels_its_watchdog
  services::session_resume::tests::critical_section_watchdog_records_once_without_cancelling_work
  services::session_resume::tests::discover_excludes_live_bound_session
  services::session_resume::tests::discover_ignores_out_of_lineage_sibling_worktree
  services::session_resume::tests::discover_returns_none_when_only_current_binding_exists
  services::session_resume::tests::discover_returns_none_without_current_cwd
  services::session_resume::tests::discover_skips_current_and_picks_newest_prior_in_lineage
  services::session_resume::tests::lineage_stem_strips_only_datetime_suffix
  services::session_resume::tests::off_runtime_discovery_preserves_selection_and_runtime_progress
  services::session_resume::tests::resume_runtime_binding_clear_absence_is_success
  services::session_resume::tests::transition_busy_response_exposes_retryable_korean_contract
  services::settings::tests::delivery_journal_mode_stays_yaml_only
  services::settings::tests::explicit_metadata_still_allows_full_replacement
  services::settings::tests::explicit_runtime_config_keys_respects_payload_metadata
  services::settings::tests::metadata_less_empty_runtime_config_omits_saved_non_explicit_keys
  services::settings::tests::metadata_less_runtime_config_preserves_omitted_explicit_override
  services::settings::tests::runtime_config_key_lookup_exposes_only_known_value_keys
  services::settings::tests::runtime_config_write_plan_has_one_blob_authority_and_cleanup_only
  services::settings::tests::seeded_runtime_config_applies_yaml_overrides_over_seeded_defaults
  services::settings::tests::seeded_runtime_config_preserves_explicit_api_overrides_over_yaml
  services::settings::tests::seeded_runtime_config_preserves_intentional_empty_explicit_metadata
  services::settings::tests::seeded_runtime_config_rebases_non_explicit_saved_values
  services::settings::tests::settings_response_dtos_serialize_existing_contract_fields
  services::settings::tests::settings_write_response_serializes_ok_contract
  services::stale_turn_reconciler::tests::tmux_identity_rejects_provider_mismatch_and_spinner_is_busy
  utils::async_bridge::tests::block_on_pg_result_fails_fast_when_bridge_deadline_already_passed
  utils::redact::tests::dsn_password_extracts_postgres_password_only
  utils::redact::tests::mask_dsn_password_redacts_postgres_password
  voice::announce_meta::tests::contains_does_not_consume_entry
  voice::announce_meta::tests::handoff_store_returns_none_when_absent
  voice::announce_meta::tests::handoff_store_round_trips_typed_metadata
  voice::announce_meta::tests::pending_handoff_reservation_binds_to_message_id
  voice::announce_meta::tests::pending_handoff_reservation_can_win_before_message_bind
  voice::announce_meta::tests::refresh_handoff_deadline_extends_ttl_when_entry_has_short_remaining
  voice::announce_meta::tests::refresh_handoff_deadline_preserves_meta_content
  voice::announce_meta::tests::refresh_handoff_deadline_returns_false_when_absent
  voice::announce_meta::tests::refresh_handoff_deadline_returns_false_when_ttl_already_at_max
  voice::announce_meta::tests::store_distinguishes_accepted_replay_entries
  voice::announce_meta::tests::store_is_one_shot
  voice::turn_link::tests::advisory_lock_key_is_stable
)
# END generated non-PG lane selection
readonly -a NON_PG_SKIP_ARGS
readonly -a NON_PG_FILTER_REPLAY

# Derive the positive PostgreSQL selector from the same canonical pairs. This
# keeps the nightly PG/non-PG selection sets complementary as the filter moves.
PG_INCLUDE_ARGS=()
for ((index = 1; index < ${#NON_PG_SKIP_ARGS[@]}; index += 2)); do
  PG_INCLUDE_ARGS+=("${NON_PG_SKIP_ARGS[$index]}")
done
unset index
readonly -a PG_INCLUDE_ARGS

# Windows caps a command line at 32767 bytes and the replay list is larger than
# that, so batch it instead of paying one process per test.
NON_PG_REPLAY_BATCH_BYTES="${NON_PG_REPLAY_BATCH_BYTES:-24000}"

run_non_pg_filter_replay() {
  local -a batch=()
  local budget=0 test_filter

  for test_filter in "${NON_PG_FILTER_REPLAY[@]}"; do
    if [ "${#batch[@]}" -gt 0 ] \
      && [ $((budget + ${#test_filter} + 1)) -gt "$NON_PG_REPLAY_BATCH_BYTES" ]; then
      cargo test --lib -- --exact "${batch[@]}" || return 1
      batch=()
      budget=0
    fi
    batch+=("$test_filter")
    budget=$((budget + ${#test_filter} + 1))
  done

  if [ "${#batch[@]}" -gt 0 ]; then
    cargo test --lib -- --exact "${batch[@]}" || return 1
  fi
}
