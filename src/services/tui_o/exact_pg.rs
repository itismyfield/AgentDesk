use super::exact_episode::*;
use uuid::Uuid;

pub(crate) struct ConsistentEpisodeSnapshot {
    records: Vec<EpisodeMetadata>,
}
impl ConsistentEpisodeSnapshot {
    fn decode(payloads: Vec<serde_json::Value>) -> Self {
        Self {
            records: payloads
                .into_iter()
                .map(serde_json::from_value)
                .collect::<Result<_, _>>()
                .unwrap_or_default(),
        }
    }
    pub(super) fn records(&self) -> &[EpisodeMetadata] {
        &self.records
    }
    #[cfg(test)]
    pub(super) fn fixture(records: Vec<EpisodeMetadata>) -> Self {
        Self::decode(
            records
                .into_iter()
                .map(|r| serde_json::to_value(r).unwrap())
                .collect(),
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DurableEvidenceAck {
    pub record: Uuid,
    pub version: u32,
    pub digest: String,
}

// The disabled producer returns before acquiring a connection or creating a writer.
pub(crate) async fn record_episode_evidence(
    enabled: bool,
    pool: &sqlx::PgPool,
    metadata: &EpisodeMetadata,
) -> Result<Option<DurableEvidenceAck>, String> {
    if !enabled {
        return Ok(None);
    }
    if !metadata.supported() {
        return Err("unsupported strict envelope".into());
    }
    #[cfg(not(unix))]
    {
        let _ = pool;
        Err("strict journal unavailable".into())
    }
    #[cfg(unix)]
    {
        crate::services::discord::append_exact_metadata(pool.clone(), metadata).await?;
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(metadata).map_err(|e| e.to_string())?;
        Ok(Some(DurableEvidenceAck {
            record: metadata.record,
            version: metadata.version,
            digest: format!("{:x}", Sha256::digest(bytes)),
        }))
    }
}

pub(crate) async fn resolve_in_tx(
    connection: &mut sqlx::PgConnection,
    episode: Uuid,
) -> Result<StrictResolution, sqlx::Error> {
    let payloads: Vec<(serde_json::Value,)> = sqlx::query_as(
        "SELECT canonical_payload FROM public.delivery_journal_events
         WHERE canonical_payload->>'namespace' = $1 AND canonical_payload->>'episode' = $2
         ORDER BY event_id",
    )
    .bind(STRICT_NAMESPACE)
    .bind(episode.to_string())
    .fetch_all(&mut *connection)
    .await?;
    let snapshot =
        ConsistentEpisodeSnapshot::decode(payloads.into_iter().map(|(payload,)| payload).collect());
    Ok(resolve_strict(episode, &snapshot))
}

// This port performs no terminal SQL yet; C7 owns association locking and the existing CAS.
pub(crate) async fn guard_terminal_in_tx(
    connection: &mut sqlx::PgConnection,
    target: &DbTarget,
    intent: TerminalIntent,
) -> Result<TerminalDisposition, sqlx::Error> {
    let _resolution = resolve_in_tx(connection, target.episode).await?;
    let _ = intent;
    // Association projection and CAS are deliberately unavailable until activation.
    Ok(TerminalDisposition::Deferred)
}

#[cfg(test)]
#[path = "exact_pg_tests.rs"]
mod tests;
