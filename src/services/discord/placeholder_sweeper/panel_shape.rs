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
    use crate::services::discord::placeholder_live_events::rendered_activity_lines_for_panel_shape_tests;

    #[test]
    fn renderer_activity_variants_and_panel_shape_classifier_stay_in_sync() {
        for (rendered, terminal) in rendered_activity_lines_for_panel_shape_tests() {
            assert_eq!(
                live_status_panel_shape(&rendered),
                !terminal,
                "rendered activity line {rendered:?} has the wrong live-panel classification"
            );
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
