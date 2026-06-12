use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use poise::serenity_prelude::{ChannelId, MessageId};

pub(in crate::services::discord) const SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES: usize = 600;
pub(in crate::services::discord) const SINGLE_MESSAGE_PANEL_SPINNER_FRAMES: &[&str] =
    &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
// Residual background agents can legitimately run for about an hour, but a
// crashed agent must not keep editing the terminal message forever.
pub(in crate::services::discord) const COMPLETION_FOOTER_MAX_IDLE_ANIMATION_SECS: i64 = 3600;
pub(in crate::services::discord) const COMPLETION_FOOTER_MAX_CONSECUTIVE_EDIT_FAILURES: u8 = 3;
const COMPLETION_FOOTER_IDLE_EXPIRED_INDICATOR: &str = "…";

pub(in crate::services::discord) fn single_message_panel_spinner_frame(
    index: usize,
) -> &'static str {
    SINGLE_MESSAGE_PANEL_SPINNER_FRAMES[index % SINGLE_MESSAGE_PANEL_SPINNER_FRAMES.len()]
}

#[derive(Debug, Clone)]
struct RegisteredCompletionFooter {
    message_id: MessageId,
    provider: super::ProviderKind,
    base_body: String,
    registered_at_unix: i64,
    consecutive_edit_failures: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct CompletionFooterEdit {
    pub(in crate::services::discord) message_id: MessageId,
    pub(in crate::services::discord) text: String,
    pub(in crate::services::discord) remove_after_edit: bool,
}

fn completion_footer_registry() -> &'static Mutex<HashMap<u64, RegisteredCompletionFooter>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, RegisteredCompletionFooter>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

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

pub(in crate::services::discord) fn separate_status_panel_enabled(
    status_panel_v2_enabled: bool,
) -> bool {
    separate_status_panel_enabled_for_flags(enabled(), status_panel_v2_enabled)
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
    } else if let Some(status_block) = compose_merged_footer_status_block(indicator, panel_text) {
        status_block
    } else {
        spinner
    };
    clamp_footer_status_block(status_block)
}

pub(in crate::services::discord) fn compose_completion_footer_text(
    body: &str,
    completion_block: Option<&str>,
) -> String {
    let body = body.trim_end();
    let Some(block) = completion_block
        .map(str::trim)
        .filter(|block| !block.is_empty())
    else {
        return body.to_string();
    };
    if body.is_empty() {
        return clamp_footer_status_block(block.to_string());
    }

    let suffix = format!("\n\n{block}");
    let max_len = super::DISCORD_MSG_LIMIT.saturating_sub(suffix.len());
    let base = if body.len() > max_len {
        let safe_end = super::formatting::floor_char_boundary(body, max_len);
        &body[..safe_end]
    } else {
        body
    }
    .trim_end();
    format!("{base}{suffix}")
}

pub(in crate::services::discord) fn finalize_streaming_footer_with_completion(
    last_edit_text: &str,
    provider: &super::ProviderKind,
    completion_block: Option<&str>,
) -> Option<String> {
    let cleaned = completion_footer_base_body(last_edit_text, provider);
    let finalized = compose_completion_footer_text(&cleaned, completion_block);
    if finalized.trim().is_empty() {
        None
    } else if finalized == last_edit_text {
        None
    } else {
        Some(finalized)
    }
}

pub(in crate::services::discord) fn completion_footer_base_body(
    text: &str,
    provider: &super::ProviderKind,
) -> String {
    strip_streaming_footer(text, provider).unwrap_or_else(|| text.trim_end().to_string())
}

pub(in crate::services::discord) fn register_completion_footer_target(
    channel_id: ChannelId,
    message_id: MessageId,
    provider: &super::ProviderKind,
    registered_at_unix: i64,
    base_body: &str,
    has_unfinished_entries: bool,
) {
    let mut guard = completion_footer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if has_unfinished_entries {
        guard.insert(
            channel_id.get(),
            RegisteredCompletionFooter {
                message_id,
                provider: provider.clone(),
                base_body: completion_footer_base_body(base_body, provider),
                registered_at_unix,
                consecutive_edit_failures: 0,
            },
        );
    } else {
        guard.remove(&channel_id.get());
    }
}

