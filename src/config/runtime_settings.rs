//! Runtime-adjustable operational settings and their default/empty contract.
use super::*;

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct RuntimeSettingsConfig {
    #[serde(default, skip_serializing_if = "is_legacy_delivery_journal_mode")]
    pub delivery_journal_mode: DeliveryJournalMode,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub delivery_journal_cohort_percent: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delivery_journal_internal_channel_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "is_off_intake_delivery_settlement")]
    pub intake_delivery_settlement: IntakeDeliverySettlementStage,
    #[serde(default, skip_serializing_if = "is_legacy_execution_identity_mode")]
    pub execution_identity_mode: ExecutionIdentityMode,
    #[serde(default, skip_serializing_if = "is_legacy_publication_permit_mode")]
    pub publication_permit_mode: PublicationPermitMode,
    #[serde(default, skip_serializing_if = "is_structural_relay_verdict_source")]
    pub relay_verdict_source: RelayVerdictSource,
    /// #5464 T5 S1: rollout stage for the AC2-R relay-authority warrant.
    #[serde(default, skip_serializing_if = "is_legacy_relay_authority_mode")]
    pub relay_authority_mode: RelayAuthorityMode,
    /// #5464 T5 S1: percentage of channels admitted to the relay-authority
    /// cohort. `0` (the shipped default) admits none and `100` admits all;
    /// larger values clamp at the admission site rather than here, so a typo
    /// widens the cohort to everyone instead of wrapping to a narrow one.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub relay_authority_cohort_percent: u8,
    /// Heartbeat-absence TTL for stale dispatched debt; unset defaults to 1800 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intake_delivery_sweep_dispatched_cutoff_secs: Option<u64>,
    /// Heartbeat-absence TTL for stale spawned debt; defaults to 1800s because queued forwarding can stay spawned for a full turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intake_delivery_sweep_spawned_cutoff_secs: Option<u64>,
    /// Per-state sweep batch limit; unset defaults to 200 and values clamp to 1..=500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intake_delivery_sweep_batch_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_timeout_min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_progress_stale_min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_turn_alert_interval_min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_compact_percent: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_compact_percent_codex: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_compact_percent_claude: Option<u64>,
    /// YAML-only absolute window for new Claude TUI launches, independent of model.
    /// Unset exports nothing (#5935); a set value clamps to 100_000..=1_000_000 at launch.
    /// Raw numeric values are preserved here; zero clamps to the minimum, not off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_compact_window_claude: Option<u64>,
    /// Provider-neutral minimum token occupancy for requesting context compaction.
    /// Unset uses the live consumer default (currently 300_000 tokens for Claude).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_compact_lower_bound_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_poll_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_sync_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_issue_sync_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_rate_limit_poll_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_rate_limit_poll_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_triage_poll_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceo_warn_depth: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entry_retries: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_dispatched_grace_min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_dispatched_terminal_statuses: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_dispatched_recover_null_dispatch: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_dispatched_recover_missing_dispatch: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_reminder_min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_warning_pct: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_danger_pct: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_repo_cache_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_stale_sec: Option<u64>,
    /// Number of completed user/assistant pairs from the same Discord channel
    /// added as background context when a fresh provider session starts.
    /// Unset defaults to 3, `0` disables the layer, and values are clamped to 10.
    /// Read live for each turn through `config_live_reload::current()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_context_recent_pairs: Option<u64>,
    /// Seconds StreamJson CLIs (Grok, AGY) may stay silent before the first
    /// non-empty stdout line. Unset or zero keeps the compiled-in 60s default.
    /// Read live via `config_live_reload::current()` on each launch; no restart needed.
    /// Clamped to 24h. A caller that passes a zero timeout still uses the
    /// separate 90s unset handshake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_json_startup_output_timeout_secs: Option<u64>,
    /// Follow-up TUI readiness timeout in seconds; unset or zero uses the Claude
    /// and Codex default of 45s (`FOLLOWUP_PROMPT_READY_TIMEOUT`).
    /// Read live via `config_live_reload::current()` each wait; no restart needed.
    ///
    /// Claude's wait also has an independent 900s busy-turn ceiling
    /// (`PROMPT_READY_ACTIVE_TURN_WAIT_CEILING`). Codex has no such ceiling:
    /// a long prior turn can block its follow-up for the full configured duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub followup_prompt_ready_timeout_secs: Option<u64>,
    /// Master rollback flag for the read-only DB active-session mismatch audit
    /// surfaced on `/api/health/detail` (`active_session_audit` block). When
    /// unset it defaults to ON; `Some(false)` makes the audit report
    /// `enabled:false` with empty candidates and skips the DB query entirely.
    /// Read live via `config_live_reload::current()` so an `agentdesk.yaml` edit
    /// applies on the next `/api/health/detail` call without a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_audit_enabled: Option<bool>,
    /// Minimum seconds since `last_heartbeat` before a raw-active session can be
    /// flagged by the active-session mismatch audit (post-restart/long-turn
    /// grace). Unset (or `0`) falls back to the compiled-in 120s default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_audit_stale_secs: Option<u64>,
    /// Hard cap on audit candidate rows AND the SQL `LIMIT`. Unset falls back to
    /// the compiled-in 50 default; clamped to `1..=500` when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_audit_max_candidates: Option<u64>,
    /// TTL (seconds) for the in-memory TUI hook registry buffer. A hook that has
    /// been buffered longer than this is swept and never replayed to a claiming
    /// listener, so a stale Stop from a previous turn cannot wake a fresh turn.
    /// When unset (or `0`) the compiled-in 30s default
    /// (`hook_registry::DEFAULT_HOOK_BUFFER_TTL`) is used.
    ///
    /// NOT hot-reloadable: this value is captured ONCE when the process-global
    /// `hook_registry::GLOBAL` is first accessed (effectively at process start)
    /// and stored on the immutable `HookRegistry.ttl`. Editing it in
    /// `agentdesk.yaml` takes effect only on the next process start (restart
    /// required). Only `tui_hook_registry_enabled` is read live per-hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui_hook_buffer_ttl_secs: Option<u64>,
    /// Diagnostic delay (milliseconds) before an unclaimed Stop in the TUI hook
    /// registry is considered "elapsed". Diagnostic-only in P0 — it never
    /// triggers a transcript sync or finalization. When unset (or `0`) the
    /// compiled-in 2000ms default (`hook_registry::DEFAULT_UNCLAIMED_STOP_DELAY`)
    /// is used.
    ///
    /// NOT hot-reloadable: like `tui_hook_buffer_ttl_secs`, this is captured ONCE
    /// when `hook_registry::GLOBAL` is first accessed (process start) and stored
    /// on the immutable `HookRegistry.unclaimed_stop_delay`. Editing it in
    /// `agentdesk.yaml` takes effect only on the next process start (restart
    /// required). Only `tui_hook_registry_enabled` is genuinely hot-reloadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui_unclaimed_stop_delay_ms: Option<u64>,
    /// Rollback switch for the TUI hook registry buffering layer. Defaults to ON
    /// (`None` => enabled). Set to `false` in `agentdesk.yaml` to stop feeding
    /// the registry from the hook receiver, leaving the legacy broadcast +
    /// polling path exactly as before. Genuinely hot-reloadable (no restart
    /// required): `registry_enabled()` reads it live per-hook. This is the ONLY
    /// live-reloadable key of the three TUI hook registry settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui_hook_registry_enabled: Option<bool>,
    /// Enable the in-process Codex rollout discovery index cache used by the
    /// Codex TUI resume / follow-up readiness paths
    /// (`codex_tui::rollout_index`). The cache avoids re-walking
    /// `~/.codex/sessions` and re-reading rollout headers on every lookup.
    ///
    /// Defaults ON when unset (`None`). Set to `false` to force the legacy
    /// per-lookup recursive scan + header read — the built-in rollback for the
    /// `codex-rollout-index-cache` feature. Read live via
    /// `config_live_reload::current()` so an `agentdesk.yaml` edit applies on the
    /// next lookup without a restart; not part of the restart-required set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_rollout_index_cache_enabled: Option<bool>,
    /// Rate-limit-aware dispatch gate toggle (feature:
    /// rate-limit-aware-dispatch-gate). When `Some(true)` or unset (`None` —
    /// the safe default is ON), auto-queue activation defers a pending entry
    /// whose target provider is at/above `rate_limit_danger_pct` utilization in
    /// the live in-memory rate-limit snapshot, instead of creating a doomed
    /// dispatch. The entry stays `pending` (never `skipped`) and resumes
    /// automatically once pressure clears. Read live via
    /// `config_live_reload::current()` so an `agentdesk.yaml` edit applies on
    /// the next activation without a restart. Set to `false` to disable the
    /// gate cleanly (every activation then falls through to normal dispatch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_rate_limit_gate_enabled: Option<bool>,
    /// Gate-specific danger threshold (utilization %) for the rate-limit-aware
    /// dispatch gate (feature: rate-limit-aware-dispatch-gate). This is a
    /// SEPARATE knob from `rate_limit_danger_pct` (which drives the dashboard's
    /// "danger" coloring at 95): the operator wants the dispatch gate to defer
    /// ONLY when a provider is fully rate-limited (utilization at/above 100),
    /// so the gate defaults to 100 here and never touches `rate_limit_danger_pct`
    /// — other consumers of `rate_limit_danger_pct` are unaffected. When unset
    /// (`None`), the gate uses the compiled-in default of 100. Read live (via
    /// the persisted runtime-config / `config_live_reload::current()`) so an
    /// edit applies on the next activation without a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_rate_limit_gate_danger_pct: Option<u8>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reset_overrides_on_restart: bool,
}

