use sqlx::{PgPool, Row as SqlxRow};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SlotThreadBinding {
    pub(crate) agent_id: String,
    pub(crate) slot_index: i64,
    pub(crate) thread_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReviewFollowupCard {
    pub(crate) agent_id: String,
    pub(crate) title: String,
    pub(crate) issue_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DispatchReactionRow {
    pub(crate) status: String,
    pub(crate) context: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SlotThreadNameInput<'a> {
    pub(crate) dispatch_id: &'a str,
    pub(crate) card_id: &'a str,
    pub(crate) slot_index: i64,
    pub(crate) issue_number: Option<i64>,
    pub(crate) title: &'a str,
}

fn json_object_from_context(existing: Option<&str>) -> serde_json::Value {
    existing
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| serde_json::json!({}))
}

fn context_slot_index(dispatch_context: Option<&serde_json::Value>) -> Option<i64> {
    dispatch_context
        .and_then(|ctx| ctx.get("slot_index"))
        .and_then(|value| value.as_i64())
}

fn thread_id_from_slot_map(thread_id_map: Option<&str>, channel_id: u64) -> Option<String> {
    thread_id_map
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|map| {
            map.get(&channel_id.to_string())
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
        })
}

fn push_unique_thread_candidate(candidates: &mut Vec<String>, thread_id: Option<&str>) {
    let Some(thread_id) = thread_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };
    if !candidates.iter().any(|existing| existing == thread_id) {
        candidates.push(thread_id.to_string());
    }
}

async fn get_thread_for_channel_pg(
    pool: &PgPool,
    card_id: &str,
    channel_id: u64,
) -> Result<Option<String>, String> {
    let row = sqlx::query(
        "SELECT channel_thread_map::text AS channel_thread_map, active_thread_id
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres thread map for {card_id}: {error}"))?;

    let Some(row) = row else {
        return Ok(None);
    };

    let map_json: Option<String> = row
        .try_get("channel_thread_map")
        .map_err(|error| format!("read postgres channel_thread_map for {card_id}: {error}"))?;
    let active_thread_id: Option<String> = row
        .try_get("active_thread_id")
        .map_err(|error| format!("read postgres active_thread_id for {card_id}: {error}"))?;

    if let Some(thread_id) = map_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|map| {
            map.get(&channel_id.to_string())
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
    {
        return Ok(Some(thread_id));
    }

    if map_json
        .as_deref()
        .map_or(true, |value| value.is_empty() || value == "{}")
    {
        return Ok(active_thread_id.filter(|value| !value.trim().is_empty()));
    }

    Ok(None)
}

pub(crate) async fn persist_dispatch_message_target_pg(
    pool: &PgPool,
    dispatch_id: &str,
    channel_id: &str,
    message_id: &str,
) -> Result<(), String> {
    let existing: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT context FROM task_dispatches WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres dispatch context for {dispatch_id}: {error}"))?
    .flatten();

    let mut context = json_object_from_context(existing.as_deref());
    context["discord_message_channel_id"] = serde_json::json!(channel_id);
    context["discord_message_id"] = serde_json::json!(message_id);

    sqlx::query(
        "UPDATE task_dispatches
         SET context = $1,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(context.to_string())
    .bind(dispatch_id)
    .execute(pool)
    .await
    .map_err(|error| {
        format!("persist postgres dispatch message target for {dispatch_id}: {error}")
    })?;
    Ok(())
}

pub(crate) async fn persist_dispatch_thread_id_pg(
    pool: &PgPool,
    dispatch_id: &str,
    thread_id: &str,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE task_dispatches
         SET thread_id = $1,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(thread_id)
    .bind(dispatch_id)
    .execute(pool)
    .await
    .map_err(|error| format!("persist postgres thread_id for {dispatch_id}: {error}"))?;
    Ok(())
}

pub(crate) async fn load_dispatch_reaction_row_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<Option<DispatchReactionRow>, String> {
    let row = sqlx::query("SELECT status, context FROM task_dispatches WHERE id = $1")
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            format!("load postgres dispatch reaction target for {dispatch_id}: {error}")
        })?;
    row.map(|row| {
        Ok(DispatchReactionRow {
            status: row.try_get("status").map_err(|error| {
                format!("read postgres dispatch status for {dispatch_id}: {error}")
            })?,
            context: row.try_get("context").map_err(|error| {
                format!("read postgres dispatch context for {dispatch_id}: {error}")
            })?,
        })
    })
    .transpose()
}

pub(crate) async fn persist_dispatch_slot_index_pg(
    pool: &PgPool,
    dispatch_id: &str,
    slot_index: i64,
) -> Result<(), String> {
    let existing: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT context FROM task_dispatches WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres dispatch context for {dispatch_id}: {error}"))?
    .flatten();
    let mut context = json_object_from_context(existing.as_deref());
    if context.get("slot_index").and_then(|value| value.as_i64()) == Some(slot_index) {
        return Ok(());
    }
    context["slot_index"] = serde_json::json!(slot_index);
    sqlx::query(
        "UPDATE task_dispatches
         SET context = $1,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(context.to_string())
    .bind(dispatch_id)
    .execute(pool)
    .await
    .map_err(|error| format!("persist postgres slot index for {dispatch_id}: {error}"))?;
    Ok(())
}

async fn persist_dispatch_slot_index_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    slot_index: i64,
) -> Result<(), String> {
    let existing: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT context
         FROM task_dispatches
         WHERE id = $1
         FOR UPDATE",
    )
    .bind(dispatch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("load postgres dispatch context for {dispatch_id}: {error}"))?
    .flatten();
    let mut context = json_object_from_context(existing.as_deref());
    if context.get("slot_index").and_then(|value| value.as_i64()) == Some(slot_index) {
        return Ok(());
    }
    context["slot_index"] = serde_json::json!(slot_index);
    sqlx::query(
        "UPDATE task_dispatches
         SET context = $1,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(context.to_string())
    .bind(dispatch_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("persist postgres slot index for {dispatch_id}: {error}"))?;
    Ok(())
}

pub(crate) async fn ensure_agent_slot_pool_rows_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_pool_size: i64,
) -> Result<(), String> {
    for slot_index in 0..slot_pool_size.clamp(1, 32) {
        sqlx::query(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ($1, $2, '{}'::jsonb)
             ON CONFLICT (agent_id, slot_index) DO NOTHING",
        )
        .bind(agent_id)
        .bind(slot_index)
        .execute(pool)
        .await
        .map_err(|error| {
            format!("ensure postgres slot pool row {agent_id}:{slot_index}: {error}")
        })?;
    }
    Ok(())
}

