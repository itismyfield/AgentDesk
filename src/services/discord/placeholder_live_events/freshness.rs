//! #3812: live/stale confidence indicator + status-header rendering for the
//! Discord status panel.
//!
//! Offloaded from the at-cap `status_panel.rs` (mirrors the #3811 `turn_anchor.rs`
//! split): `status_panel.rs` keeps only the header-build call site, while the
//! derived-status label, the confidence classifier, the confidence-line renderer,
//! and the store-facing line builder all live here with their tests.
//!
//! The confidence label is derived from ONLY deterministic ADK status signals —
//! never from timestamp age alone — so a healthy but quiet long-running turn stays
//! `live`, and `stale` is reached only with corroborating relay/watch evidence
//! (`TerminalDeliveryUnconfirmed`: the answer was relayed but session termination
//! could not be confirmed).
//!
//! The relative age is rendered with Discord's native `<t:UNIX:R>` relative-time
//! token (the same convention the header already uses for the turn start), anchored
//! to the store's STABLE per-channel last-activity unix stamp. That keeps the panel
//! text byte-identical across heartbeat ticks with no new content (so the message
//! is not needlessly re-edited — the #3477 stability invariant), while Discord
//! renders the localized live age (`마지막 업데이트 18초 전`, `final · 2분 전`) on
//! the client and keeps it ticking with zero server churn.

use poise::serenity_prelude::ChannelId;

use crate::services::provider::ProviderKind;

use super::common::{escape_status_panel_markdown, tool_prefix, truncate_chars};
use super::status_panel::{CompletedKind, DerivedStatus, StatusPanelState};

impl super::PlaceholderLiveEvents {
    /// #3812: builds the panel's live/stale confidence line from deterministic
    /// store signals — the snapshot's derived status (the class) and the channel's
    /// STABLE last-activity unix stamp (the relative age). The store hook lives
    /// here (not in the at-cap `mod.rs` / `status_panel.rs`), mirroring the #3811
    /// `turn_anchor.rs` split. Always `Some` so every panel surfaces its confidence.
    pub(super) fn panel_confidence_line(
        &self,
        channel_id: ChannelId,
        snapshot: &StatusPanelState,
        started_at_unix: i64,
    ) -> Option<String> {
        let last_activity_unix = self
            .last_recent_event_unix
            .get(&channel_id)
            .map(|stamp| *stamp.value());
        Some(confidence_line(
            &snapshot.status,
            last_activity_unix,
            started_at_unix,
        ))
    }
}

/// #3812: user-facing confidence class for the status panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PanelConfidence {
    /// Actively running (or intentionally quiet, e.g. monitor/scheduled wait).
    Live,
    /// The turn completed cleanly — the panel is just the final status.
    Final,
    /// Corroborating relay/watch evidence says the turn stalled — investigate.
    Stale,
    /// Ambiguous or pending evidence — surfaced as `상태 불명확`, never a false stale.
    Unknown,
}

/// #3812: maps the panel's derived status to a confidence class using ONLY
/// deterministic ADK status signals (never timestamp age alone):
/// - a clean `Completed { .. }` turn → `Final`;
/// - `TerminalDeliveryUnconfirmed` (answer relayed, session-end could NOT be
///   confirmed) is corroborating relay/watch evidence of a stall → `Stale`;
/// - `TerminalDeliveryPending` (delivery done, termination still confirming) is
///   ambiguous / evidence-pending → `Unknown`;
/// - every actively-running or intentionally-quiet status (tool / subagent /
///   workflow / monitor-wait / scheduled-wakeup) → `Live`.
///
/// A quiet long-running turn therefore stays `Live` — it is never reclassified
/// `Stale` from age, matching the conservative intake-gate policy (#3812 evidence:
/// `router/intake_gate.rs:407/442/502`).
pub(super) fn panel_confidence(status: &DerivedStatus) -> PanelConfidence {
    match status {
        DerivedStatus::Completed { .. } => PanelConfidence::Final,
        DerivedStatus::TerminalDeliveryUnconfirmed => PanelConfidence::Stale,
        DerivedStatus::TerminalDeliveryPending => PanelConfidence::Unknown,
        DerivedStatus::Running
        | DerivedStatus::MonitorWait
        | DerivedStatus::ScheduleWakeup(_)
        | DerivedStatus::ToolRunning { .. }
        | DerivedStatus::SubagentRunning { .. }
        | DerivedStatus::WorkflowRunning { .. } => PanelConfidence::Live,
    }
}

