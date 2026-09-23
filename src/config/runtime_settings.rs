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
    /// Channel cohort percentage: 0 admits none, 100 admits all.
    /// Values above 100 clamp at admission rather than wrapping.
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
    /// YAML-only Claude TUI window. Unset exports nothing; all set values,
    /// including zero, clamp to 100_000..=1_000_000 at launch.
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
    /// Completed same-channel pairs supplied to fresh sessions.
    /// Read live per turn; defaults to 3, zero disables, maximum 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_context_recent_pairs: Option<u64>,
    /// First non-empty stdout deadline for Grok/AGY, read live per launch.
    /// Unset/zero uses 60s; maximum 24h. A caller-supplied zero timeout
    /// still uses the separate 90s unset handshake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_json_startup_output_timeout_secs: Option<u64>,
    /// Live follow-up readiness timeout; unset/zero defaults to 45s.
    /// Claude additionally caps busy-turn waits at 900s; Codex does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub followup_prompt_ready_timeout_secs: Option<u64>,
    /// Live read-only DB mismatch audit switch; defaults on.
    /// False skips the query and reports disabled with no candidates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_audit_enabled: Option<bool>,
    /// Heartbeat grace before mismatch auditing; unset/zero uses 120s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_audit_stale_secs: Option<u64>,
    /// Audit row and SQL LIMIT cap; unset uses 50, otherwise clamps to 1..=500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_audit_max_candidates: Option<u64>,
    /// Buffered hook TTL; unset/zero uses 30s. Expired hooks are not replayed.
    /// Captured on first GLOBAL access; changes require a process restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui_hook_buffer_ttl_secs: Option<u64>,
    /// Diagnostic-only unclaimed Stop delay; unset/zero uses 2000ms.
    /// Does not sync/finalize. Captured on first GLOBAL access; restart required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui_unclaimed_stop_delay_ms: Option<u64>,
    /// Live per-hook buffering switch; defaults on. False preserves the
    /// legacy broadcast/polling path. Unlike TTL/delay, no restart is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui_hook_registry_enabled: Option<bool>,
    /// Live Codex rollout-index cache switch; defaults on. False restores
    /// recursive scanning and header reads on every resume/follow-up lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_rollout_index_cache_enabled: Option<bool>,
    /// Live dispatch pressure gate; defaults on. Entries remain pending
    /// until provider utilization clears the threshold. False bypasses the gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_rate_limit_gate_enabled: Option<bool>,
    /// Live dispatch-only utilization threshold; defaults to 100.
    /// Independent of rate_limit_danger_pct used by dashboard coloring.
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