impl RuntimeSettingsConfig {
    pub fn is_empty(&self) -> bool {
        self.delivery_journal_mode == DeliveryJournalMode::Legacy
            && self.delivery_journal_cohort_percent == 0
            && self.delivery_journal_internal_channel_ids.is_empty()
            && self.intake_delivery_settlement == IntakeDeliverySettlementStage::Off
            && self.execution_identity_mode == ExecutionIdentityMode::Legacy
            && self.publication_permit_mode == PublicationPermitMode::Legacy
            && self.relay_verdict_source == RelayVerdictSource::Structural
            && self.relay_authority_mode == RelayAuthorityMode::Legacy
            && self.relay_authority_cohort_percent == 0
            && self.intake_delivery_sweep_dispatched_cutoff_secs.is_none()
            && self.intake_delivery_sweep_spawned_cutoff_secs.is_none()
            && self.intake_delivery_sweep_batch_limit.is_none()
            && self.requested_timeout_min.is_none()
            && self.in_progress_stale_min.is_none()
            && self.long_turn_alert_interval_min.is_none()
            && self.context_compact_percent.is_none()
            && self.context_compact_percent_codex.is_none()
            && self.context_compact_percent_claude.is_none()
            && self.context_compact_window_claude.is_none()
            && self.context_compact_lower_bound_tokens.is_none()
            && self.dispatch_poll_sec.is_none()
            && self.agent_sync_sec.is_none()
            && self.github_issue_sync_sec.is_none()
            && self.claude_rate_limit_poll_sec.is_none()
            && self.codex_rate_limit_poll_sec.is_none()
            && self.issue_triage_poll_sec.is_none()
            && self.ceo_warn_depth.is_none()
            && self.max_retries.is_none()
            && self.max_entry_retries.is_none()
            && self.stale_dispatched_grace_min.is_none()
            && self.stale_dispatched_terminal_statuses.is_none()
            && self.stale_dispatched_recover_null_dispatch.is_none()
            && self.stale_dispatched_recover_missing_dispatch.is_none()
            && self.review_reminder_min.is_none()
            && self.rate_limit_warning_pct.is_none()
            && self.rate_limit_danger_pct.is_none()
            && self.github_repo_cache_sec.is_none()
            && self.rate_limit_stale_sec.is_none()
            && self.session_context_recent_pairs.is_none()
            && self.stream_json_startup_output_timeout_secs.is_none()
            && self.followup_prompt_ready_timeout_secs.is_none()
            && self.active_session_audit_enabled.is_none()
            && self.active_session_audit_stale_secs.is_none()
            && self.active_session_audit_max_candidates.is_none()
            && self.tui_hook_buffer_ttl_secs.is_none()
            && self.tui_unclaimed_stop_delay_ms.is_none()
            && self.tui_hook_registry_enabled.is_none()
            && self.codex_rollout_index_cache_enabled.is_none()
            && self.dispatch_rate_limit_gate_enabled.is_none()
            && self.dispatch_rate_limit_gate_danger_pct.is_none()
            && !self.reset_overrides_on_restart
    }

    pub(crate) fn intake_delivery_sweep_settings(&self) -> (u64, u64, i64) {
        (
            self.intake_delivery_sweep_dispatched_cutoff_secs
                .unwrap_or(1800)
                .min(MAX_INTAKE_SWEEP_CUTOFF_SECS),
            self.intake_delivery_sweep_spawned_cutoff_secs
                .unwrap_or(1800)
                .min(MAX_INTAKE_SWEEP_CUTOFF_SECS),
            self.intake_delivery_sweep_batch_limit
                .unwrap_or(200)
                .clamp(1, 500) as i64,
        )
    }
}
