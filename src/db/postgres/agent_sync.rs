use std::collections::BTreeSet;

use sqlx::{Acquire, PgConnection, PgPool, Postgres, Row, Transaction};

use crate::config::{AgentChannel, AgentDef};

const LEGACY_AGENT_PREFIX: &str = "openclaw-";

/// Syncs the `agents` table to config in one transaction, so a failed boot leaves no partial sync.
pub async fn sync_agents_from_config_pg(
    pool: &PgPool,
    agents: &[AgentDef],
) -> Result<usize, String> {
    crate::voice::commands::validate_agent_alias_collisions(agents)
        .map_err(|error| format!("validate voice aliases: {error}"))?;

    let config_ids: BTreeSet<&str> = agents.iter().map(|agent| agent.id.as_str()).collect();
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin agent sync: {error}"))?;

    for agent in agents {
        upsert_agent_from_config_pg(&mut tx, agent).await?;
    }

    migrate_legacy_agent_aliases_pg(&mut tx, agents, &config_ids).await?;

    let existing_rows = sqlx::query("SELECT id FROM agents")
        .fetch_all(&mut *tx)
        .await
        .map_err(|error| format!("list postgres agents: {error}"))?;

    for row in existing_rows {
        let db_id: String = row
            .try_get("id")
            .map_err(|error| format!("read postgres agent id: {error}"))?;
        if !config_ids.contains(db_id.as_str()) {
            retire_agent_pg(&mut tx, &db_id).await?;
        }
    }

    tx.commit()
        .await
        .map_err(|error| format!("commit agent sync: {error}"))?;
    Ok(agents.len())
}

/// Deletes an agent removed from config; one still referenced by a row that can't drop
/// it (an agent-kind scheduled message) is kept, and boot continues.
async fn retire_agent_pg(tx: &mut Transaction<'_, Postgres>, agent_id: &str) -> Result<(), String> {
    let db = |what: &'static str| move |error: sqlx::Error| format!("{what} {agent_id}: {error}");
    let mut savepoint = (&mut *tx)
        .begin()
        .await
        .map_err(db("agent retire savepoint"))?;
    match delete_agent_row_pg(&mut savepoint, agent_id).await {
        Ok(()) => savepoint
            .commit()
            .await
            .map_err(db("agent retire savepoint")),
        Err(error)
            if error
                .as_database_error()
                .is_some_and(|e| e.is_foreign_key_violation()) =>
        {
            savepoint
                .rollback()
                .await
                .map_err(db("agent retire savepoint"))?;
            tracing::warn!(
                "[agent-sync] Keeping agent '{agent_id}' removed from config: still referenced ({error})"
            );
            Ok(())
        }
        Err(error) => Err(db("delete postgres agent")(error)),
    }
}

/// Clears the nullable references `move_legacy_agent_references_pg` would move, then
/// deletes the row; an agent-kind scheduled message still blocks that delete.
async fn delete_agent_row_pg(conn: &mut PgConnection, agent_id: &str) -> Result<(), sqlx::Error> {
    for sql in [
        "UPDATE github_repos SET default_agent_id = NULL WHERE default_agent_id = $1",
        "UPDATE pipeline_stages SET agent_override_id = NULL WHERE agent_override_id = $1",
        "UPDATE kanban_cards SET assigned_agent_id = NULL WHERE assigned_agent_id = $1",
        "UPDATE kanban_cards SET owner_agent_id = NULL WHERE owner_agent_id = $1",
        "UPDATE kanban_cards SET requester_agent_id = NULL WHERE requester_agent_id = $1",
        "UPDATE sessions SET agent_id = NULL WHERE agent_id = $1",
        "UPDATE scheduled_messages SET agent_id = NULL \
         WHERE agent_id = $1 AND delivery_kind <> 'agent'",
        "DELETE FROM agents WHERE id = $1",
    ] {
        sqlx::query(sql).bind(agent_id).execute(&mut *conn).await?;
    }
    Ok(())
}

fn legacy_agent_alias(agent_id: &str) -> Option<String> {
    if agent_id.starts_with(LEGACY_AGENT_PREFIX) {
        return None;
    }
    Some(format!("{LEGACY_AGENT_PREFIX}{agent_id}"))
}

async fn postgres_agent_exists(conn: &mut PgConnection, agent_id: &str) -> Result<bool, String> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agents WHERE id = $1)")
        .bind(agent_id)
        .fetch_one(conn)
        .await
        .map_err(|error| format!("check postgres agent {agent_id}: {error}"))
}

