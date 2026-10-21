//! Exact registered-runtime receipts; an invalid historical row never proves foreign ownership.
use std::collections::{BTreeMap, BTreeSet};

use crate::services::discord::adk_session;
use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReceiptRuntime {
    pub provider: ProviderKind,
    pub token_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReceiptRow {
    pub id: String,
    pub turn_id: Option<String>,
    pub channel_id: Option<String>,
}

impl ReceiptRow {
    fn started_channel(&self) -> Option<u64> {
        self.turn_id
            .as_ref()
            .filter(|turn| !turn.trim().is_empty())?;
        self.channel_id.as_ref()?.parse().ok()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct KeyReceipts {
    pub name: String,
    pub rows: Vec<ReceiptRow>,
}

impl KeyReceipts {
    pub(super) fn channels(&self) -> BTreeSet<u64> {
        self.rows
            .iter()
            .filter_map(ReceiptRow::started_channel)
            .collect()
    }

    pub(super) fn invalid(&self) -> bool {
        self.rows.iter().any(|row| row.started_channel().is_none())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RoutineEvidence {
    /// Empty row vectors preserve that an exact registered key was queried and had no receipt.
    pub keys: BTreeMap<String, KeyReceipts>,
}

impl RoutineEvidence {
    pub(super) fn for_name<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a KeyReceipts> {
        self.keys.values().filter(move |proof| proof.name == name)
    }

    pub(super) fn self_claim(&self, name: &str, target: u64) -> bool {
        self.for_name(name)
            .any(|proof| proof.channels().contains(&target))
    }
}

pub(super) async fn routine_turn_evidence(
    pool: &sqlx::PgPool,
    runtimes: &[ReceiptRuntime],
    names: &[String],
) -> Result<RoutineEvidence, String> {
    let mut keys = BTreeMap::new();
    for name in names {
        let Some((provider, _)) = parse_provider_and_channel_from_tmux_name(name) else {
            continue;
        };
        for runtime in runtimes
            .iter()
            .filter(|runtime| runtime.provider == provider)
        {
            if runtime.token_hash.trim().is_empty() {
                return Err("registered runtime token identity unavailable".into());
            }
            let key =
                adk_session::build_namespaced_session_key(&runtime.token_hash, &provider, name);
            keys.entry(key).or_insert_with(|| KeyReceipts {
                name: name.clone(),
                rows: Vec::new(),
            });
        }
    }
    if keys.is_empty() {
        return Ok(RoutineEvidence { keys });
    }
    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, owned_tmux_session, turn_id, result_json ->> 'channel_id'
           FROM routine_runs
          WHERE owned_tmux_session = ANY($1)
          ORDER BY owned_tmux_session, id",
    )
    .bind(keys.keys().cloned().collect::<Vec<_>>())
    .fetch_all(pool)
    .await
    .map_err(|error| format!("routine ownership read: {error}"))?;
    for (id, key, turn_id, channel_id) in rows {
        let proof = keys.get_mut(&key).ok_or("foreign routine receipt key")?;
        proof.rows.push(ReceiptRow {
            id,
            turn_id,
            channel_id,
        });
    }
    Ok(RoutineEvidence { keys })
}

#[cfg(test)]
async fn routine_turn_channels(
    pool: &sqlx::PgPool,
    token_hash: &str,
    channel: u64,
    names: &[String],
) -> Result<BTreeMap<String, u64>, String> {
    let runtimes: Vec<_> = names
        .iter()
        .filter_map(|name| {
            let (provider, _) = parse_provider_and_channel_from_tmux_name(name)?;
            Some(ReceiptRuntime {
                provider,
                token_hash: token_hash.into(),
            })
        })
        .collect();
    let evidence = routine_turn_evidence(pool, &runtimes, names).await?;
    Ok(evidence
        .keys
        .into_values()
        .filter_map(|proof| {
            if proof.invalid() {
                return None;
            }
            let channels = proof.channels();
            let owner = if channels.contains(&channel) {
                channel
            } else {
                *channels.first()?
            };
            Some((proof.name, owner))
        })
        .collect())
}

#[cfg(test)]
#[path = "routine_owner_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "routine_registered_receipt_tests.rs"]
mod registered_receipt_tests;
