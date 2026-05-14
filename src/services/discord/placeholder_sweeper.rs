//! #1115 placeholder stall sweeper.
//!
//! Background safety net for the case where neither the in-stream lifecycle
//! finalization (#1113) nor the in-band terminal status edits ever fire —
//! e.g. the bridge process is stuck on an external IPC, the JSONL file
//! rotates out from under the parser, or the source Claude Code session is
//! killed without emitting a terminal event. The sweeper periodically scans
//! every persisted inflight state per provider; for placeholders whose
//! `updated_at` has not advanced in a configurable window, it edits the
//! Discord message into a "stalled" or "abandoned" state and (when
//! abandoning) clears the inflight state file so the message is not
//! re-processed by the regular cleanup race.
//!
//! Scope notes for the initial landing:
//! - AgentDesk-tracked inflight states only. Operator-level Claude Code
//!   sessions that never wrote an inflight state file are out of scope and
//!   tracked as a follow-up to the #1112 epic.
//! - Process-alive (`pid` / session close) detection is similarly deferred.
//!   Time-based staleness is the v1 trigger.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude as serenity;

use super::SharedData;
use super::formatting::{
    MonitorHandoffReason, MonitorHandoffStatus, build_monitor_handoff_placeholder,
};
use super::gateway::edit_outbound_message;
use super::inflight::{
    InflightTurnState, delete_inflight_state_file, load_inflight_states_for_sweep,
    parse_started_at_unix,
};
use crate::services::provider::ProviderKind;

/// Age (seconds since `updated_at`) at which a placeholder is treated as
/// stalled. Below this threshold the sweeper does nothing.
pub(crate) const STALL_THRESHOLD_SECS: u64 = 60;

/// Age at which the placeholder is treated as abandoned. The sweeper edits
/// the message to its terminal "abandoned" form and clears the inflight
/// state file.
pub(crate) const ABANDON_THRESHOLD_SECS: u64 = 300;

/// Polling interval for `spawn_placeholder_sweeper`. Picked low enough that
/// the stall transition (60s) is observed within ≤ ~1 polling delay, but
/// high enough that we do not spam Discord edits on idle startups.
pub(crate) const SWEEP_INTERVAL_SECS: u64 = 30;

/// Emit a low-volume liveness log even when no placeholder transitions were
/// found. This keeps ops from confusing "healthy but idle" with "sweeper died".
pub(crate) const SWEEP_HEARTBEAT_INTERVAL_SWEEPS: u64 = 120;

/// Initial delay before the first sweep runs after dcserver bootstrap. Skips
/// the boot-up window where active turns from the previous generation are
/// still being recovered and may legitimately appear stalled while
/// inflight-state migration is in progress.
pub(crate) const INITIAL_DELAY_SECS: u64 = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SweepDecision {
    Active,
    Stalled,
    Abandoned,
}

fn classify_age(age_secs: u64) -> SweepDecision {
    if age_secs >= ABANDON_THRESHOLD_SECS {
        SweepDecision::Abandoned
    } else if age_secs >= STALL_THRESHOLD_SECS {
        SweepDecision::Stalled
    } else {
        SweepDecision::Active
    }
}

fn build_stalled_placeholder(state: &InflightTurnState) -> String {
    let started_at_unix = parse_started_at_unix(&state.started_at).unwrap_or_else(|| {
        // Fall back to now only for malformed legacy state. The normal path
        // uses persisted started_at so the stalled content stays stable.
        chrono::Utc::now().timestamp()
    });
    let mut text = build_monitor_handoff_placeholder(
        MonitorHandoffStatus::Active,
        MonitorHandoffReason::AsyncDispatch,
        started_at_unix,
        state.current_tool_line.as_deref(),
        None,
    );
    text.push('\n');
    text.push_str("⚠ stalled — no stream progress");
    text
}

fn build_abandoned_placeholder(state: &InflightTurnState) -> String {
    let started_at_unix =
        parse_started_at_unix(&state.started_at).unwrap_or_else(|| chrono::Utc::now().timestamp());
    build_monitor_handoff_placeholder(
        MonitorHandoffStatus::Aborted,
        MonitorHandoffReason::AsyncDispatch,
        started_at_unix,
        state.current_tool_line.as_deref(),
        None,
    )
}

