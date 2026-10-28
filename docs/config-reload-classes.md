# Config Reload Classes

<!-- Generated from src/config/reload_classes/table.rs; do not edit by hand. -->
<!-- Regenerate: ADK_WRITE_RELOAD_CLASSES_DOC=1 cargo test --lib config::reload_classes:: -->

When an edit to `agentdesk.yaml` takes effect. Config hot reload stores every
valid edit in the live snapshot, but only `live` fields act on it. A change to a
`restart` or `seeded-at-boot` field logs one WARN line naming the field.

Daemon snapshot readers retain the last validated boot/reload value when hot reload
is disabled or its watcher cannot start. The bot or standalone server owns the watcher;
ending the HTTP task does not stop it. The config directory is polled every two
seconds with content comparison, independently of native filesystem notifications.

- `live`: read from the live snapshot (or disk) at use time.
- `restart`: captured at boot or on first access; restart to apply.
- `seeded-at-boot`: copied into the DB/kv at boot, and runtime readers read that
  copy; restart to re-seed.

`*` stands for any map key or list item.

| Field | Class | Read or captured by |
|---|---|---|
| `server.port` | restart | `src/server/mod.rs` run (listener bind); also seeded as kv server_port by `src/db/postgres/shared_config.rs` startup_reseed |
| `server.host` | restart | `src/server/mod.rs` run (bind addr) |
| `server.auth_token` | restart | `src/server/routes/auth.rs` auth_middleware (AppState.config Arc built in server::run) |
| `server.allow_insecure_nonloopback_bind` | restart | `src/server/mod.rs` run (bind_host decision) |
| `server.peer_filter` | restart | `src/server/web_surface.rs` router (PeerFilter::new(state.config.server.peer_filter)) |
| `server.browser_guard` | restart | `src/server/web_surface.rs` router (BrowserGuard::new copies the mode into the HTTP layer and DashboardAccess) |
| `discord.bots.*.token` | restart | `src/cli/dcserver.rs` handle_dcserver (load_discord_bot_launch_configs -> one run_bot per token) |
| `discord.bots.*.description` | live | `src/services/discord/agentdesk_config.rs` load_discord_bot_configs (no daemon reader) |
| `discord.bots.*.provider` | restart | `src/services/discord/settings/read.rs` load_bot_settings (run_bot -> SharedData.settings) |
| `discord.bots.*.agent` | restart | `src/services/discord/settings/read.rs` load_bot_settings (run_bot -> SharedData.settings) |
| `discord.bots.*.auth` | restart | `src/services/discord/settings/read.rs` load_bot_settings (SharedData.settings at run_bot) |
| `discord.guild_id` | restart | `src/services/auto_queue/fsm.rs` AutoQueueActivateDeps::from_state (AppState.config) |
| `discord.dm_default_agent` | live | `src/services/discord/agentdesk_config.rs` resolve_dm_default_agent (validated snapshot per DM) |
| `discord.owner_id` | restart | `src/services/discord/settings/read.rs` load_bot_settings (owner_user_id into SharedData.settings) |
| `discord.scheduled_message_required_mention_user_ids` | restart | `src/server/routes/scheduled_messages/discord_mentions.rs` required_from_state (AppState.config) |
| `providers` | restart | `src/services/provider_hosting.rs` install_provider_hosting_config (process-global map); remote_ssh_enabled gated in `src/bootstrap.rs` initialize |
| `voice.enabled` | restart | `src/services/discord/voice_barge_in/runtime_lifecycle.rs` VoiceBargeInRuntime::from_voice_config (runtime_bootstrap.rs:run_bot) |
| `voice.verbose_progress` | restart | `src/services/discord/voice_barge_in/runtime_lifecycle.rs` VoiceBargeInRuntime::from_voice_config (AtomicBool) |
| `voice.audio.recordings_dir` | restart | `src/voice/receiver.rs` VoiceReceiverConfig::from_voice_config (runtime_bootstrap/voice.rs:run_bot_init_voice_workers) |
| `voice.audio.transcripts_dir` | restart | `src/services/discord/voice_barge_in/utility.rs` transcript_dirs_from_config (via from_voice_config) |
| `voice.audio.tts_cache_dir` | live | `src/voice/config.rs` VoiceAudioDirs (no daemon reader) |
| `voice.audio.temp_dir` | restart | `src/server/worker_registry/registry.rs` start_worker (MaintenanceScheduler voice_cache_sweep::Config::from_voice_config) |
| `voice.stt` | restart | `src/services/discord/voice_barge_in/runtime_lifecycle.rs` VoiceBargeInRuntime::from_voice_config (VoiceSttRuntime::from_voice_config) |
| `voice.tts` | restart | `src/services/discord/voice_barge_in/runtime_lifecycle.rs` VoiceBargeInRuntime::from_voice_config (TtsRuntime::from_voice_config) |
| `voice.thresholds.speech_start_db` | restart | `src/voice/stt.rs` SttConfig::from_voice_config (via VoiceSttRuntime at boot) |
| `voice.idle.segment_idle_ms` | restart | `src/voice/receiver.rs` VoiceReceiverConfig::from_voice_config |
| `voice.idle.utterance_idle_ms` | restart | `src/voice/receiver.rs` VoiceReceiverConfig::from_voice_config |
| `voice.idle.channel_idle_disconnect_secs` | live | `src/voice/config.rs` VoiceIdleTimings (no daemon reader) |
| `voice.idle.wake_listen_window_secs` | live | `src/voice/config.rs` VoiceIdleTimings (no daemon reader) |
| `voice.barge_in` | restart | `src/services/discord/voice_barge_in/runtime_lifecycle.rs` VoiceBargeInRuntime::from_voice_config |
| `voice.wake_words` | restart | `src/services/discord/voice_barge_in/runtime_lifecycle.rs` VoiceBargeInRuntime::from_voice_config (voice_config_state RwLock) |
| `voice.allowed_user_ids` | restart | `src/voice/receiver.rs` VoiceReceiverConfig::from_voice_config |
| `voice.lobby_channel_id` | restart | `src/services/discord/runtime_bootstrap/voice.rs` run_bot_spawn_voice_auto_join (auto_join_channel_ids_with_lobby) |
| `voice.active_agent_ttl_seconds` | live | `src/voice/commands.rs` resolve_voice_lobby_route (cached_config from voice_barge_in/routing.rs:resolve_voice_turn_target) |
| `voice.foreground.provider` | live | `src/services/discord/voice_barge_in/routing.rs` resolve_effective_foreground_config (cached_config 5s TTL) |
| `voice.foreground.model` | live | `src/services/discord/voice_barge_in/routing.rs` resolve_effective_foreground_config |
| `voice.foreground.max_chars` | live | `src/services/discord/voice_barge_in/routing.rs` resolve_effective_foreground_config |
| `voice.foreground.timeout_ms` | live | `src/services/discord/voice_barge_in/routing.rs` resolve_effective_foreground_config |
| `voice.conductor.max_wait_secs` | live | `src/server/routes/voice_conductor.rs` say (live_voice_config reads the validated snapshot) -> services/voice_conductor.rs:gather |
| `voice.spoken_result.max_chars` | live | `src/services/discord/voice_barge_in/final_result_playback.rs` spawn_spoken_result_playback (cached_config) |
| `voice.default_sensitivity_mode` | live | `src/server/routes/voice_config.rs` (load_editable_config re-reads disk) |
| `voice.auto_join_channel_ids` | restart | `src/services/discord/runtime_bootstrap/voice.rs` run_bot_spawn_voice_auto_join |
| `voice.keep_recordings` | restart | `src/voice/receiver.rs` VoiceReceiverConfig::from_voice_config |
| `shared_prompt` | live | `src/services/discord/settings/content.rs` load_shared_prompt_for_profile (agentdesk_config::load_shared_prompt_path reads the validated snapshot) |
| `mcp_servers` | restart | `src/bootstrap.rs` initialize (mcp_config sync_*_mcp_servers write provider configs once) |
| `review_mcp_allowlist` | live | `src/services/mcp_config.rs` review_mcp_allowlist via claude_mcp_config_arg (validated snapshot per Claude spawn) |
| `mcp` | restart | `src/services/discord/runtime_bootstrap/framework_setup.rs` run_bot_framework_setup (credential watcher spawned once) |
| `agents.*.id` | seeded-at-boot | `src/db/postgres/agent_sync.rs` upsert_agent_from_config_pg (startup_reseed; retire_agent_pg deletes rows missing from YAML) |
| `agents.*.memory_group` | live | `src/services/discord/settings/memory.rs` resolve_agent_memory_settings (AgentDef from the validated snapshot in agentdesk_config role binding) |
| `agents.*.name` | seeded-at-boot | `src/db/postgres/agent_sync.rs` upsert_agent_from_config_pg (agents.name) |
| `agents.*.name_ko` | seeded-at-boot | `src/db/postgres/agent_sync.rs` upsert_agent_from_config_pg (agents.name_ko) |
| `agents.*.aliases` | live | `src/voice/commands.rs` resolve_voice_lobby_route (agent_voice_aliases on cached_config) |
| `agents.*.wake_word` | live | `src/voice/commands.rs` transcript_after_agent_wake_word (cached_config) |
| `agents.*.voice_enabled` | live | `src/voice/commands.rs` resolve_voice_lobby_route (cached_config) |
| `agents.*.sensitivity_mode` | live | `src/server/routes/voice_config.rs` (disk re-read; DTO display/edit) |
| `agents.*.voice.channel_id` | restart | `src/services/discord/runtime_bootstrap/voice.rs` voice_auto_join_provider_map (run_bot_spawn_voice_auto_join) |
| `agents.*.voice.foreground.provider` | restart | `src/services/discord/runtime_bootstrap/voice.rs` voice_auto_join_provider_map |
| `agents.*.voice.foreground.model` | live | `src/services/discord/voice_barge_in/routing.rs` resolve_effective_foreground_config (cached_config) |
| `agents.*.voice.foreground.max_chars` | live | `src/services/discord/voice_barge_in/routing.rs` resolve_effective_foreground_config |
| `agents.*.voice.foreground.timeout_ms` | live | `src/services/discord/voice_barge_in/routing.rs` resolve_effective_foreground_config |
| `agents.*.provider` | seeded-at-boot | `src/db/postgres/agent_sync.rs` upsert_agent_from_config_pg (agents.provider) |
| `agents.*.channels.*.id` | seeded-at-boot | `src/db/postgres/agent_sync.rs` upsert_agent_from_config_pg (discord_channel_id/alt/cc/cdx) |
| `agents.*.channels.*.name` | seeded-at-boot | `src/db/postgres/agent_sync.rs` upsert_agent_from_config_pg (target fallback when id absent) |
| `agents.*.channels.*.aliases` | seeded-at-boot | `src/db/postgres/agent_sync.rs` upsert_agent_from_config_pg (target fallback only when id and name absent) |
| `agents.*.channels.*.prompt_file` | live | `src/services/discord/agentdesk_config.rs` role_binding_from_channel (resolve_role_binding reads the validated snapshot) |
| `agents.*.channels.*.workspace` | live | `src/services/discord/agentdesk_config.rs` resolve_workspace |
| `agents.*.channels.*.provider` | restart | `src/services/provider_hosting.rs` install_provider_hosting_config (channel map key) |
| `agents.*.channels.*.tui_hosting` | restart | `src/services/provider_hosting.rs` install_provider_hosting_config |
| `agents.*.channels.*.runtime` | restart | `src/services/provider_hosting.rs` install_provider_hosting_config |
| `agents.*.channels.*.model` | live | `src/services/discord/agentdesk_config.rs` role_binding_from_channel |
| `agents.*.channels.*.reasoning_effort` | live | `src/services/discord/agentdesk_config.rs` role_binding_from_channel |
| `agents.*.channels.*.peer_agents` | live | `src/services/discord/agentdesk_config.rs` role_binding_from_channel |
| `agents.*.channels.*.quality_feedback_injection` | live | `src/services/discord/agentdesk_config.rs` role_binding_from_channel |
| `agents.*.channels.*.dispatch_profile` | live | `src/services/discord/agentdesk_config.rs` resolve_dispatch_profile |
| `agents.*.channels.*.isolate_override` | live | `src/services/discord/agentdesk_config.rs` resolve_worktree_isolation_policy |
| `agents.*.channels.*.cache_ttl_minutes` | live | `src/services/discord/agentdesk_config.rs` resolve_cache_ttl_minutes |
| `agents.*.keywords` | live | `src/voice/commands.rs` agent_voice_aliases / `src/services/discord/agentdesk_config.rs` load_meeting_config fallback |
| `agents.*.peer_keywords` | live | `src/services/discord/agentdesk_config.rs` load_peer_agents (directory only; validated snapshot per prompt) |
| `agents.*.peer_directory` | live | `src/services/discord/agentdesk_config.rs` load_peer_agents (validated snapshot per prompt) |
| `agents.*.department` | seeded-at-boot | `src/db/postgres/agent_sync.rs` upsert_agent_from_config_pg (agents.department) |
| `agents.*.avatar_emoji` | seeded-at-boot | `src/db/postgres/agent_sync.rs` upsert_agent_from_config_pg (agents.avatar_emoji) |
| `agents.*.preferred_intake_node_labels` | seeded-at-boot | `src/db/postgres/agent_sync.rs` upsert_agent_from_config_pg (COALESCE: absent key keeps DB value) |
| `meeting.channel_name` | live | `src/services/discord/agentdesk_config.rs` load_meeting_config (validated snapshot per meeting start) |
| `meeting.max_rounds` | live | `src/services/discord/agentdesk_config.rs` load_meeting_config (validated snapshot per meeting start) |
| `meeting.max_participants` | live | `src/services/discord/agentdesk_config.rs` load_meeting_config (validated snapshot per meeting start) |
| `meeting.summary_agent` | live | `src/services/discord/agentdesk_config.rs` load_meeting_config (validated snapshot per meeting start) |
| `meeting.available_agents` | live | `src/services/discord/agentdesk_config.rs` meeting_agent_from_entry via load_meeting_config (validated values and presence metadata per meeting) |
| `github.repos` | seeded-at-boot | `src/db/postgres/shared_config.rs` startup_reseed (register_repo INSERT ... ON CONFLICT DO NOTHING into github_repos) |
| `github.repo_dirs` | restart | `src/services/cluster/readiness.rs` collect (config captured by readiness::spawn_probe at node_registry::bootstrap) |
| `github.sync_interval_minutes` | restart | `src/server/worker_registry/registry.rs` start_worker (GithubSync arm captures sync_interval) |
| `policies` | restart | `src/engine/mod.rs` PolicyEngine::new_with_runtime_deps (runtime limits and watcher dir) |
| `data.dir` | restart | `src/services/cluster/readiness.rs` collect (boot-captured config; fallback root only when runtime_root() is None) |
| `data.db_name` | live | `src/cli/doctor/orchestrator.rs` check_db_integrity via run_doctor_report (load_graceful per doctor run) |
| `database` | restart | `src/db/postgres.rs` connect (pool built once + install_foreground_reserve) |
| `cluster.enabled` | restart | `src/services/cluster/node_registry.rs` bootstrap |
| `cluster.instance_id` | restart | `src/services/cluster/node_registry.rs` bootstrap (SELF_INSTANCE_ID OnceLock via resolve_instance_id) |
| `cluster.role` | restart | `src/services/cluster/node_registry.rs` bootstrap (configured_role, leader lease) |
| `cluster.runtime_profile` | restart | `src/server/mod.rs` run (runtime_profile.modules()) |
| `cluster.execution_slots` | restart | `src/services/cluster/execution_capacity.rs` LOCAL (OnceLock get_or_init in publish/acquire) |
| `cluster.heartbeat_interval_secs` | restart | `src/services/cluster/node_registry.rs` spawn_heartbeat_loop |
| `cluster.lease_ttl_secs` | restart | `src/services/cluster/node_registry.rs` spawn_heartbeat_loop (stale_threshold + intake sweep TTL) |
| `cluster.api_base_url` | restart | `src/services/cluster/node_registry.rs` bootstrap (node_capabilities -> base_capabilities in heartbeat; machine_resources::spawn) |
| `cluster.gateway_preferred_instance_id` | restart | `src/services/discord/runtime_bootstrap/gateway_lease.rs` run_bot_spawn_gateway_lease_keepalive (gateway_preference() resolved once per task) |
| `cluster.gateway_yield_grace_secs` | restart | `src/services/discord/runtime_bootstrap/gateway_lease.rs` run_bot_spawn_gateway_lease_keepalive (GatewayPreference.yield_grace) |
| `cluster.gateway_lease_idle_expiry_secs` | live | `src/services/discord/runtime_bootstrap/gateway_lease.rs` try_acquire_discord_gateway_lease (config::snapshot per acquisition) |
| `cluster.gateway_handback_breaker.enabled` | live | `src/services/discord/runtime_bootstrap/gateway_handback_breaker.rs` GatewayHandbackBreaker::for_owner (config closure = config::snapshot per call) |
| `cluster.gateway_handback_breaker.window_secs` | live | `src/services/discord/runtime_bootstrap/gateway_handback_breaker.rs` GatewayHandbackBreaker::for_owner (config closure = config::snapshot per call) |
| `cluster.gateway_handback_breaker.max_empty` | live | `src/services/discord/runtime_bootstrap/gateway_handback_breaker.rs` GatewayHandbackBreaker::for_owner (config closure = config::snapshot per call) |
| `cluster.gateway_handback_breaker.suppress_secs` | live | `src/services/discord/runtime_bootstrap/gateway_handback_breaker.rs` GatewayHandbackBreaker::for_owner (config closure = config::snapshot per call) |
| `cluster.labels` | restart | `src/services/cluster/node_registry.rs` bootstrap (labels JSON captured into spawn_heartbeat_loop) |
| `cluster.capabilities` | restart | `src/services/cluster/node_registry.rs` bootstrap (base_capabilities captured into heartbeat) |
| `cluster.nodes` | restart | `src/services/dispatches/wait_queue.rs` RUNTIME_CLUSTER_CONFIG (set once in server::run); forwarding trust from boot AppState |
| `cluster.blackout_windows` | restart | `src/services/dispatches/wait_queue.rs` RUNTIME_CLUSTER_CONFIG (set once in server::run) and dispatch_outbox_loop |
| `cluster.dispatch_routing.default_preferred_labels` | restart | `src/services/dispatches/wait_queue.rs` RUNTIME_CLUSTER_CONFIG (set once in server::run) + outbox_queue.rs:dispatch_outbox_loop + node_registry.rs:spawn_stale_claim_owner_reassignment_loop |
| `cluster.dispatch_routing.opt_out_dispatch_types` | live | `src/dispatch/dispatch_create.rs` dispatch_required_capabilities (validated snapshot) |
| `cluster.dispatch_routing.constraints` | restart | `src/services/dispatches/wait_queue.rs` RUNTIME_CLUSTER_CONFIG (set once in server::run) + outbox_queue.rs:dispatch_outbox_loop + node_registry.rs:spawn_stale_claim_owner_reassignment_loop |
| `cluster.dispatch_routing.wait_timeout_secs` | restart | `src/services/dispatches/wait_queue.rs` wake_waiting_dispatch_outbox_pg (only invoked with boot copies) |
| `cluster.dispatch_routing.wake_interval_secs` | restart | `src/services/dispatches/outbox_queue.rs` dispatch_outbox_loop |
| `cluster.intake_routing.capacity_aware` | live | `src/services/cluster/execution_capacity.rs` automatic_enabled (config::snapshot) |
| `cluster.intake_routing.enabled` | restart | `src/services/discord/runtime_bootstrap/intake.rs` run_bot_maybe_spawn_intake_worker (worker consumer spawned only if effective mode != disabled at bot boot) |
| `cluster.intake_routing.mode` | restart | `src/services/discord/runtime_bootstrap/intake.rs` run_bot_maybe_spawn_intake_worker |
| `cluster.intake_routing.owner_authority_channel_ids` | live | `src/services/cluster/intake_routing_config.rs` effective_intake_routing_config (config::snapshot) |
| `cluster.intake_routing.forward_pre_claim_timeout_secs` | live | `src/services/discord/router/intake_dispatch.rs` admit_text_intake (effective_intake_routing_config) |
| `cluster.intake_routing.stale_claim_recovery_secs` | live | `src/services/cluster/intake_routing_config.rs` EffectiveIntakeRoutingConfig::status_json |
| `cluster.intake_routing.max_attempts_per_message` | live | `src/services/cluster/node_registry.rs` current_intake_retry_config (per heartbeat tick) |
| `cluster.intake_routing.retry_authorization_secs` | live | `src/services/cluster/node_registry.rs` current_intake_retry_config (per heartbeat tick) |
| `cluster.semaphores.*.capacity` | live | `src/server/task_dispatch_claims/config_read.rs` claim_task_dispatches (config::snapshot per claim) |
| `cluster.semaphores.*.scope` | live | `src/server/task_dispatch_claims/config_read.rs` claim_task_dispatches (config::snapshot per claim) |
| `cluster.session_bound_relay_enabled` | restart | `src/server/worker_registry/registry.rs` start_worker (WatcherSupervisor spawn gate) |
| `kanban.manager_channel_id` | seeded-at-boot | `src/services/settings.rs` config_default_seed_actions (applied by db/postgres/shared_config.rs:startup_reseed) -> kv kanban_manager_channel_id |
| `kanban.deadlock_manager_channel_id` | seeded-at-boot | `src/services/settings.rs` config_default_seed_actions (applied by db/postgres/shared_config.rs:startup_reseed) -> kv deadlock_manager_channel_id |
| `kanban.relay_alert_threshold` | seeded-at-boot | `src/services/settings.rs` config_default_seed_actions (applied by db/postgres/shared_config.rs:startup_reseed) -> kv kanban_relay_alert_threshold |
| `review.enabled` | seeded-at-boot | `src/services/settings.rs` config_default_seed_actions (applied by db/postgres/shared_config.rs:startup_reseed) -> kv review_enabled |
| `review.max_rounds` | seeded-at-boot | `src/services/settings.rs` config_default_seed_actions (applied by db/postgres/shared_config.rs:startup_reseed) -> kv max_review_rounds |
| `placeholder` | restart | `src/cli/dcserver.rs` handle_dcserver (UiFeatureFlags into SharedData) |
| `runtime.execution_identity_mode` | live | `src/services/discord/tmux_watcher_registry/fences.rs` execution_identity_mode (current()) |
| `runtime.publication_permit_mode` | live | `src/services/discord/delivery_lease_cell/source_epoch_observer.rs` marker_if_enabled (current()) |
| `runtime.relay_verdict_source` | live | `src/services/discord/health/reachability/composite.rs` relay_verdict_source (current()) |
| `runtime.intake_delivery_sweep_dispatched_cutoff_secs` | live | `src/services/discord/runtime_bootstrap/intake_delivery_sweep.rs` spawn_intake_delivery_sweep (current() per tick) |
| `runtime.intake_delivery_sweep_spawned_cutoff_secs` | live | `src/services/discord/runtime_bootstrap/intake_delivery_sweep.rs` spawn_intake_delivery_sweep (current() per tick) |
| `runtime.intake_delivery_sweep_batch_limit` | live | `src/services/discord/runtime_bootstrap/intake_delivery_sweep.rs` spawn_intake_delivery_sweep (current() per tick) |
| `runtime.requested_timeout_min` | seeded-at-boot | `src/services/settings.rs` config_default_seed_actions (kv scalar; via shared_config.rs:startup_reseed) -> requested_timeout_min |
| `runtime.in_progress_stale_min` | seeded-at-boot | `src/services/settings.rs` config_default_seed_actions (kv scalar; via shared_config.rs:startup_reseed) -> in_progress_stale_min |
| `runtime.context_compact_percent` | seeded-at-boot | `src/services/settings.rs` config_default_seed_actions (kv scalar; via shared_config.rs:startup_reseed) -> context_compact_percent |
| `runtime.context_compact_percent_codex` | seeded-at-boot | `src/services/settings.rs` config_default_seed_actions (kv scalar; via shared_config.rs:startup_reseed) -> context_compact_percent_codex |
| `runtime.context_compact_percent_claude` | seeded-at-boot | `src/services/settings.rs` config_default_seed_actions (kv scalar; via shared_config.rs:startup_reseed) -> context_compact_percent_claude |
| `runtime.context_compact_lower_bound_tokens` | seeded-at-boot | `src/services/settings.rs` config_default_seed_actions (kv scalar; via shared_config.rs:startup_reseed) -> context_compact_lower_bound_tokens |
| `runtime.context_compact_window_claude` | live | `src/services/claude_tui/session.rs` write_launch_script (validated snapshot per launch) |
| `runtime.max_retries` | seeded-at-boot | `src/services/settings.rs` seed_runtime_config_defaults_pg (kv runtime-config blob; via startup_reseed) -> maxRetries |
| `runtime.max_entry_retries` | seeded-at-boot | `src/services/settings.rs` seed_runtime_config_defaults_pg (kv runtime-config blob; via startup_reseed) -> maxEntryRetries |
| `runtime.stale_dispatched_grace_min` | seeded-at-boot | `src/services/settings.rs` seed_runtime_config_defaults_pg (kv runtime-config blob; via startup_reseed) -> staleDispatchedGraceMin |
| `runtime.stale_dispatched_terminal_statuses` | seeded-at-boot | `src/services/settings.rs` seed_runtime_config_defaults_pg (kv runtime-config blob; via startup_reseed) -> staleDispatchedTerminalStatuses |
| `runtime.stale_dispatched_recover_null_dispatch` | seeded-at-boot | `src/services/settings.rs` seed_runtime_config_defaults_pg (kv runtime-config blob; via startup_reseed) -> staleDispatchedRecoverNullDispatch |
| `runtime.stale_dispatched_recover_missing_dispatch` | seeded-at-boot | `src/services/settings.rs` seed_runtime_config_defaults_pg (kv runtime-config blob; via startup_reseed) -> staleDispatchedRecoverMissingDispatch |
| `runtime.rate_limit_warning_pct` | seeded-at-boot | `src/services/settings.rs` seed_runtime_config_defaults_pg (kv runtime-config blob; via startup_reseed) -> rateLimitWarningPct |
| `runtime.rate_limit_danger_pct` | seeded-at-boot | `src/services/settings.rs` seed_runtime_config_defaults_pg (kv runtime-config blob; via startup_reseed) -> rateLimitDangerPct |
| `runtime.rate_limit_stale_sec` | seeded-at-boot | `src/services/settings.rs` seed_runtime_config_defaults_pg (kv runtime-config blob; via startup_reseed) -> rateLimitStaleSec |
| `runtime.session_context_recent_pairs` | live | `src/services/discord/prompt_builder/channel_recent_context.rs` configured_recent_pairs (current()) |
| `runtime.stream_json_startup_output_timeout_secs` | live | `src/services/stream_json_cli/runner.rs` configured_startup_output_timeout_secs (current()) |
| `runtime.followup_prompt_ready_timeout_secs` | live | `src/services/claude_tui/input.rs` followup_prompt_ready_timeout (current()) |
| `runtime.tui_hook_buffer_ttl_secs` | restart | `src/services/claude_tui/hook_registry.rs` GLOBAL (LazyLock; current_registry_durations on first access) |
| `runtime.tui_unclaimed_stop_delay_ms` | restart | `src/services/claude_tui/hook_registry.rs` GLOBAL (LazyLock; current_registry_durations on first access) |
| `runtime.tui_hook_registry_enabled` | live | `src/services/claude_tui/hook_registry.rs` registry_enabled (current()) |
| `runtime.codex_rollout_index_cache_enabled` | live | `src/services/codex_tui/rollout_index.rs` cache_enabled (current()) |
| `runtime.dispatch_rate_limit_gate_enabled` | seeded-at-boot | `src/services/settings.rs` seed_runtime_config_defaults_pg (kv runtime-config blob; via startup_reseed) -> dispatchRateLimitGateEnabled |
| `runtime.dispatch_rate_limit_gate_danger_pct` | seeded-at-boot | `src/services/settings.rs` seed_runtime_config_defaults_pg (kv runtime-config blob; via startup_reseed) -> dispatchRateLimitGateDangerPct |
| `runtime.herdr_turn_enabled` | live | `src/services/turn_host.rs` herdr_turn_switched_on (current()) |
| `runtime.herdr_codex_turn_enabled` | live | `src/services/turn_host.rs` herdr_turn_switched_on_for (current()) |
| `runtime.herdr_cancel_enabled` | live | `src/services/provider/cancel_token_claude_interrupt.rs` herdr_cancel_enabled (current()) |
| `runtime.herdr_terminate_enabled` | live | `src/services/session_host/herdr_gate.rs` send_close_pinned (current()) |
| `runtime.reset_overrides_on_restart` | restart | `src/services/settings.rs` config_default_seed_actions / seeded_runtime_config_map (consumed only by startup_reseed) |
| `routines.enabled` | restart | `src/server/worker_registry/registry.rs` start_worker (RoutineRuntime gate) |
| `routines.dir` | restart | `src/server/mod.rs` routine_runtime_loop (script_dirs captured; hot-reload pass reuses boot dirs) |
| `routines.additional_dirs` | restart | `src/server/mod.rs` routine_runtime_loop (script_dirs captured) |
| `routines.tick_interval_secs` | restart | `src/server/worker_registry/registry.rs` start_worker (validate_routine_runtime_config -> interval) |
| `routines.max_due_per_tick` | live | `src/server/mod.rs` routine_runtime_loop (per-tick current()) |
| `routines.max_agent_polls_per_tick` | live | `src/server/mod.rs` routine_runtime_loop (per-tick current()) |
| `routines.default_timezone` | restart | `src/server/mod.rs` routine_runtime_loop (RoutineStore::new_with_timezone_and_checkpoint_limit) |
| `routines.agent_timeout_secs` | restart | `src/server/mod.rs` routine_runtime_loop (RoutineAgentExecutor::new) |
| `routines.headless_fresh_agents` | restart | `src/server/mod.rs` routine_runtime_loop (RoutineAgentExecutor::with_headless_fresh_agents) |
| `routines.max_checkpoint_bytes` | restart | `src/server/mod.rs` routine_runtime_loop (RoutineStore) |
| `routines.hot_reload` | live | `src/server/mod.rs` routine_runtime_loop (per-tick current()) |
| `routines.stale_paused_alert_secs` | live | `src/server/mod.rs` routine_runtime_loop (per-tick current()) |
| `routines.stale_paused_alert_ttl_secs` | live | `src/server/mod.rs` routine_runtime_loop (per-tick current()) |
| `routines.failure_pause_auto_resume_secs` | restart | `src/server/routes/routines/handlers.rs` run_routine_now (AppState.config) |
| `routines.startup_grace_secs` | live | `src/server/mod.rs` routine_runtime_loop (after_startup_grace with per-tick value) |
| `routines.max_consecutive_failures` | restart | `src/server/routes/routines/handlers.rs` run_routine_now (AppState.config) |
| `onboarding.guild_id` | restart | `src/services/auto_queue/fsm.rs` AutoQueueActivateDeps::from_state (AppState effective_guild_id) |
| `onboarding.default_categories` | live | `src/services/onboarding/channel.rs` apply_runtime_config_defaults (current() -> OnboardingConfig::resolve_category) |
| `onboarding.provider_suffix_map` | live | `src/services/provider.rs` ProviderKind::from_channel_suffix (current()) |
| `onboarding.provider_options` | live | `src/config.rs` OnboardingConfig::is_empty (no daemon reader) |
| `onboarding.default_provider` | live | `src/services/provider.rs` ProviderKind::default_channel_provider (current()) |
| `memory.backend` | live | `src/runtime_layout/mod.rs` load_memory_backend (load_from_path per call) via services/discord/settings/memory.rs:configured_memory_backend_name |
| `memory.file.sak_path` | live | `src/runtime_layout/mod.rs` load_memory_backend (load_from_path per call) via runtime_layout/mod.rs:shared_agent_knowledge_path |
| `memory.file.sam_path` | live | `src/runtime_layout/mod.rs` memory_backend_from_config (no daemon reader) |
| `memory.file.ltm_root` | live | `src/runtime_layout/mod.rs` load_memory_backend (load_from_path per call) via runtime_layout/mod.rs:long_term_memory_root |
| `memory.file.auto_memory_root` | live | `src/runtime_layout/mod.rs` memory_backend_from_config (no daemon reader) |
| `memory.mcp.endpoint` | live | `src/runtime_layout/mod.rs` load_memory_backend (load_from_path per call) via services/memory/memento.rs:runtime_config |
| `memory.mcp.access_key_env` | live | `src/runtime_layout/mod.rs` load_memory_backend (load_from_path per call) via services/memory/memento.rs:runtime_config |
| `prompt_manifest_retention` | restart | `src/db/prompt_manifests/retention.rs` install_retention_config (OnceLock set from bootstrap::initialize) |
| `tui_o.writer.channels` | restart | `src/services/tui_o/channel_policy.rs` install (BOOT OnceLock; bootstrap::install_boot_snapshots) |
| `tui_o.writer.all_tui` | restart | `src/services/tui_o/channel_policy.rs` install (BOOT OnceLock; bootstrap::install_boot_snapshots) |
| `tui_o.writer.late_adopt` | restart | `src/services/tui_o/channel_policy.rs` install (BOOT OnceLock; bootstrap::install_boot_snapshots) (late_adopt_capable) |
| `tui_o.writer.dormant_channels` | restart | `src/services/tui_o/channel_policy.rs` install (BOOT OnceLock; bootstrap::install_boot_snapshots) |
| `tui_o.tool_cards.enabled` | live | `src/services/tui_o/store/tool_card_switch.rs` global_enabled (current()) |
| `tui_o.turn` | restart | `src/services/discord/tui_direct_pending_start/turn_retirement.rs` confirm_at_boot (turn_mode CONFIRMED set) |
| `tui_o.alert_channel_id` | restart | `src/services/tui_o/alarm.rs` AlarmRouter::for_process (runtime_bootstrap/o_writer_host.rs:parts at gateway boot) |
| `session_hosts` | restart | `src/config/session_hosts.rs` install_selected (BOOT OnceLock; bootstrap::install_boot_snapshots) |
| `config_hot_reload` | restart | `src/bootstrap.rs` prepare_dcserver_boot + `src/launch.rs` after_database_ready (watcher guard owned by bot/server lifetime) |

## Files outside `agentdesk.yaml`

| File | Class | Read or captured by |
|---|---|---|
| `policies/default-pipeline.yaml` | restart | `src/pipeline.rs` PIPELINE (OnceLock loaded by handle_dcserver; the policy watcher reloads only *.js) |