async fn upsert_agent_from_config_pg(
    conn: &mut PgConnection,
    agent: &AgentDef,
) -> Result<(), String> {
    let discord_channel_cc = agent.channels.get("claude").and_then(AgentChannel::target);
    let discord_channel_cdx = agent.channels.get("codex").and_then(AgentChannel::target);
    let provider_primary = match crate::services::provider::ProviderKind::from_str(&agent.provider)
    {
        Some(crate::services::provider::ProviderKind::Claude)
        | Some(crate::services::provider::ProviderKind::Codex)
        | None => None,
        Some(_) => agent
            .channels
            .get(&agent.provider)
            .and_then(AgentChannel::target),
    };
    let discord_channel_id = provider_primary.or_else(|| discord_channel_cc.clone());
    let discord_channel_alt = discord_channel_cdx.clone();

    // #3667: config-declared intake node-affinity labels. `None` (key absent
    // from yaml) binds SQL NULL; the COALESCE then keeps the existing DB value
    // so an out-of-band label (e.g. ch-td's DB-only `["mac-book"]`, not in yaml)
    // is never wiped by a routine restart sync. `Some([...])` sets/overwrites it
    // and `Some([])` explicitly clears it. We reference the bound `$11` (not
    // EXCLUDED) in the conflict branch so the NULL signal survives the VALUES
    // COALESCE used to satisfy the NOT NULL column on insert.
    let preferred_intake_node_labels = agent
        .preferred_intake_node_labels
        .as_ref()
        .map(|labels| serde_json::json!(labels));

    sqlx::query(
        "INSERT INTO agents (
            id, name, name_ko, provider, department, avatar_emoji,
            discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx,
            preferred_intake_node_labels
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, COALESCE($11, '[]'::JSONB))
         ON CONFLICT (id) DO UPDATE
         SET name = EXCLUDED.name,
             name_ko = EXCLUDED.name_ko,
             provider = EXCLUDED.provider,
             department = EXCLUDED.department,
             avatar_emoji = EXCLUDED.avatar_emoji,
             discord_channel_id = EXCLUDED.discord_channel_id,
             discord_channel_alt = EXCLUDED.discord_channel_alt,
             discord_channel_cc = EXCLUDED.discord_channel_cc,
             discord_channel_cdx = EXCLUDED.discord_channel_cdx,
             preferred_intake_node_labels =
                 COALESCE($11, agents.preferred_intake_node_labels)",
    )
    .bind(&agent.id)
    .bind(&agent.name)
    .bind(&agent.name_ko)
    .bind(&agent.provider)
    .bind(&agent.department)
    .bind(&agent.avatar_emoji)
    .bind(discord_channel_id)
    .bind(discord_channel_alt)
    .bind(discord_channel_cc)
    .bind(discord_channel_cdx)
    .bind(preferred_intake_node_labels)
    .execute(conn)
    .await
    .map_err(|error| format!("upsert postgres agent {}: {error}", agent.id))?;

    Ok(())
}

async fn migrate_legacy_agent_aliases_pg(
    conn: &mut PgConnection,
    agents: &[AgentDef],
    config_ids: &BTreeSet<&str>,
) -> Result<(), String> {
    for agent in agents {
        let Some(legacy_id) = legacy_agent_alias(&agent.id) else {
            continue;
        };
        if config_ids.contains(legacy_id.as_str()) {
            tracing::info!(
                "[agent-sync] Preserving configured legacy agent '{}' while syncing '{}'",
                legacy_id,
                agent.id
            );
            continue;
        }

        if postgres_agent_exists(conn, &legacy_id).await? {
            copy_runtime_fields_from_legacy_pg(conn, &legacy_id, &agent.id).await?;
        }
        move_legacy_agent_references_pg(conn, &legacy_id, &agent.id).await?;

        if postgres_agent_exists(conn, &legacy_id).await? {
            sqlx::query("DELETE FROM agents WHERE id = $1")
                .bind(&legacy_id)
                .execute(&mut *conn)
                .await
                .map_err(|error| format!("delete postgres legacy agent {legacy_id}: {error}"))?;
            tracing::info!(
                "[agent-sync] Migrated legacy agent '{}' -> '{}'",
                legacy_id,
                agent.id
            );
        }
    }

    Ok(())
}

