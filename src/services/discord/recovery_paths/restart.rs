//! Restart-path helpers (issue #1074 landing zone; first occupant: #3293).
//!
//! Hosts the side-effecting epilogue for the boot-time recovery branches in
//! `recovery_engine::restore_inflight_turns` whose terminal relay to Discord
//! did NOT deliver. The pure decision matrix lives in [`super::shared`]; this
//! module executes the chosen [`RowDisposition`] with the no-silent-delete
//! guarantees from the #3293 design: every force-clear writes an on-disk
//! force-clear report (full response text + row metadata, see
//! [`persist_force_clear_report`]) + a termination audit + a structured WARN,
//! and the preserve path persists the attempt counter through a
//! restart-marker-preserving identity-guarded bump.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use poise::serenity_prelude as serenity;
use poise::serenity_prelude::ChannelId;
use serde::{Deserialize, Serialize};

use super::super::recovery_engine::{finish_recovered_turn_mailbox, save_missing_session_handoff};
use super::super::runtime_store::{atomic_write, discord_recovery_force_clear_root};
use super::super::{SharedData, inflight};
use super::shared::{
    ChannelProbeVerdict, RecoveryRelayOutcome, RowDisposition, classify_channel_probe_status,
    disposition_reason_code, unrecoverable_relay_disposition,
};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ChannelMailboxRegistry;

/// #3297 finding 2: active channel-liveness probe used as the second opinion
/// after a transient-looking relay failure. A direct `get_channel` bypasses
/// the String-flattened formatting error chain entirely: 404/403/410 on the
/// CHANNEL itself is an authoritative "gone" verdict; everything else
/// (success, 5xx, 429, transport) stays inconclusive so the conservative
/// transient classification survives.
pub(in crate::services::discord) async fn probe_channel_liveness(
    http: &serenity::Http,
    channel_id: ChannelId,
) -> ChannelProbeVerdict {
    match http.get_channel(channel_id).await {
        Ok(_) => ChannelProbeVerdict::Inconclusive,
        Err(serenity::Error::Http(http_err)) => {
            classify_channel_probe_status(http_err.status_code().map(|status| status.as_u16()))
        }
        Err(_) => ChannelProbeVerdict::Inconclusive,
    }
}

/// #3293 (c): finish the recovered turn's mailbox ONLY when a registry entry
/// already exists. `finish_recovered_turn_mailbox` routes through the turn
/// finalizer, whose channel-scoped resolution mints a mailbox actor on first
/// touch — on a force-clear of a row for a non-existent (bogus) channel that
/// would re-pollute the registry with a permanent entry. Peek, never create.
pub(in crate::services::discord) async fn finish_recovered_turn_mailbox_if_registered(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    stop_source: &'static str,
) {
    let registered = shared.mailbox_peek(channel_id).is_some()
        || ChannelMailboxRegistry::global_handle(channel_id).is_some();
    if !registered {
        tracing::debug!(
            provider = %provider.as_str(),
            channel = channel_id.get(),
            stop_source,
            "recovery force-clear: no mailbox registry entry — skipping finish to avoid creating one"
        );
        return;
    }
    finish_recovered_turn_mailbox(shared, provider, channel_id, stop_source).await;
}

/// #3293: shared epilogue for the five recovery notice branches. Computes the
/// [`RowDisposition`] from the relay `outcome` and executes it:
///
/// * `FinishAndClear` (delivered) — the branch's historical epilogue:
///   `finish_recovered_turn_mailbox(finish_stop_source)` + clear.
/// * everything else — [`apply_undeliverable_relay_disposition`].
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn dispose_recovery_relay_outcome(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    outcome: RecoveryRelayOutcome,
    tmux_alive: bool,
    finish_stop_source: &'static str,
    branch: &'static str,
    best_response: &str,
    handoff_already_saved: bool,
) {
    match unrecoverable_relay_disposition(
        outcome,
        state.recovery_relay_attempts,
        inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
        tmux_alive,
    ) {
        RowDisposition::FinishAndClear => {
            finish_recovered_turn_mailbox(
                shared,
                provider,
                ChannelId::new(state.channel_id),
                finish_stop_source,
            )
            .await;
            inflight::clear_inflight_state(provider, state.channel_id);
        }
        disposition => {
            apply_undeliverable_relay_disposition(
                shared,
                provider,
                state,
                disposition,
                branch,
                tmux_alive,
                best_response,
                handoff_already_saved,
            )
            .await;
        }
    }
}