#[cfg(test)]
pub(in crate::services::discord) fn completion_footer_registered_failure_count(
    channel_id: ChannelId,
) -> Option<u8> {
    completion_footer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&channel_id.get())
        .map(|target| target.consecutive_edit_failures)
}

pub(in crate::services::discord) fn completion_footer_has_registered_target(
    channel_id: ChannelId,
) -> bool {
    completion_footer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains_key(&channel_id.get())
}

pub(in crate::services::discord) fn completion_footer_edit_for_registered_target(
    shared: &super::SharedData,
    channel_id: ChannelId,
    indicator: &str,
) -> Option<CompletionFooterEdit> {
    completion_footer_edit_for_registered_target_at(
        shared,
        channel_id,
        indicator,
        chrono::Utc::now().timestamp(),
    )
}

pub(in crate::services::discord) fn completion_footer_edit_for_registered_target_at(
    shared: &super::SharedData,
    channel_id: ChannelId,
    indicator: &str,
    now_unix: i64,
) -> Option<CompletionFooterEdit> {
    let target = completion_footer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&channel_id.get())
        .cloned()?;
    let idle_expired =
        completion_footer_idle_animation_expired(target.registered_at_unix, now_unix);
    let render_indicator = if idle_expired {
        COMPLETION_FOOTER_IDLE_EXPIRED_INDICATOR
    } else {
        indicator
    };
    let rendered = shared.placeholder_live_events.render_completion_footer(
        channel_id,
        &target.provider,
        render_indicator,
    );
    let text = compose_completion_footer_text(&target.base_body, rendered.block.as_deref());
    if text.trim().is_empty() {
        if idle_expired {
            completion_footer_forget_registered_target(channel_id);
        }
        return None;
    }
    Some(CompletionFooterEdit {
        message_id: target.message_id,
        text,
        remove_after_edit: idle_expired || !rendered.has_unfinished_entries,
    })
}

fn completion_footer_idle_animation_expired(registered_at_unix: i64, now_unix: i64) -> bool {
    now_unix.saturating_sub(registered_at_unix) >= COMPLETION_FOOTER_MAX_IDLE_ANIMATION_SECS
}

pub(in crate::services::discord) fn completion_footer_record_edit_result(
    channel_id: ChannelId,
    remove_after_edit: bool,
    edited: bool,
) {
    if remove_after_edit {
        completion_footer_forget_registered_target(channel_id);
        return;
    }

    let mut guard = completion_footer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(target) = guard.get_mut(&channel_id.get()) else {
        return;
    };
    if edited {
        target.consecutive_edit_failures = 0;
        return;
    }
    target.consecutive_edit_failures = target.consecutive_edit_failures.saturating_add(1);
    if target.consecutive_edit_failures >= COMPLETION_FOOTER_MAX_CONSECUTIVE_EDIT_FAILURES {
        guard.remove(&channel_id.get());
    }
}

pub(in crate::services::discord) fn completion_footer_forget_registered_target(
    channel_id: ChannelId,
) {
    completion_footer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&channel_id.get());
}

fn compose_merged_footer_status_block(indicator: &str, panel_text: &str) -> Option<String> {
    let (header_line, panel_body) = panel_text.split_once('\n').unwrap_or((panel_text, ""));
    let header = merged_footer_header_line(indicator, header_line)?;
    let panel_body = clamp_footer_panel_text(panel_body);
    if panel_body.trim().is_empty() {
        Some(header)
    } else {
        Some(format!("{header}\n{panel_body}"))
    }
}