/// #3812: renders the compact confidence line from a class + a stable unix anchor
/// for the relative age. The age uses Discord's `<t:UNIX:R>` token so the text is
/// heartbeat-stable while the client shows the live localized age. `Unknown`
/// deliberately omits an age (the point is that the evidence is missing).
pub(super) fn render_confidence_line(confidence: PanelConfidence, age_anchor_unix: i64) -> String {
    let age = format!("<t:{age_anchor_unix}:R>");
    match confidence {
        PanelConfidence::Live => format!("신뢰도: live · 마지막 업데이트 {age}"),
        PanelConfidence::Final => format!("신뢰도: final · {age}"),
        PanelConfidence::Stale => format!("신뢰도: stale · {age} · 조사 권장"),
        PanelConfidence::Unknown => "신뢰도: 상태 불명확".to_string(),
    }
}

/// #3812: store-facing builder for the confidence line appended under the header.
///
/// `last_activity_unix` is the store's STABLE per-channel last-live-content arrival
/// stamp (set once when the content arrived, never recomputed at render time, so
/// the line is identical across heartbeat ticks). It falls back to the turn's
/// `started_at_unix` when no live content has arrived yet (a just-started turn).
pub(super) fn confidence_line(
    status: &DerivedStatus,
    last_activity_unix: Option<i64>,
    started_at_unix: i64,
) -> String {
    let confidence = panel_confidence(status);
    let age_anchor_unix = last_activity_unix.unwrap_or(started_at_unix);
    render_confidence_line(confidence, age_anchor_unix)
}

/// #3812: builds the status-panel header line (derived-status label + provider +
/// relative start time) and, when present, appends the compact confidence line as
/// a second header row so the freshness signal leads the panel's first metadata
/// block. Kept here (not in the at-cap `status_panel.rs`) alongside
/// `render_derived_status`.
pub(super) fn render_status_header(
    status: &DerivedStatus,
    provider: &ProviderKind,
    started_at_unix: i64,
    confidence_line: Option<&str>,
) -> String {
    let mut header = format!(
        "{} — {} (<t:{started_at_unix}:R>)",
        render_derived_status(status),
        provider.as_str()
    );
    if let Some(line) = confidence_line
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        header.push('\n');
        header.push_str(line);
    }
    header
}