async fn slot_has_active_dispatch_excluding_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent_id: &str,
    slot_index: i64,
    exclude_dispatch_id: Option<&str>,
) -> Result<bool, String> {
    let exclude_id = exclude_dispatch_id.unwrap_or("");
    let auto_queue_active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM auto_queue_entries
         WHERE agent_id = $1
           AND slot_index = $2
           AND status = 'dispatched'
           AND COALESCE(dispatch_id, '') != $3",
    )
    .bind(agent_id)
    .bind(slot_index)
    .bind(exclude_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| {
        format!("load postgres active slot entries for {agent_id}:{slot_index}: {error}")
    })?;
    if auto_queue_active > 0 {
        return Ok(true);
    }

    let rows = sqlx::query(
        "SELECT id, context
         FROM task_dispatches
         WHERE to_agent_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(agent_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| {
        format!("load postgres active dispatches for {agent_id}:{slot_index}: {error}")
    })?;

    for row in rows {
        let dispatch_id: String = row.try_get("id").map_err(|error| {
            format!("read postgres dispatch id for {agent_id}:{slot_index}: {error}")
        })?;
        if dispatch_id == exclude_id {
            continue;
        }
        let context: Option<String> = row.try_get("context").ok().flatten();
        let Some(context) = context else {
            continue;
        };
        let Some(context_json) = serde_json::from_str::<serde_json::Value>(&context).ok() else {
            continue;
        };
        if context_json
            .get("slot_index")
            .and_then(|value| value.as_i64())
            != Some(slot_index)
        {
            continue;
        }
        if context_json
            .get("sidecar_dispatch")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        if context_json.get("phase_gate").is_some() {
            continue;
        }
        return Ok(true);
    }

    Ok(false)
}

