//! Which execution consumed an accepted input and how it ended, read only from owner records:
//! its inflight row, then its `turns` receipt. A delivery response is never evidence.

use serde_json::Value;
use sqlx::PgPool;

use crate::db::turns::{OriginReceipt, find_origin_receipt};

pub const MAX_ORIGIN_ID_LEN: usize = 256;

/// The metadata path every start carries its origin on, shared with the deliver route.
const ORIGIN_POINTER: &str = "/human_input/origin_id";

/// The origin a start request carried, if any.
pub(crate) fn metadata_origin_id(metadata: Option<&Value>) -> Option<String> {
    let origin = metadata?.pointer(ORIGIN_POINTER)?.as_str()?.trim();
    (!origin.is_empty() && origin.len() <= MAX_ORIGIN_ID_LEN).then(|| origin.to_string())
}

/// Places a start body's `origin_id` where the execution reads it; a blank one changes nothing.
pub(crate) fn start_metadata_with_origin(
    metadata: Option<Value>,
    origin_id: Option<&str>,
) -> Result<Option<Value>, &'static str> {
    let Some(origin) = origin_id.map(str::trim).filter(|origin| !origin.is_empty()) else {
        return Ok(metadata);
    };
    if origin.len() > MAX_ORIGIN_ID_LEN {
        return Err("origin_id_too_long");
    }
    let mut metadata = metadata.unwrap_or_else(|| serde_json::json!({}));
    let Some(fields) = metadata.as_object_mut() else {
        return Err("metadata_not_object");
    };
    let human = fields
        .entry("human_input")
        .or_insert_with(|| serde_json::json!({}));
    let Some(human) = human.as_object_mut() else {
        return Err("metadata_not_object");
    };
    match human.get("origin_id").and_then(Value::as_str) {
        Some(existing) if existing.trim() != origin => return Err("origin_id_conflict"),
        _ => {}
    }
    human.insert("origin_id".into(), Value::String(origin.to_string()));
    Ok(Some(metadata))
}

/// One channel of the agent and the provider whose inflight row would run there.
pub struct OriginChannel {
    pub provider: String,
    pub channel_id: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InputResult {
    /// An inflight row names the origin; no terminal is committed yet.
    Running {
        channel_id: u64,
        execution_id: String,
    },
    Terminal(OriginReceipt),
}

/// Reads the inflight rows before the receipts: a writer commits the receipt before it clears
/// the row, so an execution passing between the two reads is still found by the second.
pub async fn lookup(
    pool: &PgPool,
    agent_id: &str,
    origin_id: &str,
    channels: &[OriginChannel],
) -> Result<Option<InputResult>, String> {
    let mut running = None;
    for channel in channels {
        if running.is_none() {
            running = running_execution(channel, origin_id)?;
        }
    }
    let channel_ids: Vec<String> = channels
        .iter()
        .map(|channel| channel.channel_id.to_string())
        .collect();
    let receipt = find_origin_receipt(pool, origin_id, agent_id, &channel_ids)
        .await
        .map_err(|error| error.to_string())?;
    Ok(receipt.map(InputResult::Terminal).or(running))
}

fn running_execution(
    channel: &OriginChannel,
    origin_id: &str,
) -> Result<Option<InputResult>, String> {
    let root = crate::config::runtime_root().ok_or("runtime root unavailable")?;
    let path = root
        .join("runtime")
        .join("discord_inflight")
        .join(&channel.provider)
        .join(format!("{}.json", channel.channel_id));
    let data = match std::fs::read_to_string(&path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("inflight row unreadable: {error}")),
    };
    let row: Value =
        serde_json::from_str(&data).map_err(|error| format!("inflight row unparsable: {error}"))?;
    let names_origin = row
        .get("input_origin_ids")
        .and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(origin_id)));
    let message = row.get("user_msg_id").and_then(Value::as_u64).unwrap_or(0);
    if !names_origin || message == 0 {
        return Ok(None);
    }
    Ok(Some(InputResult::Running {
        channel_id: channel.channel_id,
        execution_id: format!("discord:{}:{message}", channel.channel_id),
    }))
}

#[cfg(test)]
#[path = "input_result_tests.rs"]
mod tests;
