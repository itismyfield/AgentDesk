use super::*;

pub(super) fn deploy_phase_api_enabled(state: &AppState) -> bool {
    state
        .config
        .server
        .auth_token
        .as_deref()
        .map(|token| !token.trim().is_empty())
        .unwrap_or(false)
}

pub(super) fn pg_unavailable_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "postgres pool is not configured"})),
    )
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn slot_thread_map_has_bindings(
    conn: &sqlite_test::Connection,
    agent_id: &str,
    slot_index: i64,
) -> bool {
    let raw_map: Option<String> = conn
        .query_row(
            "SELECT thread_id_map
             FROM auto_queue_slots
             WHERE agent_id = ?1 AND slot_index = ?2",
            sqlite_test::params![agent_id, slot_index],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    raw_map
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| value.as_object().cloned())
        .map(|map| {
            map.values().any(|value| {
                value
                    .as_str()
                    .map(|raw| !raw.trim().is_empty())
                    .or_else(|| value.as_u64().map(|_| true))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn slot_has_dispatch_thread_history(
    conn: &sqlite_test::Connection,
    agent_id: &str,
    slot_index: i64,
) -> bool {
    conn.query_row(
        "SELECT COUNT(*) > 0
         FROM task_dispatches
         WHERE to_agent_id = ?1
           AND thread_id IS NOT NULL
           AND TRIM(thread_id) != ''
           AND CASE
                 WHEN context IS NULL OR TRIM(context) = '' OR json_valid(context) = 0
                     THEN NULL
                 ELSE CAST(json_extract(context, '$.slot_index') AS INTEGER)
               END = ?2",
        sqlite_test::params![agent_id, slot_index],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn slot_requires_thread_reset_before_reuse(
    _conn: &sqlite_test::Connection,
    _agent_id: &str,
    _slot_index: i64,
    _newly_assigned: bool,
    _reassigned_from_other_group: bool,
) -> bool {
    // Slot bindings are sticky. If the saved Discord thread is stale, the
    // dispatch delivery path probes it and replaces it with a fresh thread.
    false
}

pub(super) fn json_value_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

pub(super) async fn slot_thread_map_has_bindings_pg(
    pool: &sqlx::PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<bool, String> {
    let raw_map = sqlx::query_scalar::<_, Option<String>>(
        "SELECT COALESCE(thread_id_map::text, '{}')
         FROM auto_queue_slots
         WHERE agent_id = $1 AND slot_index = $2",
    )
    .bind(agent_id)
    .bind(slot_index)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres slot thread map for {agent_id}:{slot_index}: {error}"))?
    .flatten()
    .unwrap_or_else(|| "{}".to_string());

    let thread_map = match serde_json::from_str::<serde_json::Value>(&raw_map) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                agent_id,
                slot_index,
                %error,
                "[auto-queue] invalid postgres slot thread_id_map JSON while checking thread reuse"
            );
            return Ok(false);
        }
    };
    let Some(thread_map) = thread_map.as_object() else {
        if raw_map.trim() != "{}" && raw_map.trim() != "null" {
            tracing::warn!(
                agent_id,
                slot_index,
                json_type = json_value_kind(&thread_map),
                "[auto-queue] postgres slot thread_id_map is not an object while checking thread reuse"
            );
        }
        return Ok(false);
    };

    Ok(thread_map.values().any(|value| {
        value
            .as_str()
            .map(|raw| !raw.trim().is_empty())
            .or_else(|| value.as_u64().map(|_| true))
            .unwrap_or(false)
    }))
}

pub(super) async fn slot_has_dispatch_thread_history_pg(
    pool: &sqlx::PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<bool, String> {
    let rows = sqlx::query(
        "SELECT id, thread_id, context
         FROM task_dispatches
         WHERE to_agent_id = $1
           AND thread_id IS NOT NULL
           AND BTRIM(thread_id) != ''",
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await
    .map_err(|error| {
        format!("load postgres dispatch thread history for {agent_id}:{slot_index}: {error}")
    })?;

    for row in rows {
        let dispatch_id: String = row.try_get("id").map_err(|error| {
            format!("read postgres dispatch id for {agent_id}:{slot_index}: {error}")
        })?;
        let context: Option<String> = match row.try_get("context") {
            Ok(context) => context,
            Err(error) => {
                tracing::warn!(
                    dispatch_id,
                    agent_id,
                    slot_index,
                    %error,
                    "[auto-queue] failed to decode postgres dispatch context while checking slot thread history"
                );
                continue;
            }
        };
        let Some(context) = context
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let context_json = match serde_json::from_str::<serde_json::Value>(context) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    dispatch_id,
                    agent_id,
                    slot_index,
                    %error,
                    "[auto-queue] invalid postgres dispatch context JSON while checking slot thread history"
                );
                continue;
            }
        };
        let Some(context_json) = context_json.as_object() else {
            tracing::warn!(
                dispatch_id,
                agent_id,
                slot_index,
                json_type = json_value_kind(&context_json),
                "[auto-queue] postgres dispatch context is not an object while checking slot thread history"
            );
            continue;
        };
        if context_json
            .get("slot_index")
            .and_then(|value| value.as_i64())
            == Some(slot_index)
        {
            return Ok(true);
        }
    }

    Ok(false)
}

pub(super) async fn slot_requires_thread_reset_before_reuse_pg(
    _pool: &sqlx::PgPool,
    _agent_id: &str,
    _slot_index: i64,
    _newly_assigned: bool,
    _reassigned_from_other_group: bool,
) -> Result<bool, String> {
    // Slot bindings are sticky. If the saved Discord thread is stale, the
    // dispatch delivery path probes it and replaces it with a fresh thread.
    Ok(false)
}

pub(super) fn build_auto_queue_dispatch_context(
    entry_id: &str,
    thread_group: i64,
    slot_index: Option<i64>,
    reset_slot_thread_before_reuse: bool,
    extra_fields: impl IntoIterator<Item = (&'static str, serde_json::Value)>,
) -> serde_json::Value {
    let mut context = serde_json::Map::new();
    context.insert("auto_queue".to_string(), json!(true));
    context.insert("entry_id".to_string(), json!(entry_id));
    context.insert("thread_group".to_string(), json!(thread_group));
    context.insert("slot_index".to_string(), json!(slot_index));
    if reset_slot_thread_before_reuse {
        context.insert(
            "reset_slot_thread_before_reuse".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    for (key, value) in extra_fields {
        context.insert(key.to_string(), value);
    }
    serde_json::Value::Object(context)
}

pub(super) fn resolve_activate_dispatch_channel_id(channel: &str) -> Option<u64> {
    channel
        .parse::<u64>()
        .ok()
        .or_else(|| crate::server::routes::dispatches::resolve_channel_alias_pub(channel))
}

pub(super) async fn group_has_dispatched_entries_pg(
    pool: &sqlx::PgPool,
    run_id: &str,
    thread_group: i64,
) -> Result<bool, String> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
           AND COALESCE(thread_group, 0) = $2
           AND status = 'dispatched'",
    )
    .bind(run_id)
    .bind(thread_group)
    .fetch_one(pool)
    .await
    .map_err(|error| {
        format!("count dispatched postgres auto-queue entries for {run_id}:{thread_group}: {error}")
    })?;
    Ok(count > 0)
}
