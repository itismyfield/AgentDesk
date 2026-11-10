//! Accepted iMessage input uses the existing durable outbox; display parsing never rejects input.
use crate::services::discord::bot_role::UtilityBotRole;

use crate::domain::human_input::{HumanInputDelivery, HumanInputError};
use crate::services::discord::outbound::source_registry::IMESSAGE_INPUT_NOTICE_SOURCE;
use crate::services::message_outbox::{self, OutboxMessage};
use sqlx::PgPool;

fn command(line: &str, prefix: &str) -> bool {
    let Some(tail) = line.trim().strip_prefix(prefix) else {
        return false;
    };
    let mut words = tail.split_whitespace();
    words.next() == Some("python3")
        && words
            .next()
            .is_some_and(|path| path == "imsg_reply.py" || path.ends_with("/imsg_reply.py"))
}

fn envelope(text: &str) -> Option<(String, String)> {
    let mut lines: Vec<_> = text.lines().collect();
    let first = lines.first()?.trim().strip_prefix('[')?.strip_suffix(']')?;
    let mut pieces = first.split('·').map(str::trim);
    if pieces.next()? != "아이메시지 요청" {
        return None;
    }
    let label = pieces
        .next()?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if label.is_empty() {
        return None;
    }
    let reply = pieces.any(|piece| piece.ends_with("답장"));
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if !command(lines.last()?, "회신:") {
        return None;
    }
    lines.pop();
    if reply {
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
        let reference = lines.last()?;
        if !command(reference, "답장 대상 원문:")
            || !reference.split_whitespace().any(|word| word == "--show")
        {
            return None;
        }
        lines.pop();
    }
    Some((label, lines.get(1..)?.join("\n")))
}

pub(super) fn notice_line(source: Option<&str>, text: &str) -> Option<String> {
    if source.map(str::trim) != Some("imessage") {
        return None;
    }
    let (label, body) = envelope(text).unwrap_or_else(|| ("iMessage".into(), text.into()));
    let text = format!("{label}: {body}")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    Some(crate::services::discord::input_notice_line(&text))
}

pub(super) fn not_echoed(line: Option<&str>, channel: u64, identity: &str, status: &str) {
    if line.is_some() {
        tracing::info!(
            reason = "imessage_input_not_echoed",
            status,
            channel,
            identity,
            "iMessage input was not echoed"
        );
    }
}

pub(super) async fn echo(pool: &PgPool, channel: u64, identity: &str, line: Option<&str>) {
    let Some(line) = line else { return };
    #[cfg(all(test, unix))]
    let _ = super::imessage_input_notice_tests::ENQUEUE_ATTEMPTS
        .try_with(|calls| calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst));
    let target = format!("channel:{channel}");
    let key = format!("imessage_input_notice:{channel}:{identity}");
    let message = OutboxMessage {
        target: &target,
        content: line,
        bot: UtilityBotRole::Notify.alias(),
        source: IMESSAGE_INPUT_NOTICE_SOURCE,
        reason_code: Some("imessage_input_notice"),
        session_key: Some(&key),
    };
    if let Err(error) =
        message_outbox::enqueue_outbox_pg_returning_id_with_persistent_dedupe(pool, message).await
    {
        tracing::warn!(reason = "imessage_input_not_echoed", status = "enqueue_failed", channel, identity, %error, "iMessage input echo enqueue failed");
    }
}

pub(super) async fn delivered(
    pool: &PgPool,
    channel: u64,
    identity: &str,
    line: Option<&str>,
    result: Result<HumanInputDelivery, HumanInputError>,
) -> Result<HumanInputDelivery, HumanInputError> {
    match &result {
        Ok(_) => echo(pool, channel, identity, line).await,
        Err(_) => not_echoed(line, channel, identity, "backend_error"),
    }
    result
}

#[cfg(test)]
#[path = "imessage_input_notice_display_tests.rs"]
mod tests;
