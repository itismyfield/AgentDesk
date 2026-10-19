//! Read-only routine-run evidence of the channel a live routine tmux session ran its turns in.
use std::collections::{BTreeMap, BTreeSet};

use crate::services::discord::adk_session;
use crate::services::provider::parse_provider_and_channel_from_tmux_name;

/// Each named session's recorded turn channel, `channel` itself if any run started there; a name
/// stays out unless every run recording its exact namespaced key started a turn in a known channel.
pub(super) async fn routine_turn_channels(
    pool: &sqlx::PgPool,
    token_hash: &str,
    channel: u64,
    names: &[String],
) -> Result<BTreeMap<String, u64>, String> {
    let keys: BTreeMap<String, &String> = names
        .iter()
        .filter_map(|name| {
            let (provider, _) = parse_provider_and_channel_from_tmux_name(name)?;
            let key = adk_session::build_namespaced_session_key(token_hash, &provider, name);
            Some((key, name))
        })
        .collect();
    if keys.is_empty() {
        return Ok(BTreeMap::new());
    }
    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT owned_tmux_session, turn_id, result_json ->> 'channel_id'
           FROM routine_runs
          WHERE owned_tmux_session = ANY($1)",
    )
    .bind(keys.keys().cloned().collect::<Vec<_>>())
    .fetch_all(pool)
    .await
    .map_err(|error| format!("routine ownership read: {error}"))?;
    let mut recorded: BTreeMap<String, Option<BTreeSet<u64>>> = BTreeMap::new();
    for (key, turn, turn_channel) in rows {
        let started = turn.is_some_and(|turn| !turn.trim().is_empty());
        let turn_channel = turn_channel.and_then(|id| id.parse::<u64>().ok());
        let entry = recorded.entry(key).or_insert_with(|| Some(BTreeSet::new()));
        match (entry.as_mut(), turn_channel.filter(|_| started)) {
            (Some(channels), Some(id)) => {
                channels.insert(id);
            }
            _ => *entry = None,
        }
    }
    Ok(recorded
        .into_iter()
        .filter_map(|(key, channels)| {
            let channels = channels?;
            let owner = if channels.contains(&channel) {
                channel
            } else {
                *channels.first()?
            };
            Some(((*keys.get(&key)?).clone(), owner))
        })
        .collect())
}

#[cfg(test)]
#[path = "routine_owner_tests.rs"]
mod tests;
