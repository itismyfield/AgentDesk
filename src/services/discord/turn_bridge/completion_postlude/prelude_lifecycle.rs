use std::io::{BufRead, Seek};

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::provider::ProviderKind;

pub(super) fn completed(state: &super::CompletionPostludeState) -> bool {
    state.terminal_delivery_committed
        && !state.transport_error
        && !state.provider_failure
        && !state.cancelled
        && !state.recovery_retry
}

pub(super) fn confirmed_input(state: &InflightTurnState) -> bool {
    if !matches!(
        state.runtime_kind,
        Some(RuntimeHandoffKind::ClaudeTui | RuntimeHandoffKind::CodexTui)
    ) {
        return true;
    }
    let Some(context) = state.retry_prompt_snapshot() else {
        return false;
    };
    let Some(prompt) = context.expected_prompt.as_deref() else {
        return false;
    };
    let (Some(path), Some(offset)) = (state.output_path.as_deref(), state.turn_start_offset) else {
        return false;
    };
    exact_user_after(
        std::path::Path::new(path),
        offset,
        prompt,
        &state.started_at,
    )
}

fn exact_user_after(path: &std::path::Path, offset: u64, prompt: &str, started: &str) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    if !file.metadata().is_ok_and(|meta| meta.len() >= offset)
        || file.seek(std::io::SeekFrom::Start(offset)).is_err()
    {
        return false;
    }
    let Ok(started) = chrono::DateTime::parse_from_rfc3339(started) else {
        return false;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    while reader.read_line(&mut line).is_ok_and(|read| read > 0) {
        let matched = line.ends_with('\n')
            && serde_json::from_str::<serde_json::Value>(&line)
                .ok()
                .is_some_and(|record| {
                    let Some(timestamp) = record
                        .get("timestamp")
                        .and_then(|value| value.as_str())
                        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                    else {
                        return false;
                    };
                    if timestamp < started
                        || record.get("isMeta").and_then(|v| v.as_bool()) == Some(true)
                    {
                        return false;
                    }
                    let text = if record.get("type").and_then(|v| v.as_str()) == Some("user") {
                        text_content(record.pointer("/message/content"))
                    } else if record.get("type").and_then(|v| v.as_str()) == Some("event_msg")
                        && record.pointer("/payload/type").and_then(|v| v.as_str())
                            == Some("user_message")
                    {
                        text_content(record.pointer("/payload/message"))
                    } else {
                        None
                    };
                    text.is_some_and(|text| exact_body(&text, prompt))
                });
        if matched {
            return true;
        }
        line.clear();
    }
    false
}

fn text_content(value: Option<&serde_json::Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return Some(text.to_owned());
    }
    value
        .as_array()?
        .iter()
        .map(|block| {
            (block.get("type")?.as_str()? == "text").then_some(())?;
            block.get("text")?.as_str().map(str::to_owned)
        })
        .collect::<Option<Vec<_>>>()
        .map(|parts| parts.concat())
}

fn exact_body(body: &str, prompt: &str) -> bool {
    if body == prompt {
        return true;
    }
    let body = body
        .strip_prefix("\n\n")
        .and_then(|s| s.strip_suffix('\n'))
        .unwrap_or(body);
    let Some(rest) = body.strip_prefix("<pasted_content id=\"") else {
        return false;
    };
    let Some((id, rest)) = rest.split_at_checked(4) else {
        return false;
    };
    if !id
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return false;
    }
    rest.strip_prefix("\">\n")
        .and_then(|s| s.strip_suffix(&format!("\n</pasted_content id=\"{id}\">")))
        == Some(prompt)
}

pub(super) async fn settle(
    pool: Option<&sqlx::PgPool>,
    provider: &ProviderKind,
    state: &InflightTurnState,
    turn_id: &str,
) -> bool {
    if !confirmed_input(state) {
        return false;
    }
    let Some(context) = state.retry_prompt_snapshot() else {
        return true;
    };
    match context
        .consume_confirmed(pool, provider, state.channel_id, turn_id)
        .await
    {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, "prompt prelude remains stashed; next reminder generation deferred");
            false
        }
    }
}

#[cfg(test)]
mod tests;
