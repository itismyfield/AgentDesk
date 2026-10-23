//! Producer-selected display metadata; the outbox row retains its original delivery identity.

use super::{OutboxEnqueueError, OutboxMessage};
use crate::db::agents::AgentChannelBindings;
use crate::services::discord::{session_identity::SessionIdentity, session_notice};
use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};
use serde_json::{Value, json};
use sqlx::{PgConnection, Postgres, Row, Transaction};

fn eligible(message: OutboxMessage<'_>) -> bool {
    matches!(
        (message.source, message.reason_code),
        (
            "lifecycle_notifier",
            Some(
                "lifecycle.monitor_auto_turn"
                    | "lifecycle.monitor_auto_turn.deferred"
                    | "lifecycle.session_resume_failed"
                    | "lifecycle.context_compacted"
                    | "lifecycle.mcp_credential_change"
                    | "lifecycle.soft_clear"
                    | "codex_goal_clear_active_turn"
                    | "codex_goal_clear"
                    | "codex_goal_pause_ignored"
                    | "codex_goal_resume_ignored"
                    | "codex_local_control"
                    | "codex_unsupported_control"
            )
        ) | ("queue_overflow_notice", Some("queue_overflow.evict"))
            | ("adopt_fence_forward_notice", Some("adopt.fence_forward"))
            | ("boot_custody_notice", Some("boot_custody.notice"))
            | ("stall_watchdog", Some("stall_watchdog_suspected_stall"))
    )
}

fn channel_target(target: &str) -> Option<String> {
    let channel = target
        .trim()
        .strip_prefix("channel:")?
        .parse::<u64>()
        .ok()?;
    (channel != 0).then(|| channel.to_string())
}

fn locator_hint(session_key: Option<&str>) -> Option<ProviderKind> {
    let key = session_key?.trim();
    if let Some(custody) = key.strip_prefix("boot_custody/") {
        let (provider, episode) = custody.split_once('/')?;
        return (!episode.is_empty() && !episode.contains('/'))
            .then(|| ProviderKind::from_str(provider))
            .flatten();
    }
    let identity = SessionIdentity::parse(key)?;
    let (provider, channel) = parse_provider_and_channel_from_tmux_name(&identity.tmux_name)?;
    if channel.is_empty()
        || channel.starts_with("scheduled-")
        || !identity
            .tmux_name
            .starts_with(&format!("AgentDesk-{}-", provider.as_str()))
        || identity
            .provider_from_key
            .as_deref()
            .is_some_and(|named| named != provider.as_str())
    {
        return None;
    }
    Some(provider)
}

async fn provider_hint(
    connection: &mut PgConnection,
    channel: &str,
    session_key: Option<&str>,
) -> Result<Option<ProviderKind>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
           FROM agents
          WHERE $1 IN (BTRIM(discord_channel_id), BTRIM(discord_channel_alt),
                       BTRIM(discord_channel_cc), BTRIM(discord_channel_cdx))",
    )
    .bind(channel)
    .fetch_all(connection)
    .await?;
    if rows.is_empty() {
        return Ok(locator_hint(session_key));
    }
    let mut selected = None;
    for row in rows {
        let bindings = AgentChannelBindings {
            provider: row.try_get("provider")?,
            discord_channel_id: row.try_get("discord_channel_id")?,
            discord_channel_alt: row.try_get("discord_channel_alt")?,
            discord_channel_cc: row.try_get("discord_channel_cc")?,
            discord_channel_cdx: row.try_get("discord_channel_cdx")?,
        };
        let Some(provider) = bindings.provider_for_channel(|bound| bound == channel) else {
            return Ok(None);
        };
        if selected
            .as_ref()
            .is_some_and(|previous| previous != &provider)
        {
            return Ok(None);
        }
        selected = Some(provider);
    }
    Ok(selected)
}

fn display_content(content: &str, completion: bool) -> String {
    let content = content.trim();
    if session_notice::is_inline(content) {
        return session_notice::finish(content.to_string());
    }
    let content = content.strip_prefix("-# ").unwrap_or(content);
    let leading_icon = [
        "⚠️", "⚠", "🔔", "♻️", "♻", "📦", "🧹", "⏏️", "⏏", "✅", "❌", "🛑", "📡", "⌨️", "📋",
        "🔄", "ℹ️", "ℹ", "🚨", "⏸️", "⏸", "🔒", "🔑",
    ]
    .into_iter()
    .find_map(|icon| {
        content
            .strip_prefix(icon)
            .map(|detail| (icon, detail.trim_start()))
    });
    let (icon, detail) = leading_icon.unwrap_or((if completion { "✅" } else { "ℹ️" }, content));
    session_notice::subtext(icon, detail)
}

pub(super) async fn designation(
    connection: &mut PgConnection,
    message: OutboxMessage<'_>,
    has_attachment: bool,
    explicit_completion: bool,
) -> Result<Option<Value>, sqlx::Error> {
    let completion = explicit_completion
        && message.source == "system"
        && message.reason_code.is_none()
        && message.bot == "notify";
    if has_attachment || !(eligible(message) || completion) {
        return Ok(None);
    }
    let Some(channel) = channel_target(message.target) else {
        return Ok(None);
    };
    let provider = provider_hint(connection, &channel, message.session_key).await?;
    Ok(Some(json!({
        "version": 1,
        "provider_hint": provider.map(|provider| provider.as_str().to_string()),
        "content": display_content(message.content, completion),
    })))
}

/// Autoqueue completion explicitly opts in without changing its source or dedupe inputs.
pub(crate) async fn enqueue_session_notice_pg_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    message: OutboxMessage<'_>,
) -> Result<i64, OutboxEnqueueError> {
    super::enqueue_plain_on_tx(tx, message, None, true).await
}
