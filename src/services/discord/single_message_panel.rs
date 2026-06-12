use std::sync::OnceLock;

pub(super) fn enabled() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let raw = std::env::var("AGENTDESK_SINGLE_MESSAGE_PANEL").ok();
        let enabled = parse_single_message_panel_flag(raw.as_deref());
        let state = if enabled { "enabled" } else { "disabled" };
        tracing::info!("  ✓ single_message_panel: {state}");
        enabled
    })
}

fn parse_single_message_panel_flag(raw: Option<&str>) -> bool {
    raw.map(str::trim)
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

pub(in crate::services::discord) fn footer_mode_enabled(
    single_message_panel_enabled: bool,
    status_panel_v2_enabled: bool,
) -> bool {
    single_message_panel_enabled && status_panel_v2_enabled
}

pub(in crate::services::discord) fn separate_status_panel_enabled_for_flags(
    single_message_panel_enabled: bool,
    status_panel_v2_enabled: bool,
) -> bool {
    status_panel_v2_enabled
        && !footer_mode_enabled(single_message_panel_enabled, status_panel_v2_enabled)
}

pub(in crate::services::discord) fn live_events_dirty_should_force_status_update(
    live_events_dirty: bool,
    single_message_panel_footer_mode: bool,
) -> bool {
    live_events_dirty && !single_message_panel_footer_mode
}

pub(in crate::services::discord) fn compose_footer_status_block(
    indicator: &str,
    panel_text: &str,
) -> String {
    let spinner = super::formatting::build_processing_status_block(indicator);
    let panel_text = panel_text.trim();
    let status_block = if panel_text.is_empty() {
        spinner
    } else {
        format!("{spinner}\n{panel_text}")
    };
    clamp_footer_status_block(status_block)
}

fn clamp_footer_status_block(status_block: String) -> String {
    let max_bytes = super::DISCORD_MSG_LIMIT.saturating_sub(6);
    if status_block.len() <= max_bytes {
        return status_block;
    }
    let ellipsis = "…";
    let body_budget = max_bytes.saturating_sub(ellipsis.len());
    if body_budget == 0 {
        return ellipsis.to_string();
    }
    let safe_end = super::formatting::floor_char_boundary(&status_block, body_budget);
    format!("{}{}", &status_block[..safe_end], ellipsis)
}

pub(in crate::services::discord) fn finalize_streaming_footer(
    last_edit_text: &str,
    provider: &super::ProviderKind,
) -> Option<String> {
    let cleaned = strip_streaming_footer(last_edit_text, provider)?;
    if cleaned.trim().is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

pub(in crate::services::discord) fn strip_streaming_footer(
    last_edit_text: &str,
    provider: &super::ProviderKind,
) -> Option<String> {
    if let Some(finalized) =
        super::formatting::finalize_stale_streaming_footer(last_edit_text, provider)
    {
        return Some(finalized);
    }

    if footer_starts_with_spinner(last_edit_text) {
        return Some(String::new());
    }

    let (body, _footer) = split_footer(last_edit_text)?;
    let cleaned = super::formatting::format_for_discord_with_status_panel(body, provider);
    if cleaned == last_edit_text {
        None
    } else {
        Some(cleaned)
    }
}

fn split_footer(text: &str) -> Option<(&str, &str)> {
    let mut search_end = text.len();
    while let Some(idx) = text[..search_end].rfind("\n\n") {
        let body = &text[..idx];
        let footer = &text[(idx + 2)..];
        if footer_starts_with_spinner(footer) {
            return Some((body, footer));
        }
        search_end = idx;
    }
    None
}

fn footer_starts_with_spinner(footer: &str) -> bool {
    let Some(first_footer_line) = footer.lines().find(|line| !line.trim().is_empty()) else {
        return false;
    };
    super::formatting::is_streaming_placeholder_status_line(first_footer_line.trim())
}

#[cfg(test)]
mod tests {
    use super::super::DISCORD_MSG_LIMIT;
    use super::super::ProviderKind;

    #[test]
    fn single_message_panel_flag_defaults_off_when_unset() {
        assert!(!super::parse_single_message_panel_flag(None));
    }

    #[test]
    fn single_message_panel_flag_accepts_only_documented_truthy_values() {
        for raw in ["1", "true", "TRUE", "TrUe", " true "] {
            assert!(
                super::parse_single_message_panel_flag(Some(raw)),
                "{raw:?} should enable the flag"
            );
        }
    }

    #[test]
    fn single_message_panel_flag_rejects_falsy_and_garbage_values() {
        for raw in ["", "0", "false", "FALSE", "yes", "on", "garbage"] {
            assert!(
                !super::parse_single_message_panel_flag(Some(raw)),
                "{raw:?} should leave the flag disabled"
            );
        }
    }

    #[test]
    fn footer_mode_requires_both_flags() {
        assert!(super::footer_mode_enabled(true, true));
        assert!(!super::footer_mode_enabled(true, false));
        assert!(!super::footer_mode_enabled(false, true));
    }

    #[test]
    fn footer_status_block_keeps_spinner_first() {
        let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
        let block = super::compose_footer_status_block("⠸", panel);

        assert!(block.starts_with("⠸ 계속 처리 중\n🟢 진행 중"));
        assert!(block.contains("Subagents\n└ review inspect"));
    }

    #[test]
    fn terminal_footer_strip_removes_panel_block() {
        let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
        let rendered = format!(
            "Final answer\n\n{}",
            super::compose_footer_status_block("⠸", panel)
        );
        let finalized = super::finalize_streaming_footer(&rendered, &ProviderKind::Claude)
            .expect("panel footer should strip at terminal reconciliation");

        assert_eq!(finalized, "Final answer");
        assert!(!finalized.contains("계속 처리 중"));
        assert!(!finalized.contains("Subagents"));
    }

    #[test]
    fn footer_only_body_strips_to_empty_for_cleanup_callers() {
        let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
        let rendered = super::compose_footer_status_block("⠸", panel);

        assert_eq!(
            super::strip_streaming_footer(&rendered, &ProviderKind::Claude),
            Some(String::new())
        );
        assert_eq!(
            super::finalize_streaming_footer(&rendered, &ProviderKind::Claude),
            None
        );
    }

    #[test]
    fn footer_status_block_stays_within_discord_limit() {
        let huge_panel = format!(
            "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n{}",
            "└ reviewer ".repeat(1_000)
        );
        let status_block = super::compose_footer_status_block("⠸", &huge_panel);
        let rendered =
            super::super::formatting::build_streaming_placeholder_text("body", &status_block);

        assert!(rendered.len() <= DISCORD_MSG_LIMIT);
        assert!(rendered.contains("\n\n"));
    }
}
