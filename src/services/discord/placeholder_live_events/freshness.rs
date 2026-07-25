//! #3983: status-panel activity + time-line rendering for the Discord footer.
//!
//! Offloaded from the at-cap `status_panel.rs` (mirrors the #3811 `turn_anchor.rs`
//! split): `status_panel.rs` keeps only the panel-assembly call site, while the
//! derived-status activity label and the store-facing time-line builder live here
//! with their tests.
//!
//! While a turn is actively processing, the footer header shows only the latest
//! observed tool class (`🔧 마지막 도구 ([Read])`) or a non-progress fallback when
//! no tool has run yet. Raw tool arguments are intentionally excluded from this
//! channel-visible surface. Distinct wait states retain their specific labels
//! because those details are not represented by the spinner. Completed turns
//! retain their existing completion label. The spinner merge
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
use super::status_panel::{CompletedKind, DerivedStatus, LastToolCall};

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

/// #3983/#4892: the panel's first (activity) line. Generic running/tool states
/// render the latest observed tool instead of duplicating the answer message's
/// spinner/progress state. Wait states retain their specific labels because the
/// spinner does not communicate monitor, wakeup, subagent, or workflow details.
/// Completed turns retain the pre-#4892 labels.
pub(super) fn render_activity_line(status: &DerivedStatus) -> String {
    render_activity_line_with_last_tool(status, None)
}

