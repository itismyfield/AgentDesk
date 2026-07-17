# AgentDesk Architecture Guide

High-signal navigation guide for contributors. The generated inventories under `docs/generated/` and the `src/` snapshot below are the authoritative structure references. Regenerate them with `python3 scripts/generate_inventory_docs.py`.

## Repository Map

- `src/` — runtime code: CLI, HTTP server, Discord bot, orchestration, policy engine, persistence.
- `policies/` — JavaScript lifecycle hooks loaded by `src/engine`.
- `dashboard/` — React/Vite UI for the web dashboard.
- `docs/generated/module-inventory.md` — generated Rust module inventory.
- `docs/generated/route-inventory.md` — generated HTTP/WebSocket route inventory.
- `docs/generated/worker-inventory.md` — generated supervised worker inventory.

Worktree builds expect `sccache` on `PATH` via `.cargo/config.toml`; install it with `brew install sccache`, and override the documented `SCCACHE_CACHE_SIZE=10G` default only when a host needs a different local cache cap.

## Generated `src/` Tree

This block is generated from the filesystem and is checked in CI for drift.

<!-- BEGIN GENERATED: SRC TREE -->
```text
src/
├── cli/
│   ├── client/
│   │   └── runtime_config.rs
│   ├── doctor/
│   │   ├── contract.rs
│   │   ├── health.rs
│   │   ├── mailbox.rs
│   │   ├── orchestrator.rs
│   │   └── startup.rs
│   ├── migrate/
│   │   ├── apply.rs
│   │   ├── plan.rs
│   │   └── source.rs
│   ├── provider_cli/
│   │   └── mod.rs
│   ├── args.rs
│   ├── client.rs
│   ├── dcserver.rs
│   ├── dcserver_pg_bootstrap.rs
│   ├── direct.rs
│   ├── discord.rs
│   ├── discord_thread_create.rs
│   ├── discord_thread_create_lock.rs
│   ├── doctor.rs
│   ├── init.rs
│   ├── json_output.rs
│   ├── migrate.rs
│   ├── mod.rs
│   ├── monitoring.rs
│   ├── query.rs
│   ├── run.rs
│   └── utils.rs
├── compat/
│   ├── legacy_db_paths.rs
│   ├── legacy_tmp_paths.rs
│   └── mod.rs
├── db/
│   ├── auto_queue/
│   │   ├── entries/
│   │   │   └── dispatch_failure.rs
│   │   ├── claim.rs
│   │   ├── consultation.rs
│   │   ├── entries.rs
│   │   ├── mod.rs
│   │   ├── phase_gates.rs
│   │   ├── queries.rs
│   │   ├── runs.rs
│   │   ├── slot_predicate.rs
│   │   ├── slots.rs
│   │   ├── test_support.rs
│   │   └── tests.rs
│   ├── automation_candidates/
│   │   └── verdict_tests.rs
│   ├── dispatched_sessions/
│   │   └── rebind_override.rs
│   ├── dispatches/
│   │   ├── outbox/
│   │   │   ├── claim.rs
│   │   │   ├── delivery.rs
│   │   │   ├── diagnostics.rs
│   │   │   ├── followup.rs
│   │   │   ├── mod.rs
│   │   │   ├── model.rs
│   │   │   ├── notify.rs
│   │   │   └── retry.rs
│   │   ├── delivery_events.rs
│   │   ├── metadata.rs
│   │   └── mod.rs
│   ├── kanban_cards/
│   │   ├── crud.rs
│   │   ├── listing.rs
│   │   ├── metadata.rs
│   │   ├── mod.rs
│   │   └── transitions.rs
│   ├── prompt_manifests/
│   │   ├── builder.rs
│   │   ├── mod.rs
│   │   ├── model.rs
│   │   ├── redaction.rs
│   │   ├── repository.rs
│   │   ├── retention.rs
│   │   ├── storage_stats.rs
│   │   └── tests.rs
│   ├── scheduled_messages/
│   │   ├── agent.rs
│   │   ├── outbox.rs
│   │   └── postgres_tests.rs
│   ├── agents.rs
│   ├── automation_candidates.rs
│   ├── cancel_tombstones.rs
│   ├── dispatch_semaphores.rs
│   ├── dispatched_sessions.rs
│   ├── idempotency.rs
│   ├── intake_outbox.rs
│   ├── kanban.rs
│   ├── meetings.rs
│   ├── mod.rs
│   ├── postgres.rs
│   ├── relay_dead_letter.rs
│   ├── scheduled_messages.rs
│   ├── session_agent_resolution.rs
│   ├── session_observability.rs
│   ├── session_status.rs
│   ├── session_transcripts.rs
│   ├── table_metadata.rs
│   └── turns.rs
├── dispatch/
│   ├── dispatch_cancel.rs
│   ├── dispatch_channel.rs
│   ├── dispatch_context.rs
│   ├── dispatch_create.rs
│   ├── dispatch_query.rs
│   ├── dispatch_status.rs
│   ├── dispatch_summary.rs
│   ├── mod.rs
│   ├── test_support.rs
│   └── types.rs
├── engine/
│   ├── ops/
│   │   ├── agent_ops.rs
│   │   ├── auto_queue_ops.rs
│   │   ├── cards_ops.rs
│   │   ├── ci_recovery_ops.rs
│   │   ├── config_ops.rs
│   │   ├── db_ops.rs
│   │   ├── dispatch_ops.rs
│   │   ├── dm_reply_ops.rs
│   │   ├── exec_ops.rs
│   │   ├── http_ops.rs
│   │   ├── kanban_ops.rs
│   │   ├── kv_ops.rs
│   │   ├── log_ops.rs
│   │   ├── message_ops.rs
│   │   ├── pipeline_ops.rs
│   │   ├── quality_ops.rs
│   │   ├── queue_ops.rs
│   │   ├── review_automation_ops.rs
│   │   ├── review_ops.rs
│   │   ├── runtime_ops.rs
│   │   ├── timeouts_ops.rs
│   │   └── turn_ops.rs
│   ├── hooks.rs
│   ├── intent.rs
│   ├── loader.rs
│   ├── mod.rs
│   ├── ops.rs
│   ├── slow_hook_warn.rs
│   ├── sql_guard.rs
│   ├── transition.rs
│   ├── transition_executor_pg.rs
│   └── transition_timeout.rs
├── github/
│   ├── mod.rs
│   ├── sync.rs
│   └── triage.rs
├── kanban/
│   ├── audit.rs
│   ├── github_sync.rs
│   ├── github_sync_target.rs
│   ├── hooks.rs
│   ├── mod.rs
│   ├── review_tuning.rs
│   ├── state_machine.rs
│   ├── terminal_cleanup.rs
│   ├── transition_cleanup.rs
│   └── transition_core.rs
├── runtime_layout/
│   ├── config_merge.rs
│   ├── legacy_migration.rs
│   ├── mod.rs
│   ├── paths.rs
│   ├── skill_refresh.rs
│   └── skill_sync.rs
├── server/
│   ├── dto/
│   │   ├── agents.rs
│   │   ├── analytics.rs
│   │   ├── dispatches.rs
│   │   ├── kanban.rs
│   │   ├── mod.rs
│   │   └── settings.rs
│   ├── routes/
│   │   ├── dispatches/
│   │   │   ├── crud.rs
│   │   │   ├── discord_delivery.rs
│   │   │   ├── mod.rs
│   │   │   ├── outbox.rs
│   │   │   └── thread_reuse.rs
│   │   ├── docs/
│   │   │   ├── inventory/
│   │   │   │   └── endpoints/
│   │   │   │       ├── mod.rs
│   │   │   │       ├── part_01.rs
│   │   │   │       ├── part_02.rs
│   │   │   │       ├── part_03.rs
│   │   │   │       ├── part_04.rs
│   │   │   │       ├── part_05.rs
│   │   │   │       ├── part_06.rs
│   │   │   │       ├── part_07.rs
│   │   │   │       ├── part_08.rs
│   │   │   │       ├── part_09.rs
│   │   │   │       └── part_10.rs
│   │   │   ├── guides.rs
│   │   │   ├── inventory.rs
│   │   │   └── taxonomy.rs
│   │   ├── domains/
│   │   │   ├── access.rs
│   │   │   ├── admin.rs
│   │   │   ├── agents.rs
│   │   │   ├── analytics.rs
│   │   │   ├── integrations.rs
│   │   │   ├── kanban.rs
│   │   │   ├── mod.rs
│   │   │   ├── onboarding.rs
│   │   │   ├── ops.rs
│   │   │   └── reviews.rs
│   │   ├── review_verdict/
│   │   │   ├── decision_route.rs
│   │   │   ├── mod.rs
│   │   │   ├── tuning_aggregate.rs
│   │   │   └── verdict_route.rs
│   │   ├── routines/
│   │   │   ├── audit.rs
│   │   │   ├── handlers.rs
│   │   │   ├── helpers.rs
│   │   │   └── responses.rs
│   │   ├── scheduled_messages/
│   │   │   └── postgres_tests.rs
│   │   ├── tests/
│   │   │   ├── preflight_harness/
│   │   │   │   ├── types.rs
│   │   │   │   └── validation.rs
│   │   │   └── auto_queue_preflight_harness_tests.rs
│   │   ├── agents.rs
│   │   ├── agents_crud.rs
│   │   ├── agents_setup.rs
│   │   ├── analytics.rs
│   │   ├── auth.rs
│   │   ├── auto_queue.rs
│   │   ├── automation_candidates.rs
│   │   ├── claude_accounts_api.rs
│   │   ├── cluster.rs
│   │   ├── cron_api.rs
│   │   ├── departments.rs
│   │   ├── discord.rs
│   │   ├── dispatched_sessions.rs
│   │   ├── dm_reply.rs
│   │   ├── docs.rs
│   │   ├── escalation.rs
│   │   ├── github.rs
│   │   ├── github_dashboard.rs
│   │   ├── health_api.rs
│   │   ├── home_metrics.rs
│   │   ├── hooks.rs
│   │   ├── idle_recap.rs
│   │   ├── kanban.rs
│   │   ├── kanban_repos.rs
│   │   ├── maintenance.rs
│   │   ├── meetings.rs
│   │   ├── memory_api.rs
│   │   ├── message_outbox.rs
│   │   ├── messages.rs
│   │   ├── mod.rs
│   │   ├── monitoring.rs
│   │   ├── offices.rs
│   │   ├── onboarding.rs
│   │   ├── pipeline.rs
│   │   ├── pr_summary.rs
│   │   ├── prompt_manifest_retention.rs
│   │   ├── provider_cli_api.rs
│   │   ├── queue_api.rs
│   │   ├── receipt.rs
│   │   ├── resume.rs
│   │   ├── reviews.rs
│   │   ├── routines.rs
│   │   ├── scheduled_messages.rs
│   │   ├── session_activity.rs
│   │   ├── settings.rs
│   │   ├── skill_usage_analytics.rs
│   │   ├── skills_api.rs
│   │   ├── stats.rs
│   │   ├── termination_events.rs
│   │   ├── v1.rs
│   │   └── voice_config.rs
│   ├── cluster.rs
│   ├── cluster_session_routing.rs
│   ├── cron_catalog.rs
│   ├── issue_specs.rs
│   ├── maintenance.rs
│   ├── mod.rs
│   ├── multinode_regression.rs
│   ├── outbox_actionable_delivery.rs
│   ├── outbox_delivery_alert.rs
│   ├── resource_locks.rs
│   ├── state.rs
│   ├── task_dispatch_claims.rs
│   ├── test_phase_runs.rs
│   ├── worker_registry.rs
│   └── ws.rs
├── services/
│   ├── agent_quality/
│   │   ├── mod.rs
│   │   └── regression_alerts.rs
│   ├── agents/
│   │   ├── mod.rs
│   │   ├── query.rs
│   │   ├── serialization.rs
│   │   └── turn.rs
│   ├── analytics/
│   │   ├── api_usage.rs
│   │   ├── dispatch_metrics.rs
│   │   ├── dto.rs
│   │   ├── queue_metrics.rs
│   │   └── session_metrics.rs
│   ├── api_friction/
│   │   ├── core.rs
│   │   ├── issue_body.rs
│   │   ├── issues.rs
│   │   ├── markers.rs
│   │   ├── memory_sync.rs
│   │   ├── mod.rs
│   │   ├── patterns.rs
│   │   └── storage.rs
│   ├── auto_queue/
│   │   ├── activate_command.rs
│   │   ├── activate_preflight.rs
│   │   ├── activate_route.rs
│   │   ├── cancel_run.rs
│   │   ├── command.rs
│   │   ├── control_routes.rs
│   │   ├── dispatch_assignment_command.rs
│   │   ├── dispatch_command.rs
│   │   ├── dispatch_query.rs
│   │   ├── fsm.rs
│   │   ├── order_routes.rs
│   │   ├── phase_gate.rs
│   │   ├── phase_gate_catalog.rs
│   │   ├── phase_gate_violations.rs
│   │   ├── planning.rs
│   │   ├── query.rs
│   │   ├── route.rs
│   │   ├── route_generate.rs
│   │   ├── route_request_generate.rs
│   │   ├── route_types.rs
│   │   ├── runtime.rs
│   │   ├── slot_routes.rs
│   │   ├── view.rs
│   │   └── view_admin_routes.rs
│   ├── automation_candidate_materializer/
│   │   ├── allowed_path_tests.rs
│   │   └── iteration_result_tests.rs
│   ├── claude/
│   │   └── backend_routing.rs
│   ├── claude_e/
│   │   ├── cancellation.rs
│   │   ├── jsonl_parser.rs
│   │   ├── mod.rs
│   │   ├── process.rs
│   │   └── spawn_queue.rs
│   ├── claude_tui/
│   │   ├── hook_relay/
│   │   │   └── ordered_queue.rs
│   │   ├── hook_server/
│   │   │   └── relay_receipts.rs
│   │   ├── hosting/
│   │   │   ├── followup_support.rs
│   │   │   ├── mod.rs
│   │   │   └── warm_followup.rs
│   │   ├── composer_lock.rs
│   │   ├── hook_bundle.rs
│   │   ├── hook_output_guard.rs
│   │   ├── hook_output_guard_tests.rs
│   │   ├── hook_registry.rs
│   │   ├── hook_relay.rs
│   │   ├── hook_server.rs
│   │   ├── hook_server_memento_tests.rs
│   │   ├── input.rs
│   │   ├── memento_feedback.rs
│   │   ├── mod.rs
│   │   ├── session.rs
│   │   ├── startup_dialog.rs
│   │   ├── transcript_tail.rs
│   │   └── tui_relay.rs
│   ├── cluster/
│   │   ├── intake_router_hook/
│   │   │   ├── owner_record.rs
│   │   │   └── session_owner.rs
│   │   ├── stream_relay/
│   │   │   └── identity.rs
│   │   ├── capability_routing.rs
│   │   ├── intake_router_hook.rs
│   │   ├── intake_routing.rs
│   │   ├── intake_worker.rs
│   │   ├── intake_worker_capabilities.rs
│   │   ├── mod.rs
│   │   ├── node_registry.rs
│   │   ├── registry_adapter_sink.rs
│   │   ├── relay_producer_registry.rs
│   │   ├── session_discovery.rs
│   │   ├── session_matcher.rs
│   │   ├── session_registry.rs
│   │   ├── session_routing.rs
│   │   ├── stream_relay.rs
│   │   └── watcher_supervisor.rs
│   ├── codex_tui/
│   │   ├── rollout_tail/
│   │   │   └── parser.rs
│   │   ├── input.rs
│   │   ├── mod.rs
│   │   ├── rollout_index.rs
│   │   ├── rollout_tail.rs
│   │   ├── session.rs
│   │   └── warm_followup.rs
│   ├── discord/
│   │   ├── catch_up/
│   │   │   ├── classification.rs
│   │   │   ├── classification_order_tests.rs
│   │   │   ├── phase2.rs
│   │   │   └── too_old_notice.rs
│   │   ├── commands/
│   │   │   ├── diagnostics/
│   │   │   │   ├── mod.rs
│   │   │   │   └── reports.rs
│   │   │   ├── inspect/
│   │   │   │   ├── formatting.rs
│   │   │   │   ├── mod.rs
│   │   │   │   ├── model.rs
│   │   │   │   ├── query.rs
│   │   │   │   ├── render_context.rs
│   │   │   │   ├── render_last.rs
│   │   │   │   ├── render_prompt.rs
│   │   │   │   ├── render_recovery.rs
│   │   │   │   ├── render_session.rs
│   │   │   │   └── tests.rs
│   │   │   ├── voice/
│   │   │   │   └── alert.rs
│   │   │   ├── command_policy.rs
│   │   │   ├── config.rs
│   │   │   ├── control.rs
│   │   │   ├── fast_mode.rs
│   │   │   ├── goals.rs
│   │   │   ├── help.rs
│   │   │   ├── meeting_cmd.rs
│   │   │   ├── mod.rs
│   │   │   ├── model_picker.rs
│   │   │   ├── model_ui.rs
│   │   │   ├── node.rs
│   │   │   ├── receipt.rs
│   │   │   ├── recovery_ops.rs
│   │   │   ├── restart.rs
│   │   │   ├── session.rs
│   │   │   ├── sidecar.rs
│   │   │   ├── skill.rs
│   │   │   ├── steer.rs
│   │   │   ├── text_commands.rs
│   │   │   ├── tui_passthrough.rs
│   │   │   └── voice.rs
│   │   ├── footer_view_reconciler/
│   │   │   ├── mod.rs
│   │   │   └── registry.rs
│   │   ├── formatting/
│   │   │   ├── long_send_rollback.rs
│   │   │   └── rollback_journal.rs
│   │   ├── gateway/
│   │   │   └── outbound_messages.rs
│   │   ├── health/
│   │   │   ├── recovery/
│   │   │   │   ├── leak_recovery_ledger.rs
│   │   │   │   ├── stall_alert.rs
│   │   │   │   └── watchdog_decisions.rs
│   │   │   ├── headless_turn.rs
│   │   │   ├── mailbox.rs
│   │   │   ├── provider_probe.rs
│   │   │   ├── rebind_request.rs
│   │   │   ├── recovery.rs
│   │   │   ├── redaction.rs
│   │   │   ├── relay_auto_heal.rs
│   │   │   ├── relay_dead_reattach.rs
│   │   │   ├── relay_progress.rs
│   │   │   ├── runtime_resolve.rs
│   │   │   ├── session_enrichment.rs
│   │   │   ├── snapshot.rs
│   │   │   ├── stall_liveness.rs
│   │   │   ├── stall_verdict.rs
│   │   │   └── watcher_respawn.rs
│   │   ├── idle_recap/
│   │   │   ├── context_display.rs
│   │   │   ├── relay_integrity.rs
│   │   │   └── scrollback.rs
│   │   ├── inflight/
│   │   │   ├── clear_store/
│   │   │   │   ├── abandon.rs
│   │   │   │   └── mod.rs
│   │   │   ├── save_store/
│   │   │   │   ├── identity_gate.rs
│   │   │   │   └── rebind_adoption.rs
│   │   │   ├── stall_recovery_tests/
│   │   │   │   ├── flake_isolation_4361.rs
│   │   │   │   └── flake_isolation_4422.rs
│   │   │   ├── anchor_repost.rs
│   │   │   ├── budget.rs
│   │   │   ├── episode_guard.rs
│   │   │   ├── finalizer_identity.rs
│   │   │   ├── invariant_test_capture.rs
│   │   │   ├── model.rs
│   │   │   ├── orphan_relay_reclaim.rs
│   │   │   ├── ownership_ops.rs
│   │   │   ├── rebind_reap.rs
│   │   │   ├── removal.rs
│   │   │   ├── save_store.rs
│   │   │   ├── store.rs
│   │   │   └── watcher_state.rs
│   │   ├── outbound/
│   │   │   ├── confirmation.rs
│   │   │   ├── decision.rs
│   │   │   ├── delivery.rs
│   │   │   ├── delivery_frontier_probe.rs
│   │   │   ├── delivery_record.rs
│   │   │   ├── manual_delivery.rs
│   │   │   ├── message.rs
│   │   │   ├── mod.rs
│   │   │   ├── policy.rs
│   │   │   ├── reaction_control.rs
│   │   │   ├── result.rs
│   │   │   ├── send_api.rs
│   │   │   ├── send_gate.rs
│   │   │   ├── send_target.rs
│   │   │   ├── send_to_agent.rs
│   │   │   ├── serenity_reference.rs
│   │   │   ├── source_registry.rs
│   │   │   ├── transport.rs
│   │   │   └── turn_output_controller.rs
│   │   ├── placeholder_live_events/
│   │   │   ├── background_task_events.rs
│   │   │   ├── common.rs
│   │   │   ├── completion_footer.rs
│   │   │   ├── context_panel.rs
│   │   │   ├── freshness.rs
│   │   │   ├── mod.rs
│   │   │   ├── recent_events.rs
│   │   │   ├── session_banner_claim.rs
│   │   │   ├── session_panel.rs
│   │   │   ├── slot_rehydration.rs
│   │   │   ├── status_events.rs
│   │   │   ├── status_panel.rs
│   │   │   ├── subagent_panel.rs
│   │   │   ├── subagent_rollout.rs
│   │   │   ├── subagent_summary.rs
│   │   │   ├── task_panel.rs
│   │   │   ├── tests.rs
│   │   │   ├── turn_anchor.rs
│   │   │   └── workflow_panel.rs
│   │   ├── placeholder_sweeper/
│   │   │   └── abandon_guard.rs
│   │   ├── prompt_builder/
│   │   │   ├── channel_recent_context.rs
│   │   │   ├── dispatch_contract.rs
│   │   │   ├── dispatch_contract_tests.rs
│   │   │   ├── layer_rendering.rs
│   │   │   ├── manifest.rs
│   │   │   ├── memory_guidance.rs
│   │   │   ├── mod.rs
│   │   │   └── section_dedupe.rs
│   │   ├── recovery_engine/
│   │   │   ├── manual_rebind/
│   │   │   │   ├── adoption.rs
│   │   │   │   ├── codex_tui_replay.rs
│   │   │   │   ├── episode_handoff.rs
│   │   │   │   ├── mod.rs
│   │   │   │   ├── post_adoption_guard_tests.rs
│   │   │   │   └── watcher_claim.rs
│   │   │   ├── rebind_runtime/
│   │   │   │   └── codex_relay_generation.rs
│   │   │   ├── analytics_transcript.rs
│   │   │   ├── completion_delivery.rs
│   │   │   ├── crash_resume_guard.rs
│   │   │   ├── jsonl_extract.rs
│   │   │   ├── manual_rebind_output_path.rs
│   │   │   ├── manual_rebind_override.rs
│   │   │   ├── output_path_detect.rs
│   │   │   ├── phase_policy.rs
│   │   │   ├── rebind_runtime.rs
│   │   │   ├── restore_inflight.rs
│   │   │   ├── restore_persist_outcome.rs
│   │   │   ├── routing_orphan.rs
│   │   │   ├── runtime.rs
│   │   │   ├── state_extractors.rs
│   │   │   ├── status_panel.rs
│   │   │   ├── status_panel_completion_producer.rs
│   │   │   ├── terminal_text_idempotency.rs
│   │   │   └── terminal_watcher.rs
│   │   ├── recovery_paths/
│   │   │   ├── controller_cutover.rs
│   │   │   ├── mod.rs
│   │   │   ├── restart.rs
│   │   │   └── shared.rs
│   │   ├── relay_recovery/
│   │   │   └── tests/
│   │   │       └── circuit_breaker_apply.rs
│   │   ├── router/
│   │   │   ├── intake_dispatch/
│   │   │   │   ├── notice.rs
│   │   │   │   ├── queued.rs
│   │   │   │   ├── skill.rs
│   │   │   │   └── tests.rs
│   │   │   ├── intake_gate/
│   │   │   │   ├── busy_duplicate_notice.rs
│   │   │   │   ├── component_events.rs
│   │   │   │   ├── gate.rs
│   │   │   │   ├── queue_effects.rs
│   │   │   │   └── stale_turn.rs
│   │   │   ├── message_handler/
│   │   │   │   ├── intake_turn/
│   │   │   │   │   ├── race_loss/
│   │   │   │   │   │   ├── mailbox_reaction.rs
│   │   │   │   │   │   └── mailbox_reaction_tests.rs
│   │   │   │   │   ├── race_loss.rs
│   │   │   │   │   ├── turn_watchdog.rs
│   │   │   │   │   └── voice_intake.rs
│   │   │   │   ├── attachments.rs
│   │   │   │   ├── control.rs
│   │   │   │   ├── goal_lifecycle.rs
│   │   │   │   ├── headless_turn.rs
│   │   │   │   ├── intake_turn.rs
│   │   │   │   ├── latency_spans.rs
│   │   │   │   ├── provider_isolation.rs
│   │   │   │   ├── session_strategy_lifecycle_tests.rs
│   │   │   │   ├── tui_followup.rs
│   │   │   │   ├── turn_lifecycle.rs
│   │   │   │   ├── voice_announcement_route.rs
│   │   │   │   ├── voice_announcement_scope.rs
│   │   │   │   └── watchdog.rs
│   │   │   ├── authorization.rs
│   │   │   ├── dispatch_trigger.rs
│   │   │   ├── intake_dispatch.rs
│   │   │   ├── intake_gate.rs
│   │   │   ├── intake_queue_transaction.rs
│   │   │   ├── message_handler.rs
│   │   │   ├── mod.rs
│   │   │   ├── queue_status_presentation.rs
│   │   │   ├── response_format.rs
│   │   │   ├── thread_binding.rs
│   │   │   └── turn_start.rs
│   │   ├── runtime_bootstrap/
│   │   │   ├── framework_setup.rs
│   │   │   ├── gateway_lease.rs
│   │   │   ├── gateway_lease_tests.rs
│   │   │   ├── gateway_runtime.rs
│   │   │   ├── intake.rs
│   │   │   ├── orphan_recovery.rs
│   │   │   ├── queued_placeholders.rs
│   │   │   ├── recovery_flush.rs
│   │   │   ├── restored_state.rs
│   │   │   ├── session_gc.rs
│   │   │   ├── shared_data.rs
│   │   │   ├── shutdown.rs
│   │   │   ├── spawns.rs
│   │   │   ├── startup_doctor.rs
│   │   │   └── voice.rs
│   │   ├── session_relay_sink/
│   │   │   ├── idle_jsonl.rs
│   │   │   ├── orphan_reclaim.rs
│   │   │   ├── relay_format.rs
│   │   │   └── task_notification_context.rs
│   │   ├── session_runtime/
│   │   │   ├── channel_routing.rs
│   │   │   ├── restore_cwd.rs
│   │   │   └── worktree.rs
│   │   ├── settings/
│   │   │   ├── content.rs
│   │   │   ├── memory.rs
│   │   │   ├── read.rs
│   │   │   ├── validation.rs
│   │   │   └── write.rs
│   │   ├── task_notification_delivery/
│   │   │   ├── store/
│   │   │   │   ├── card_claim.rs
│   │   │   │   ├── missing_card_replacement.rs
│   │   │   │   ├── response_chunks.rs
│   │   │   │   ├── response_fence.rs
│   │   │   │   ├── response_identity.rs
│   │   │   │   ├── retention.rs
│   │   │   │   └── terminal_footer.rs
│   │   │   ├── card_post.rs
│   │   │   ├── gateway.rs
│   │   │   ├── mod.rs
│   │   │   ├── response_chunks.rs
│   │   │   ├── store.rs
│   │   │   ├── terminal_identity.rs
│   │   │   └── tests.rs
│   │   ├── tmux/
│   │   │   └── task_notification_kind_restart_roundtrip_tests.rs
│   │   ├── tmux_output_stream/
│   │   │   └── provider_output_guard_tests.rs
│   │   ├── tmux_placeholder_suppression/
│   │   │   ├── evidence.rs
│   │   │   ├── mod.rs
│   │   │   └── ops.rs
│   │   ├── tmux_watcher/
│   │   │   ├── streaming_status_tick/
│   │   │   │   └── types.rs
│   │   │   ├── commit_decisions.rs
│   │   │   ├── completion_gate.rs
│   │   │   ├── completion_gate_tests.rs
│   │   │   ├── completion_producer.rs
│   │   │   ├── controller_heartbeat.rs
│   │   │   ├── entry.rs
│   │   │   ├── jsonl_rotation.rs
│   │   │   ├── liveness.rs
│   │   │   ├── loop_poll_prologue.rs
│   │   │   ├── no_result_exits.rs
│   │   │   ├── orphan_status_panel_cleanup.rs
│   │   │   ├── panel_decisions.rs
│   │   │   ├── panel_decisions_tests.rs
│   │   │   ├── placeholder_reclaim.rs
│   │   │   ├── post_stream_exit.rs
│   │   │   ├── prompt_observe.rs
│   │   │   ├── provider_output_guard.rs
│   │   │   ├── provider_session_persistence.rs
│   │   │   ├── session_bound_ack.rs
│   │   │   ├── session_bound_ack_tests.rs
│   │   │   ├── single_message_footer.rs
│   │   │   ├── single_message_footer_tests.rs
│   │   │   ├── stall_exit.rs
│   │   │   ├── streaming_session_banner.rs
│   │   │   ├── streaming_status_tick.rs
│   │   │   ├── supervisor_relay.rs
│   │   │   ├── supervisor_relay_tests.rs
│   │   │   ├── task_response_authority.rs
│   │   │   ├── terminal_abort_exits.rs
│   │   │   ├── terminal_commit_epilogue.rs
│   │   │   ├── terminal_direct_fallback.rs
│   │   │   ├── terminal_direct_fallback_tests.rs
│   │   │   ├── terminal_long_chunks.rs
│   │   │   ├── terminal_readiness.rs
│   │   │   ├── terminal_readiness_tests.rs
│   │   │   ├── terminal_send.rs
│   │   │   ├── tests.rs
│   │   │   ├── turn_identity.rs
│   │   │   ├── turn_identity_tests.rs
│   │   │   ├── turn_stream_collector.rs
│   │   │   ├── two_message_panel.rs
│   │   │   ├── utf8_chunk_decoder.rs
│   │   │   └── utf8_chunk_decoder_tests.rs
│   │   ├── tui_direct_abort_marker/
│   │   │   ├── deferred_claim.rs
│   │   │   ├── drain.rs
│   │   │   ├── mod.rs
│   │   │   ├── store.rs
│   │   │   ├── sweep.rs
│   │   │   └── tombstone.rs
│   │   ├── tui_prompt_relay/
│   │   │   ├── synthetic_start/
│   │   │   │   └── stale_reclaim.rs
│   │   │   ├── anchor_completion.rs
│   │   │   ├── bridge_completion.rs
│   │   │   ├── bridge_gateway.rs
│   │   │   ├── claude_idle_bridge.rs
│   │   │   ├── claude_idle_runtime.rs
│   │   │   ├── claude_idle_tail.rs
│   │   │   ├── codex_idle_rollout.rs
│   │   │   ├── idle_offset_resolution.rs
│   │   │   ├── idle_transcript_scan.rs
│   │   │   ├── injected_prompt_policy.rs
│   │   │   ├── launch_script.rs
│   │   │   ├── observed_prompt_decision.rs
│   │   │   ├── rehydration.rs
│   │   │   ├── relay_ownership.rs
│   │   │   ├── synthetic_orphan_reclaim.rs
│   │   │   ├── synthetic_start.rs
│   │   │   ├── synthetic_start_wiring.rs
│   │   │   ├── task_notification_prompt.rs
│   │   │   └── tests.rs
│   │   ├── turn_bridge/
│   │   │   ├── completion_guard/
│   │   │   │   ├── completion_context.rs
│   │   │   │   └── completion_postgres.rs
│   │   │   ├── runtime_handoff_loop/
│   │   │   │   └── guarded_save.rs
│   │   │   ├── stream_loop/
│   │   │   │   ├── content_arms/
│   │   │   │   │   ├── provider_error_presentation.rs
│   │   │   │   │   └── tui_error_classification.rs
│   │   │   │   ├── content_arms.rs
│   │   │   │   └── tool_arms.rs
│   │   │   ├── terminal_outcome_delivery/
│   │   │   │   ├── empty_response_recovery/
│   │   │   │   │   ├── guidance.rs
│   │   │   │   │   └── handler.rs
│   │   │   │   ├── busy_followup_retry.rs
│   │   │   │   ├── cancel_prompt_replace.rs
│   │   │   │   ├── delivery_epilogue.rs
│   │   │   │   ├── empty_response_recovery.rs
│   │   │   │   ├── prompt_too_long_guidance.rs
│   │   │   │   ├── queue_retry_silence.rs
│   │   │   │   └── recovery_retry.rs
│   │   │   ├── tmux_runtime/
│   │   │   │   ├── claude_stop_delivery.rs
│   │   │   │   ├── interrupt_policy.rs
│   │   │   │   ├── pid_exit.rs
│   │   │   │   ├── process_backend_cancel.rs
│   │   │   │   └── process_table.rs
│   │   │   ├── activity_heartbeat.rs
│   │   │   ├── bridge_latency_spans.rs
│   │   │   ├── cancel_finalize_policy.rs
│   │   │   ├── chunk_compose.rs
│   │   │   ├── chunk_compose_tests.rs
│   │   │   ├── completion_guard.rs
│   │   │   ├── completion_postlude.rs
│   │   │   ├── context_window.rs
│   │   │   ├── early_tui_completion.rs
│   │   │   ├── finalize_epilogue.rs
│   │   │   ├── followup_requeue.rs
│   │   │   ├── guards.rs
│   │   │   ├── headless_delivery.rs
│   │   │   ├── memory_lifecycle.rs
│   │   │   ├── mod.rs
│   │   │   ├── output_lifecycle.rs
│   │   │   ├── panel_lifecycle.rs
│   │   │   ├── post_loop_finalize.rs
│   │   │   ├── recall_feedback.rs
│   │   │   ├── recovery_text.rs
│   │   │   ├── response_delivery.rs
│   │   │   ├── retry_state.rs
│   │   │   ├── runtime_handoff_loop.rs
│   │   │   ├── single_message_footer.rs
│   │   │   ├── skill_usage.rs
│   │   │   ├── stale_resume.rs
│   │   │   ├── status_panel.rs
│   │   │   ├── status_panel_tests.rs
│   │   │   ├── stream_loop.rs
│   │   │   ├── stream_receiver.rs
│   │   │   ├── stream_tick.rs
│   │   │   ├── streaming_edit_text.rs
│   │   │   ├── task_notification_lifecycle.rs
│   │   │   ├── terminal_controller_cutover.rs
│   │   │   ├── terminal_delivery.rs
│   │   │   ├── terminal_outcome_delivery.rs
│   │   │   ├── thinking.rs
│   │   │   ├── tmux_runtime.rs
│   │   │   ├── turn_analytics.rs
│   │   │   ├── two_message_panel.rs
│   │   │   ├── voice_completion.rs
│   │   │   ├── voice_completion_tests.rs
│   │   │   ├── watcher_handoff.rs
│   │   │   └── watcher_orphan_cleanup.rs
│   │   ├── turn_finalizer/
│   │   │   ├── cleanup.rs
│   │   │   ├── completion_signal.rs
│   │   │   ├── delivery_lease.rs
│   │   │   ├── finalize.rs
│   │   │   ├── finalize_context.rs
│   │   │   ├── reconcile.rs
│   │   │   └── watcher_backstop.rs
│   │   ├── turn_view_reconciler/
│   │   │   ├── orphan_sweep.rs
│   │   │   ├── queue_repair.rs
│   │   │   ├── reaction_set.rs
│   │   │   └── tests.rs
│   │   ├── voice_barge_in/
│   │   │   ├── tests/
│   │   │   │   └── pcm_harness_tests.rs
│   │   │   ├── channel_state.rs
│   │   │   ├── final_result_playback.rs
│   │   │   ├── foreground_decision.rs
│   │   │   ├── live_cut_playback.rs
│   │   │   ├── progress_playback.rs
│   │   │   ├── receive_hook.rs
│   │   │   ├── routing.rs
│   │   │   ├── stt.rs
│   │   │   └── tts_pipeline.rs
│   │   ├── watchers/
│   │   │   ├── lifecycle/
│   │   │   │   └── activity.rs
│   │   │   ├── codex_tui_restore.rs
│   │   │   ├── lifecycle.rs
│   │   │   └── lifecycle_decision.rs
│   │   ├── abandon_request_store.rs
│   │   ├── adk_session.rs
│   │   ├── agent_handoff.rs
│   │   ├── agentdesk_config.rs
│   │   ├── answer_flush_barrier.rs
│   │   ├── bot_role.rs
│   │   ├── catch_up.rs
│   │   ├── delivery_lease_key.rs
│   │   ├── destructive_cancel_capture.rs
│   │   ├── destructive_cancel_gate.rs
│   │   ├── discord_io.rs
│   │   ├── dispatch_policy.rs
│   │   ├── formatting.rs
│   │   ├── gateway.rs
│   │   ├── gateway_voice_queue.rs
│   │   ├── health.rs
│   │   ├── http.rs
│   │   ├── idle_detector.rs
│   │   ├── idle_recap.rs
│   │   ├── idle_recap_interaction.rs
│   │   ├── idle_relay_drift.rs
│   │   ├── inflight.rs
│   │   ├── inflight_heartbeat_sweeper.rs
│   │   ├── internal_api.rs
│   │   ├── jsonl_watcher.rs
│   │   ├── mailbox_finish.rs
│   │   ├── mcp_credential_watcher.rs
│   │   ├── meeting_artifact_store.rs
│   │   ├── meeting_orchestrator.rs
│   │   ├── meeting_state_machine.rs
│   │   ├── metrics.rs
│   │   ├── mod.rs
│   │   ├── model_catalog.rs
│   │   ├── model_picker_interaction.rs
│   │   ├── monitoring_status.rs
│   │   ├── org_schema.rs
│   │   ├── org_writer.rs
│   │   ├── placeholder_cleanup.rs
│   │   ├── placeholder_controller.rs
│   │   ├── placeholder_sweeper.rs
│   │   ├── queue_dispatch.rs
│   │   ├── queue_io.rs
│   │   ├── queue_marker.rs
│   │   ├── queue_overflow_dlq.rs
│   │   ├── queue_reactions.rs
│   │   ├── queued_placeholders_store.rs
│   │   ├── reaction_cleanup.rs
│   │   ├── reaction_lifecycle.rs
│   │   ├── readopted_mailbox_ledger.rs
│   │   ├── recovery_engine.rs
│   │   ├── relay_health.rs
│   │   ├── relay_owner_observability.rs
│   │   ├── relay_recovery.rs
│   │   ├── relay_recovery_auto_heal_apply.rs
│   │   ├── relay_recovery_auto_heal_attempts.rs
│   │   ├── relay_recovery_auto_heal_confirm.rs
│   │   ├── relay_recovery_circuit_breaker.rs
│   │   ├── relay_recovery_completion_footer.rs
│   │   ├── relay_recovery_reattach_apply.rs
│   │   ├── replace_outcome_policy.rs
│   │   ├── response_sanitizer.rs
│   │   ├── restart_ctrl.rs
│   │   ├── restart_mode.rs
│   │   ├── restart_report.rs
│   │   ├── role_map.rs
│   │   ├── runtime_bootstrap.rs
│   │   ├── runtime_store.rs
│   │   ├── semantic_boundaries.rs
│   │   ├── session_banner.rs
│   │   ├── session_identity.rs
│   │   ├── session_relay_sink.rs
│   │   ├── session_runtime.rs
│   │   ├── settings.rs
│   │   ├── shared_memory.rs
│   │   ├── shared_state.rs
│   │   ├── sidecar_interaction.rs
│   │   ├── single_message_panel.rs
│   │   ├── stall_recovery.rs
│   │   ├── standby_relay.rs
│   │   ├── startup_reclaim.rs
│   │   ├── status_panel_orphan_store.rs
│   │   ├── status_panel_orphan_store_tests.rs
│   │   ├── steering.rs
│   │   ├── streaming_finalizer.rs
│   │   ├── subagent_notification_card.rs
│   │   ├── task_supervisor.rs
│   │   ├── terminal_ui_obligation.rs
│   │   ├── tmux.rs
│   │   ├── tmux_error_detect.rs
│   │   ├── tmux_kill_policy.rs
│   │   ├── tmux_lifecycle.rs
│   │   ├── tmux_output_stream.rs
│   │   ├── tmux_overload_retry.rs
│   │   ├── tmux_reaper.rs
│   │   ├── tmux_reattach_offsets.rs
│   │   ├── tmux_restart_handoff.rs
│   │   ├── tmux_session_files.rs
│   │   ├── tmux_watcher.rs
│   │   ├── tui_direct_pending_start.rs
│   │   ├── tui_prompt_relay.rs
│   │   ├── tui_task_card.rs
│   │   ├── turn_completion_events.rs
│   │   ├── turn_end_wip_warning.rs
│   │   ├── turn_finalizer.rs
│   │   ├── turn_view_reconciler.rs
│   │   ├── voice_acknowledgement.rs
│   │   ├── voice_background_driver.rs
│   │   ├── voice_barge_in.rs
│   │   ├── voice_config_cache.rs
│   │   ├── voice_id_sequences.rs
│   │   ├── voice_lifecycle.rs
│   │   ├── voice_routing.rs
│   │   └── voice_sensitivity.rs
│   ├── dispatches/
│   │   ├── discord_delivery/
│   │   │   ├── guard.rs
│   │   │   ├── mod.rs
│   │   │   ├── orchestration.rs
│   │   │   ├── thread_reuse.rs
│   │   │   └── transport.rs
│   │   ├── dtos.rs
│   │   ├── mod.rs
│   │   ├── outbox_claiming.rs
│   │   ├── outbox_queue.rs
│   │   ├── outbox_route.rs
│   │   ├── result_header.rs
│   │   ├── routing_constraint.rs
│   │   ├── thread_reuse.rs
│   │   └── wait_queue.rs
│   ├── git/
│   │   ├── branch_resolver.rs
│   │   ├── commit_resolver.rs
│   │   ├── mod.rs
│   │   ├── remote.rs
│   │   ├── repo_resolver.rs
│   │   ├── runner.rs
│   │   └── worktree_resolver.rs
│   ├── maintenance/
│   │   ├── jobs/
│   │   │   ├── db_retention.rs
│   │   │   ├── hang_dump_cleanup.rs
│   │   │   ├── memento_consolidation.rs
│   │   │   ├── mod.rs
│   │   │   ├── target_sweep.rs
│   │   │   ├── voice_cache_sweep.rs
│   │   │   └── worktree_orphan_sweep.rs
│   │   └── mod.rs
│   ├── memory/
│   │   ├── local.rs
│   │   ├── memento.rs
│   │   ├── memento_instructions_cache.rs
│   │   ├── memento_throttle.rs
│   │   ├── mod.rs
│   │   └── runtime_state.rs
│   ├── observability/
│   │   ├── emit.rs
│   │   ├── events.rs
│   │   ├── helpers.rs
│   │   ├── metrics.rs
│   │   ├── mod.rs
│   │   ├── pg_io.rs
│   │   ├── queries.rs
│   │   ├── recovery_audit.rs
│   │   ├── relay_signal_alert.rs
│   │   ├── retention.rs
│   │   ├── session_inventory.rs
│   │   ├── turn_lifecycle.rs
│   │   ├── watcher_latency.rs
│   │   └── worker.rs
│   ├── onboarding/
│   │   ├── channel.rs
│   │   ├── mod.rs
│   │   └── provider.rs
│   ├── platform/
│   │   ├── tmux/
│   │   │   └── availability.rs
│   │   ├── binary_resolver.rs
│   │   ├── dump_tool.rs
│   │   ├── mod.rs
│   │   ├── shell.rs
│   │   └── tmux.rs
│   ├── provider/
│   │   ├── cancel_token_claude_interrupt.rs
│   │   └── provider_conformance_invariant_tests.rs
│   ├── provider_cli/
│   │   ├── canary.rs
│   │   ├── context.rs
│   │   ├── diagnostics.rs
│   │   ├── io.rs
│   │   ├── mod.rs
│   │   ├── orchestration.rs
│   │   ├── paths.rs
│   │   ├── registry.rs
│   │   ├── retention.rs
│   │   ├── session_guard.rs
│   │   ├── smoke.rs
│   │   ├── snapshot.rs
│   │   └── upgrade.rs
│   ├── review_decision/
│   │   ├── accept.rs
│   │   ├── adapters.rs
│   │   ├── dismiss_finalize.rs
│   │   ├── dispute.rs
│   │   ├── pending.rs
│   │   ├── repo_card.rs
│   │   ├── repo_dispatch.rs
│   │   ├── review_state_repo.rs
│   │   ├── tuning_aggregate.rs
│   │   └── worktree_stale.rs
│   ├── routines/
│   │   ├── action.rs
│   │   ├── agent_executor.rs
│   │   ├── discord_log.rs
│   │   ├── fresh_session_reaper.rs
│   │   ├── loader.rs
│   │   ├── migrated.rs
│   │   ├── mod.rs
│   │   ├── runtime.rs
│   │   ├── runtime_config.rs
│   │   ├── session_control.rs
│   │   └── store.rs
│   ├── scheduled_messages/
│   │   ├── evidence.rs
│   │   ├── postgres_tests.rs
│   │   └── timing.rs
│   ├── session_backend/
│   │   ├── stream_line.rs
│   │   └── terminal_usage.rs
│   ├── settings/
│   │   └── runtime_config_put.rs
│   ├── slo/
│   │   └── mod.rs
│   ├── tui_prompt_dedupe/
│   │   └── synthetic_prompt.rs
│   ├── tui_turn_state/
│   │   └── completion_scan.rs
│   ├── turn_orchestrator/
│   │   ├── active_source_dedup.rs
│   │   ├── dispatch_reservation.rs
│   │   ├── overflow.rs
│   │   ├── pending_queue_persistence.rs
│   │   ├── registry_purge.rs
│   │   ├── source_generation.rs
│   │   └── turn_finished_signal.rs
│   ├── agent_protocol.rs
│   ├── analytics.rs
│   ├── auto_queue.rs
│   ├── automation_candidate_contract.rs
│   ├── automation_candidate_materializer.rs
│   ├── claude.rs
│   ├── claude_command.rs
│   ├── claude_compact_context.rs
│   ├── claude_compact_trigger.rs
│   ├── claude_gateway_proxy.rs
│   ├── codex.rs
│   ├── codex_remote_policy.rs
│   ├── codex_tmux_wrapper.rs
│   ├── cswap.rs
│   ├── discord_config_audit.rs
│   ├── discord_dm_reply_store.rs
│   ├── disk_monitor.rs
│   ├── dispatch_gate.rs
│   ├── dispatch_watchdog.rs
│   ├── dispatched_sessions.rs
│   ├── dispatches_followup.rs
│   ├── escalation_settings.rs
│   ├── gemini.rs
│   ├── github_issue_creation.rs
│   ├── health_active_session_audit.rs
│   ├── health_diagnostics.rs
│   ├── issue_announcements.rs
│   ├── kanban.rs
│   ├── kanban_cards.rs
│   ├── long_turn_watchdog.rs
│   ├── mcp_config.rs
│   ├── message_outbox.rs
│   ├── message_outbox_recovery.rs
│   ├── message_outbox_recovery_support.rs
│   ├── message_outbox_recovery_tests.rs
│   ├── mod.rs
│   ├── monitoring_store.rs
│   ├── opencode.rs
│   ├── operator_connectors.rs
│   ├── pane_readiness.rs
│   ├── pipeline_override.rs
│   ├── pipeline_routes.rs
│   ├── pr_summary.rs
│   ├── process.rs
│   ├── provider.rs
│   ├── provider_auth.rs
│   ├── provider_error_transcript.rs
│   ├── provider_exec.rs
│   ├── provider_hosting.rs
│   ├── provider_output_guard.rs
│   ├── provider_output_guard_tests.rs
│   ├── provider_runtime.rs
│   ├── queue.rs
│   ├── qwen.rs
│   ├── qwen_tmux_wrapper.rs
│   ├── remote_stub.rs
│   ├── retrospectives.rs
│   ├── review_decision.rs
│   ├── scheduled_messages.rs
│   ├── scheduling.rs
│   ├── service_error.rs
│   ├── session_activity.rs
│   ├── session_backend.rs
│   ├── session_forwarding.rs
│   ├── session_selector_validity.rs
│   ├── settings.rs
│   ├── shell_guard.rs
│   ├── termination_audit.rs
│   ├── tmux_common.rs
│   ├── tmux_diagnostics.rs
│   ├── tmux_wrapper.rs
│   ├── tool_output_guard.rs
│   ├── tui_prompt_control.rs
│   ├── tui_prompt_dedupe.rs
│   ├── tui_turn_state.rs
│   ├── turn_cancel_finalizer.rs
│   ├── turn_lifecycle.rs
│   └── turn_orchestrator.rs
├── supervisor/
│   └── mod.rs
├── ui/
│   ├── ai_screen.rs
│   └── mod.rs
├── utils/
│   ├── api.rs
│   ├── async_bridge.rs
│   ├── auth.rs
│   ├── discord.rs
│   ├── format.rs
│   ├── github_links.rs
│   ├── loopback_url.rs
│   ├── mod.rs
│   ├── redact.rs
│   ├── secret_file.rs
│   └── wip_detect.rs
├── voice/
│   ├── tts/
│   │   ├── chunks.rs
│   │   ├── edge.rs
│   │   ├── mod.rs
│   │   └── playback.rs
│   ├── announce_meta.rs
│   ├── barge_in.rs
│   ├── cancel_tombstone.rs
│   ├── commands.rs
│   ├── config.rs
│   ├── flight.rs
│   ├── metrics.rs
│   ├── mod.rs
│   ├── progress.rs
│   ├── prompt.rs
│   ├── receiver.rs
│   ├── runtime_boundary.rs
│   ├── sanitizer.rs
│   ├── stt.rs
│   ├── stt_streaming.rs
│   ├── turn_link.rs
│   └── utils.rs
├── api_caller_observability.rs
├── app_state.rs
├── bootstrap.rs
├── config.rs
├── config_live_reload.rs
├── credential.rs
├── error.rs
├── eventbus.rs
├── high_risk_recovery.rs
├── launch.rs
├── lib.rs
├── logging.rs
├── main.rs
├── manual_intervention.rs
├── pipeline.rs
├── receipt.rs
└── reconcile.rs
```
<!-- END GENERATED: SRC TREE -->