/// Execute a non-`Delivered` [`RowDisposition`] for a recovery branch.
///
/// * `ClearPermanent` / `ClearBudgetExhausted` — persist the on-disk
///   force-clear report (the full response text + row metadata; a write
///   failure WARNs but never blocks the clear), termination audit (when the
///   row carries a `session_key`), the legacy missing-session handoff WARN
///   (unless the branch already emitted one), a structured WARN that ALWAYS
///   fires, then finish the mailbox (existing entries only) and clear the
///   inflight row.
/// * `PreserveAndCount` — persist `recovery_relay_attempts + 1` through
///   [`inflight::budget::bump_recovery_relay_attempts_if_matches_identity`],
///   which preserves restart/rebind markers on the carrier row (the #3297
///   finding-1 fix) while still refusing rows owned by a different turn, and
///   WARN with the attempt budget so the loop is observable.
/// * `FinishAndClear` is the caller's delivered epilogue — a no-op here.
#[allow(clippy::too_many_arguments)]
async fn apply_undeliverable_relay_disposition(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    disposition: RowDisposition,
    branch: &'static str,
    tmux_alive: bool,
    best_response: &str,
    handoff_already_saved: bool,
) {
    match disposition {
        RowDisposition::FinishAndClear => {}
        RowDisposition::ClearPermanent | RowDisposition::ClearBudgetExhausted => {
            let reason_code = disposition_reason_code(disposition)
                .expect("clearing dispositions always carry a reason code");
            // #3297 finding 3: preserve the full response + row metadata on
            // disk BEFORE the row is destroyed. Failure to write must not
            // block the clear (the loop must still terminate) but is WARNed.
            let report = RecoveryForceClearReport::from_state(
                provider,
                state,
                branch,
                reason_code,
                best_response,
            );
            let report_path = match persist_force_clear_report(&report) {
                Ok(path) => Some(path),
                Err(error) => {
                    tracing::warn!(
                        provider = %provider.as_str(),
                        channel = state.channel_id,
                        reason_code,
                        error = %error,
                        "recovery force-clear report write FAILED — clearing anyway; \
                         full response text is NOT preserved on disk for this row"
                    );
                    None
                }
            };
            if let Some(ref session_key) = state.session_key {
                crate::services::termination_audit::record_termination_with_handles(
                    None::<&crate::db::Db>,
                    shared.pg_pool.as_ref(),
                    session_key,
                    state.dispatch_id.as_deref(),
                    "recovery",
                    reason_code,
                    Some("recovery terminal relay unrecoverable; inflight force-cleared"),
                    None,
                    Some(state.last_offset),
                    Some(tmux_alive),
                );
            }
            if !handoff_already_saved {
                save_missing_session_handoff(provider, state, best_response);
            }
            let report_path_display = report_path.as_ref().map(|path| path.display().to_string());
            tracing::warn!(
                provider = %provider.as_str(),
                channel = state.channel_id,
                user_msg_id = state.user_msg_id,
                branch,
                reason_code,
                attempts = state.recovery_relay_attempts,
                budget = inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
                report_path = report_path_display.as_deref(),
                "recovery relay unrecoverable — force-clearing inflight row"
            );
            finish_recovered_turn_mailbox_if_registered(
                shared,
                provider,
                ChannelId::new(state.channel_id),
                reason_code,
            )
            .await;
            inflight::clear_inflight_state(provider, state.channel_id);
        }
        RowDisposition::PreserveAndCount => {
            let identity = inflight::InflightTurnIdentity::from_state(state);
            // #3297 finding 1: the carrier row is restart-marked on every
            // boot, so the counter must persist through a bump that PRESERVES
            // the marker instead of the generic guarded save that refuses it.
            let save_outcome = inflight::budget::bump_recovery_relay_attempts_if_matches_identity(
                provider,
                state.channel_id,
                &identity,
                state.turn_start_offset,
            );
            tracing::warn!(
                provider = %provider.as_str(),
                channel = state.channel_id,
                branch,
                attempts = state.recovery_relay_attempts.saturating_add(1),
                budget = inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
                counter_persisted = matches!(save_outcome, inflight::GuardedSaveOutcome::Saved),
                "recovery relay failed — preserving inflight for retry on next restart"
            );
        }
    }
}

const FORCE_CLEAR_REPORT_VERSION: u32 = 1;

/// #3297 finding 3: the durable artifact a recovery force-clear leaves
/// behind. Written to `runtime/discord_recovery_force_clear/<provider>/`
/// immediately before the inflight row is destroyed, so the full assistant
/// response and enough row metadata to re-deliver it by hand survive even a
/// misclassified clear (e.g. a transient 403 surge read as permanent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(in crate::services::discord) struct RecoveryForceClearReport {
    pub version: u32,
    pub provider: String,
    pub channel_id: u64,
    pub user_msg_id: u64,
    pub current_msg_id: u64,
    pub session_key: Option<String>,
    pub dispatch_id: Option<String>,
    pub tmux_session_name: Option<String>,
    pub branch: String,
    pub reason_code: String,
    pub attempts: u32,
    pub budget: u32,
    pub started_at: String,
    pub cleared_at: String,
    /// The FULL recovered response text (best of extracted output and the
    /// row's accumulated `full_response`) — never truncated.
    pub full_response: String,
}

