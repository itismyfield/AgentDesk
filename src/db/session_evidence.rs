//! Observation-only session binding: one SELECT snapshot, with ambiguous owners refused.

use super::agents::AgentChannelBindings;
use serde::Serialize;
use sqlx::{PgPool, Row, postgres::PgRow};

#[derive(Debug, Serialize)]
pub(crate) struct SessionEvidence {
    pub agent_id: String,
    pub channel_id: Option<String>,
    pub provider: Option<String>,
    pub session_key: Option<String>,
    pub raw_provider_session_id: Option<String>,
}

pub(crate) struct SessionEvidenceRow(PgRow);

impl SessionEvidenceRow {
    pub(crate) fn decode_binding(&self) -> Result<(String, AgentChannelBindings), sqlx::Error> {
        let agent_id = self.0.try_get("agent_id")?;
        let bindings = AgentChannelBindings {
            provider: self.0.try_get("agent_provider")?,
            discord_channel_id: self.0.try_get("discord_channel_id")?,
            discord_channel_alt: self.0.try_get("discord_channel_alt")?,
            discord_channel_cc: self.0.try_get("discord_channel_cc")?,
            discord_channel_cdx: self.0.try_get("discord_channel_cdx")?,
        };
        Ok((agent_id, bindings))
    }

    pub(crate) fn matches_session(
        &self,
        channel_id: &Option<String>,
        provider: &Option<String>,
    ) -> Result<bool, sqlx::Error> {
        Ok(
            self.0.try_get::<Option<String>, _>("channel_id")? == *channel_id
                && self.0.try_get::<Option<String>, _>("provider")? == *provider
                && provider.is_some(),
        )
    }

    pub(crate) fn decode_session_key(&self) -> Result<Option<String>, sqlx::Error> {
        self.0.try_get("session_key")
    }

    pub(crate) fn decode_raw_provider_session_id(&self) -> Result<Option<String>, sqlx::Error> {
        self.0.try_get("raw_provider_session_id")
    }
}

pub(crate) async fn load_session_evidence_rows_pg(
    pool: &PgPool,
    identifier: &str,
) -> Result<Vec<SessionEvidenceRow>, sqlx::Error> {
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
    Ok(rows.into_iter().map(SessionEvidenceRow).collect())
}
