//! Shared single-line rendering and channel-runtime credentials for status notices.

use super::{ProviderKind, tui_task_card};
use poise::serenity_prelude::{ChannelId, Http};
use std::sync::Arc;

// Injected-prompt classification and input notices, re-imported by `tui_prompt_relay`.
pub(in crate::services::discord) mod injected_prompt_policy;
pub(in crate::services::discord) mod input_notice;
mod input_notice_memo;
pub(in crate::services::discord) mod observed_prompt_decision;

pub(super) fn preview(text: &str) -> String {
    let text = tui_task_card::strip_terminal_controls(text)
        .replace("\r\n", "\n")
        .replace(['\r', '\u{2028}', '\u{2029}'], "\n");
    let mut lines = text.trim().lines();
    let first = lines
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let remaining = lines.count();
    let mut escaped = String::new();
    for ch in first.chars() {
        match ch {
            '\\' | '`' | '*' | '_' | '~' | '|' | '[' | ']' | '#' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            '<' => escaped.push('‹'),
            '>' => escaped.push('›'),
            _ => escaped.push(ch),
        }
    }
    let mut escaped = tui_task_card::truncate_chars_ascii(&escaped, 600);
    if remaining > 0 {
        escaped.push_str(&format!(" (+{remaining}줄)"));
    }
    escaped
}

pub(crate) fn subtext(icon: &str, text: &str) -> String {
    finish(format!("-# {icon} {}", preview(text)))
}

pub(crate) fn finish(text: String) -> String {
    // Bound the preview before applying the shared Discord UTF-16 clamp.
    tui_task_card::clamp_discord_message_content(&tui_task_card::truncate_chars_ascii(&text, 900))
}

pub(crate) fn is_inline(text: &str) -> bool {
    text.starts_with("-# ")
        && !text
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
}

/// Channel runtime lookup for `provider_http`; the health registry implements it.
pub(super) trait ChannelRuntimeHttp {
    /// `None` when no single runtime owns the channel, `Some(None)` when it has no credentials.
    fn channel_runtime_http(
        &self,
        provider: &ProviderKind,
        channel: ChannelId,
    ) -> impl std::future::Future<Output = Option<Option<Arc<Http>>>> + Send;
}

pub(super) async fn provider_http(
    registry: &impl ChannelRuntimeHttp,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<Arc<Http>, String> {
    let shared = registry
        .channel_runtime_http(provider, channel)
        .await
        .ok_or_else(|| {
            format!(
                "channel provider runtime unavailable or ambiguous: {} on {channel}",
                provider.as_str()
            )
        })?;
    shared.ok_or_else(|| "channel provider credentials unavailable".to_string())
}