impl RecoveryForceClearReport {
    fn from_state(
        provider: &ProviderKind,
        state: &inflight::InflightTurnState,
        branch: &str,
        reason_code: &str,
        best_response: &str,
    ) -> Self {
        Self {
            version: FORCE_CLEAR_REPORT_VERSION,
            provider: provider.as_str().to_string(),
            channel_id: state.channel_id,
            user_msg_id: state.user_msg_id,
            current_msg_id: state.current_msg_id,
            session_key: state.session_key.clone(),
            dispatch_id: state.dispatch_id.clone(),
            tmux_session_name: state.tmux_session_name.clone(),
            branch: branch.to_string(),
            reason_code: reason_code.to_string(),
            attempts: state.recovery_relay_attempts,
            budget: inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
            started_at: state.started_at.clone(),
            cleared_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            full_response: best_response.to_string(),
        }
    }
}

/// Persist a [`RecoveryForceClearReport`] under the process runtime root.
/// File names carry a nanosecond suffix so repeated clears of the same
/// channel never overwrite an earlier artifact.
fn persist_force_clear_report(report: &RecoveryForceClearReport) -> Result<PathBuf, String> {
    let Some(root) = discord_recovery_force_clear_root() else {
        return Err("runtime root not resolvable".to_string());
    };
    persist_force_clear_report_in_root(&root, report)
}

/// Root-explicit inner form for unit tests.
fn persist_force_clear_report_in_root(
    root: &Path,
    report: &RecoveryForceClearReport,
) -> Result<PathBuf, String> {
    let dir = root.join(&report.provider);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!(
        "{}-{}-{nanos}.json",
        report.channel_id, report.reason_code
    ));
    let json = serde_json::to_string_pretty(report).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    //! #3297 finding-3 red-green: a force-clear must leave a real on-disk
    //! artifact carrying the FULL response text (the prior implementation
    //! only WARN-logged a 160-char tail and wrote nothing).
    use tempfile::TempDir;

    use super::super::super::inflight::InflightTurnState;
    use super::{RecoveryForceClearReport, persist_force_clear_report_in_root};
    use crate::services::provider::ProviderKind;

    fn make_state(channel_id: u64) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-cdx".to_string()),
            7,
            42,
            43,
            "hello".to_string(),
            Some("session-3293".to_string()),
            Some(format!("AgentDesk-codex-adk-cdx-{channel_id}")),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.session_key = Some("sk-3293".to_string());
        state.dispatch_id = Some("disp-3293".to_string());
        state.recovery_relay_attempts = 2;
        state
    }

    #[test]
    fn force_clear_report_preserves_full_response_and_metadata_on_disk() {
        let temp = TempDir::new().unwrap();
        let state = make_state(932_951);
        // Long enough that the legacy 160-char WARN tail provably truncates.
        let full_response = "A".repeat(4000) + " — final answer body";
        let report = RecoveryForceClearReport::from_state(
            &ProviderKind::Codex,
            &state,
            "missing_tmux",
            "recovery_retry_budget_exhausted",
            &full_response,
        );

        let path = persist_force_clear_report_in_root(temp.path(), &report)
            .expect("force-clear report must be written to disk");
        assert!(path.starts_with(temp.path().join("codex")));

        let written = std::fs::read_to_string(&path).expect("artifact must exist on disk");
        let parsed: RecoveryForceClearReport =
            serde_json::from_str(&written).expect("artifact must round-trip");
        assert_eq!(
            parsed.full_response, full_response,
            "FULL text, no truncation"
        );
        assert_eq!(parsed.channel_id, 932_951);
        assert_eq!(parsed.session_key.as_deref(), Some("sk-3293"));
        assert_eq!(parsed.dispatch_id.as_deref(), Some("disp-3293"));
        assert_eq!(parsed.reason_code, "recovery_retry_budget_exhausted");
        assert_eq!(parsed.branch, "missing_tmux");
        assert_eq!(parsed.attempts, 2);
        assert_eq!(
            parsed.tmux_session_name.as_deref(),
            Some("AgentDesk-codex-adk-cdx-932951")
        );
    }

    #[test]
    fn repeated_force_clears_never_overwrite_earlier_artifacts() {
        let temp = TempDir::new().unwrap();
        let state = make_state(932_952);
        let first = RecoveryForceClearReport::from_state(
            &ProviderKind::Codex,
            &state,
            "missing_tmux",
            "recovery_permanent_relay_failure",
            "first response",
        );
        let second = RecoveryForceClearReport::from_state(
            &ProviderKind::Codex,
            &state,
            "missing_tmux",
            "recovery_permanent_relay_failure",
            "second response",
        );
        let path_a = persist_force_clear_report_in_root(temp.path(), &first).unwrap();
        let path_b = persist_force_clear_report_in_root(temp.path(), &second).unwrap();
        assert_ne!(path_a, path_b, "artifacts must be uniquely named");
        assert!(path_a.exists() && path_b.exists());
    }
}
