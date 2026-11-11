//! Session binding selection from one stored snapshot and live provider defaults.

use crate::db::session_evidence::{SessionEvidence, load_session_evidence_rows_pg};
use sqlx::PgPool;
use std::collections::BTreeMap;

pub(crate) async fn load_session_evidence_pg(
    pool: &PgPool,
    identifier: &str,
) -> Result<Vec<SessionEvidence>, sqlx::Error> {
    let rows = load_session_evidence_rows_pg(pool, identifier).await?;
    let mut targets = BTreeMap::new();
    let mut evidence = Vec::new();
    for row in rows {
        let (agent_id, bindings) = row.decode_binding()?;
        let channel_id = if agent_id == identifier {
            bindings.primary_channel()
        } else {
            Some(identifier.to_string())
        };
        let provider = channel_id
            .as_ref()
            .and_then(|channel| bindings.provider_for_channel(|bound| bound == channel))
            .map(|provider| provider.as_str().to_string());
        let matched = row.matches_session(&channel_id, &provider)?;
        let target = targets
            .entry(agent_id.clone())
            .or_insert((channel_id, provider, false));
        if matched {
            evidence.push(SessionEvidence {
                agent_id,
                channel_id: target.0.clone(),
                provider: target.1.clone(),
                session_key: row.decode_session_key()?,
                raw_provider_session_id: row.decode_raw_provider_session_id()?,
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