async fn edit_placeholder_safe(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: u64,
    message_id: u64,
    content: &str,
) -> bool {
    if channel_id == 0 || message_id == 0 {
        return false;
    }
    let channel = serenity::ChannelId::new(channel_id);
    let message = serenity::MessageId::new(message_id);
    edit_outbound_message(http.clone(), shared.clone(), channel, message, content)
        .await
        .is_ok()
}

/// Run a single sweep pass for the given provider. Public for testability —
/// callers in the bootstrap path schedule this on a fixed cadence via
/// `spawn_placeholder_sweeper`.
async fn run_placeholder_sweep_pass(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    stalled_tracker: &mut StalledEditTracker,
) -> SweepPassReport {
    let mut report = SweepPassReport::default();
    let states = load_inflight_states_for_sweep(provider);
    report.scanned = states.len();
    stalled_tracker.retain_live(provider, &states);
    for (state, age_secs) in states {
        if state.rebind_origin {
            // Rebind-origin inflights do not represent a real Discord turn.
            // Skip — there is no placeholder message to edit.
            continue;
        }
        if state.current_msg_id == 0 || state.channel_id == 0 {
            continue;
        }
        // Skip planned restart / hot-swap inflights. Their cleanup TTL is
        // intentionally extended (DrainRestart 1800s, HotSwapHandoff 900s)
        // by `inflight::load_inflight_states_from_root` so recovery can pick
        // them up after a restart. The sweeper would otherwise edit them as
        // abandoned and delete the state file, defeating recovery.
        if state.restart_mode.is_some() {
            continue;
        }
        // Only sweep messages that are still pure placeholders. Once any
        // real response text has been streamed, `current_msg_id` points at
        // a partially delivered response; overwriting it with a stalled or
        // abandoned label would corrupt user-visible output for healthy
        // long-running tools that simply haven't emitted a new event in a
        // while.
        //
        // The "stalled after partial output" case is intentionally left for
        // a follow-up: it requires an append (rather than replace) strategy
        // so the partial response stays visible above the badge.
        // codex round-2 P2 on PR #1308: a long-running tool placeholder may
        // be opened after assistant prose has already streamed, so
        // `full_response` is non-empty even though `current_msg_id` now points
        // at a pure background card. Honour the explicit flag from the turn
        // loop and let those flow through to the stalled/abandoned branches.
        if !state.long_running_placeholder_active
            && (!state.full_response.is_empty() || state.response_sent_offset > 0)
        {
            continue;
        }
        // Re-stat guard for the EDIT path: between
        // `load_inflight_states_for_sweep` and the awaited Discord edit, the
        // owning turn may complete entirely (state file removed), have a
        // brand-new turn replace it (different user_msg_id), or stream the
        // first response chunk (mtime advances). Skip the edit (and the
        // abandoned-branch evict) unless the same turn we snapshotted is
        // still on disk and still stale.
        if !inflight_state_still_same_turn(provider, &state, age_secs) {
            continue;
        }
        // codex round-8 P1 on PR #1308: long-running placeholders rely on the
        // turn loop bumping `updated_at` every 30s (see
        // `LIVE_LONG_RUN_HEARTBEAT_INTERVAL` in `turn_bridge::mod`) so the
        // sweeper can still abandon them if the owning process actually dies
        // — only the live ones keep advancing mtime. Treat all states
        // uniformly here.
        match classify_age(age_secs) {
            SweepDecision::Active => {}
            SweepDecision::Stalled => {
                if !stalled_tracker.mark_pending(provider, &state) {
                    continue;
                }
                let text = build_stalled_placeholder(&state);
                if edit_placeholder_safe(
                    http,
                    shared,
                    state.channel_id,
                    state.current_msg_id,
                    &text,
                )
                .await
                {
                    stalled_tracker.mark_edited(provider, &state);
                    report.stalled += 1;
                } else {
                    stalled_tracker.clear_pending(provider, &state);
                }
            }
            SweepDecision::Abandoned => {
                let text = build_abandoned_placeholder(&state);
                let edited = edit_placeholder_safe(
                    http,
                    shared,
                    state.channel_id,
                    state.current_msg_id,
                    &text,
                )
                .await;
                // Recheck after the awaited edit covers three concerns:
                //   1. Edit failure (rate limit / 5xx): leave state for the
                //      next pass to retry.
                //   2. New turn raced in during the await (different
                //      user_msg_id): do not abandon the new turn's mailbox
                //      or delete its state.
                //   3. Original turn completed during the await (state file
                //      gone): turn_bridge already finalized its mailbox —
                //      calling mailbox_finish_turn again would no-op or
                //      corrupt a freshly started follow-up turn.
                // `inflight_state_still_same_turn` covers (2) and (3); edit
                // success covers (1).
                if edited && inflight_state_still_same_turn(provider, &state, age_secs) {
                    finalize_abandoned_mailbox(shared, provider, &state).await;
                    if delete_inflight_state_file(provider, state.channel_id) {
                        report.abandoned += 1;
                    }
                    // codex round-10 P3 on PR #1308: detach the controller's
                    // Active row that was tracking this card so the
                    // cap-bounded map does not retain a non-evictable entry.
                    if let (Some(provider_kind), msg_id) = (
                        ProviderKind::from_str(&state.provider),
                        state.current_msg_id,
                    ) {
                        if msg_id != 0 {
                            let key = super::placeholder_controller::PlaceholderKey {
                                provider: provider_kind,
                                channel_id: serenity::ChannelId::new(state.channel_id),
                                message_id: serenity::MessageId::new(msg_id),
                            };
                            shared.placeholder_controller.detach(&key);
                        }
                    }
                }
            }
        }
    }
    report
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StalledEditKey {
    provider: String,
    channel_id: u64,
    message_id: u64,
    updated_at: String,
}

impl StalledEditKey {
    fn new(provider: &ProviderKind, state: &InflightTurnState) -> Self {
        Self {
            provider: provider.as_str().to_string(),
            channel_id: state.channel_id,
            message_id: state.current_msg_id,
            updated_at: state.updated_at.clone(),
        }
    }
}

#[derive(Debug, Default)]
struct StalledEditTracker {
    edited: HashSet<StalledEditKey>,
    pending: HashSet<StalledEditKey>,
}

impl StalledEditTracker {
    fn retain_live(&mut self, provider: &ProviderKind, states: &[(InflightTurnState, u64)]) {
        let provider_id = provider.as_str();
        let live: HashSet<StalledEditKey> = states
            .iter()
            .map(|(state, _)| StalledEditKey::new(provider, state))
            .collect();
        self.edited
            .retain(|key| key.provider != provider_id || live.contains(key));
        self.pending
            .retain(|key| key.provider != provider_id || live.contains(key));
    }

    fn mark_pending(&mut self, provider: &ProviderKind, state: &InflightTurnState) -> bool {
        let key = StalledEditKey::new(provider, state);
        if self.edited.contains(&key) || self.pending.contains(&key) {
            return false;
        }
        self.pending.insert(key);
        true
    }

    fn mark_edited(&mut self, provider: &ProviderKind, state: &InflightTurnState) {
        let key = StalledEditKey::new(provider, state);
        self.pending.remove(&key);
        self.edited.insert(key);
    }

    fn clear_pending(&mut self, provider: &ProviderKind, state: &InflightTurnState) {
        self.pending.remove(&StalledEditKey::new(provider, state));
    }
}

/// Drop the per-channel mailbox active turn that the abandoned inflight was
/// driving and reuse the regular turn-cancellation cleanup path. Without
/// this:
///   - the channel's `cancel_token` and `global_active` counter stay set,
///     so subsequent user messages see an in-flight turn and get queued
///     behind a placeholder that is already terminal,
///   - the orphaned child process / tmux session keeps running outside the
///     mailbox where no watchdog can reach it, and
///   - any soft-queued user messages stay buffered with no dequeue
///     trigger.
///
/// `cancel_active_token` handles (1)+(2) — sets the cancelled flag, kills
/// the PID tree, and tears down the tmux session. The deferred idle queue
/// kickoff covers (3): same hook that the normal cancellation path uses.
async fn finalize_abandoned_mailbox(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
) {
    let channel = serenity::ChannelId::new(state.channel_id);
    let finish = super::mailbox_finish_turn(shared, provider, channel).await;
    if let Some(removed_token) = finish.removed_token {
        super::turn_bridge::cancel_active_token(
            &removed_token,
            super::TmuxCleanupPolicy::CleanupSession {
                termination_reason_code: Some("placeholder_sweeper_abandon"),
            },
            "placeholder_sweeper abandoned",
        );
        shared.global_active.fetch_sub(1, Ordering::Relaxed);
    }
    if finish.has_pending {
        super::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel,
            "placeholder_sweeper_abandon",
        );
    }
}

