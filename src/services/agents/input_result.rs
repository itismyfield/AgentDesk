//! Which execution consumed an accepted input and how it ended, read only from owner records: its
//! queue file, its inflight row, then its `turns` receipt. A delivery response is never evidence.

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
    /// Admitted and still waiting in the channel's owner queue; no execution has taken it.
    Accepted {
        channel_id: u64,
    },
}

/// Reads queue files, inflight rows, then receipts, each writer committing the next before removing
/// the last; any terminal outranks a running row, which outranks a waiting one.
pub async fn lookup(
    pool: &PgPool,
    agent_id: &str,
    origin_id: &str,
    channels: &[OriginChannel],
) -> Result<Option<InputResult>, String> {
    let waiting = queued(origin_id, channels);
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
    let marked = matches!(running, Some(InputResult::Terminal(_)));
    let owed = matches!(waiting, Ok(Some(InputResult::Terminal(_))));
    match find_origin_receipt(pool, origin_id, agent_id, &channel_ids).await {
        Ok(Some(receipt)) => Ok(Some(InputResult::Terminal(receipt))),
        // A terminal not yet in the database: a marked row's, then a removal still owed.
        _ if marked => Ok(running),
        _ if owed => waiting,
        Ok(None) if running.is_some() => Ok(running),
        Ok(None) => waiting,
        Err(error) => Err(error.to_string()),
    }
}

/// An origin still in an owner queue file reads accepted; one removed unrun reads its owed receipt.
fn queued(origin_id: &str, channels: &[OriginChannel]) -> Result<Option<InputResult>, String> {
    use crate::services::turn_orchestrator::{Waiting, waiting_origin};
    for channel in channels {
        let id = channel.channel_id;
        match waiting_origin(&channel.provider, id, origin_id)? {
            Some(Waiting::Accepted) => return Ok(Some(InputResult::Accepted { channel_id: id })),
            Some(Waiting::Owed(owed)) => {
                let at = chrono::DateTime::from_timestamp_millis(owed.at_ms as i64)
                    .ok_or("owed receipt instant out of range")?;
                return Ok(Some(InputResult::Terminal(OriginReceipt {
                    turn_id: format!("discord:{id}:{}", owed.message_id),
                    channel_id: id.to_string(),
                    terminal_outcome: owed.outcome.as_str().to_string(),
                    terminal_confirmed_at: at,
                })));
            }
            None => {}
        }
    }
    Ok(None)
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
    let execution_id = format!("discord:{}:{message}", channel.channel_id);
    let marked = row.get("terminal_receipt").and_then(|mark| {
        let outcome = mark.get("outcome")?.as_str()?;
        let confirmed_at = mark.get("confirmed_at")?.as_str()?;
        Some((outcome.to_string(), confirmed_at.parse().ok()?))
    });
    Ok(Some(match marked {
        Some((terminal_outcome, terminal_confirmed_at)) => InputResult::Terminal(OriginReceipt {
            turn_id: execution_id,
            channel_id: channel.channel_id.to_string(),
            terminal_outcome,
            terminal_confirmed_at,
        }),
        None => InputResult::Running {
            channel_id: channel.channel_id,
            execution_id,
        },
    }))
}

#[cfg(test)]
#[path = "input_result_tests.rs"]
mod tests;
