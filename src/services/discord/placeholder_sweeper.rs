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
) {
    if channel_id == 0 || message_id == 0 {
        return;
    }
    let channel = serenity::ChannelId::new(channel_id);
    let message = serenity::MessageId::new(message_id);
    let _ = channel
        .edit_message(http, message, serenity::EditMessage::new().content(content))
        .await;
}

/// Run a single sweep pass for the given provider. Public for testability —
/// callers in the bootstrap path schedule this on a fixed cadence via
/// `spawn_placeholder_sweeper`.
pub(super) async fn run_placeholder_sweep_pass(
    http: &Arc<serenity::Http>,
    _shared: &Arc<SharedData>,
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
        match classify_age(age_secs) {
            SweepDecision::Active => {}
            SweepDecision::Stalled => {
                let text = build_stalled_placeholder(&state, age_secs);
                edit_placeholder_safe(http, state.channel_id, state.current_msg_id, &text).await;
                report.stalled += 1;
            }
            SweepDecision::Abandoned => {
                let text = build_abandoned_placeholder(&state);
                edit_placeholder_safe(http, state.channel_id, state.current_msg_id, &text).await;
                if delete_inflight_state_file(provider, state.channel_id) {
                    report.abandoned += 1;
                }
            }
        }
    }
    report
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
}
