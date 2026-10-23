//! Shared single-line rendering and channel-runtime credentials for status notices.

use super::{ProviderKind, health::HealthRegistry, tui_task_card};
use poise::serenity_prelude::{ChannelId, Http};
use std::sync::Arc;

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
    if remaining > 0 {
        escaped.push_str(&format!(" (+{remaining}줄)"));
    }
    escaped
}

pub(super) fn subtext(icon: &str, text: &str) -> String {
    finish(format!("-# {icon} {}", preview(text)))
}

pub(super) fn finish(text: String) -> String {
    // Bound the preview before applying the shared Discord UTF-16 clamp.
    tui_task_card::clamp_discord_message_content(&tui_task_card::truncate_chars_ascii(&text, 900))
}

pub(super) fn is_inline(text: &str) -> bool {
    text.starts_with("-# ")
        && !text
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
}

pub(super) async fn provider_http(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<Arc<Http>, String> {
    let shared = registry
        .shared_for_provider_on_channel(provider, channel)
        .await
        .ok_or_else(|| {
            format!(
                "channel provider runtime unavailable or ambiguous: {} on {channel}",
                provider.as_str()
            )
        })?;
    shared
        .serenity_http_or_token_fallback()
        .ok_or_else(|| "channel provider credentials unavailable".to_string())
}
