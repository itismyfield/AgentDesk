//! Structural classifiers for markerless status-panel and legacy handoff cards.

pub(super) fn live_status_panel_shape(content: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "🟢 진행 중",
        "🔧 도구 실행 중",
        "🧵 subagent 실행 중",
        "🧬 workflow 실행 중",
        "💤 monitor 대기",
        "⏰ scheduled wakeup",
    ];
    let first = content.lines().next().unwrap_or_default();
    PREFIXES.iter().any(|prefix| {
        first == *prefix
            || first
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with(" ("))
    })
}

#[cfg(test)]
mod tests {
    use super::live_status_panel_shape;
    use crate::services::discord::{
        formatting::PLACEHOLDER_PROBE_MARKER,
        placeholder_live_events::{
            rendered_answers_for_probe_tests, rendered_panels_for_probe_tests,
        },
        placeholder_sweeper::is_message_still_placeholder,
        single_message_panel::compose_footer_status_block,
    };

    #[test]
    fn renderer_activity_variants_and_panel_shape_classifier_stay_in_sync() {
        let panels = rendered_panels_for_probe_tests();
        assert_eq!(panels.len(), 9, "consume every macro representative");
        for (rendered, expected_placeholder) in panels {
            assert_eq!(
                rendered.contains(PLACEHOLDER_PROBE_MARKER),
                expected_placeholder
            );
            assert_eq!(
                is_message_still_placeholder(&rendered),
                expected_placeholder,
                "actual panel {rendered:?} has the wrong placeholder classification"
            );
        }
    }

    #[test]
    fn actual_panel_footer_and_answers_are_not_placeholders() {
        for (panel, _) in rendered_panels_for_probe_tests() {
            let footer = compose_footer_status_block("⠸", &panel);
            assert!(!footer.contains(PLACEHOLDER_PROBE_MARKER), "{footer:?}");
            assert!(!is_message_still_placeholder(&footer), "{footer:?}");
            assert_eq!(
                footer,
                compose_footer_status_block("⠸", &panel.replace(PLACEHOLDER_PROBE_MARKER, ""))
            );
        }
        for answer in rendered_answers_for_probe_tests() {
            assert!(!is_message_still_placeholder(&answer), "{answer:?}");
        }
    }

    #[test]
    fn legacy_markerless_prefixes_preserve_compatibility_without_last_tool_prose() {
        // These six historical shapes remain supported; current bare labels
        // need not be recognized because current producers carry the marker.
        for prefix in [
            "🟢 진행 중",
            "🔧 도구 실행 중",
            "🧵 subagent 실행 중",
            "🧬 workflow 실행 중",
            "💤 monitor 대기",
            "⏰ scheduled wakeup",
        ] {
            for panel in [prefix.to_string(), format!("{prefix} (fixture)\nbody")] {
                assert!(live_status_panel_shape(&panel), "{panel:?}");
                assert!(is_message_still_placeholder(&panel), "{panel:?}");
            }
        }
        for prose in [
            "✅ 완료",
            "✅ 백그라운드 완료",
            "🔧 마지막 도구 ([Read])\n실제 답변",
        ] {
            assert!(!live_status_panel_shape(prose), "{prose:?}");
            assert!(!is_message_still_placeholder(prose), "{prose:?}");
        }
    }
}

/// Locale-independent structural detector for legacy (pre-marker) handoff cards.
pub(super) fn legacy_handoff_card_shape(lines: &[&str]) -> bool {
    let has_started_at = lines
        .iter()
        .any(|line| line.trim().starts_with("> **") && line.contains(": <t:"));
    let blockquote_field_lines = lines
        .iter()
        .filter(|line| {
            let line = line.trim();
            line.starts_with("> **") && line.contains("**:")
        })
        .count();
    has_started_at && blockquote_field_lines >= 2
}
