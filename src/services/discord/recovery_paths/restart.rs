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

/// #3610 PR-2 (stale-anchor range guard, pure/testable): does the durable anchor's
/// recorded JSONL `range` belong to THIS recovered turn?
///
/// The anchor reader ([`delivery_record::current_generation_delivered_anchor`])
/// already enforces the #1270 GENERATION gate — a stale prior-generation frontier
/// (e.g. a same-named tmux respawn) is rejected before we get here. This adds the
/// per-turn TEMPORAL check on top: the frontier is channel-scoped and holds the
/// LATEST commit, so for it to be THIS turn's anchor its recorded `range` must be
/// (a) a real non-empty slice (`end > start`) and (b) reach at least as far as the
/// turn's persisted `last_offset` — the committed terminal answer covered the
/// output the row last saw. `turn_start_offset` (when present) must not sit ABOVE
/// the anchor's end, which would mean the anchor predates this turn's slice.
///
/// Conservative on missing data: an absent `last_offset` signal cannot happen
/// (it is a plain `u64`), but a `range_end == 0` / empty range fails (a), so a
/// blank/sentinel frontier never reposts.
fn anchor_range_matches_turn(
    anchor_range: (u64, u64),
    turn_start_offset: Option<u64>,
    last_offset: u64,
) -> bool {
    let (start, end) = anchor_range;
    // (a) real non-empty slice.
    if end <= start {
        return false;
    }
    // (b) the commit reached the output the row last observed for this turn.
    if end < last_offset {
        return false;
    }
    // (c) the turn's own start must not be beyond the anchor's end (a later turn).
    if let Some(turn_start) = turn_start_offset {
        if turn_start > end {
            return false;
        }
    }
    true
}

/// #3610 PR-2 (pure/testable): given a placeholder probe of the durable anchor,
/// should the recovery fallback REPOST the terminal text? ONLY when the anchored
/// message is permanently GONE (404/403/410). Every other verdict is a no-op:
/// the message is still there (`StillPlaceholder` / `AlreadyDelivered` — reposting
/// would DUPLICATE a live message) or the probe was inconclusive (`ProbeFailed` —
/// a transient GET error must never be read as "gone").
fn anchor_probe_should_repost(probe: super::super::placeholder_sweeper::PlaceholderProbe) -> bool {
    matches!(
        probe,
        super::super::placeholder_sweeper::PlaceholderProbe::MessageGone
    )
}

