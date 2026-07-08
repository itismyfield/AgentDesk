//! #3038 S1 tmux watcher prompt observation helpers.

use super::*;

pub(super) fn observe_qwen_user_prompts_in_buffer(
    buffer: &str,
    provider: &crate::services::provider::ProviderKind,
    tmux_session_name: &str,
) {
    if !matches!(provider, crate::services::provider::ProviderKind::Qwen) {
        return;
    }
    for line in buffer.lines() {
        let _ = crate::services::qwen::observe_qwen_user_prompt_line(line, Some(tmux_session_name));
    }
}

pub(super) fn watcher_batch_contains_relayable_response(data: &[u8]) -> bool {
    let text = String::from_utf8_lossy(data);
    text.contains("\"type\":\"assistant\"")
        || text.contains("\"type\": \"assistant\"")
        || text.contains("\"type\":\"result\"")
        || text.contains("\"type\": \"result\"")
}

pub(super) fn watcher_batch_contains_assistant_event(data: &[u8]) -> bool {
    let text = String::from_utf8_lossy(data);
    text.contains("\"type\":\"assistant\"") || text.contains("\"type\": \"assistant\"")
}

pub(super) fn legacy_wrapper_prompt_candidates_from_pane(pane: &str) -> Vec<String> {
    let mut collecting = false;
    let mut current_block: Vec<String> = Vec::new();
    let mut last_submitted_block: Vec<String> = Vec::new();

    for raw_line in pane.lines() {
        let line = raw_line.trim_matches('\r').trim();
        if line.contains("Ready for input") {
            collecting = true;
            current_block.clear();
            continue;
        }
        if line == "[sending...]" {
            if collecting && !current_block.is_empty() {
                last_submitted_block = current_block.clone();
            }
            collecting = false;
            current_block.clear();
            continue;
        }
        if collecting && !line.is_empty() {
            current_block.push(line.to_string());
        }
    }

    if last_submitted_block.is_empty() {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    for candidate in [
        last_submitted_block.join(""),
        last_submitted_block.join(" "),
        last_submitted_block.join("\n"),
    ] {
        let candidate = candidate.trim();
        if candidate.is_empty() {
            continue;
        }
        if !candidates.iter().any(|existing: &String| {
            crate::services::tui_prompt_dedupe::prompts_match(existing, candidate)
        }) {
            candidates.push(candidate.to_string());
        }
    }
    candidates
}

pub(super) fn observe_legacy_wrapper_direct_prompt_from_pane(
    provider: &crate::services::provider::ProviderKind,
    tmux_session_name: &str,
    channel_id: serenity::ChannelId,
    data_start_offset: u64,
    current_offset: u64,
) -> crate::services::tui_prompt_dedupe::PromptObservation {
    let Some(pane) = crate::services::platform::tmux::capture_pane(tmux_session_name, -160) else {
        return crate::services::tui_prompt_dedupe::PromptObservation::Ignored;
    };
    let candidates = legacy_wrapper_prompt_candidates_from_pane(&pane);
    if candidates.is_empty() {
        return crate::services::tui_prompt_dedupe::PromptObservation::Ignored;
    }
    let observation =
        crate::services::tui_prompt_dedupe::observe_prompt_candidates_by_tmux_for_relay_lease(
            provider.as_str(),
            tmux_session_name,
            &candidates,
        );
    tracing::info!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session = %tmux_session_name,
        data_start_offset,
        current_offset,
        observation = ?observation,
        "watcher: observed legacy wrapper pane prompt before post-terminal suppression"
    );
    observation
}