pub(crate) async fn read_slot_thread_binding_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
    channel_id: u64,
) -> Result<Option<SlotThreadBinding>, String> {
    ensure_agent_slot_pool_rows_pg(pool, agent_id, slot_index + 1).await?;
    let thread_id_map: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT thread_id_map::text
         FROM auto_queue_slots
         WHERE agent_id = $1 AND slot_index = $2",
    )
    .bind(agent_id)
    .bind(slot_index)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres slot thread map for {agent_id}:{slot_index}: {error}"))?
    .flatten();
    Ok(Some(SlotThreadBinding {
        agent_id: agent_id.to_string(),
        slot_index,
        thread_id: thread_id_from_slot_map(thread_id_map.as_deref(), channel_id),
    }))
}

async fn recent_slot_thread_history_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<Vec<String>, String> {
    let rows = sqlx::query(
        "SELECT id, thread_id, context
         FROM task_dispatches
         WHERE to_agent_id = $1
           AND thread_id IS NOT NULL
           AND BTRIM(thread_id) != ''
         ORDER BY COALESCE(updated_at, created_at) DESC",
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load postgres slot history for {agent_id}:{slot_index}: {error}"))?;

    let mut candidates = Vec::new();
    for row in rows {
        let dispatch_id: String = row.try_get("id").map_err(|error| {
            format!("read postgres dispatch id for {agent_id}:{slot_index}: {error}")
        })?;
        let thread_id: Option<String> = match row.try_get("thread_id") {
            Ok(thread_id) => thread_id,
            Err(error) => {
                tracing::warn!(
                    dispatch_id,
                    agent_id,
                    slot_index,
                    %error,
                    "[dispatch] failed to decode postgres thread_id while checking recent slot history"
                );
                continue;
            }
        };
        let context: Option<String> = match row.try_get("context") {
            Ok(context) => context,
            Err(error) => {
                tracing::warn!(
                    dispatch_id,
                    agent_id,
                    slot_index,
                    %error,
                    "[dispatch] failed to decode postgres dispatch context while checking recent slot history"
                );
                continue;
            }
        };
        let matches_slot = crate::services::discord_delivery_metadata::parse_pg_dispatch_context(
            &dispatch_id,
            context.as_deref(),
            "recent_slot_thread_history_pg",
        )
        .and_then(|value| value.get("slot_index").and_then(|value| value.as_i64()))
            == Some(slot_index);
        if matches_slot {
            push_unique_thread_candidate(&mut candidates, thread_id.as_deref());
        }
    }
    Ok(candidates)
}

pub(crate) async fn collect_slot_thread_candidates_pg(
    pool: &PgPool,
    agent_id: &str,
    card_id: &str,
    slot_binding: Option<&SlotThreadBinding>,
    channel_id: u64,
    include_card_thread: bool,
    include_recent_slot_history: bool,
) -> Result<Vec<String>, String> {
    let mut candidates = Vec::new();
    push_unique_thread_candidate(
        &mut candidates,
        slot_binding.and_then(|binding| binding.thread_id.as_deref()),
    );
    if include_card_thread {
        push_unique_thread_candidate(
            &mut candidates,
            get_thread_for_channel_pg(pool, card_id, channel_id)
                .await?
                .as_deref(),
        );
    }
    if include_recent_slot_history && let Some(binding) = slot_binding {
        for thread_id in recent_slot_thread_history_pg(pool, agent_id, binding.slot_index).await? {
            push_unique_thread_candidate(&mut candidates, Some(thread_id.as_str()));
        }
    }
    Ok(candidates)
}

pub(crate) async fn allocate_manual_slot_binding_pg(
    pool: &PgPool,
    agent_id: &str,
    dispatch_id: &str,
    channel_id: u64,
    max_slots: i64,
) -> Result<Option<SlotThreadBinding>, String> {
    ensure_agent_slot_pool_rows_pg(pool, agent_id, max_slots).await?;
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres manual slot allocation: {error}"))?;

    for slot_index in 0..max_slots {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(format!("agentdesk:slot:{agent_id}:{slot_index}"))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!("lock postgres slot allocation for {agent_id}:{slot_index}: {error}")
            })?;

        if slot_has_active_dispatch_excluding_pg_tx(
            &mut tx,
            agent_id,
            slot_index,
            Some(dispatch_id),
        )
        .await?
        {
            continue;
        }
        persist_dispatch_slot_index_pg_tx(&mut tx, dispatch_id, slot_index).await?;
        tx.commit()
            .await
            .map_err(|error| format!("commit postgres manual slot allocation: {error}"))?;
        return read_slot_thread_binding_pg(pool, agent_id, slot_index, channel_id).await;
    }
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres manual slot allocation miss: {error}"))?;
    Ok(None)
}

