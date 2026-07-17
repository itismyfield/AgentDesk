//! #3983: status-panel activity + time-line rendering for the Discord footer.
//!
//! Offloaded from the at-cap `status_panel.rs` (mirrors the #3811 `turn_anchor.rs`
//! split): `status_panel.rs` keeps only the panel-assembly call site, while the
//! derived-status activity label and the store-facing time-line builder live here
//! with their tests.
//!
//! The footer header starts with the derived-status ACTIVITY label (`🟢 진행 중` /
//! `🔧 도구 실행 중 (…)` / `✅ 완료`). The spinner merge
//! (`single_message_panel::merged_footer_header_line`) swaps the leading status
//! emoji for the animated spinner, so the marker set there must stay in sync with
//! the emojis this label can start with. The request anchor follows the activity
//! line, then the stable TIME lines render one field per line in turn-start / last-
//! update order. The fixed KST absolute times keep mobile clients readable when
//! they do not refresh Discord's relative token. This replaces the pre-#3983
//! confidence line + `진행 중 — provider` header; the freshness class is absorbed
//! into the activity emoji, and the provider moved off the footer entirely.
//!
//! Both times derive from STABLE store stamps (never "now"), so the footer text
//! stays byte-identical across heartbeat ticks — the message is not needlessly
//! re-edited (the #3477 stability invariant) while Discord renders the live
//! localized age client-side.

use poise::serenity_prelude::ChannelId;

use super::common::{escape_status_panel_markdown, tool_prefix, truncate_chars};
use super::status_panel::{CompletedKind, DerivedStatus};

impl super::PlaceholderLiveEvents {
    /// #3983: builds the panel's time line from the channel's STABLE last-activity
    /// unix stamp (set once when the content arrived, never recomputed at render
    /// time), falling back to the turn's `started_at_unix` when no live content has
    /// arrived yet. The store hook lives here (not in the at-cap `mod.rs` /
    /// `status_panel.rs`), mirroring the #3811 `turn_anchor.rs` split.
    pub(super) fn panel_time_line(&self, channel_id: ChannelId, started_at_unix: i64) -> String {
        let last_activity_unix = self
            .last_recent_event_unix
            .get(&channel_id)
            .map(|stamp| *stamp.value());
        render_time_line(last_activity_unix, started_at_unix)
    }
}

/// #4572: renders a stable Unix stamp as a fixed KST time for the footer.
fn render_kst_time(unix: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(unix, 0)
        .expect("status-panel timestamps must be valid Unix seconds")
        .with_timezone(&chrono_tz::Asia::Seoul)
        .format("%m-%d %H:%M:%S")
        .to_string()
}

/// #4572/#4601: renders the footer's fixed KST and relative time fields on
/// separate lines, with turn start before last update. `last_activity_unix` is
/// the store's STABLE per-channel last-live-content arrival stamp; it falls back
/// to the turn start when no live content has arrived yet. The injected stamps
/// keep the text identical across heartbeat ticks (never re-edited) while
/// Discord can still show the localized relative age.
pub(super) fn render_time_line(last_activity_unix: Option<i64>, started_at_unix: i64) -> String {
    let last = last_activity_unix.unwrap_or(started_at_unix);
    format!(
        "턴 시작 : {} (<t:{started_at_unix}:R>)\n마지막 업데이트 : {} (<t:{last}:R>)",
        render_kst_time(started_at_unix),
        render_kst_time(last),
    )
}

