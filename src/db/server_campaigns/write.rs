use super::*;

fn number(value: u64) -> StoreResult<i64> {
    i64::try_from(value).map_err(|_| StoreError::InconsistentState)
}

pub(super) async fn persist(
    tx: &mut Transaction<'_, Postgres>,
    previous: Option<&CampaignState>,
    decision: &Decision,
) -> StoreResult<()> {
    let state = &decision.state;
    let id = &state.id;
    let sequence = number(state.event_sequence)?;
    let revision = number(state.definition_revision)?;
    if decision.events.is_empty() {
        return Ok(());
    }
    if let Some(run) = &state.run {
        sqlx::query(
            "INSERT INTO server_campaign_runs (campaign_id, run_id, document, nodes, definition_pin) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (campaign_id, run_id) DO UPDATE \
             SET document = EXCLUDED.document, nodes = EXCLUDED.nodes",
        ).bind(id).bind(&run.id).bind(Json(run)).bind(Json(&state.nodes)).bind(Json(&state.definition))
            .execute(&mut **tx).await?;
        sqlx::query(
            "INSERT INTO server_campaign_run_history (campaign_id, run_id, sequence, document, nodes, definition_pin) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        ).bind(id).bind(&run.id).bind(sequence).bind(Json(run)).bind(Json(&state.nodes))
            .bind(Json(&state.definition)).execute(&mut **tx).await?;
        for (attempt_id, attempt) in &run.attempts {
            let affected = sqlx::query(
                "INSERT INTO server_campaign_attempts (campaign_id, run_id, attempt_id, document, definition_pin) \
                 VALUES ($1, $2, $3, $4, $5) ON CONFLICT (campaign_id, run_id, attempt_id) DO UPDATE \
                 SET document = EXCLUDED.document WHERE server_campaign_attempts.document->'binding' = EXCLUDED.document->'binding'",
            ).bind(id).bind(&run.id).bind(attempt_id).bind(Json(attempt)).bind(Json(&state.definition))
                .execute(&mut **tx).await?.rows_affected();
            if affected != 1 {
                return Err(StoreError::InconsistentState);
            }
        }
        for (action_id, action) in &run.actions {
            let affected = sqlx::query(
                "INSERT INTO server_campaign_actions (campaign_id, run_id, action_id, document) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (campaign_id, run_id, action_id) DO UPDATE SET document = EXCLUDED.document \
                 WHERE server_campaign_actions.document->'binding' = EXCLUDED.document->'binding' \
                 AND server_campaign_actions.document->'target' = EXCLUDED.document->'target' \
                 AND server_campaign_actions.document->'kind' = EXCLUDED.document->'kind'",
            ).bind(id).bind(&run.id).bind(action_id).bind(Json(action)).execute(&mut **tx).await?.rows_affected();
            if affected != 1 {
                return Err(StoreError::InconsistentState);
            }
            sqlx::query(
                "INSERT INTO server_campaign_action_history (campaign_id, run_id, action_id, sequence, document) \
                 VALUES ($1, $2, $3, $4, $5)",
            ).bind(id).bind(&run.id).bind(action_id).bind(sequence).bind(Json(action)).execute(&mut **tx).await?;
        }
    }
    sqlx::query(
        "INSERT INTO server_campaign_definitions \
         (campaign_id, revision, definition, control, run_id, nodes, event_sequence, last_event_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (campaign_id) DO UPDATE SET \
         revision = EXCLUDED.revision, definition = EXCLUDED.definition, control = EXCLUDED.control, \
         run_id = EXCLUDED.run_id, nodes = EXCLUDED.nodes, event_sequence = EXCLUDED.event_sequence, \
         last_event_at = EXCLUDED.last_event_at",
    ).bind(id).bind(revision).bind(Json(&state.definition)).bind(Json(state.control))
        .bind(state.run.as_ref().map(|r| &r.id)).bind(Json(&state.nodes)).bind(sequence).bind(state.last_event_at)
        .execute(&mut **tx).await?;
    if previous.is_none_or(|old| old.definition_revision != state.definition_revision) {
        let change = decision.events.iter().find_map(|event| match &event.kind {
            EventKind::DefinitionEdited { change, .. } => change.as_ref(),
            _ => None,
        });
        sqlx::query(
            "INSERT INTO server_campaign_revisions (campaign_id, revision, definition, change) VALUES ($1, $2, $3, $4)",
        ).bind(id).bind(revision).bind(Json(&state.definition)).bind(change.map(Json))
            .execute(&mut **tx).await?;
        sqlx::query(
            "DELETE FROM server_campaign_revisions WHERE campaign_id = $1 AND revision NOT IN \
             (SELECT revision FROM server_campaign_revisions WHERE campaign_id = $1 ORDER BY revision DESC LIMIT 10)",
        ).bind(id).execute(&mut **tx).await?;
    }
    for event in &decision.events {
        sqlx::query(
            "INSERT INTO server_campaign_events (campaign_id, sequence, run_id, document) VALUES ($1, $2, $3, $4)",
        ).bind(id).bind(number(event.sequence)?).bind(&event.run_id).bind(Json(event)).execute(&mut **tx).await?;
    }
    Ok(())
}