fn merged_footer_header_line(indicator: &str, header_line: &str) -> Option<String> {
    let header = strip_panel_header_status_marker(header_line)?;
    if header.is_empty() {
        None
    } else {
        Some(format!("{indicator} {header}"))
    }
}

fn strip_panel_header_status_marker(header_line: &str) -> Option<&str> {
    let header_line = header_line.trim();
    if header_line.is_empty() {
        return None;
    }

    let mut chars = header_line.char_indices();
    let (_, first) = chars.next()?;
    let rest_start = chars
        .next()
        .map(|(idx, _)| idx)
        .unwrap_or(header_line.len());
    if is_panel_header_status_marker(first) {
        Some(header_line[rest_start..].trim_start())
    } else {
        Some(header_line)
    }
}

fn is_panel_header_status_marker(marker: char) -> bool {
    matches!(marker, '🟢' | '💤' | '⏰' | '✅' | '🔧' | '🧵' | '🧬')
}

fn clamp_footer_panel_text(panel_text: &str) -> String {
    if panel_text.len() <= SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES {
        return panel_text.to_string();
    }

    const TRUNCATION_MARKER: &str = "…";
    if SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES <= TRUNCATION_MARKER.len() {
        let safe_end = super::formatting::floor_char_boundary(
            TRUNCATION_MARKER,
            SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES,
        );
        return TRUNCATION_MARKER[..safe_end].to_string();
    }

    let lines: Vec<&str> = panel_text.lines().collect();
    for keep_count in (1..=lines.len()).rev() {
        let prefix = lines[..keep_count].join("\n");
        let candidate = format!("{prefix}\n{TRUNCATION_MARKER}");
        if candidate.len() <= SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES {
            return candidate;
        }
    }

    let first_line = lines.first().copied().unwrap_or_default();
    let first_line_budget = SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES
        .saturating_sub(TRUNCATION_MARKER.len())
        .saturating_sub(1);
    let safe_end = super::formatting::floor_char_boundary(first_line, first_line_budget);
    if safe_end == 0 {
        TRUNCATION_MARKER.to_string()
    } else {
        format!("{}\n{TRUNCATION_MARKER}", &first_line[..safe_end])
    }
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
    if footer_starts_with_spinner(last_edit_text) {
        return Some(String::new());
    }

    if completion_footer_starts(last_edit_text) {
        return Some(String::new());
    }

    if let Some(cleaned) = strip_completion_footer(last_edit_text, provider) {
        return Some(cleaned);
    }

    if let Some((body, _footer)) = split_footer(last_edit_text) {
        let cleaned = super::formatting::format_for_discord_with_status_panel(body, provider);
        return if cleaned == last_edit_text {
            None
        } else {
            Some(cleaned)
        };
    }

    if let Some(finalized) =
        super::formatting::finalize_stale_streaming_footer(last_edit_text, provider)
    {
        return Some(finalized);
    }

    None
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

fn strip_completion_footer(text: &str, provider: &super::ProviderKind) -> Option<String> {
    let mut search_end = text.len();
    while let Some(idx) = text[..search_end].rfind("\n\n") {
        let body = &text[..idx];
        let footer = &text[(idx + 2)..];
        if completion_footer_starts_after_body(footer, body) {
            if completion_footer_first_line_is_section_header(footer)
                && (body_ends_with_completion_context_line(body)
                    || body_ends_with_single_message_footer_status_line(body))
            {
                search_end = idx;
                continue;
            }
            return Some(super::formatting::format_for_discord_with_status_panel(
                body, provider,
            ));
        }
        search_end = idx;
    }

    None
}

fn completion_footer_starts(footer: &str) -> bool {
    let mut lines = footer.lines().filter(|line| !line.trim().is_empty());
    let Some(first) = lines.next().map(str::trim) else {
        return false;
    };
    completion_footer_context_line(first)
        || (completion_footer_section_header(first) && completion_footer_has_slot_shape(footer))
}

fn completion_footer_starts_after_body(footer: &str, body: &str) -> bool {
    let mut lines = footer.lines().filter(|line| !line.trim().is_empty());
    let Some(first) = lines.next().map(str::trim) else {
        return false;
    };
    if completion_footer_context_line(first) {
        return true;
    }
    completion_footer_section_header(first)
        && (completion_footer_has_slot_shape(footer)
            || body_ends_with_completion_context_line(body)
            || body_ends_with_single_message_footer_status_line(body))
}

fn completion_footer_context_line(line: &str) -> bool {
    line.starts_with("Context   ")
}

fn completion_footer_section_header(line: &str) -> bool {
    line == "Tasks" || line == "Subagents"
}

fn completion_footer_has_slot_shape(footer: &str) -> bool {
    footer.lines().any(|line| line.starts_with("└ "))
}

fn completion_footer_first_line_is_section_header(footer: &str) -> bool {
    footer
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .is_some_and(completion_footer_section_header)
}

fn body_ends_with_completion_context_line(body: &str) -> bool {
    body.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .is_some_and(completion_footer_context_line)
}

fn body_ends_with_single_message_footer_status_line(body: &str) -> bool {
    body.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .is_some_and(is_single_message_footer_status_line)
}

fn footer_starts_with_spinner(footer: &str) -> bool {
    let Some(first_footer_line) = footer.lines().find(|line| !line.trim().is_empty()) else {
        return false;
    };
    is_single_message_footer_status_line(first_footer_line.trim())
}

fn is_single_message_footer_status_line(line: &str) -> bool {
    super::formatting::is_streaming_placeholder_status_line(line)
        || is_merged_footer_status_line(line)
}

fn is_merged_footer_status_line(line: &str) -> bool {
    let Some(status) = strip_footer_braille_spinner_prefix(line) else {
        return false;
    };
    status.contains(" — ")
        && status.contains("(<t:")
        && (status.starts_with("진행 중")
            || status.starts_with("monitor 대기")
            || status.starts_with("scheduled wakeup")
            || status.starts_with("**백그라운드 완료**")
            || status.starts_with("**응답 완료**")
            || status.starts_with("도구 실행 중")
            || status.starts_with("subagent 실행 중")
            || status.starts_with("workflow 실행 중"))
}

fn strip_footer_braille_spinner_prefix(line: &str) -> Option<&str> {
    const BRAILLE_SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

    let mut chars = line.chars();
    let first = chars.next()?;
    if !BRAILLE_SPINNER_FRAMES.contains(&first) || !chars.next().is_some_and(char::is_whitespace) {
        return None;
    }
    Some(chars.as_str().trim())
}

#[cfg(test)]
mod tests {
    use super::super::DISCORD_MSG_LIMIT;
    use super::super::ProviderKind;
    use crate::services::agent_protocol::StatusEvent;
    use poise::serenity_prelude::{ChannelId, MessageId};

    fn panel_portion(status_block: &str) -> &str {
        status_block
            .split_once('\n')
            .map(|(_, panel)| panel)
            .unwrap_or("")
    }

    fn footer_header(status_block: &str) -> &str {
        status_block.lines().next().unwrap_or("")
    }

    fn push_unfinished_subagent(channel_id: ChannelId) -> std::sync::Arc<super::super::SharedData> {
        let shared = super::super::make_shared_data_for_tests();
        shared.placeholder_live_events.push_status_event(
            channel_id,
            StatusEvent::SubagentStart {
                subagent_type: Some("reviewer".to_string()),
                desc: Some("Long background job".to_string()),
                tool_use_id: Some(format!("tool-{}", channel_id.get())),
                background: true,
            },
        );
        shared
    }

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

        assert!(block.starts_with("⠸ 진행 중 — Claude (<t:1700000000:R>)"));
        assert!(!footer_header(&block).contains('🟢'));
        assert!(!block.contains("계속 처리 중"));
        assert!(block.contains("\n\nSubagents\n└ review inspect"));
    }

    #[test]
    fn footer_status_block_empty_panel_falls_back_to_processing_line() {
        assert_eq!(
            super::compose_footer_status_block("⠸", ""),
            "⠸ 계속 처리 중"
        );
        assert_eq!(
            super::compose_footer_status_block("⠸", " \n\t "),
            "⠸ 계속 처리 중"
        );
    }

    #[test]
    fn footer_panel_under_budget_is_unchanged_s3() {
        let panel = "Header\n\nTools\n└ cargo test";
        let block = super::compose_footer_status_block("⠸", panel);

        assert_eq!(block, "⠸ Header\n\nTools\n└ cargo test");
        assert!(!panel_portion(&block).ends_with("\n…"));
    }

    #[test]
    fn footer_panel_over_budget_excludes_merged_header_from_budget_s3() {
        let huge_panel = format!(
            "🟢 진행 중 — Claude (<t:1700000000:R>)\n{}\n{}\n{}",
            "a".repeat(290),
            "b".repeat(290),
            "c".repeat(100)
        );
        let block = super::compose_footer_status_block("⠸", &huge_panel);
        let (header, panel) = block
            .split_once('\n')
            .expect("over-budget panel should keep merged header and panel body");

        assert_eq!(header, "⠸ 진행 중 — Claude (<t:1700000000:R>)");
        assert!(panel.len() <= super::SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES);
        assert!(panel.ends_with("\n…") || panel == "…");
        assert!(block.len() > super::SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES);
    }

    #[test]
    fn footer_panel_truncates_on_line_boundaries_s3() {
        let second = "a".repeat(250);
        let third = "b".repeat(250);
        let fourth = "c".repeat(250);
        let panel =
            format!("🟢 진행 중 — Claude (<t:1700000000:R>)\n\n{second}\n{third}\n{fourth}");
        let block = super::compose_footer_status_block("⠸", &panel);
        let truncated_lines: Vec<&str> = panel_portion(&block).lines().collect();

        assert_eq!(
            truncated_lines,
            vec!["", second.as_str(), third.as_str(), "…"]
        );
        assert!(!panel_portion(&block).contains(fourth.as_str()));
        assert!(panel_portion(&block).len() <= super::SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES);
    }

    #[test]
    fn footer_panel_byte_clamps_first_line_on_char_boundary_s3() {
        let panel_body_first_line = "가🙂".repeat(200);
        let panel = format!(
            "🟢 진행 중 — Claude (<t:1700000000:R>)\n{panel_body_first_line}\nSubagents\n└ reviewer inspect"
        );
        let block = super::compose_footer_status_block("⠸", &panel);
        let panel = panel_portion(&block);
        let panel_lines: Vec<&str> = panel.lines().collect();

        assert!(std::str::from_utf8(panel.as_bytes()).is_ok());
        assert!(panel.len() <= super::SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES);
        assert_eq!(panel_lines.last().copied(), Some("…"));
        assert_eq!(panel_lines.len(), 2);
        assert!(!panel.contains("Subagents"));
    }

    #[test]
    fn footer_rollover_reservation_is_bound_by_panel_budget_s3() {
        const STREAMING_PLACEHOLDER_MARGIN_BYTES: usize = 10;

        let huge_panel = format!(
            "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nTools\n{}",
            "└ cargo test --lib single_message_panel ".repeat(120)
        );
        let status_block = super::compose_footer_status_block("⠸", &huge_panel);
        let merged_header = footer_header(&status_block);
        let max_footer_len =
            2 + merged_header.len() + 1 + super::SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES;
        let footer = format!("\n\n{status_block}");
        let expected_body_budget = DISCORD_MSG_LIMIT
            .saturating_sub(footer.len() + STREAMING_PLACEHOLDER_MARGIN_BYTES)
            .max(1);
        let minimum_body_budget = DISCORD_MSG_LIMIT
            .saturating_sub(max_footer_len + STREAMING_PLACEHOLDER_MARGIN_BYTES)
            .max(1);
        let current_portion = "x".repeat(expected_body_budget + 1);
        let plan =
            super::super::formatting::plan_streaming_rollover(&current_portion, &status_block)
                .expect("body should roll over after reserving the bounded footer");

        assert!(footer.len() <= max_footer_len);
        assert_eq!(plan.split_at, expected_body_budget);
        assert!(plan.split_at >= minimum_body_budget);
        assert!(plan.display_snapshot.ends_with(&footer));
    }

    #[test]
    fn footer_rollover_seed_carries_merged_header_but_frozen_chunk_does_not() {
        let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nTools\n└ cargo test --lib single_message_panel";
        let status_block = super::compose_footer_status_block("⠸", panel);
        let current_portion = "streamed body ".repeat(220);
        let plan =
            super::super::formatting::plan_streaming_rollover(&current_portion, &status_block)
                .expect("body should roll over after reserving the footer");
        let seed = super::super::formatting::build_streaming_placeholder_text("", &status_block);

        assert!(seed.starts_with("⠸ 진행 중 — Claude (<t:1700000000:R>)"));
        assert!(seed.contains("Tools\n└ cargo test --lib single_message_panel"));
        assert!(!plan.frozen_chunk.contains("진행 중 — Claude"));
        assert!(!plan.frozen_chunk.contains("Tools\n└ cargo test"));
        assert!(
            plan.display_snapshot
                .ends_with(&format!("\n\n{status_block}"))
        );
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
        assert!(!finalized.contains("진행 중 — Claude"));
        assert!(!finalized.contains("Subagents"));
    }

    #[test]
    fn terminal_footer_strip_preserves_body_text_that_mentions_running_status() {
        let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
        let body = "Final answer\n\n본문에 진행 중 문구가 있어도 유지";
        let rendered = format!(
            "{body}\n\n{}",
            super::compose_footer_status_block("⠸", panel)
        );
        let finalized = super::finalize_streaming_footer(&rendered, &ProviderKind::Claude)
            .expect("merged footer should strip at terminal reconciliation");

        assert_eq!(finalized, body);
    }

    #[test]
    fn footer_mode_strip_preserves_spinner_prefixed_user_body_without_panel_timestamp() {
        let body = "Final answer\n\n⠋ 진행 중 — user-authored line";

        assert_eq!(
            super::strip_streaming_footer(body, &ProviderKind::Claude),
            None
        );
        assert_eq!(
            super::finalize_streaming_footer(body, &ProviderKind::Claude),
            None
        );
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
    fn terminal_footer_replacement_keeps_completion_context_block() {
        let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
        let rendered = format!(
            "Final answer\n\n{}",
            super::compose_footer_status_block("⠸", panel)
        );
        let completion = "Context   📦 154.6k / 1.0M tokens (15%) · auto-compact 60%";

        let finalized = super::finalize_streaming_footer_with_completion(
            &rendered,
            &ProviderKind::Claude,
            Some(completion),
        )
        .expect("streaming footer should be replaced by completion block");

        assert_eq!(finalized, format!("Final answer\n\n{completion}"));
        assert!(!finalized.contains("진행 중 — Claude"));
        assert!(!finalized.contains("Subagents"));
    }

    #[test]
    fn completion_footer_strip_supports_suppression_exposure_test() {
        let completion = "Context   📦 154.6k / 1.0M tokens (15%) · auto-compact 60%\n\nSubagents\n└ bgworker Long job ✓";

        assert_eq!(
            super::strip_streaming_footer(completion, &ProviderKind::Claude),
            Some(String::new())
        );
        assert_eq!(
            super::strip_streaming_footer(
                &format!("visible assistant body\n\n{completion}"),
                &ProviderKind::Claude,
            ),
            Some("visible assistant body".to_string())
        );
    }

    #[test]
    fn completion_footer_strip_preserves_bare_user_section_heading_without_slot_evidence() {
        let body = "visible assistant body\n\nSubagents\n- user-authored note";

        assert_eq!(
            super::strip_streaming_footer(body, &ProviderKind::Claude),
            None
        );
        assert_eq!(
            super::finalize_streaming_footer(body, &ProviderKind::Claude),
            None
        );
    }

    #[test]
    fn completion_footer_strip_still_removes_real_slot_section() {
        let body = "visible assistant body\n\nSubagents\n└ bgworker Long job ✓";

        assert_eq!(
            super::strip_streaming_footer(body, &ProviderKind::Claude),
            Some("visible assistant body".to_string())
        );
    }

    #[test]
    fn completion_footer_ttl_freezes_unfinished_entries_then_forgets_target() {
        let channel_id = ChannelId::new(3_089_001);
        super::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let now = 1_800_000_000;
        super::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_101),
            &ProviderKind::Claude,
            now - super::COMPLETION_FOOTER_MAX_IDLE_ANIMATION_SECS - 1,
            "Final answer",
            true,
        );

        let edit = super::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            now,
        )
        .expect("expired unfinished footer should render one freeze edit");

        assert!(edit.remove_after_edit);
        assert!(edit.text.contains("Subagents\n└ "));
        assert!(edit.text.contains('…'));
        assert!(!edit.text.contains('⠸'));
        assert!(!edit.text.contains('✓'));

        super::completion_footer_record_edit_result(channel_id, edit.remove_after_edit, true);
        assert!(!super::completion_footer_has_registered_target(channel_id));
    }

    #[test]
    fn completion_footer_below_ttl_keeps_animating_registered_target() {
        let channel_id = ChannelId::new(3_089_002);
        super::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let now = 1_800_000_000;
        super::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_102),
            &ProviderKind::Claude,
            now - super::COMPLETION_FOOTER_MAX_IDLE_ANIMATION_SECS + 1,
            "Final answer",
            true,
        );

        let edit = super::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            now,
        )
        .expect("non-expired unfinished footer should render an animated edit");

        assert!(!edit.remove_after_edit);
        assert!(edit.text.contains("Subagents\n└ "));
        assert!(edit.text.contains('⠸'));

        super::completion_footer_record_edit_result(channel_id, edit.remove_after_edit, true);
        assert!(super::completion_footer_has_registered_target(channel_id));
        super::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn completion_footer_consecutive_edit_failures_evict_registered_target() {
        let channel_id = ChannelId::new(3_089_003);
        super::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        super::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_103),
            &ProviderKind::Claude,
            1_800_000_000,
            "Final answer",
            true,
        );

        for expected_failures in 1..super::COMPLETION_FOOTER_MAX_CONSECUTIVE_EDIT_FAILURES {
            super::completion_footer_record_edit_result(channel_id, false, false);
            assert_eq!(
                super::completion_footer_registered_failure_count(channel_id),
                Some(expected_failures)
            );
            assert!(super::completion_footer_has_registered_target(channel_id));
        }

        super::completion_footer_record_edit_result(channel_id, false, false);
        assert!(!super::completion_footer_has_registered_target(channel_id));
        assert_eq!(
            super::completion_footer_edit_for_registered_target_at(
                shared.as_ref(),
                channel_id,
                "⠸",
                1_800_000_005,
            ),
            None
        );
    }

    #[test]
    fn completion_footer_forget_registered_target_suppresses_future_edits() {
        let channel_id = ChannelId::new(3_089_004);
        super::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        super::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_104),
            &ProviderKind::Claude,
            1_800_000_000,
            "Final answer",
            true,
        );

        super::completion_footer_forget_registered_target(channel_id);

        assert_eq!(
            super::completion_footer_edit_for_registered_target_at(
                shared.as_ref(),
                channel_id,
                "⠸",
                1_800_000_005,
            ),
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
