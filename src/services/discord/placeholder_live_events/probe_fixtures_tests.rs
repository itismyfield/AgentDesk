//! Shared actual renderer fixtures for panel, footer, watcher, and drain contracts.
use super::*;

#[cfg(test)]
pub(in crate::services::discord) fn rendered_panels_for_probe_tests() -> Vec<(String, bool)> {
    DerivedStatus::panel_shape_test_variants()
        .into_iter()
        .map(|status| {
            // Independent of the production macro's terminal metadata.
            let expected_placeholder = match &status {
                DerivedStatus::Running
                | DerivedStatus::MonitorWait
                | DerivedStatus::ScheduleWakeup(_)
                | DerivedStatus::ToolRunning { .. }
                | DerivedStatus::SubagentRunning { .. }
                | DerivedStatus::WorkflowRunning { .. } => true,
                DerivedStatus::Completed {
                    kind: CompletedKind::Foreground,
                }
                | DerivedStatus::Completed {
                    kind: CompletedKind::Background,
                } => false,
            };
            let visible_header = format!("-# {}", freshness::render_activity_line(&status));
            let mut snapshot = StatusPanelState::default();
            snapshot.status = status;
            let panel = render_status_panel(
                snapshot,
                &ProviderKind::Claude,
                "턴 시작 : fixture".into(),
                None,
            );
            assert_eq!(
                panel
                    .lines()
                    .next()
                    .unwrap()
                    .replace(super::super::formatting::PLACEHOLDER_PROBE_MARKER, ""),
                visible_header,
                "the marker must not change the visible activity label"
            );
            (panel, expected_placeholder)
        })
        .collect()
}

#[cfg(test)]
pub(in crate::services::discord) fn multiline_panels_for_probe_tests() -> Vec<String> {
    let mut panels = Vec::new();
    for separator in ["\n", "\r\n"] {
        for status in [
            DerivedStatus::SubagentRunning {
                desc: format!("first{separator}second"),
            },
            DerivedStatus::WorkflowRunning {
                label: format!("first{separator}second"),
            },
        ] {
            let mut snapshot = StatusPanelState::default();
            snapshot.status = status;
            let panel = render_status_panel(snapshot, &ProviderKind::Claude, "time".into(), None);
            panels.push(panel);
        }
    }
    assert_eq!(panels.len(), 4);
    panels
}

#[cfg(test)]
pub(in crate::services::discord) fn rendered_answers_for_probe_tests() -> Vec<String> {
    let panel = rendered_panels_for_probe_tests().remove(0).0;
    let footer = super::super::single_message_panel::compose_footer_status_block("⠸", &panel);
    vec![
        "🔧 마지막 도구 ([Read])\n실제 답변".into(),
        "-# 실제 답변".into(),
        format!("실제 답변\n{footer}"),
    ]
}
