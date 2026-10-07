//! Observation-only session binding: one SELECT snapshot, with ambiguous owners refused.

use super::agents::AgentChannelBindings;
use serde::Serialize;
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;

#[derive(Debug, Serialize)]
pub(crate) struct SessionEvidence {
    pub agent_id: String,
    pub channel_id: Option<String>,
    pub provider: Option<String>,
    pub session_key: Option<String>,
    pub raw_provider_session_id: Option<String>,
}

pub(crate) async fn load_session_evidence_pg(
    pool: &PgPool,
    identifier: &str,
) -> Result<Vec<SessionEvidence>, sqlx::Error> {
    // Only stored fixed-channel identities count; channel-less free slots are not evidence.
    let rows = sqlx::query(
        "SELECT a.id AS agent_id, a.provider AS agent_provider,
                a.discord_channel_id, a.discord_channel_alt,
                a.discord_channel_cc, a.discord_channel_cdx,
                s.provider, s.channel_id, s.session_key, s.raw_provider_session_id
         FROM agents a
         LEFT JOIN sessions s ON (s.agent_id = a.id OR s.agent_id IS NULL)
             AND s.thread_channel_id IS NULL
             AND s.identity_kind IS DISTINCT FROM 'scheduled_snapshot'
             AND s.session_key NOT LIKE '%:AgentDesk-' || s.provider || '-scheduled-%'
             AND s.channel_id IN (BTRIM(a.discord_channel_id), BTRIM(a.discord_channel_alt),
                                  BTRIM(a.discord_channel_cc), BTRIM(a.discord_channel_cdx))
         WHERE a.id = $1 OR $1 IN (BTRIM(a.discord_channel_id), BTRIM(a.discord_channel_alt),
                                  BTRIM(a.discord_channel_cc), BTRIM(a.discord_channel_cdx))
         ORDER BY a.id, s.id",
    )
    .bind(identifier)
    .fetch_all(pool)
    .await?;
    let mut targets = BTreeMap::new();
    let mut evidence = Vec::new();
    for row in rows {
        let agent_id: String = row.try_get("agent_id")?;
        let bindings = AgentChannelBindings {
            provider: row.try_get("agent_provider")?,
            discord_channel_id: row.try_get("discord_channel_id")?,
            discord_channel_alt: row.try_get("discord_channel_alt")?,
            discord_channel_cc: row.try_get("discord_channel_cc")?,
            discord_channel_cdx: row.try_get("discord_channel_cdx")?,
        };
        let channel_id = if agent_id == identifier {
            bindings.primary_channel()
        } else {
            Some(identifier.to_string())
        };
        let provider = channel_id
            .as_ref()
            .and_then(|channel| bindings.provider_for_channel(|bound| bound == channel))
            .map(|provider| provider.as_str().to_string());
        let matched = row.try_get::<Option<String>, _>("channel_id")? == channel_id
            && row.try_get::<Option<String>, _>("provider")? == provider
            && provider.is_some();
        let target = targets
            .entry(agent_id.clone())
            .or_insert((channel_id, provider, false));
        if matched {
            evidence.push(SessionEvidence {
                agent_id,
                channel_id: target.0.clone(),
                provider: target.1.clone(),
                session_key: row.try_get("session_key")?,
                raw_provider_session_id: row.try_get("raw_provider_session_id")?,
            });
            target.2 = true;
        }
    }
    for (agent_id, (channel_id, provider, matched)) in targets {
        if !matched {
            evidence.push(SessionEvidence {
                agent_id,
                channel_id,
                provider,
                session_key: None,
                raw_provider_session_id: None,
            });
        }
    }
    Ok(evidence)
}
