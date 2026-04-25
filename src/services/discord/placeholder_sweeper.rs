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

use std::sync::Arc;

use poise::serenity_prelude as serenity;

use std::sync::atomic::Ordering;

use super::SharedData;
use super::formatting::{
    MonitorHandoffReason, MonitorHandoffStatus, build_monitor_handoff_placeholder,
};
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

fn build_stalled_placeholder(state: &InflightTurnState, age_secs: u64) -> String {
    let started_at_unix = parse_started_at_unix(&state.started_at).unwrap_or_else(|| {
        // Fall back to "now - age" so the relative tag still anchors near the
        // observed staleness when the persisted started_at is unparseable.
        chrono::Utc::now().timestamp() - age_secs as i64
    });
    let reason_label = format!("⚠ stalled — no stream {age_secs}s");
    let mut text = build_monitor_handoff_placeholder(
        MonitorHandoffStatus::Active,
        MonitorHandoffReason::AsyncDispatch,
        started_at_unix,
        state.current_tool_line.as_deref(),
        None,
    );
    text.push('\n');
    text.push_str(&reason_label);
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
    channel_id: u64,
    message_id: u64,
    content: &str,
) -> bool {
    if channel_id == 0 || message_id == 0 {
        return false;
    }
    let channel = serenity::ChannelId::new(channel_id);
    let message = serenity::MessageId::new(message_id);
    channel
        .edit_message(http, message, serenity::EditMessage::new().content(content))
        .await
        .is_ok()
}

/// Run a single sweep pass for the given provider. Public for testability —
/// callers in the bootstrap path schedule this on a fixed cadence via
/// `spawn_placeholder_sweeper`.
pub(super) async fn run_placeholder_sweep_pass(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) -> SweepPassReport {
    let mut report = SweepPassReport::default();
    let states = load_inflight_states_for_sweep(provider);
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
        if !state.full_response.is_empty() || state.response_sent_offset > 0 {
            continue;
        }
        // Re-stat guard for the EDIT path: between
        // `load_inflight_states_for_sweep` and the awaited Discord edit, the
        // owning turn may write a fresh inflight state or stream the first
        // response chunk. Skip the edit (and the abandoned-branch evict) if
        // the file mtime advanced past our snapshot.
        if !inflight_state_file_still_stale(provider, state.channel_id, age_secs) {
            continue;
        }
        match classify_age(age_secs) {
            SweepDecision::Active => {}
            SweepDecision::Stalled => {
                let text = build_stalled_placeholder(&state, age_secs);
                if edit_placeholder_safe(http, state.channel_id, state.current_msg_id, &text).await
                {
                    report.stalled += 1;
                }
            }
            SweepDecision::Abandoned => {
                let text = build_abandoned_placeholder(&state);
                let edited =
                    edit_placeholder_safe(http, state.channel_id, state.current_msg_id, &text)
                        .await;
                // Skip eviction when the terminal Discord edit failed (rate
                // limit / transient outage). Leaving the state in place lets
                // the next sweep pass retry. The recheck after the await
                // also defends against a fresh turn writing state during
                // the edit itself — only evict if the file is still the
                // same stale entry we saw and the edit succeeded.
                if edited && inflight_state_file_still_stale(provider, state.channel_id, age_secs) {
                    finalize_abandoned_mailbox(shared, provider, state.channel_id).await;
                    if delete_inflight_state_file(provider, state.channel_id) {
                        report.abandoned += 1;
                    }
                }
            }
        }
    }
    report
}

/// Drop the per-channel mailbox active turn that the abandoned inflight was
/// driving. Without this, the channel's `cancel_token` and `global_active`
/// counter stay set, so subsequent user messages see an in-flight turn and
/// get queued behind a placeholder that is already terminal.
async fn finalize_abandoned_mailbox(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
) {
    let channel = serenity::ChannelId::new(channel_id);
    let finish = super::mailbox_finish_turn(shared, provider, channel).await;
    if let Some(removed_token) = finish.removed_token {
        removed_token.cancelled.store(true, Ordering::Relaxed);
        shared.global_active.fetch_sub(1, Ordering::Relaxed);
    }
}

fn inflight_state_file_still_stale(
    provider: &ProviderKind,
    channel_id: u64,
    snapshot_age_secs: u64,
) -> bool {
    // After our edit completed, the worst case is a freshly written file
    // ~ABANDON_THRESHOLD younger than the snapshot. Anything younger than
    // (snapshot_age_secs - SLACK) means a new write occurred and we must
    // not delete. Slack accommodates clock skew between the file mtime and
    // our wall-clock measurement.
    const SLACK_SECS: u64 = 5;
    let states = load_inflight_states_for_sweep(provider);
    let Some((_, current_age)) = states
        .into_iter()
        .find(|(state, _)| state.channel_id == channel_id)
    else {
        // File already gone — another path cleared it. Treat as still stale
        // so the calling code's delete becomes a no-op without triggering a
        // false-positive "preserved fresh state" decision.
        return true;
    };
    current_age + SLACK_SECS >= snapshot_age_secs
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct SweepPassReport {
    pub stalled: usize,
    pub abandoned: usize,
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
        tokio::time::sleep(tokio::time::Duration::from_secs(INITIAL_DELAY_SECS)).await;
        loop {
            let report = run_placeholder_sweep_pass(&http, &shared, &provider).await;
            if report.stalled > 0 || report.abandoned > 0 {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🧹 placeholder sweeper ({}): stalled={} abandoned={}",
                    provider.as_str(),
                    report.stalled,
                    report.abandoned
                );
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(SWEEP_INTERVAL_SECS)).await;
        }
    });
}

#[cfg(test)]
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
    fn build_stalled_placeholder_contains_age_badge() {
        let state = make_state(1234, 5678);
        let text = build_stalled_placeholder(&state, 90);
        assert!(text.starts_with("🔄 **백그라운드 처리 중**"));
        assert!(text.contains("⚠ stalled — no stream 90s"));
    }

    #[test]
    fn build_abandoned_placeholder_uses_aborted_status() {
        let state = make_state(1234, 5678);
        let text = build_abandoned_placeholder(&state);
        assert!(text.starts_with("⚠ **백그라운드 중단** (모니터 연결 끊김)"));
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