async fn copy_runtime_fields_from_legacy_pg(
    conn: &mut PgConnection,
    legacy_id: &str,
    canonical_id: &str,
) -> Result<(), String> {
    if !postgres_agent_exists(conn, legacy_id).await?
        || !postgres_agent_exists(conn, canonical_id).await?
    {
        return Ok(());
    }

    sqlx::query(
        "UPDATE agents
         SET status = legacy.status,
             xp = legacy.xp,
             skills = legacy.skills,
             created_at = COALESCE(legacy.created_at, agents.created_at),
             sprite_number = COALESCE(legacy.sprite_number, agents.sprite_number),
             description = COALESCE(legacy.description, agents.description),
             system_prompt = COALESCE(legacy.system_prompt, agents.system_prompt),
             pipeline_config = COALESCE(legacy.pipeline_config, agents.pipeline_config),
             -- #3667: inherit a legacy DB-only intake label only when the
             -- canonical row has none yet (config-set value wins; the column is
             -- NOT NULL so a plain COALESCE would clobber it with '[]').
             preferred_intake_node_labels = CASE
                 WHEN jsonb_array_length(agents.preferred_intake_node_labels) = 0
                      AND jsonb_array_length(legacy.preferred_intake_node_labels) > 0
                     THEN legacy.preferred_intake_node_labels
                 ELSE agents.preferred_intake_node_labels
             END
         FROM agents AS legacy
         WHERE legacy.id = $1
           AND agents.id = $2",
    )
    .bind(legacy_id)
    .bind(canonical_id)
    .execute(conn)
    .await
    .map_err(|error| {
        format!("copy postgres legacy runtime fields {legacy_id} -> {canonical_id}: {error}")
    })?;

    Ok(())
}

async fn move_legacy_agent_references_pg(
    conn: &mut PgConnection,
    legacy_id: &str,
    canonical_id: &str,
) -> Result<(), String> {
    for sql in [
        "UPDATE github_repos SET default_agent_id = $1 WHERE default_agent_id = $2",
        "UPDATE pipeline_stages SET agent_override_id = $1 WHERE agent_override_id = $2",
        "UPDATE kanban_cards SET assigned_agent_id = $1 WHERE assigned_agent_id = $2",
        "UPDATE kanban_cards SET owner_agent_id = $1 WHERE owner_agent_id = $2",
        "UPDATE kanban_cards SET requester_agent_id = $1 WHERE requester_agent_id = $2",
        "UPDATE task_dispatches SET from_agent_id = $1 WHERE from_agent_id = $2",
        "UPDATE task_dispatches SET to_agent_id = $1 WHERE to_agent_id = $2",
        "UPDATE sessions SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE meeting_transcripts SET speaker_agent_id = $1 WHERE speaker_agent_id = $2",
        "UPDATE skill_usage SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE turns SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE dispatch_outbox SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE auto_queue_runs SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE auto_queue_entries SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE api_friction_events SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE session_transcripts SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE memento_feedback_turn_stats SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE scheduled_messages SET agent_id = $1 WHERE agent_id = $2",
        "UPDATE routines SET fallback_agent_id = $1 WHERE fallback_agent_id = $2",
    ] {
        sqlx::query(sql)
            .bind(canonical_id)
            .bind(legacy_id)
            .execute(&mut *conn)
            .await
            .map_err(|error| {
                format!("rewrite postgres legacy references {legacy_id} -> {canonical_id}: {error}")
            })?;
    }

    sqlx::query(
        "INSERT INTO auto_queue_slots (
            agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map, created_at, updated_at
         )
         SELECT $1, slot_index, assigned_run_id, assigned_thread_group, thread_id_map, created_at, updated_at
           FROM auto_queue_slots
          WHERE agent_id = $2
         ON CONFLICT (agent_id, slot_index) DO NOTHING",
    )
    .bind(canonical_id)
    .bind(legacy_id)
    .execute(&mut *conn)
    .await
    .map_err(|error| {
        format!("upsert postgres auto_queue_slots {legacy_id} -> {canonical_id}: {error}")
    })?;
    sqlx::query("DELETE FROM auto_queue_slots WHERE agent_id = $1")
        .bind(legacy_id)
        .execute(&mut *conn)
        .await
        .map_err(|error| format!("delete postgres auto_queue_slots {legacy_id}: {error}"))?;

    Ok(())
}