pub(crate) async fn resolve_slot_thread_binding_pg(
    pool: &PgPool,
    agent_id: &str,
    card_id: &str,
    dispatch_id: &str,
    dispatch_context: Option<&serde_json::Value>,
    dispatch_type: Option<&str>,
    channel_id: u64,
    independent_slot_thread: bool,
    max_slots: i64,
) -> Result<Option<SlotThreadBinding>, String> {
    if let Some(slot_index) = context_slot_index(dispatch_context) {
        return read_slot_thread_binding_pg(pool, agent_id, slot_index, channel_id).await;
    }

    let auto_queue_slot: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT slot_index
         FROM auto_queue_entries
         WHERE dispatch_id = $1
           AND agent_id = $2
           AND slot_index IS NOT NULL
         LIMIT 1",
    )
    .bind(dispatch_id)
    .bind(agent_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres dispatch slot for {dispatch_id}: {error}"))?
    .flatten();

    if let Some(slot_index) = auto_queue_slot {
        let binding = read_slot_thread_binding_pg(pool, agent_id, slot_index, channel_id).await?;
        persist_dispatch_slot_index_pg(pool, dispatch_id, slot_index).await?;
        return Ok(binding);
    }

    if independent_slot_thread {
        let binding =
            allocate_manual_slot_binding_pg(pool, agent_id, dispatch_id, channel_id, max_slots)
                .await?;
        if binding.is_none() {
            return Err(format!(
                "no free slot available for independent {dispatch_type:?} dispatch {dispatch_id}"
            ));
        }
        return Ok(binding);
    }

    let same_card_slot: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT slot_index
             FROM auto_queue_entries
             WHERE kanban_card_id = $1
               AND agent_id = $2
               AND status IN ('pending', 'dispatched')
               AND slot_index IS NOT NULL
             ORDER BY CASE status WHEN 'dispatched' THEN 0 ELSE 1 END,
                      priority_rank ASC
             LIMIT 1",
    )
    .bind(card_id)
    .bind(agent_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres card slot for {card_id}: {error}"))?
    .flatten();

    if let Some(slot_index) = same_card_slot {
        let binding = read_slot_thread_binding_pg(pool, agent_id, slot_index, channel_id).await?;
        persist_dispatch_slot_index_pg(pool, dispatch_id, slot_index).await?;
        return Ok(binding);
    }

    allocate_manual_slot_binding_pg(pool, agent_id, dispatch_id, channel_id, max_slots).await
}

pub(crate) async fn upsert_slot_thread_id_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
    channel_id: u64,
    thread_id: &str,
) -> Result<(), String> {
    let existing: String = sqlx::query_scalar::<_, Option<String>>(
        "SELECT COALESCE(thread_id_map::text, '{}')
         FROM auto_queue_slots
         WHERE agent_id = $1 AND slot_index = $2",
    )
    .bind(agent_id)
    .bind(slot_index)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres slot map for {agent_id}:{slot_index}: {error}"))?
    .flatten()
    .unwrap_or_else(|| "{}".to_string());
    let mut map = json_object_from_context(Some(&existing));
    map[channel_id.to_string()] = serde_json::json!(thread_id);
    sqlx::query(
        "UPDATE auto_queue_slots
         SET thread_id_map = $1::jsonb,
             updated_at = NOW()
         WHERE agent_id = $2 AND slot_index = $3",
    )
    .bind(map.to_string())
    .bind(agent_id)
    .bind(slot_index)
    .execute(pool)
    .await
    .map_err(|error| format!("save postgres slot map for {agent_id}:{slot_index}: {error}"))?;
    Ok(())
}

