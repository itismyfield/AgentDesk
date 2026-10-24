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
    match input_receipt(state) {
        Ok(confirmed) => confirmed,
        Err(reason) => {
            tracing::warn!(channel_id = state.channel_id, %reason,
                "input receipt unavailable; using normal completion for prelude settlement");
            true
        }
    }
}

fn input_receipt(state: &InflightTurnState) -> Result<bool, &'static str> {
    if !matches!(
        state.runtime_kind,
        Some(RuntimeHandoffKind::ClaudeTui | RuntimeHandoffKind::CodexTui)
    ) {
        return Ok(true);
    }
    let context = state
        .retry_prompt_snapshot()
        .ok_or("missing prompt snapshot")?;
    let prompt = context
        .expected_prompt
        .as_deref()
        .ok_or("missing expected prompt")?;
    let path = state.output_path.as_deref().ok_or("missing output path")?;
    let offset = state.turn_start_offset.ok_or("missing turn start offset")?;
    exact_user_after(
        std::path::Path::new(path),
        offset,
        prompt,
        &state.started_at,
        state.runtime_kind == Some(RuntimeHandoffKind::CodexTui),
    )
}

fn exact_user_after(
    path: &std::path::Path,
    offset: u64,
    prompt: &str,
    started: &str,
    codex: bool,
) -> Result<bool, &'static str> {
    let started = crate::services::discord::inflight::parse_started_at_unix(started)
        .ok_or("unparseable started_at")?;
    let mut file = std::fs::File::open(path).map_err(|_| "cannot open native transcript")?;
    if file
        .metadata()
        .map_err(|_| "cannot stat native transcript")?
        .len()
        < offset
    {
        return Err("native transcript shorter than turn start offset");
    }
    file.seek(std::io::SeekFrom::Start(offset))
        .map_err(|_| "cannot seek native transcript")?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    while reader
        .read_line(&mut line)
        .map_err(|_| "cannot read native transcript")?
        > 0
    {
        if line.ends_with('\n')
            && let Ok(record) = serde_json::from_str::<serde_json::Value>(&line)
        {
            let timestamp = record
                .get("timestamp")
                .and_then(|v| v.as_str())
                .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok());
            // Rebind relays contain normalized stream events, without native user timestamps.
            if codex && timestamp.is_none() {
                return Err("Codex normalized relay has no native input receipt");
            }
            if timestamp.is_some_and(|value| value.timestamp() >= started)
                && record.get("isMeta").and_then(|v| v.as_bool()) != Some(true)
            {
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
                if text.is_some_and(|text| exact_body(&text, prompt)) {
                    return Ok(true);
                }
            }
        }
        line.clear();
    }
    Ok(false)
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

#[cfg(test)]
mod settlement_pg_tests;

#[cfg(test)]
mod receipt_fallback_tests;