## High-Signal Module Map

### Top-Level Rust Modules

This table is generated from the current `src/` root and fails CI when a new top-level module or directory lacks a description.

<!-- BEGIN GENERATED: TOP LEVEL MODULE MAP -->
> Generated by `python3 scripts/generate_inventory_docs.py`. Update `TOP_LEVEL_MODULE_PURPOSES` when `src/` top-level entries change.

| Path | Purpose |
| --- | --- |
| `src/cli/` | Operator-facing CLI commands, direct API shims, migrations, and Discord send helpers. |
| `src/compat/` | Centralised home for compatibility/legacy/fallback shims (#1076). Each public item carries a `REMOVE_WHEN` comment so retirement is grep-driven. |
| `src/db/` | PostgreSQL access layer, migration helpers, and schema authority. |
| `src/dispatch/` | Dispatch context construction, review metadata, and worktree targeting. |
| `src/engine/` | QuickJS policy runtime, hook wiring, transition logic, and Rust-JS bridge ops. |
| `src/github/` | GitHub sync, issue triage, and Definition-of-Done mirroring. |
| `src/kanban/` | High-level kanban orchestration, state machine facade, and shared test support. |
| `src/runtime_layout/` | Managed runtime layout, memory-path migration, shared prompt sync, and skill deployment. |
| `src/server/` | Axum server boot, routes, workers, background loops, and WebSocket broadcast. |
| `src/services/` | Core runtime services: provider runners, Discord bot, queueing, memory, and platform helpers. |
| `src/supervisor/` | Runtime supervisor signals and recovery decisions for orphaned or stalled work. |
| `src/ui/` | Compatibility shims for persisted UI/session types used by the Discord runtime. |
| `src/utils/` | Shared formatting and Unicode-safe string utilities. |
| `src/voice/` | Voice command, STT/TTS, prompt, progress, metrics, receiver, and barge-in helpers. |
| `src/api_caller_observability.rs` | Request-principal classification and uniform log-only API caller attribution records for identity-consuming mutation paths. |
| `src/app_state.rs` | Shared HTTP route-handler state (`AppState`); lives at crate root below server+services so service-layer handlers reference it without a service→server backflow. |
| `src/bootstrap.rs` | Builds config, database, policy engine, and shared app state before launch. |
| `src/config.rs` | `agentdesk.yaml` parsing, configuration defaults, and shared test env helpers. |
| `src/config_live_reload.rs` | Hot-reloads `agentdesk.yaml` without a restart: a debounced `notify` watcher pre-validates edits and atomically swaps a process-global config snapshot, keeping the running config on failure and reporting restart-required infra changes. |
| `src/credential.rs` | Reads runtime credential files such as Discord bot tokens from the AgentDesk root. |
| `src/error.rs` | Shared HTTP and policy error type with typed codes and JSON response helpers. |
| `src/eventbus.rs` | In-process broadcast event bus (history/replay/batching) shared by the WS server layer and background services without a service→server backflow. |
| `src/high_risk_recovery.rs` | PG-only high-risk recovery tests for boot reconciliation and review refire paths. |
| `src/launch.rs` | Starts the Tokio runtime and hands off to server boot. |
| `src/lib.rs` | Library crate boundary that exposes the server/CLI modules for the slim binary entry point and tests. |
| `src/logging.rs` | Tracing span helpers that stamp dispatch, card, agent, and hook context onto logs. |
| `src/main.rs` | Binary entry point. Dispatches CLI commands or boots the server runtime. |
| `src/manual_intervention.rs` | Manual intervention parsing and helpers shared by Discord reply/requeue flows. |
| `src/pipeline.rs` | Pipeline stage loading, resolution, and transition helpers. |
| `src/receipt.rs` | Receipt parsing and workspace attribution helpers. |
| `src/reconcile.rs` | Boot-time reconciliation for persisted state and dispatch-runtime drift. |
<!-- END GENERATED: TOP LEVEL MODULE MAP -->

### Discord Runtime

| Path | Purpose |
| --- | --- |
| `src/services/discord/mod.rs` | Shared bot state, boot wiring, cross-module exports. |
| `src/services/discord/router/` | Message intake, thread binding, dispatch guard, control intent parsing. |
| `src/services/discord/turn_bridge/` | Turn execution lifecycle, completion guard, retry handling, memory capture. |
| `src/services/discord/session_runtime.rs` | Session bootstrap, path/worktree resolution, per-channel session state. |
| `src/services/discord/tmux.rs` / `tmux_reaper.rs` | tmux watcher lifecycle, stale session cleanup, reaping. |
| `src/services/discord/recovery_engine.rs` | Restart-time inflight turn recovery. |
| `src/services/discord/gateway.rs` / `discord_io.rs` / `queue_io.rs` | Discord gateway bridge and outbound/inbound message plumbing. |
| `src/services/discord/commands/` | Slash command handlers for session, config, diagnostics, meetings, models, receipts, skills. |
| `src/services/discord/agentdesk_config.rs` / `config_audit.rs` | YAML/DB/legacy config source-of-truth handling and audits. |
| `src/services/discord/prompt_builder.rs` / `shared_memory.rs` / `role_map.rs` | Turn prompt assembly and org/shared memory context. |

### Provider and Execution Services

| Path | Purpose |
| --- | --- |
| `src/services/claude.rs`, `codex.rs`, `gemini.rs`, `qwen.rs` | Provider-specific session execution and stream handling. |
| `src/services/provider.rs` / `provider_exec.rs` / `provider_runtime.rs` | Provider abstraction, dispatch, runtime metadata. |
| `src/services/session_backend.rs` | Child-process session backend for non-tmux execution paths. |
| `src/services/tmux_wrapper.rs`, `codex_tmux_wrapper.rs`, `qwen_tmux_wrapper.rs` | Provider wrappers used inside tmux-managed sessions. |
| `src/services/process.rs` / `platform/` | Process-tree control, shell helpers, binary resolution, tmux/platform utilities. |
| `src/services/queue.rs` / `turn_orchestrator.rs` / `turn_lifecycle.rs` | Per-channel queueing, cancellation, active turn bookkeeping. |

### Server and API

| Path | Purpose |
| --- | --- |
| `src/server/boot.rs` / `src/server/mod.rs` | Axum boot, router assembly, background/tick startup. |
| `src/server/routes/mod.rs` | API route registration under `/api`. |
| `src/server/routes/dispatches/` | Dispatch CRUD, Discord delivery, outbox, thread reuse. |
| `src/server/routes/review_verdict/` | Review verdict and decision routes plus review-state storage helpers. |
| `src/server/ws.rs` | Top-level WebSocket endpoint and broadcast plumbing. |
| `src/server/worker_registry.rs` | Supervised worker specs; mirrored to `docs/generated/worker-inventory.md`. |

## Generated Inventories

- `docs/generated/module-inventory.md` is the fastest way to answer “which module owns this code?”
- `docs/generated/route-inventory.md` is the authoritative endpoint-to-handler map. Prefer it over manually maintained tables.
- `docs/generated/worker-inventory.md` shows every supervised worker, its start stage, restart policy, and owner.
- `python3 scripts/generate_inventory_docs.py --check` is the CI drift gate for these inventories, the generated `src/` snapshot above, and the top-level module coverage table.

## Troubleshooting: Where to Look

### Discord turn did not start

1. `src/services/discord/router/message_handler.rs` — intake, session/worktree selection, dispatch context hints.
2. `src/services/discord/turn_bridge/mod.rs` — turn spawn, stream loop, completion path.
3. Provider file: `src/services/claude.rs`, `codex.rs`, `gemini.rs`, or `qwen.rs`.

### Session died or output stopped

1. `src/services/discord/tmux.rs` — watcher, session kill, resume, orphan handling.
2. `src/services/discord/turn_bridge/tmux_runtime.rs` — active token and watcher handoff helpers.
3. `src/services/tmux_diagnostics.rs` / `src/services/process.rs` — exit diagnostics and process-tree cleanup.
4. `src/services/discord/recovery_engine.rs` — restart-time restoration.

### Worktree or cwd is wrong

1. `src/services/discord/session_runtime.rs` — session path/worktree creation.
2. `src/dispatch/mod.rs` — card-scoped worktree resolution and dispatch context injection.
3. `src/cli/client.rs` — completion payload fallback for `completed_worktree_path`.

### Kanban or review state is wrong

1. `src/kanban.rs` — high-level card orchestration.
2. `src/engine/transition.rs` — canonical state transitions.
3. `src/engine/ops/kanban_ops.rs` — review-state sync bridge and SQL-side helpers.
4. `src/server/routes/review_verdict/` — review verdict/decision HTTP surface.

### API endpoint is missing or behaving unexpectedly

1. `src/server/routes/mod.rs` — confirm registration.
2. Relevant handler file under `src/server/routes/`.
3. `docs/generated/route-inventory.md` — confirm method/path/handler mapping.
4. `src/server/ws.rs` — for the top-level `/ws` endpoint.

### Startup failed

1. `src/bootstrap.rs` — config/db/runtime assembly.
2. `src/config.rs` — config load/defaults.
3. `src/db/mod.rs` and `src/db/schema.rs` — DB open/migrations.
4. `src/launch.rs`, `src/server/boot.rs`, `src/server/mod.rs` — runtime and HTTP boot.

## Policy and Runtime Flow

- Policy definitions live in `policies/*.js`.
- Hook contracts live in `src/engine/hooks.rs`.
- Policy loading and execution live in `src/engine/loader.rs` and `src/engine/mod.rs`.
- Rust bridge functions live in `src/engine/ops.rs` plus `src/engine/ops/*.rs`.
- Tick orchestration lives in `src/server/tick.rs` and the server boot path.

## Session Execution Paths

### tmux-backed providers

1. Message intake: `src/services/discord/router/message_handler.rs`
2. Turn spawn: `src/services/discord/turn_bridge/mod.rs`
3. Provider execution: provider module + `*_tmux_wrapper.rs`
4. Watch/recovery: `src/services/discord/tmux.rs` and `src/services/discord/recovery_engine.rs`

### Child-process backend

1. Message intake: `src/services/discord/router/message_handler.rs`
2. Session spawn: `src/services/session_backend.rs`
3. Cleanup: `src/services/process.rs`

### Session runtime state (issue #892)

Tmux-backed sessions keep four kinds of runtime state alongside each pane:
the provider jsonl stream (`.jsonl`), the input FIFO (`.input`), the launch
script (`.sh`), and an owner marker (`.owner`). These files live in a
**persistent** per-runtime directory rather than `/tmp/` so that a dcserver
restart does not render a still-alive tmux pane "unusable":

- **Persistent path:** `runtime_root()/runtime/sessions/` (mode `0o700`),
  resolved via `tmux_common::session_temp_path(session_name, ext)`. The
  directory is created at dcserver startup in `src/cli/dcserver.rs` and
  lazily re-created inside `agentdesk_temp_dir()` for early callers.
- **Legacy `/tmp/` fallback:** wrappers spawned before this migration
  still hold open fds on `/tmp/agentdesk-*` files. Readers go through
  `tmux_common::resolve_session_temp_path` which prefers the new path
  and falls back to the legacy `/tmp` location so `session_usable`
  checks (`claude::execute_streaming_local_tmux`,
  `codex::execute_streaming_local_tmux`,
  `qwen::execute_streaming_local_tmux`) keep re-attaching to live panes.
  Legacy files are **never** swept at startup — pre-migration wrappers
  may still be writing into them.
- **Size cap policy:** 20 MB rolling head-truncate. The watcher in
  `src/services/discord/tmux.rs::tmux_output_watcher` periodically
  (~every 60 loop ticks) calls `truncate_jsonl_head_safe(path, 20 MB, 15 MB)`
  which rewrites the file keeping only the last ~15 MB worth of complete
  lines. Any partial leading line is dropped so downstream stream-json
  parsers never observe half-records.
- **Cleanup triggers:**
  - Recreate path inside each provider module calls
    `cleanup_session_temp_files(session_name)` before building a fresh
    session; it hits both persistent and legacy locations.
  - `turn_lifecycle::stop_turn_with_policy` calls
    `cleanup_session_temp_files` after a successful forced kill
    (force_kill_turn, `/clear`, etc).
  - `discord::tmux_reaper::reap_orphan_tmux_files` uses the same helper.
  - Startup orphan sweep: `discord::tmux::sweep_orphan_session_files`
    removes files in the persistent sessions dir whose stem has no
    matching live tmux session and whose oldest mtime is older than
    10 minutes. Deliberately skips `/tmp/` to avoid stomping on
    pre-migration wrappers.
- **Key helpers:** `src/services/tmux_common.rs`
  (`session_temp_path`, `legacy_tmp_session_path`,
  `resolve_session_temp_path`, `cleanup_session_temp_files`,
  `truncate_jsonl_head_safe`, `ensure_sessions_dir_on_startup`,
  `persistent_sessions_dir`).

## Data Model Anchors

- `src/db/schema.rs` is the authoritative schema.
- Most operational state hangs off `agents`, `kanban_cards`, `task_dispatches`, `sessions`, `auto_queue_runs`, `auto_queue_entries`, `github_repos`, and `kv_meta`.
- If a doc or handler disagrees with the schema, trust `src/db/schema.rs` and update the doc.