/// #3983: the panel's first (activity) line — the derived-status label alone (no
/// provider, no timestamp; those moved to the time line). The final confidence
/// class is absorbed into the emoji here (item B): a clean completion reads
/// `✅ 완료`.
pub(super) fn render_activity_line(status: &DerivedStatus) -> String {
    match status {
        DerivedStatus::Running => "🟢 진행 중".to_string(),
        DerivedStatus::MonitorWait => "💤 monitor 대기".to_string(),
        DerivedStatus::ScheduleWakeup(Some(eta_secs)) => {
            format!("⏰ scheduled wakeup ({eta_secs}s 후)")
        }
        DerivedStatus::ScheduleWakeup(None) => "⏰ scheduled wakeup".to_string(),
        DerivedStatus::Completed {
            kind: CompletedKind::Background,
        } => "✅ 백그라운드 완료".to_string(),
        DerivedStatus::Completed {
            kind: CompletedKind::Foreground,
        } => "✅ 완료".to_string(),
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

    // ---- render_activity_line: the derived-status labels ------------------

    #[test]
    fn running_turn_renders_green_activity_label() {
        assert_eq!(render_activity_line(&DerivedStatus::Running), "🟢 진행 중");
    }

    #[test]
    fn tool_running_renders_wrench_activity_label() {
        assert_eq!(
            render_activity_line(&DerivedStatus::ToolRunning {
                name: "Bash".to_string(),
                summary: None,
            }),
            "🔧 도구 실행 중 ([Bash])"
        );
    }

    #[test]
    fn completed_turn_renders_final_check_label() {
        // #3983 item B: `final` is absorbed into the ✅ activity emoji.
        assert_eq!(
            render_activity_line(&DerivedStatus::Completed {
                kind: CompletedKind::Foreground
            }),
            "✅ 완료"
        );
        assert_eq!(
            render_activity_line(&DerivedStatus::Completed {
                kind: CompletedKind::Background
            }),
            "✅ 백그라운드 완료"
        );
    }

    #[test]
    fn activity_labels_lead_with_a_spinner_swap_marker() {
        // Every actively-rendered label must lead with a status emoji so the
        // spinner-merge swaps it for the animation cleanly (spinner-prefix parity).
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
            DerivedStatus::WorkflowRunning {
                label: "review".to_string(),
            },
            DerivedStatus::Completed {
                kind: CompletedKind::Foreground,
            },
        ] {
            let line = render_activity_line(&status);
            let first = line.chars().next().expect("non-empty label");
            assert!(
                ['🟢', '💤', '⏰', '🔧', '🧵', '🧬', '✅'].contains(&first),
                "label {line:?} must lead with a spinner-swap marker"
            );
        }
    }

    // ---- render_time_line: anchor selection + heartbeat stability ---------

    #[test]
    fn time_line_renders_start_then_update_on_separate_lines() {
        assert_eq!(
            render_time_line(Some(LAST_ACTIVITY), STARTED_AT),
            "턴 시작 : 11-15 07:13:20 (<t:1700000000:R>)\n마지막 업데이트 : 11-15 07:18:20 (<t:1700000300:R>)"
        );
    }

    #[test]
    fn time_line_includes_fixed_kst_and_discord_relative_tokens() {
        let line = render_time_line(Some(LAST_ACTIVITY), STARTED_AT);

        assert!(line.contains("턴 시작 : 11-15 07:13:20 (<t:1700000000:R>)"));
        assert!(line.contains("마지막 업데이트 : 11-15 07:18:20 (<t:1700000300:R>)"));
        assert!(!line.contains(" / "), "time fields must not share one line");
    }

    #[test]
    fn missing_activity_stamp_falls_back_to_turn_start() {
        // No live content yet → the update age anchors to the turn start.
        assert_eq!(
            render_time_line(None, STARTED_AT),
            "턴 시작 : 11-15 07:13:20 (<t:1700000000:R>)\n마지막 업데이트 : 11-15 07:13:20 (<t:1700000000:R>)"
        );
    }

    #[test]
    fn time_line_is_independent_of_render_time() {
        // Depends only on the stable stamps, never on "now" — two renders between
        // heartbeats are byte-identical and never re-edit the Discord message.
        let a = render_time_line(Some(LAST_ACTIVITY), STARTED_AT);
        let b = render_time_line(Some(LAST_ACTIVITY), STARTED_AT);
        assert_eq!(a, b);
    }
}