pub(crate) async fn clear_slot_thread_id_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
    channel_id: u64,
) -> Result<(), String> {
    let existing: String = sqlx::query_scalar::<_, Option<String>>(
        "SELECT COALESCE(thread_id_map::text, '{}')
         FROM auto_queue_slots
         WHERE agent_id = $1 AND slot_index = $2",
    )
    .bind(agent_id)
    .bind(slot_index)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres slot map for {agent_id}:{slot_index}: {error}"))?
    .flatten()
    .unwrap_or_else(|| "{}".to_string());
    if let Ok(mut map) = serde_json::from_str::<serde_json::Value>(&existing)
        && let Some(obj) = map.as_object_mut()
    {
        obj.remove(&channel_id.to_string());
        sqlx::query(
            "UPDATE auto_queue_slots
             SET thread_id_map = $1::jsonb,
                 updated_at = NOW()
             WHERE agent_id = $2 AND slot_index = $3",
        )
        .bind(map.to_string())
        .bind(agent_id)
        .bind(slot_index)
        .execute(pool)
        .await
        .map_err(|error| format!("clear postgres slot map for {agent_id}:{slot_index}: {error}"))?;
    }
    Ok(())
}

pub(crate) async fn build_slot_thread_name_pg(
    pool: &PgPool,
    input: SlotThreadNameInput<'_>,
) -> Result<String, String> {
    let mut batch_phase_for_label = 0i64;
    let group_info = sqlx::query(
        "SELECT run_id, COALESCE(thread_group, 0)::BIGINT AS thread_group, COALESCE(batch_phase, 0)::BIGINT AS batch_phase
         FROM auto_queue_entries
         WHERE dispatch_id = $1
         LIMIT 1",
    )
    .bind(input.dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres slot group for {}: {error}", input.dispatch_id))?
    .map(|row| {
        Ok::<_, String>((
            row.try_get::<String, _>("run_id").map_err(|error| {
                format!("read postgres run_id for {}: {error}", input.dispatch_id)
            })?,
            row.try_get::<i64, _>("thread_group").map_err(|error| {
                format!("read postgres thread_group for {}: {error}", input.dispatch_id)
            })?,
            row.try_get::<i64, _>("batch_phase").map_err(|error| {
                format!("read postgres batch_phase for {}: {error}", input.dispatch_id)
            })?,
        ))
    })
    .transpose()?
    .or(
        sqlx::query(
            "SELECT run_id, COALESCE(thread_group, 0)::BIGINT AS thread_group, COALESCE(batch_phase, 0)::BIGINT AS batch_phase
             FROM auto_queue_entries
             WHERE kanban_card_id = $1
               AND status IN ('pending', 'dispatched')
             ORDER BY CASE status WHEN 'dispatched' THEN 0 ELSE 1 END,
                      priority_rank ASC
             LIMIT 1",
        )
        .bind(input.card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load postgres card slot group for {}: {error}", input.card_id))?
        .map(|row| {
            Ok::<_, String>((
                row.try_get::<String, _>("run_id").map_err(|error| {
                    format!("read postgres run_id for {}: {error}", input.card_id)
                })?,
                row.try_get::<i64, _>("thread_group").map_err(|error| {
                    format!("read postgres thread_group for {}: {error}", input.card_id)
                })?,
                row.try_get::<i64, _>("batch_phase").map_err(|error| {
                    format!("read postgres batch_phase for {}: {error}", input.card_id)
                })?,
            ))
        })
        .transpose()?,
    );

    let grouped_issue_label = if let Some((run_id, thread_group, batch_phase)) = group_info {
        batch_phase_for_label = batch_phase;
        let rows = sqlx::query(
            "SELECT kc.github_issue_number, e.kanban_card_id
             FROM auto_queue_entries e
             JOIN kanban_cards kc ON kc.id = e.kanban_card_id
             WHERE e.run_id = $1
               AND COALESCE(e.thread_group, 0) = $2
               AND COALESCE(e.batch_phase, 0) = (
                   SELECT COALESCE(e2.batch_phase, 0)
                   FROM auto_queue_entries e2
                   WHERE e2.kanban_card_id = $3
                     AND e2.run_id = $1
                   LIMIT 1
               )
               AND kc.github_issue_number IS NOT NULL
             ORDER BY e.priority_rank ASC",
        )
        .bind(&run_id)
        .bind(thread_group)
        .bind(input.card_id)
        .fetch_all(pool)
        .await
        .map_err(|error| {
            format!(
                "load postgres grouped issues for {}: {error}",
                input.card_id
            )
        })?;
        let issues: Vec<(i64, String)> = rows
            .into_iter()
            .filter_map(|row| {
                Some((
                    row.try_get::<i64, _>("github_issue_number").ok()?,
                    row.try_get::<String, _>("kanban_card_id").ok()?,
                ))
            })
            .collect();
        if issues.len() > 1 {
            Some(
                issues
                    .into_iter()
                    .map(|(issue_number, issue_card_id)| {
                        if issue_card_id == input.card_id {
                            format!("▸{}", issue_number)
                        } else {
                            format!("#{}", issue_number)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        } else {
            None
        }
    } else {
        None
    };

    let base = if let Some(grouped) = grouped_issue_label {
        grouped
    } else if let Some(number) = input.issue_number {
        let short_title: String = input.title.chars().take(80).collect();
        format!("#{} {}", number, short_title)
    } else {
        input.title.chars().take(90).collect()
    };
    let phase_prefix = if batch_phase_for_label > 0 {
        format!("P{} ", batch_phase_for_label)
    } else {
        String::new()
    };
    Ok(
        format!("[slot {}] {}{}", input.slot_index, phase_prefix, base)
            .chars()
            .take(100)
            .collect(),
    )
}

pub(crate) async fn latest_work_dispatch_thread_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<String>, String> {
    let rows = sqlx::query(
        "SELECT id, thread_id, context
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type IN ('implementation', 'rework')
         ORDER BY
           CASE status
             WHEN 'dispatched' THEN 0
             WHEN 'pending' THEN 1
             WHEN 'completed' THEN 2
             ELSE 3
           END,
           COALESCE(completed_at, updated_at, created_at) DESC",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load postgres work dispatch thread for {card_id}: {error}"))?;

    for row in rows {
        let dispatch_id: String = row
            .try_get("id")
            .map_err(|error| format!("read postgres work dispatch id for {card_id}: {error}"))?;
        let thread_id: Option<String> = match row.try_get("thread_id") {
            Ok(thread_id) => thread_id,
            Err(error) => {
                tracing::warn!(
                    dispatch_id,
                    card_id,
                    %error,
                    "[dispatch] failed to decode postgres work thread_id while loading reusable thread"
                );
                continue;
            }
        };
        if let Some(thread_id) = thread_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            return Ok(Some(thread_id));
        }
        let context: Option<String> = match row.try_get("context") {
            Ok(context) => context,
            Err(error) => {
                tracing::warn!(
                    dispatch_id,
                    card_id,
                    %error,
                    "[dispatch] failed to decode postgres work context while loading reusable thread"
                );
                continue;
            }
        };
        if let Some(thread_id) =
            crate::services::discord_delivery_metadata::parse_pg_dispatch_context(
                &dispatch_id,
                context.as_deref(),
                "latest_work_dispatch_thread_pg",
            )
            .and_then(|value| {
                value
                    .get("thread_id")
                    .and_then(|value| value.as_str())
                    .map(std::string::ToString::to_string)
            })
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            return Ok(Some(thread_id));
        }
    }

    Ok(None)
}

pub(crate) async fn load_review_followup_card_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<ReviewFollowupCard, String> {
    let row = sqlx::query(
        "SELECT assigned_agent_id, title, github_issue_url
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres card {card_id} for review followup: {error}"))?;
    let Some(row) = row else {
        return Err(format!("card {card_id} not found or missing agent"));
    };

    let agent_id: Option<String> = row
        .try_get("assigned_agent_id")
        .map_err(|error| format!("read postgres assigned_agent_id for {card_id}: {error}"))?;
    Ok(ReviewFollowupCard {
        agent_id: agent_id
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("card {card_id} not found or missing agent"))?,
        title: row
            .try_get("title")
            .map_err(|error| format!("read postgres title for {card_id}: {error}"))?,
        issue_url: row
            .try_get("github_issue_url")
            .map_err(|error| format!("read postgres github_issue_url for {card_id}: {error}"))?,
    })
}

pub(crate) async fn load_dispatch_context_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<Option<String>, String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT context FROM task_dispatches WHERE id = $1")
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            format!("load postgres review dispatch context for {dispatch_id}: {error}")
        })
        .map(|value| value.flatten())
}

pub(crate) async fn review_followup_already_resolved_pg(pool: &PgPool, card_id: &str) -> bool {
    sqlx::query_scalar::<_, Option<String>>("SELECT review_status FROM kanban_cards WHERE id = $1")
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
        .map(|s| s == "rework_pending" || s == "dilemma_pending")
        .unwrap_or(false)
}