pub(super) fn render_activity_line_with_last_tool(
    status: &DerivedStatus,
    last_tool: Option<&LastToolCall>,
) -> String {
    match status {
        DerivedStatus::Completed {
            kind: CompletedKind::Background,
        } => "✅ 백그라운드 완료".to_string(),
        DerivedStatus::Completed {
            kind: CompletedKind::Foreground,
        } => "✅ 완료".to_string(),
        DerivedStatus::Running => render_last_tool(last_tool),
        DerivedStatus::MonitorWait => "💤 monitor 대기".to_string(),
        DerivedStatus::ScheduleWakeup(Some(eta_secs)) => {
            format!("⏰ scheduled wakeup ({eta_secs}s 후)")
        }
        DerivedStatus::ScheduleWakeup(None) => "⏰ scheduled wakeup".to_string(),
        DerivedStatus::ToolRunning { name, summary: _ } => render_tool_activity(name),
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

fn render_last_tool(last_tool: Option<&LastToolCall>) -> String {
    last_tool.map_or_else(
        || "🔧 마지막 도구 (아직 없음)".to_string(),
        |tool| render_tool_activity(&tool.name),
    )
}

/// #4892: tool inputs and names are untrusted and may contain credentials,
/// tenant identifiers, signed URLs, private paths, or arbitrary payloads. The
/// classifier returns only static labels, so no caller-controlled bytes reach
/// this channel-visible surface.
fn render_tool_activity(name: &str) -> String {
    format!("🔧 마지막 도구 ({})", tool_prefix(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STARTED_AT: i64 = 1_700_000_000;
    const LAST_ACTIVITY: i64 = 1_700_000_300; // 5 min after start

    // ---- render_activity_line: the derived-status labels ------------------

    #[test]
    fn running_turn_without_tool_uses_non_duplicate_fallback() {
        let rendered = render_activity_line_with_last_tool(&DerivedStatus::Running, None);

        assert_eq!(rendered, "🔧 마지막 도구 (아직 없음)");
        assert!(!rendered.contains("진행 중"));
    }

    #[test]
    fn running_turn_renders_only_last_tool_class() {
        let raw_path = "src/services/discord/status_panel.rs";
        let last_tool = LastToolCall {
            name: "Read".to_string(),
            summary: Some(raw_path.to_string()),
        };
        let rendered =
            render_activity_line_with_last_tool(&DerivedStatus::Running, Some(&last_tool));

        assert_eq!(rendered, "🔧 마지막 도구 ([Read])");
        assert!(!rendered.contains(raw_path));
        assert!(!rendered.contains("진행 중"));
    }

    #[test]
    fn tool_running_uses_same_class_only_format() {
        let raw_summary = "check status";
        let rendered = render_activity_line_with_last_tool(
            &DerivedStatus::ToolRunning {
                name: "Bash".to_string(),
                summary: Some(raw_summary.to_string()),
            },
            None,
        );

        assert_eq!(rendered, "🔧 마지막 도구 ([Bash])");
        assert!(!rendered.contains(raw_summary));
    }

    fn assert_raw_tool_input_hidden(name: &str, raw: &str) {
        let last_tool = LastToolCall {
            name: name.to_string(),
            summary: Some(raw.to_string()),
        };
        let rendered =
            render_activity_line_with_last_tool(&DerivedStatus::Running, Some(&last_tool));

        assert!(
            !rendered.contains(raw),
            "raw tool input must not reach the status panel: {rendered}"
        );
        assert!(
            !rendered.contains("Hunter2")
                && !rendered.contains("hvs.CAES")
                && !rendered.contains("secret/prod/db"),
            "sensitive tool detail must not reach the status panel: {rendered}"
        );
    }

    #[test]
    fn last_tool_hides_bash_database_url() {
        assert_raw_tool_input_hidden(
            "Bash",
            "psql postgresql://svc:Hunter2@db.prod:5432/main -c SELECT",
        );
    }

    #[test]
    fn last_tool_hides_vault_json() {
        assert_raw_tool_input_hidden(
            "mcp__vault__read",
            r#"read: {"path":"secret/prod/db","token":"hvs.CAES..."}"#,
        );
    }

    #[test]
    fn last_tool_hides_non_json_payload() {
        assert_raw_tool_input_hidden("Monitor", "non-json payload hvs.CAES... secret/prod/db");
    }

    #[test]
    fn unknown_tool_names_fail_closed_without_namespace_disclosure() {
        let rendered = render_activity_line_with_last_tool(
            &DerivedStatus::ToolRunning {
                name: "internal_prod_deploy_secret".to_string(),
                summary: None,
            },
            None,
        );

        assert_eq!(rendered, "🔧 마지막 도구 ([Tool])");
        assert!(
            !rendered.contains("internal_prod"),
            "unknown tool identity must fail closed: {rendered}"
        );
    }

    #[test]
    fn mcp_tool_names_collapse_to_one_static_label() {
        for name in [
            "mcp__vault-prod",
            "mcp__a__b__c",
            "mcp__hr__lookup_employee_alice",
        ] {
            let rendered = render_activity_line_with_last_tool(
                &DerivedStatus::ToolRunning {
                    name: name.to_string(),
                    summary: None,
                },
                None,
            );

            assert_eq!(rendered, "🔧 마지막 도구 ([MCP])", "input: {name}");
            for private_fragment in ["vault-prod", "a__b__c", "hr", "lookup_employee_alice"] {
                assert!(
                    !rendered.contains(private_fragment),
                    "MCP names and operations must never reach the status panel: {rendered}"
                );
            }
        }
    }

    #[test]
    fn tool_activity_output_is_bounded_by_the_static_label_set() {
        let rendered = render_activity_line_with_last_tool(
            &DerivedStatus::ToolRunning {
                name: format!("mcp__private_server__{}", "a".repeat(300)),
                summary: None,
            },
            None,
        );

        assert_eq!(rendered, "🔧 마지막 도구 ([MCP])");
        assert!(!rendered.contains("private_server"));
    }

    #[test]
    fn codex_execution_function_names_map_to_bash() {
        for name in ["command_execution", "exec", "exec_command", "run_cmd"] {
            let rendered = render_activity_line_with_last_tool(
                &DerivedStatus::ToolRunning {
                    name: name.to_string(),
                    summary: None,
                },
                None,
            );

            assert_eq!(rendered, "🔧 마지막 도구 ([Bash])", "input: {name}");
        }
    }

    #[test]
    fn monitor_wait_keeps_specific_label_instead_of_last_tool() {
        let last_tool = LastToolCall {
            name: "Monitor".to_string(),
            summary: Some("wait for build".to_string()),
        };

        assert_eq!(
            render_activity_line_with_last_tool(&DerivedStatus::MonitorWait, Some(&last_tool),),
            "💤 monitor 대기"
        );
    }

    #[test]
    fn schedule_wakeup_keeps_eta_label_instead_of_last_tool() {
        let last_tool = LastToolCall {
            name: "ScheduleWakeup".to_string(),
            summary: Some("30 seconds".to_string()),
        };

        assert_eq!(
            render_activity_line_with_last_tool(
                &DerivedStatus::ScheduleWakeup(Some(30)),
                Some(&last_tool),
            ),
            "⏰ scheduled wakeup (30s 후)"
        );
        assert_eq!(
            render_activity_line_with_last_tool(
                &DerivedStatus::ScheduleWakeup(None),
                Some(&last_tool),
            ),
            "⏰ scheduled wakeup"
        );
    }

    #[test]
    fn subagent_running_keeps_description_label_instead_of_last_tool() {
        let last_tool = LastToolCall {
            name: "Task".to_string(),
            summary: Some("review".to_string()),
        };

        assert_eq!(
            render_activity_line_with_last_tool(
                &DerivedStatus::SubagentRunning {
                    desc: "review_status".to_string(),
                },
                Some(&last_tool),
            ),
            r"🧵 subagent 실행 중 (review\_status)"
        );
    }

    #[test]
    fn workflow_running_keeps_label_instead_of_last_tool() {
        let last_tool = LastToolCall {
            name: "Workflow".to_string(),
            summary: Some("CI".to_string()),
        };

        assert_eq!(
            render_activity_line_with_last_tool(
                &DerivedStatus::WorkflowRunning {
                    label: "CI_review".to_string(),
                },
                Some(&last_tool),
            ),
            r"🧬 workflow 실행 중 (CI\_review)"
        );
    }

    #[test]
    fn completed_turn_renders_final_check_label() {
        // #3983 item B: `final` is absorbed into the ✅ activity emoji.
        assert_eq!(
            render_activity_line_with_last_tool(
                &DerivedStatus::Completed {
                    kind: CompletedKind::Foreground
                },
                None,
            ),
            "✅ 완료"
        );
        assert_eq!(
            render_activity_line_with_last_tool(
                &DerivedStatus::Completed {
                    kind: CompletedKind::Background
                },
                None,
            ),
            "✅ 백그라운드 완료"
        );
    }

    #[test]
    fn activity_labels_lead_with_a_spinner_swap_marker() {
        // Every actively-rendered label must lead with a status emoji so the
        // spinner-merge swaps it for the animation cleanly (spinner-prefix parity).
        let last_tool = LastToolCall {
            name: "Read".to_string(),
            summary: None,
        };
        for (status, last_tool, expected_prefix) in [
            (DerivedStatus::Running, None, "🔧"),
            (DerivedStatus::MonitorWait, Some(&last_tool), "💤"),
            (
                DerivedStatus::ScheduleWakeup(Some(30)),
                Some(&last_tool),
                "⏰",
            ),
            (
                DerivedStatus::ToolRunning {
                    name: "Bash".to_string(),
                    summary: None,
                },
                None,
                "🔧",
            ),
            (
                DerivedStatus::SubagentRunning {
                    desc: "explore".to_string(),
                },
                Some(&last_tool),
                "🧵",
            ),
            (
                DerivedStatus::WorkflowRunning {
                    label: "review".to_string(),
                },
                Some(&last_tool),
                "🧬",
            ),
            (
                DerivedStatus::Completed {
                    kind: CompletedKind::Foreground,
                },
                None,
                "✅",
            ),
        ] {
            let line = render_activity_line_with_last_tool(&status, last_tool);
            assert!(
                line.starts_with(expected_prefix),
                "label {line:?} must lead with spinner-swap marker {expected_prefix:?}"
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