fn render_derived_status(status: &DerivedStatus) -> String {
    match status {
        DerivedStatus::Running => "🟢 진행 중".to_string(),
        DerivedStatus::MonitorWait => "💤 monitor 대기".to_string(),
        DerivedStatus::ScheduleWakeup(Some(eta_secs)) => {
            format!("⏰ scheduled wakeup ({eta_secs}s 후)")
        }
        DerivedStatus::ScheduleWakeup(None) => "⏰ scheduled wakeup".to_string(),
        DerivedStatus::TerminalDeliveryPending => "↻ 응답 전달됨 · 세션 종료 확인 중".to_string(),
        DerivedStatus::TerminalDeliveryUnconfirmed => {
            "⚠ 응답 전달됨 · 세션 종료 미확인".to_string()
        }
        DerivedStatus::Completed {
            kind: CompletedKind::Background,
        } => "✅ **백그라운드 완료**".to_string(),
        DerivedStatus::Completed {
            kind: CompletedKind::Foreground,
        } => "✅ **응답 완료**".to_string(),
        DerivedStatus::ToolRunning { name, summary: _ } => {
            let rendered = tool_prefix(name);
            format!("🔧 도구 실행 중 ({})", truncate_chars(&rendered, 140))
        }
        DerivedStatus::SubagentRunning { desc } => {
            let desc = escape_status_panel_markdown(desc);
            format!("🧵 subagent 실행 중 ({})", truncate_chars(&desc, 120))
        }
        DerivedStatus::WorkflowRunning { label } => {
            let label = escape_status_panel_markdown(label);
            format!("🧬 workflow 실행 중 ({})", truncate_chars(&label, 120))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STARTED_AT: i64 = 1_700_000_000;
    const LAST_ACTIVITY: i64 = 1_700_000_300; // 5 min after start

    // ---- panel_confidence: the five acceptance cases ----------------------

    #[test]
    fn live_running_status_classifies_live() {
        // A plainly-running turn, and an intentionally-quiet monitor/scheduled
        // wait, are all `live` — never stale from quietness.
        for status in [
            DerivedStatus::Running,
            DerivedStatus::MonitorWait,
            DerivedStatus::ScheduleWakeup(Some(30)),
            DerivedStatus::ToolRunning {
                name: "Bash".to_string(),
                summary: None,
            },
            DerivedStatus::SubagentRunning {
                desc: "explore".to_string(),
            },
        ] {
            assert_eq!(
                panel_confidence(&status),
                PanelConfidence::Live,
                "{status:?}"
            );
        }
    }

    #[test]
    fn completed_turn_classifies_final() {
        assert_eq!(
            panel_confidence(&DerivedStatus::Completed {
                kind: CompletedKind::Foreground
            }),
            PanelConfidence::Final
        );
        assert_eq!(
            panel_confidence(&DerivedStatus::Completed {
                kind: CompletedKind::Background
            }),
            PanelConfidence::Final
        );
    }

    #[test]
    fn relay_stalled_unconfirmed_delivery_classifies_stale() {
        // Answer relayed but session-end could not be confirmed = corroborating
        // relay/watch evidence of a stall (the intake-gate `RelayStalled` class).
        assert_eq!(
            panel_confidence(&DerivedStatus::TerminalDeliveryUnconfirmed),
            PanelConfidence::Stale
        );
    }

    #[test]
    fn queue_orphan_quiet_running_turn_is_not_stale_from_age() {
        // A long-quiet running turn (the shape a queue-blocked/orphan suspicion
        // would otherwise mis-flag) must stay `live` — `stale` requires corroborating
        // evidence, never age alone. The confidence line still surfaces the age.
        assert_eq!(
            panel_confidence(&DerivedStatus::Running),
            PanelConfidence::Live
        );
        let line = confidence_line(&DerivedStatus::Running, Some(LAST_ACTIVITY), STARTED_AT);
        assert_eq!(line, "신뢰도: live · 마지막 업데이트 <t:1700000300:R>");
        assert!(!line.contains("stale"));
    }

    #[test]
    fn ambiguous_pending_delivery_classifies_unknown() {
        assert_eq!(
            panel_confidence(&DerivedStatus::TerminalDeliveryPending),
            PanelConfidence::Unknown
        );
    }

    // ---- render_confidence_line: the proposed states ----------------------

    #[test]
    fn render_confidence_line_matches_proposed_states() {
        assert_eq!(
            render_confidence_line(PanelConfidence::Live, LAST_ACTIVITY),
            "신뢰도: live · 마지막 업데이트 <t:1700000300:R>"
        );
        assert_eq!(
            render_confidence_line(PanelConfidence::Final, LAST_ACTIVITY),
            "신뢰도: final · <t:1700000300:R>"
        );
        assert_eq!(
            render_confidence_line(PanelConfidence::Stale, LAST_ACTIVITY),
            "신뢰도: stale · <t:1700000300:R> · 조사 권장"
        );
        assert_eq!(
            render_confidence_line(PanelConfidence::Unknown, LAST_ACTIVITY),
            "신뢰도: 상태 불명확"
        );
    }

    // ---- confidence_line: anchor selection + heartbeat stability ----------

    #[test]
    fn fresh_running_turn_anchors_age_to_last_activity() {
        let line = confidence_line(&DerivedStatus::Running, Some(LAST_ACTIVITY), STARTED_AT);
        assert_eq!(line, "신뢰도: live · 마지막 업데이트 <t:1700000300:R>");
    }

    #[test]
    fn completed_turn_renders_final_state() {
        let line = confidence_line(
            &DerivedStatus::Completed {
                kind: CompletedKind::Foreground,
            },
            Some(LAST_ACTIVITY),
            STARTED_AT,
        );
        assert_eq!(line, "신뢰도: final · <t:1700000300:R>");
    }

    #[test]
    fn unconfirmed_delivery_renders_stale_marker() {
        let line = confidence_line(
            &DerivedStatus::TerminalDeliveryUnconfirmed,
            Some(LAST_ACTIVITY),
            STARTED_AT,
        );
        assert_eq!(line, "신뢰도: stale · <t:1700000300:R> · 조사 권장");
    }

    #[test]
    fn missing_activity_stamp_falls_back_to_turn_start() {
        // No live content yet → the age anchors to the turn start (still stable).
        let line = confidence_line(&DerivedStatus::Running, None, STARTED_AT);
        assert_eq!(line, "신뢰도: live · 마지막 업데이트 <t:1700000000:R>");
    }

    #[test]
    fn confidence_line_is_independent_of_render_time() {
        // The line depends only on the snapshot status + the stable activity anchor,
        // never on "now" — so two renders between heartbeats are byte-identical and
        // never re-edit the Discord message.
        let a = confidence_line(&DerivedStatus::Running, None, STARTED_AT);
        let b = confidence_line(&DerivedStatus::Running, None, STARTED_AT);
        assert_eq!(a, b);
    }

    // ---- render_status_header: placement ---------------------------------

    #[test]
    fn header_appends_confidence_line_as_second_row() {
        let header = render_status_header(
            &DerivedStatus::Running,
            &ProviderKind::Claude,
            STARTED_AT,
            Some("신뢰도: live · 마지막 업데이트 <t:1700000300:R>"),
        );
        let mut lines = header.lines();
        assert_eq!(lines.next(), Some("🟢 진행 중 — claude (<t:1700000000:R>)"));
        assert_eq!(
            lines.next(),
            Some("신뢰도: live · 마지막 업데이트 <t:1700000300:R>")
        );
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn header_without_confidence_line_is_single_row() {
        let header = render_status_header(
            &DerivedStatus::Running,
            &ProviderKind::Claude,
            STARTED_AT,
            None,
        );
        assert_eq!(header, "🟢 진행 중 — claude (<t:1700000000:R>)");
    }
}