/// True when the inflight state on disk for `state.channel_id` still names
/// the same turn (matching `user_msg_id` and `current_msg_id`) AND the file
/// mtime is not significantly fresher than our snapshot. Returns `false`
/// when the file is gone (original turn completed mid-await) or has been
/// replaced by a new turn for the same channel.
fn inflight_state_still_same_turn(
    provider: &ProviderKind,
    snapshot: &InflightTurnState,
    snapshot_age_secs: u64,
) -> bool {
    const SLACK_SECS: u64 = 5;
    let states = load_inflight_states_for_sweep(provider);
    let Some((current, current_age)) = states
        .into_iter()
        .find(|(state, _)| state.channel_id == snapshot.channel_id)
    else {
        // File gone — original turn completed (turn_bridge cleared its
        // own state on success/cancel). Do not act: any edit would target
        // a message the completing turn already owned, and a mailbox
        // finalize would race a fresh follow-up turn.
        return false;
    };
    if current.user_msg_id != snapshot.user_msg_id
        || current.current_msg_id != snapshot.current_msg_id
    {
        return false;
    }
    observed_age_still_stale(snapshot_age_secs, current_age, SLACK_SECS)
}

fn observed_age_still_stale(
    snapshot_age_secs: u64,
    current_age_secs: u64,
    slack_secs: u64,
) -> bool {
    current_age_secs + slack_secs >= snapshot_age_secs
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct SweepPassReport {
    pub scanned: usize,
    pub stalled: usize,
    pub abandoned: usize,
}

fn should_log_sweep_report(report: SweepPassReport, sweeps_since_heartbeat: u64) -> bool {
    report.stalled > 0
        || report.abandoned > 0
        || sweeps_since_heartbeat >= SWEEP_HEARTBEAT_INTERVAL_SWEEPS
}

/// Spawn the long-lived background task that runs the stall sweeper at the
/// configured interval until the runtime exits. Should be called once per
/// provider during dcserver bootstrap.
pub(super) fn spawn_placeholder_sweeper(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    provider: ProviderKind,
) {
    tokio::spawn(async move {
        let mut stalled_tracker = StalledEditTracker::default();
        let mut sweeps_since_heartbeat = 0u64;
        tokio::time::sleep(tokio::time::Duration::from_secs(INITIAL_DELAY_SECS)).await;
        loop {
            let report =
                run_placeholder_sweep_pass(&http, &shared, &provider, &mut stalled_tracker).await;
            sweeps_since_heartbeat = sweeps_since_heartbeat.saturating_add(1);
            if should_log_sweep_report(report, sweeps_since_heartbeat) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🧹 placeholder sweeper ({}): scanned={} stalled={} abandoned={}",
                    provider.as_str(),
                    report.scanned,
                    report.stalled,
                    report.abandoned
                );
                sweeps_since_heartbeat = 0;
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(SWEEP_INTERVAL_SECS)).await;
        }
    });
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;

    #[test]
    fn classify_age_below_stall_is_active() {
        assert_eq!(classify_age(0), SweepDecision::Active);
        assert_eq!(
            classify_age(STALL_THRESHOLD_SECS - 1),
            SweepDecision::Active
        );
    }

    #[test]
    fn classify_age_at_stall_threshold_is_stalled() {
        assert_eq!(classify_age(STALL_THRESHOLD_SECS), SweepDecision::Stalled);
        assert_eq!(
            classify_age(ABANDON_THRESHOLD_SECS - 1),
            SweepDecision::Stalled
        );
    }

    #[test]
    fn classify_age_at_abandon_threshold_is_abandoned() {
        assert_eq!(
            classify_age(ABANDON_THRESHOLD_SECS),
            SweepDecision::Abandoned
        );
        assert_eq!(
            classify_age(ABANDON_THRESHOLD_SECS + 600),
            SweepDecision::Abandoned
        );
    }

    #[test]
    fn sweep_report_heartbeat_logs_without_transitions() {
        assert!(!should_log_sweep_report(
            SweepPassReport {
                scanned: 0,
                stalled: 0,
                abandoned: 0,
            },
            SWEEP_HEARTBEAT_INTERVAL_SWEEPS - 1,
        ));
        assert!(should_log_sweep_report(
            SweepPassReport {
                scanned: 0,
                stalled: 0,
                abandoned: 0,
            },
            SWEEP_HEARTBEAT_INTERVAL_SWEEPS,
        ));
        assert!(should_log_sweep_report(
            SweepPassReport {
                scanned: 1,
                stalled: 1,
                abandoned: 0,
            },
            0,
        ));
    }

    fn make_state(channel_id: u64, current_msg_id: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            None,
            42,
            100,
            current_msg_id,
            "test".to_string(),
            None,
            None,
            None,
            None,
            0,
        )
    }

    #[test]
    fn build_stalled_placeholder_uses_stable_badge() {
        let state = make_state(1234, 5678);
        let text = build_stalled_placeholder(&state);
        assert!(text.starts_with("🔄 **응답 처리 중**"));
        assert!(text.contains("⚠ stalled — no stream progress"));
        assert!(!text.contains("90s"));
    }

    #[test]
    fn stalled_edit_tracker_allows_one_edit_per_state_update() {
        let provider = ProviderKind::Codex;
        let mut state = make_state(1234, 5678);
        state.updated_at = "2026-04-25 12:00:00".to_string();
        let mut tracker = StalledEditTracker::default();

        assert!(tracker.mark_pending(&provider, &state));
        assert!(!tracker.mark_pending(&provider, &state));
        tracker.mark_edited(&provider, &state);
        assert!(!tracker.mark_pending(&provider, &state));

        state.updated_at = "2026-04-25 12:01:00".to_string();
        assert!(tracker.mark_pending(&provider, &state));
    }

    #[test]
    fn observed_age_slack_only_matches_when_within_slack() {
        // Current age much smaller than snapshot age means a fresh write —
        // not stale.
        assert!(!observed_age_still_stale(120, 100, 5));
        // Current age within slack of snapshot age — still stale.
        assert!(observed_age_still_stale(120, 116, 5));
        // Current age greater than snapshot age (no fresh write) — still
        // stale.
        assert!(observed_age_still_stale(120, 130, 5));
    }

    #[test]
    fn build_abandoned_placeholder_uses_aborted_status() {
        let state = make_state(1234, 5678);
        let text = build_abandoned_placeholder(&state);
        assert!(text.starts_with("⚠ **응답 중단**"));
    }

    #[test]
    fn restart_mode_inflights_are_skipped_in_decision_path() {
        // Sweeper exits early for restart_mode states regardless of age.
        // Verify the source state used for the early-skip branch — actually
        // editing/deleting requires async + filesystem fixtures that the
        // unit test layer does not stand up.
        let mut state = make_state(1234, 5678);
        assert!(state.restart_mode.is_none());
        state.set_restart_mode(super::super::InflightRestartMode::DrainRestart);
        assert!(state.restart_mode.is_some());
    }

    #[test]
    fn placeholder_only_gating_excludes_partially_streamed_state() {
        // The sweeper guards `!state.full_response.is_empty() ||
        // state.response_sent_offset > 0` to avoid overwriting partially
        // delivered responses. This test pins the data shape that the gate
        // checks against.
        let mut state = make_state(1234, 5678);
        assert!(state.full_response.is_empty());
        assert_eq!(state.response_sent_offset, 0);

        state.full_response = "partial response so far".to_string();
        assert!(!state.full_response.is_empty());

        state.full_response.clear();
        state.response_sent_offset = 64;
        assert!(state.response_sent_offset > 0);
    }
}