/// #3610 PR-2: anchor-based recovery repost fallback (the #3607 "committed, then
/// the message disappeared" backstop). Flag-gated DARK (default OFF) — when OFF
/// this returns `None` BEFORE any record read / probe / relay, so the recovery
/// loop is a byte-for-byte no-op.
///
/// Called ONLY from the committed branch of `restore_inflight_turns`
/// (`recovery_terminal_delivery_already_committed(&state)` true) — the anchor is
/// recorded ONLY on a committed delivery (PR-1~1d's `is_delivered` gate), so a
/// committed row's current-generation anchor is THIS turn's, never a stale one.
///
/// FIVE guards, each a distinct duplicate-repost defense:
/// * **G1** — flag OFF → `None` (outermost; dark-deploy no-op).
/// * **G2** — no trustworthy anchor → `None`. The reader enforces the #1270
///   generation gate AND a populated non-zero `(panel_msg_id, panel_channel_id)`;
///   we additionally reject an EMPTY `terminal_text` (no blank repost) and a
///   `range` that does not match this turn ([`anchor_range_matches_turn`]).
/// * **G3** — probe the anchor: repost ONLY on `MessageGone` (404/403/410). A live
///   message (`StillPlaceholder` / `AlreadyDelivered`) or a transient
///   `ProbeFailed` → `None` (never duplicate a live or unverified message).
/// * **G4** — relay as a NEW message: the anchor is gone so it cannot be edited;
///   we pass `placeholder = None`, which routes through `send_long_message_raw`
///   (NOT an edit). Returns `Some(outcome)` for the caller to `dispose_*`.
/// * **G5 (passive)** — after a `Delivered` repost the caller's `dispose_*` clears
///   the row; the watcher cannot re-relay this range because it sits within the
///   committed floor (`committed_floor_for_resend_dedup` ≥ this generation's
///   `delivered_frontier.end`, and `range_already_committed` suppresses it). We
///   read the anchor only — no offset is written — so the dedup math is unchanged.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn try_recover_anchor_repost(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    terminal_text: &str,
) -> Option<RecoveryRelayOutcome> {
    use super::super::outbound::delivery_record;
    use super::super::placeholder_sweeper;

    // G1: flag OFF → no-op (outermost guard; dark deploy is byte-identical).
    if !super::shared::recovery_anchor_repost_enabled() {
        return None;
    }

    // G2a: never repost a blank body.
    if terminal_text.trim().is_empty() {
        return None;
    }

    // G2b: resolve the current-generation, fully-populated anchor (the reader is
    // the stale-anchor structural guard: generation-gated + non-zero pair).
    //
    // KNOWN LIMITATION (codex r2 Issue-1) — same-channel coverage only.
    // The delivery record is FILE-KEYED by the offset-authority channel
    // (`watcher_owner_channel_id`; delivery_record.rs `DeliveredCommit` doc), but
    // here we can only key by `state.channel_id`, the DELIVERY channel. For the
    // common case these are identical (sink / watcher-owned turns set
    // `watcher_owner_channel_id == delivery_channel_id == channel_id`; the bulk of
    // the watcher-owned recovery population), so this covers them.
    //
    // The bridge-reused-watcher CROSS-channel case (owner ≠ delivery — a recovered
    // bridge edits its own dispatch channel while leasing on a DIFFERENT resolved
    // owner channel; terminal_controller_cutover `Channel split`) is NOT covered:
    // its record lives under the owner-channel file, so this owner-blind read
    // returns `None` and the caller falls through to the byte-identical legacy
    // finish+clear (a coverage GAP — a missed repost — NOT a mis-repost; current
    // behavior is preserved). We deliberately do NOT guess the owner key:
    // `InflightTurnState` carries no `watcher_owner_channel_id` (its only non-
    // delivery channel field, `logical_channel_id`, is the THREAD→parent mapping,
    // a different axis — keying on it would read the WRONG record and risk
    // duplicating a live message). Extending coverage requires persisting the
    // owner channel on the inflight row (or an owner→delivery index); until then
    // cross-channel rows stay a no-op here.
    let channel = ChannelId::new(state.channel_id);
    let tmux_session_name = state.tmux_session_name.as_deref().unwrap_or("");
    let anchor =
        delivery_record::current_generation_delivered_anchor(provider, channel, tmux_session_name)?;

    // G2c: the anchor's recorded range must belong to THIS recovered turn.
    if !anchor_range_matches_turn(anchor.range, state.turn_start_offset, state.last_offset) {
        tracing::warn!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            anchor_range = ?anchor.range,
            turn_start_offset = ?state.turn_start_offset,
            last_offset = state.last_offset,
            "  ✗ recovery anchor-repost: anchor range does not match this turn — no-op (stale-anchor guard)"
        );
        return None;
    }

    // G3: probe the anchored message; repost ONLY when it is permanently gone.
    let probe = placeholder_sweeper::probe_placeholder_state(
        http,
        anchor.panel_channel_id,
        anchor.panel_msg_id,
    )
    .await;
    if !anchor_probe_should_repost(probe) {
        tracing::info!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            anchor_msg_id = anchor.panel_msg_id,
            anchor_channel_id = anchor.panel_channel_id,
            ?probe,
            "  · recovery anchor-repost: anchor probe is not MessageGone — no-op"
        );
        return None;
    }

    // G4: the anchor is gone → send a NEW message (placeholder = None → send-new,
    // NOT an edit). Repost into the channel the anchor lived in.
    tracing::warn!(
        provider = %provider.as_str(),
        channel = state.channel_id,
        anchor_msg_id = anchor.panel_msg_id,
        anchor_channel_id = anchor.panel_channel_id,
        "  ↻ recovery anchor-repost: committed terminal message is GONE — reposting (send-new)"
    );
    let outcome = super::super::recovery_engine::relay_recovered_terminal_text_to_placeholder(
        http,
        shared,
        ChannelId::new(anchor.panel_channel_id),
        None,
        terminal_text,
    )
    .await;
    // G5 (passive): see doc — the committed floor already covers this range, so the
    // watcher will not re-relay it; the caller disposes `outcome`.
    Some(outcome)
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
    use super::super::super::placeholder_sweeper::PlaceholderProbe;
    use super::{
        RecoveryForceClearReport, anchor_probe_should_repost, anchor_range_matches_turn,
        persist_force_clear_report_in_root,
    };
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

    // ---- #3610 PR-2 anchor-repost pure guards ---------------------------------

    #[test]
    fn anchor_probe_reposts_only_on_message_gone() {
        // The duplicate-repost defense: a permanently-gone message is the ONLY
        // verdict that reposts. A live message must never be duplicated, and a
        // transient probe error must never be read as "gone".
        assert!(anchor_probe_should_repost(PlaceholderProbe::MessageGone));
        assert!(!anchor_probe_should_repost(
            PlaceholderProbe::StillPlaceholder
        ));
        assert!(!anchor_probe_should_repost(
            PlaceholderProbe::AlreadyDelivered
        ));
        assert!(!anchor_probe_should_repost(PlaceholderProbe::ProbeFailed));
    }

    #[test]
    fn anchor_range_matches_turn_accepts_this_turns_slice() {
        // A real slice whose end reaches the row's last_offset, with the turn
        // start at/below the anchor end → this turn's anchor.
        assert!(anchor_range_matches_turn((10, 443_154), Some(10), 443_154));
        // last_offset slightly below end is fine (commit covered all the row saw).
        assert!(anchor_range_matches_turn((0, 500), Some(0), 480));
        // No turn_start_offset signal (None) → the start check is skipped.
        assert!(anchor_range_matches_turn((0, 500), None, 500));
    }

    #[test]
    fn anchor_range_matches_turn_rejects_mismatched_or_empty() {
        // (a) empty / zero range → reject (blank/sentinel frontier).
        assert!(!anchor_range_matches_turn((0, 0), Some(0), 0));
        assert!(!anchor_range_matches_turn((42, 42), Some(42), 42));
        // (b) anchor end BELOW the row's last_offset → the commit did not cover
        // what this turn last observed (stale/partial anchor) → reject.
        assert!(!anchor_range_matches_turn((0, 100), Some(0), 443_154));
        // (c) the turn's start is ABOVE the anchor end → a later turn → reject.
        assert!(!anchor_range_matches_turn((0, 100), Some(200), 100));
    }
}
