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
