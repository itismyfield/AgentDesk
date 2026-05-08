use anyhow::Result;
use serde_json::json;
use sqlx::{PgPool, Postgres, Row};

use crate::db::Db;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use crate::db::agents::{
    resolve_agent_channel_for_provider_on_conn, resolve_agent_dispatch_channel_on_conn,
};
use crate::db::agents::{resolve_agent_channel_for_provider_pg, resolve_agent_dispatch_channel_pg};
use crate::engine::PolicyEngine;

use super::dispatch_channel::{
    dispatch_destination_provider_override, dispatch_uses_alt_channel, resolve_dispatch_channel_id,
};
use super::dispatch_context::{
    ReviewTargetTrust, TargetRepoSource, build_review_context,
    dispatch_context_with_session_strategy, dispatch_context_worktree_target,
    dispatch_type_requires_fresh_worktree, ensure_card_worktree,
    inject_review_dispatch_identifiers, json_string_field, resolve_card_target_repo_ref,
    resolve_card_worktree, resolve_parent_dispatch_context,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use super::dispatch_context::{
    build_review_context_sqlite_test, inject_review_dispatch_identifiers_sqlite_test,
    resolve_card_target_repo_ref_sqlite_test, resolve_card_worktree_sqlite_test,
    resolve_parent_dispatch_context_sqlite_test,
};
use super::dispatch_query::query_dispatch_row_pg;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use super::dispatch_status::{
    ensure_dispatch_notify_outbox_on_conn, record_dispatch_status_event_on_conn,
};
use super::{DispatchCreateOptions, cancel_dispatch_and_reset_auto_queue_on_pg_tx};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use super::{cancel_dispatch_and_reset_auto_queue_on_conn, dispatch_query::query_dispatch_row};

fn dispatch_context_requests_sidecar(context: &serde_json::Value) -> bool {
    context
        .get("sidecar_dispatch")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
        || context
            .get("phase_gate")
            .and_then(|value| value.as_object())
            .is_some()
}

fn inject_work_dispatch_baseline_commit(dispatch_type: &str, context: &mut serde_json::Value) {
    if !matches!(dispatch_type, "implementation" | "rework") {
        return;
    }
    let Some(obj) = context.as_object_mut() else {
        return;
    };
    if obj.contains_key("baseline_commit") {
        return;
    }

    let target_repo = obj
        .get("target_repo")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty());
    let baseline_commit =
        crate::services::platform::shell::resolve_repo_dir_for_target(target_repo)
            .ok()
            .flatten()
            .and_then(|repo_dir| {
                crate::services::platform::shell::git_dispatch_baseline_commit(&repo_dir)
            });

    if let Some(baseline_commit) = baseline_commit {
        obj.insert("baseline_commit".to_string(), json!(baseline_commit));
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn load_existing_thread_for_channel(
    conn: &sqlite_test::Connection,
    card_id: &str,
    channel_id: u64,
) -> Result<Option<String>> {
    let map_json: Option<String> = conn
        .query_row(
            "SELECT channel_thread_map FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    if let Some(json_str) = map_json.as_deref()
        && !json_str.is_empty()
        && json_str != "{}"
    {
        let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(json_str)
            .map_err(|e| {
                anyhow::anyhow!(
                    "Cannot create dispatch for card {}: invalid channel_thread_map JSON: {}",
                    card_id,
                    e
                )
            })?;

        if let Some(value) = map.get(&channel_id.to_string()) {
            let thread_id = value.as_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "Cannot create dispatch for card {}: non-string thread mapping for channel {}",
                    card_id,
                    channel_id
                )
            })?;
            return Ok(Some(thread_id.to_string()));
        }
        return Ok(None);
    }

    Ok(conn
        .query_row(
            "SELECT active_thread_id FROM kanban_cards WHERE id = ?1 AND active_thread_id IS NOT NULL",
            [card_id],
            |row| row.get(0),
        )
        .ok())
}

async fn load_existing_thread_for_channel_pg(
    pool: &PgPool,
    card_id: &str,
    channel_id: u64,
) -> Result<Option<String>> {
    let row = match sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT channel_thread_map::text, active_thread_id
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(error) => {
            tracing::warn!(
                card_id,
                channel_id,
                %error,
                "[dispatch] failed to load postgres channel_thread_map; creating a new thread"
            );
            return Ok(None);
        }
    };

    let Some((map_json, active_thread_id)) = row else {
        return Ok(None);
    };

    if let Some(json_str) = map_json.as_deref()
        && !json_str.is_empty()
        && json_str != "{}"
    {
        let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(json_str)
            .map_err(|e| {
                anyhow::anyhow!(
                    "Cannot create dispatch for card {}: invalid channel_thread_map JSON: {}",
                    card_id,
                    e
                )
            })?;

        if let Some(value) = map.get(&channel_id.to_string()) {
            let thread_id = value.as_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "Cannot create dispatch for card {}: non-string thread mapping for channel {}",
                    card_id,
                    channel_id
                )
            })?;
            return Ok(Some(thread_id.to_string()));
        }
        return Ok(None);
    }

    Ok(active_thread_id.filter(|value| !value.trim().is_empty()))
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn lookup_active_dispatch_id(
    conn: &sqlite_test::Connection,
    card_id: &str,
    dispatch_type: &str,
) -> Option<String> {
    conn.query_row(
        "SELECT id FROM task_dispatches \
         WHERE kanban_card_id = ?1 AND dispatch_type = ?2 \
         AND status IN ('pending', 'dispatched') \
         ORDER BY rowid DESC LIMIT 1",
        sqlite_test::params![card_id, dispatch_type],
        |row| row.get(0),
    )
    .ok()
}

async fn lookup_active_dispatch_id_pg(
    pool: &PgPool,
    card_id: &str,
    dispatch_type: &str,
) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = $2
           AND status IN ('pending', 'dispatched')
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
    )
    .bind(card_id)
    .bind(dispatch_type)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn is_single_active_dispatch_violation(error: &sqlite_test::Error) -> bool {
    matches!(
        error,
        sqlite_test::Error::SqliteFailure(_, Some(message))
            if message.contains("UNIQUE constraint failed")
                && message.contains("task_dispatches.kanban_card_id")
    )
}

fn is_single_active_dispatch_violation_pg(error: &sqlx::Error) -> bool {
    let Some(db_error) = error.as_database_error() else {
        return false;
    };
    if db_error.code().as_deref() != Some("23505") {
        return false;
    }
    matches!(
        db_error.constraint(),
        Some(
            "idx_single_active_review"
                | "idx_single_active_review_decision"
                | "idx_single_active_create_pr"
        )
    )
}

const SESSION_AFFINITY_WORKER_LEASE_TTL_SECS: i64 = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
enum DispatchSessionAffinity {
    SessionId(i64),
    SessionKey(String),
}

fn dispatch_session_affinity_from_context(
    context_str: Option<&str>,
) -> Option<DispatchSessionAffinity> {
    let context =
        context_str.and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())?;
    let object = context.as_object()?;

    if let Some(session_id) = object.get("session_id") {
        if let Some(id) = session_id.as_i64().filter(|id| *id > 0) {
            return Some(DispatchSessionAffinity::SessionId(id));
        }
        if let Some(raw) = session_id
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if let Ok(id) = raw.parse::<i64>()
                && id > 0
            {
                return Some(DispatchSessionAffinity::SessionId(id));
            }
            return Some(DispatchSessionAffinity::SessionKey(raw.to_string()));
        }
    }

    object
        .get("session_key")
        .or_else(|| object.get("sessionKey"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| DispatchSessionAffinity::SessionKey(value.to_string()))
}

async fn load_live_session_owner_by_id_pg_tx(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    session_id: i64,
) -> Result<Option<String>> {
    sqlx::query_scalar::<_, String>(
        "WITH session_owner AS (
            SELECT NULLIF(BTRIM(instance_id), '') AS instance_id
              FROM sessions
             WHERE id = $1
             FOR UPDATE
         )
         SELECT so.instance_id
           FROM session_owner so
           JOIN worker_nodes wn ON wn.instance_id = so.instance_id
          WHERE wn.status = 'online'
            AND wn.last_heartbeat_at IS NOT NULL
            AND wn.last_heartbeat_at >= NOW() - ($2::BIGINT * INTERVAL '1 second')
          LIMIT 1",
    )
    .bind(session_id)
    .bind(SESSION_AFFINITY_WORKER_LEASE_TTL_SECS)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!("load live session owner for session id {session_id}: {error}")
    })
}

async fn load_live_session_owner_by_key_pg_tx(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    session_key: &str,
) -> Result<Option<String>> {
    sqlx::query_scalar::<_, String>(
        "WITH session_owner AS (
            SELECT NULLIF(BTRIM(instance_id), '') AS instance_id
              FROM sessions
             WHERE session_key = $1
             FOR UPDATE
         )
         SELECT so.instance_id
           FROM session_owner so
           JOIN worker_nodes wn ON wn.instance_id = so.instance_id
          WHERE wn.status = 'online'
            AND wn.last_heartbeat_at IS NOT NULL
            AND wn.last_heartbeat_at >= NOW() - ($2::BIGINT * INTERVAL '1 second')
          LIMIT 1",
    )
    .bind(session_key)
    .bind(SESSION_AFFINITY_WORKER_LEASE_TTL_SECS)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!("load live session owner for session key {session_key}: {error}")
    })
}

async fn load_session_affinity_claim_owner_pg_tx(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    context_str: Option<&str>,
) -> Result<Option<String>> {
    match dispatch_session_affinity_from_context(context_str) {
        Some(DispatchSessionAffinity::SessionId(session_id)) => {
            load_live_session_owner_by_id_pg_tx(tx, session_id).await
        }
        Some(DispatchSessionAffinity::SessionKey(session_key)) => {
            load_live_session_owner_by_key_pg_tx(tx, &session_key).await
        }
        None => Ok(None),
    }
}

fn non_empty_dispatch_required_capabilities(
    required: Option<&serde_json::Value>,
) -> Option<&serde_json::Value> {
    match required {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Object(map)) if map.is_empty() => None,
        Some(required) => Some(required),
    }
}

fn has_hard_required_capabilities(required: &serde_json::Value) -> bool {
    if let Some(hard_required) = required.get("required") {
        return capability_value_is_non_empty(hard_required);
    }
    match required {
        serde_json::Value::Null => false,
        serde_json::Value::Object(map) => map
            .iter()
            .any(|(key, value)| key != "preferred" && capability_value_is_non_empty(value)),
        _ => true,
    }
}

fn capability_value_is_non_empty(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Object(map) => !map.is_empty(),
        serde_json::Value::Array(items) => !items.is_empty(),
        _ => true,
    }
}

fn has_preferred_capabilities(required: &serde_json::Value) -> bool {
    required
        .get("preferred")
        .and_then(|value| value.as_object())
        .is_some_and(|map| !map.is_empty())
}

async fn load_live_capability_route_nodes_pg_tx(
    tx: &mut sqlx::Transaction<'_, Postgres>,
) -> Result<Vec<serde_json::Value>> {
    let rows = sqlx::query(
        "SELECT instance_id, labels, capabilities, last_heartbeat_at
         FROM worker_nodes
         WHERE status = 'online'
           AND last_heartbeat_at IS NOT NULL
           AND last_heartbeat_at >= NOW() - ($1::BIGINT * INTERVAL '1 second')
         ORDER BY last_heartbeat_at DESC, instance_id ASC",
    )
    .bind(SESSION_AFFINITY_WORKER_LEASE_TTL_SECS)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| anyhow::anyhow!("load live capability route worker nodes: {error}"))?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let labels = row
                .try_get::<Option<serde_json::Value>, _>("labels")
                .ok()
                .flatten()
                .unwrap_or_else(|| json!([]));
            let capabilities = row
                .try_get::<Option<serde_json::Value>, _>("capabilities")
                .ok()
                .flatten()
                .unwrap_or_else(|| json!({}));
            json!({
                "instance_id": row.try_get::<String, _>("instance_id").ok(),
                "status": "online",
                "labels": labels,
                "capabilities": capabilities,
                "last_heartbeat_at": row
                    .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_heartbeat_at")
                    .ok()
                    .flatten(),
            })
        })
        .collect())
}

async fn load_capability_claim_owner_pg_tx(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    required_capabilities: Option<&serde_json::Value>,
) -> Result<Option<String>> {
    let Some(required) = non_empty_dispatch_required_capabilities(required_capabilities) else {
        return Ok(None);
    };

    let worker_nodes = load_live_capability_route_nodes_pg_tx(tx).await?;
    let route_candidates = crate::server::cluster::select_capability_route(&worker_nodes, required);
    let Some(selected_owner) = route_candidates
        .first()
        .and_then(|candidate| candidate.decision.instance_id.as_deref())
        .map(str::to_string)
    else {
        return Ok(None);
    };

    if !has_hard_required_capabilities(required)
        && has_preferred_capabilities(required)
        && route_candidates
            .first()
            .map_or(true, |candidate| candidate.score <= 0)
    {
        return Ok(None);
    }

    let owner_node = worker_nodes.iter().find(|node| {
        node.get("instance_id").and_then(|value| value.as_str()) == Some(&selected_owner)
    });
    let decision =
        crate::services::dispatches::outbox_claiming::capability_decision_for_claim_owner(
            owner_node,
            &selected_owner,
            required,
        );

    Ok(decision.eligible.then_some(selected_owner))
}

async fn load_proactive_claim_owner_pg_tx(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    context_str: Option<&str>,
    required_capabilities: Option<&serde_json::Value>,
) -> Result<Option<String>> {
    if dispatch_session_affinity_from_context(context_str).is_some() {
        return load_session_affinity_claim_owner_pg_tx(tx, context_str).await;
    }

    load_capability_claim_owner_pg_tx(tx, required_capabilities).await
}

#[cfg(test)]
mod session_affinity_context_tests {
    use super::{DispatchSessionAffinity, dispatch_session_affinity_from_context};

    #[test]
    fn dispatch_session_affinity_prefers_numeric_session_id() {
        assert_eq!(
            dispatch_session_affinity_from_context(Some(r#"{"session_id":42}"#)),
            Some(DispatchSessionAffinity::SessionId(42))
        );
        assert_eq!(
            dispatch_session_affinity_from_context(Some(r#"{"session_id":" 42 "}"#)),
            Some(DispatchSessionAffinity::SessionId(42))
        );
    }

    #[test]
    fn dispatch_session_affinity_accepts_session_key_fallbacks() {
        assert_eq!(
            dispatch_session_affinity_from_context(Some(
                r#"{"session_id":"mac-mini:AgentDesk-codex"}"#
            )),
            Some(DispatchSessionAffinity::SessionKey(
                "mac-mini:AgentDesk-codex".to_string()
            ))
        );
        assert_eq!(
            dispatch_session_affinity_from_context(Some(r#"{"sessionKey":" session-a "}"#)),
            Some(DispatchSessionAffinity::SessionKey("session-a".to_string()))
        );
    }

    #[test]
    fn dispatch_session_affinity_ignores_missing_or_malformed_context() {
        assert_eq!(dispatch_session_affinity_from_context(None), None);
        assert_eq!(dispatch_session_affinity_from_context(Some("{")), None);
        assert_eq!(
            dispatch_session_affinity_from_context(Some(r#"{"session_id":" "}"#)),
            None
        );
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn validate_dispatch_target_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    context_json: Option<&str>,
) -> Result<()> {
    let provider_override =
        dispatch_destination_provider_override(Some(dispatch_type), context_json);
    let channel_role = if let Some(provider) = provider_override.as_deref() {
        format!("{provider} provider")
    } else if dispatch_uses_alt_channel(dispatch_type) {
        "counter-model".to_string()
    } else {
        "primary".to_string()
    };

    let channel_value: Option<String> = (if let Some(provider) = provider_override.as_deref() {
        resolve_agent_channel_for_provider_on_conn(conn, to_agent_id, Some(provider))
    } else {
        resolve_agent_dispatch_channel_on_conn(conn, to_agent_id, Some(dispatch_type))
    })
    .ok()
    .flatten()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty());

    let channel_value = channel_value.ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot create {} dispatch: agent '{}' has no {} discord channel (card {})",
            dispatch_type,
            to_agent_id,
            channel_role,
            card_id
        )
    })?;

    let channel_id = resolve_dispatch_channel_id(&channel_value).ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot create {} dispatch: agent '{}' has invalid {} discord channel '{}' (card {})",
            dispatch_type,
            to_agent_id,
            channel_role,
            channel_value,
            card_id
        )
    })?;

    if let Some(thread_id) = load_existing_thread_for_channel(conn, card_id, channel_id)?
        && thread_id.parse::<u64>().is_err()
    {
        return Err(anyhow::anyhow!(
            "Cannot create {} dispatch: card '{}' has invalid thread '{}' for channel {}",
            dispatch_type,
            card_id,
            thread_id,
            channel_id
        ));
    }

    Ok(())
}

async fn validate_dispatch_target_on_pg(
    pool: &PgPool,
    card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    context_json: Option<&str>,
) -> Result<()> {
    let provider_override =
        dispatch_destination_provider_override(Some(dispatch_type), context_json);
    let channel_role = if let Some(provider) = provider_override.as_deref() {
        format!("{provider} provider")
    } else if dispatch_uses_alt_channel(dispatch_type) {
        "counter-model".to_string()
    } else {
        "primary".to_string()
    };

    let channel_value: Option<String> = (if let Some(provider) = provider_override.as_deref() {
        resolve_agent_channel_for_provider_pg(pool, to_agent_id, Some(provider)).await
    } else {
        resolve_agent_dispatch_channel_pg(pool, to_agent_id, Some(dispatch_type)).await
    })
    .ok()
    .flatten()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty());

    let channel_value = channel_value.ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot create {} dispatch: agent '{}' has no {} discord channel (card {})",
            dispatch_type,
            to_agent_id,
            channel_role,
            card_id
        )
    })?;

    let channel_id = resolve_dispatch_channel_id(&channel_value).ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot create {} dispatch: agent '{}' has invalid {} discord channel '{}' (card {})",
            dispatch_type,
            to_agent_id,
            channel_role,
            channel_value,
            card_id
        )
    })?;

    if let Some(thread_id) = load_existing_thread_for_channel_pg(pool, card_id, channel_id).await?
        && thread_id.parse::<u64>().is_err()
    {
        return Err(anyhow::anyhow!(
            "Cannot create {} dispatch: card '{}' has invalid thread '{}' for channel {}",
            dispatch_type,
            card_id,
            thread_id,
            channel_id
        ));
    }

    Ok(())
}

fn block_on_dispatch_pg<F, T>(
    pool: &PgPool,
    future_factory: impl FnOnce(PgPool) -> F + Send + 'static,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    crate::utils::async_bridge::block_on_pg_result(pool, future_factory, |error| {
        anyhow::anyhow!("{error}")
    })
}

fn normalize_required_capabilities(required: serde_json::Value) -> Option<serde_json::Value> {
    match &required {
        serde_json::Value::Null => None,
        serde_json::Value::Object(map) if map.is_empty() => None,
        _ => Some(required),
    }
}

pub(crate) fn dispatch_required_capabilities_from_routing(
    context: &serde_json::Value,
    dispatch_type: &str,
    routing: &crate::config::ClusterDispatchRoutingConfig,
) -> Option<serde_json::Value> {
    if let Some(required) = context.get("required_capabilities") {
        return normalize_required_capabilities(required.clone());
    }
    if routing.is_opted_out(dispatch_type) || routing.default_preferred_labels.is_empty() {
        return None;
    }

    Some(json!({
        "preferred": {
            "labels": routing.default_preferred_labels.clone(),
        }
    }))
}

fn dispatch_required_capabilities(
    context_str: &str,
    dispatch_type: &str,
) -> Option<serde_json::Value> {
    let context = serde_json::from_str::<serde_json::Value>(context_str).ok()?;
    let config = crate::config::load_graceful();
    dispatch_required_capabilities_from_routing(
        &context,
        dispatch_type,
        &config.cluster.dispatch_routing,
    )
}

#[allow(clippy::too_many_arguments)]
async fn create_dispatch_core_internal(
    pg_pool: &PgPool,
    dispatch_id: &str,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
    mut options: DispatchCreateOptions,
) -> Result<(String, String, bool)> {
    let (old_status, card_repo_id, card_agent_id) =
        sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
            "SELECT status, repo_id, assigned_agent_id
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(kanban_card_id)
        .fetch_optional(pg_pool)
        .await
        .map_err(|error| anyhow::anyhow!("Card lookup error: {error}"))?
        .ok_or_else(|| anyhow::anyhow!("Card not found: {kanban_card_id}"))?;

    let agent_exists = sqlx::query_scalar::<_, i32>(
        "SELECT 1
         FROM agents
         WHERE id = $1
         LIMIT 1",
    )
    .bind(to_agent_id)
    .fetch_optional(pg_pool)
    .await
    .map_err(|error| anyhow::anyhow!("Agent lookup error: {error}"))?
    .is_some();
    if !agent_exists {
        return Err(anyhow::anyhow!(
            "Cannot create {} dispatch: agent '{}' not found (card {})",
            dispatch_type,
            to_agent_id,
            kanban_card_id
        ));
    }

    if dispatch_context_requests_sidecar(context) {
        options.sidecar_dispatch = true;
    }

    crate::pipeline::ensure_loaded();
    let effective = crate::pipeline::resolve_for_card_pg(
        pg_pool,
        card_repo_id.as_deref(),
        card_agent_id.as_deref(),
    )
    .await;
    let is_terminal = effective.is_terminal(&old_status);
    if is_terminal && !options.sidecar_dispatch {
        return Err(anyhow::anyhow!(
            "Cannot create {} dispatch for terminal card {} (status: {}) — cannot revert terminal card",
            dispatch_type,
            kanban_card_id,
            old_status
        ));
    }

    if dispatch_type != "review-decision"
        && let Some(existing_id) =
            lookup_active_dispatch_id_pg(pg_pool, kanban_card_id, dispatch_type).await
    {
        tracing::info!(
            "DEDUP: reusing existing dispatch {} for card {} type {}",
            existing_id,
            kanban_card_id,
            dispatch_type
        );
        return Ok((existing_id, old_status, true));
    }

    let (parent_dispatch_id, chain_depth) =
        resolve_parent_dispatch_context(pg_pool, kanban_card_id, context).await?;

    let caller_target_repo_source = if json_string_field(context, "target_repo").is_some() {
        TargetRepoSource::CallerSupplied
    } else {
        TargetRepoSource::CardScopeDefault
    };
    let mut context_with_session_strategy =
        dispatch_context_with_session_strategy(dispatch_type, context);
    let target_repo = resolve_card_target_repo_ref(
        pg_pool,
        kanban_card_id,
        Some(&context_with_session_strategy),
    )
    .await;
    if let Some(target_repo) = target_repo.as_deref()
        && let Some(obj) = context_with_session_strategy.as_object_mut()
    {
        obj.entry("target_repo".to_string())
            .or_insert_with(|| json!(target_repo));
    }
    inject_work_dispatch_baseline_commit(dispatch_type, &mut context_with_session_strategy);
    let context_str = if dispatch_type == "review" {
        build_review_context(
            pg_pool,
            kanban_card_id,
            to_agent_id,
            &context_with_session_strategy,
            ReviewTargetTrust::Untrusted,
            caller_target_repo_source,
        )
        .await?
    } else {
        let mut base = serde_json::to_string(&context_with_session_strategy)?;
        let phase_gate_sidecar = context_with_session_strategy
            .get("phase_gate")
            .and_then(|value| value.as_object())
            .is_some();
        let worktree_target = if let Some((wt_path, wt_branch)) =
            dispatch_context_worktree_target(&context_with_session_strategy)?
        {
            Some((wt_path, wt_branch, false))
        } else if phase_gate_sidecar {
            None
        } else if dispatch_type_requires_fresh_worktree(Some(dispatch_type)) {
            let (wt_path, wt_branch, _, created) = ensure_card_worktree(
                pg_pool,
                kanban_card_id,
                Some(&context_with_session_strategy),
            )
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Cannot create {} dispatch for card {}: fresh worktree required but card issue/repo could not be resolved",
                    dispatch_type,
                    kanban_card_id
                )
            })?;
            Some((wt_path, Some(wt_branch), created))
        } else {
            resolve_card_worktree(
                pg_pool,
                kanban_card_id,
                Some(&context_with_session_strategy),
            )
            .await?
            .map(|(wt_path, wt_branch, _)| (wt_path, Some(wt_branch), false))
        };

        if let Some((wt_path, wt_branch, managed_created)) = worktree_target
            && let Ok(mut obj) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&base)
        {
            obj.insert("worktree_path".to_string(), json!(wt_path.clone()));
            if let Some(wt_branch) = wt_branch {
                obj.insert("worktree_branch".to_string(), json!(wt_branch));
            }
            if managed_created {
                obj.insert("managed_worktree".to_string(), json!(true));
                obj.insert("managed_worktree_cleanup".to_string(), json!("terminal"));
            }
            tracing::info!(
                "[dispatch] {} dispatch for card {}: injecting worktree_path={}",
                dispatch_type,
                kanban_card_id,
                wt_path
            );
            base = serde_json::to_string(&serde_json::Value::Object(obj)).unwrap_or(base);
        }
        if dispatch_type == "review-decision"
            && let Ok(mut obj) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&base)
        {
            inject_review_dispatch_identifiers(pg_pool, kanban_card_id, dispatch_type, &mut obj)
                .await;
            base = serde_json::to_string(&serde_json::Value::Object(obj)).unwrap_or(base);
        }
        base
    };
    let is_review_type = dispatch_type == "review"
        || dispatch_type == "review-decision"
        || dispatch_type == "rework"
        || dispatch_type == "consultation";
    validate_dispatch_target_on_pg(
        pg_pool,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        Some(&context_str),
    )
    .await?;

    let attach_result = apply_dispatch_attached_intents_pg(
        pg_pool,
        kanban_card_id,
        to_agent_id,
        dispatch_id,
        dispatch_type,
        is_review_type,
        &old_status,
        &effective,
        title,
        &context_str,
        parent_dispatch_id.as_deref(),
        chain_depth,
        options,
    )
    .await;

    if let Err(error) = attach_result {
        if matches!(dispatch_type, "review" | "review-decision" | "create-pr")
            && error
                .to_string()
                .contains("concurrent race prevented by DB constraint")
            && let Some(existing_id) =
                lookup_active_dispatch_id_pg(pg_pool, kanban_card_id, dispatch_type).await
        {
            tracing::info!(
                "DEDUP: reusing existing dispatch {} for card {} type {} after UNIQUE race",
                existing_id,
                kanban_card_id,
                dispatch_type
            );
            return Ok((existing_id, old_status, true));
        }
        return Err(error);
    }
    Ok((dispatch_id.to_string(), old_status, false))
}

pub async fn create_dispatch_core(
    pg_pool: &PgPool,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<(String, String, bool)> {
    create_dispatch_core_with_options(
        pg_pool,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        DispatchCreateOptions::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn create_dispatch_core_with_options(
    pg_pool: &PgPool,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
    options: DispatchCreateOptions,
) -> Result<(String, String, bool)> {
    let dispatch_id = uuid::Uuid::new_v4().to_string();
    create_dispatch_core_internal(
        pg_pool,
        &dispatch_id,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        options,
    )
    .await
}

pub async fn create_dispatch_core_with_id(
    pg_pool: &PgPool,
    dispatch_id: &str,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<(String, String, bool)> {
    create_dispatch_core_with_id_and_options(
        pg_pool,
        dispatch_id,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        DispatchCreateOptions::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn create_dispatch_core_with_id_and_options(
    pg_pool: &PgPool,
    dispatch_id: &str,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
    options: DispatchCreateOptions,
) -> Result<(String, String, bool)> {
    create_dispatch_core_internal(
        pg_pool,
        dispatch_id,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        options,
    )
    .await
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_dispatch_record_sqlite_test(
    db: &Db,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
    options: DispatchCreateOptions,
) -> Result<(String, String, bool)> {
    let dispatch_id = uuid::Uuid::new_v4().to_string();
    create_dispatch_record_with_id_sqlite_test(
        db,
        &dispatch_id,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        options,
    )
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_dispatch_record_with_id_sqlite_test(
    db: &Db,
    dispatch_id: &str,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
    mut options: DispatchCreateOptions,
) -> Result<(String, String, bool)> {
    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB conn error: {e}"))?;

    let (old_status, card_repo_id, card_agent_id): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
            [kanban_card_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|e| anyhow::anyhow!("Card not found: {e}"))?;

    let agent_exists: bool = conn
        .query_row("SELECT 1 FROM agents WHERE id = ?1", [to_agent_id], |_| {
            Ok(())
        })
        .is_ok();
    if !agent_exists {
        return Err(anyhow::anyhow!(
            "Cannot create {} dispatch: agent '{}' not found (card {})",
            dispatch_type,
            to_agent_id,
            kanban_card_id
        ));
    }
    if dispatch_context_requests_sidecar(context) {
        options.sidecar_dispatch = true;
    }

    crate::pipeline::ensure_loaded();
    let effective =
        crate::pipeline::resolve_for_card(&conn, card_repo_id.as_deref(), card_agent_id.as_deref());
    if effective.is_terminal(&old_status) && !options.sidecar_dispatch {
        return Err(anyhow::anyhow!(
            "Cannot create {} dispatch for terminal card {} (status: {}) — cannot revert terminal card",
            dispatch_type,
            kanban_card_id,
            old_status
        ));
    }

    if dispatch_type != "review-decision"
        && let Some(existing_id) = lookup_active_dispatch_id(&conn, kanban_card_id, dispatch_type)
    {
        tracing::info!(
            "DEDUP: reusing existing dispatch {} for card {} type {}",
            existing_id,
            kanban_card_id,
            dispatch_type
        );
        return Ok((existing_id, old_status, true));
    }

    let (parent_dispatch_id, chain_depth) =
        resolve_parent_dispatch_context_sqlite_test(&conn, kanban_card_id, context)?;

    let caller_target_repo_source = if json_string_field(context, "target_repo").is_some() {
        TargetRepoSource::CallerSupplied
    } else {
        TargetRepoSource::CardScopeDefault
    };
    let mut context_with_session_strategy =
        dispatch_context_with_session_strategy(dispatch_type, context);
    let target_repo = resolve_card_target_repo_ref_sqlite_test(
        db,
        kanban_card_id,
        Some(&context_with_session_strategy),
    );
    if let Some(target_repo) = target_repo.as_deref()
        && let Some(obj) = context_with_session_strategy.as_object_mut()
    {
        obj.entry("target_repo".to_string())
            .or_insert_with(|| json!(target_repo));
    }
    inject_work_dispatch_baseline_commit(dispatch_type, &mut context_with_session_strategy);
    let context_str = if dispatch_type == "review" {
        build_review_context_sqlite_test(
            db,
            kanban_card_id,
            to_agent_id,
            &context_with_session_strategy,
            ReviewTargetTrust::Untrusted,
            caller_target_repo_source,
        )?
    } else {
        let mut base = serde_json::to_string(&context_with_session_strategy)?;
        let phase_gate_sidecar = context_with_session_strategy
            .get("phase_gate")
            .and_then(|value| value.as_object())
            .is_some();
        let worktree_target = if let Some((wt_path, wt_branch)) =
            dispatch_context_worktree_target(&context_with_session_strategy)?
        {
            Some((wt_path, wt_branch))
        } else if phase_gate_sidecar {
            None
        } else {
            resolve_card_worktree_sqlite_test(
                db,
                kanban_card_id,
                Some(&context_with_session_strategy),
            )?
            .map(|(wt_path, wt_branch, _)| (wt_path, Some(wt_branch)))
        };

        if let Some((wt_path, wt_branch)) = worktree_target
            && let Ok(mut obj) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&base)
        {
            obj.insert("worktree_path".to_string(), json!(wt_path.clone()));
            if let Some(wt_branch) = wt_branch {
                obj.insert("worktree_branch".to_string(), json!(wt_branch));
            }
            tracing::info!(
                "[dispatch] {} dispatch for card {}: injecting worktree_path={}",
                dispatch_type,
                kanban_card_id,
                wt_path
            );
            base = serde_json::to_string(&serde_json::Value::Object(obj)).unwrap_or(base);
        }
        if dispatch_type == "review-decision"
            && let Ok(mut obj) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&base)
        {
            inject_review_dispatch_identifiers_sqlite_test(
                db,
                kanban_card_id,
                dispatch_type,
                &mut obj,
            );
            base = serde_json::to_string(&serde_json::Value::Object(obj)).unwrap_or(base);
        }
        base
    };
    let is_review_type = matches!(
        dispatch_type,
        "review" | "review-decision" | "rework" | "consultation"
    );
    validate_dispatch_target_on_conn(
        &conn,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        Some(&context_str),
    )?;

    if dispatch_type == "review-decision" {
        let mut stmt = conn.prepare(
            "SELECT id FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND dispatch_type = 'review-decision' \
             AND status IN ('pending', 'dispatched')",
        )?;
        let stale_ids: Vec<String> = stmt
            .query_map([kanban_card_id], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        let mut cancelled = 0;
        for stale_id in &stale_ids {
            cancelled += cancel_dispatch_and_reset_auto_queue_on_conn(
                &conn,
                stale_id,
                Some("superseded_by_new_review_decision"),
            )?;
        }
        if cancelled > 0 {
            tracing::info!(
                "[dispatch] Cancelled {} stale review-decision(s) for card {} before creating new one",
                cancelled,
                kanban_card_id
            );
        }
    }

    if let Err(error) = apply_dispatch_attached_intents(
        &conn,
        kanban_card_id,
        to_agent_id,
        dispatch_id,
        dispatch_type,
        is_review_type,
        &old_status,
        &effective,
        title,
        &context_str,
        parent_dispatch_id.as_deref(),
        chain_depth,
        options,
    ) {
        if matches!(dispatch_type, "review" | "review-decision" | "create-pr")
            && error
                .to_string()
                .contains("concurrent race prevented by DB constraint")
            && let Some(existing_id) =
                lookup_active_dispatch_id(&conn, kanban_card_id, dispatch_type)
        {
            tracing::info!(
                "DEDUP: reusing existing dispatch {} for card {} type {} after UNIQUE race",
                existing_id,
                kanban_card_id,
                dispatch_type
            );
            return Ok((existing_id, old_status, true));
        }
        return Err(error);
    }

    Ok((dispatch_id.to_string(), old_status, false))
}

pub fn create_dispatch(
    db: &Db,
    engine: &PolicyEngine,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    create_dispatch_with_options(
        db,
        engine.pg_pool(),
        engine,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        DispatchCreateOptions::default(),
    )
}

#[allow(clippy::too_many_arguments)]
fn create_dispatch_with_options_pg_backed(
    db: Option<&Db>,
    pool: &PgPool,
    engine: &PolicyEngine,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
    options: DispatchCreateOptions,
) -> Result<serde_json::Value> {
    let options = DispatchCreateOptions {
        sidecar_dispatch: options.sidecar_dispatch || dispatch_context_requests_sidecar(context),
        ..options
    };
    let card_id_owned = kanban_card_id.to_string();
    let agent_id_owned = to_agent_id.to_string();
    let dispatch_type_owned = dispatch_type.to_string();
    let title_owned = title.to_string();
    let context_owned = context.clone();
    let (dispatch_id, old_status, reused) = block_on_dispatch_pg(pool, move |pool| async move {
        create_dispatch_core_with_options(
            &pool,
            &card_id_owned,
            &agent_id_owned,
            &dispatch_type_owned,
            &title_owned,
            &context_owned,
            options,
        )
        .await
    })?;

    let dispatch = {
        let dispatch_id = dispatch_id.clone();
        block_on_dispatch_pg(pool, move |pool| async move {
            query_dispatch_row_pg(&pool, &dispatch_id).await
        })?
    };
    if reused {
        let mut d = dispatch;
        d["__reused"] = json!(true);
        return Ok(d);
    }
    if options.sidecar_dispatch {
        return Ok(dispatch);
    }

    crate::pipeline::ensure_loaded();
    let old_status_for_kickoff = old_status.clone();
    let card_id_for_kickoff = kanban_card_id.to_string();
    let kickoff_owned = block_on_dispatch_pg(pool, move |pool| async move {
        let (card_repo_id, card_agent_id): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT repo_id, assigned_agent_id
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(&card_id_for_kickoff)
        .fetch_optional(&pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "load postgres card pipeline context for {}: {}",
                card_id_for_kickoff,
                error
            )
        })?
        .unwrap_or((None, None));
        let effective = crate::pipeline::resolve_for_card_pg(
            &pool,
            card_repo_id.as_deref(),
            card_agent_id.as_deref(),
        )
        .await;
        Ok(effective
            .kickoff_for(&old_status_for_kickoff)
            .unwrap_or_else(|| {
                tracing::error!("Pipeline has no kickoff state for hook firing");
                effective.initial_state().to_string()
            })
            .to_string())
    })?;
    crate::kanban::fire_state_hooks_with_backends(
        db,
        engine,
        kanban_card_id,
        &old_status,
        &kickoff_owned,
    );

    Ok(dispatch)
}

#[allow(clippy::too_many_arguments)]
pub fn create_dispatch_with_options(
    db: &Db,
    pg_pool: Option<&PgPool>,
    engine: &PolicyEngine,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
    options: DispatchCreateOptions,
) -> Result<serde_json::Value> {
    let options = DispatchCreateOptions {
        sidecar_dispatch: options.sidecar_dispatch || dispatch_context_requests_sidecar(context),
        ..options
    };
    if let Some(pool) = pg_pool {
        return create_dispatch_with_options_pg_backed(
            Some(db),
            pool,
            engine,
            kanban_card_id,
            to_agent_id,
            dispatch_type,
            title,
            context,
            options,
        );
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    {
        return create_dispatch_with_options_sqlite_test(
            db,
            engine,
            kanban_card_id,
            to_agent_id,
            dispatch_type,
            title,
            context,
            options,
        );
    }
    #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
    {
        Err(anyhow::anyhow!(
            "Postgres pool required for create_dispatch_with_options"
        ))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn create_dispatch_pg_only(
    pg_pool: &PgPool,
    engine: &PolicyEngine,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    create_dispatch_with_options_pg_only(
        pg_pool,
        engine,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        DispatchCreateOptions::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn create_dispatch_with_options_pg_only(
    pg_pool: &PgPool,
    engine: &PolicyEngine,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
    options: DispatchCreateOptions,
) -> Result<serde_json::Value> {
    let options = DispatchCreateOptions {
        sidecar_dispatch: options.sidecar_dispatch || dispatch_context_requests_sidecar(context),
        ..options
    };
    create_dispatch_with_options_pg_backed(
        None,
        pg_pool,
        engine,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        options,
    )
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[allow(clippy::too_many_arguments)]
fn create_dispatch_with_options_sqlite_test(
    db: &Db,
    engine: &PolicyEngine,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
    options: DispatchCreateOptions,
) -> Result<serde_json::Value> {
    let (dispatch_id, old_status, reused) = create_dispatch_record_sqlite_test(
        db,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        options,
    )?;

    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB lock error: {e}"))?;
    let dispatch = query_dispatch_row(&conn, &dispatch_id)?;

    if reused {
        let mut d = dispatch;
        d["__reused"] = json!(true);
        return Ok(d);
    }
    if options.sidecar_dispatch {
        drop(conn);
        return Ok(dispatch);
    }

    crate::pipeline::ensure_loaded();
    let (card_repo_id, card_agent_id): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
            [kanban_card_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or((None, None));
    let effective =
        crate::pipeline::resolve_for_card(&conn, card_repo_id.as_deref(), card_agent_id.as_deref());
    drop(conn);
    let kickoff_owned = effective.kickoff_for(&old_status).unwrap_or_else(|| {
        tracing::error!("Pipeline has no kickoff state for hook firing");
        effective.initial_state().to_string()
    });
    crate::kanban::fire_state_hooks(db, engine, kanban_card_id, &old_status, &kickoff_owned);

    Ok(dispatch)
}

/// Test-only sqlite wrapper. Opens BEGIN/COMMIT around the on-conn variant.
///
/// Production callers use the PG helpers (`apply_dispatch_attached_intents_pg`
/// / `apply_dispatch_attached_intents_on_pg_tx`). This wrapper remains only
/// for sqlite-backed test fixtures.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[allow(clippy::too_many_arguments)]
fn apply_dispatch_attached_intents(
    conn: &sqlite_test::Connection,
    card_id: &str,
    to_agent_id: &str,
    dispatch_id: &str,
    dispatch_type: &str,
    is_review_type: bool,
    old_status: &str,
    effective: &crate::pipeline::PipelineConfig,
    title: &str,
    context_str: &str,
    parent_dispatch_id: Option<&str>,
    chain_depth: i64,
    options: DispatchCreateOptions,
) -> Result<()> {
    conn.execute_batch("BEGIN")?;
    let result = apply_dispatch_attached_intents_on_conn(
        conn,
        card_id,
        to_agent_id,
        dispatch_id,
        dispatch_type,
        is_review_type,
        old_status,
        effective,
        title,
        context_str,
        parent_dispatch_id,
        chain_depth,
        options,
    );
    match &result {
        Ok(_) => {
            conn.execute_batch("COMMIT")?;
        }
        Err(_) => {
            conn.execute_batch("ROLLBACK").ok();
        }
    }
    result
}

async fn record_dispatch_status_event_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    dispatch_id: &str,
    from_status: Option<&str>,
    to_status: &str,
    transition_source: &str,
    payload: Option<&serde_json::Value>,
) -> Result<()> {
    let row = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT kanban_card_id, dispatch_type
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!("load postgres dispatch event context {dispatch_id}: {error}")
    })?;
    let (kanban_card_id, dispatch_type) = row.unwrap_or((None, None));

    sqlx::query(
        "INSERT INTO dispatch_events (
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload_json
        ) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(dispatch_id)
    .bind(kanban_card_id)
    .bind(dispatch_type)
    .bind(from_status)
    .bind(to_status)
    .bind(transition_source)
    .bind(payload.cloned())
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!("insert postgres dispatch event for {dispatch_id}: {error}")
    })?;
    Ok(())
}

async fn ensure_dispatch_notify_outbox_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    dispatch_id: &str,
    agent_id: &str,
    card_id: &str,
    title: &str,
) -> Result<bool> {
    let dispatch_row = sqlx::query_as::<_, (String, Option<String>, Option<serde_json::Value>)>(
        "SELECT status, context, required_capabilities
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| anyhow::anyhow!("load postgres dispatch status {dispatch_id}: {error}"))?;

    let Some((dispatch_status, context_str, required_capabilities)) = dispatch_row else {
        return Ok(false);
    };

    if matches!(
        dispatch_status.as_str(),
        "completed" | "failed" | "cancelled"
    ) {
        return Ok(false);
    }

    let claim_owner = load_proactive_claim_owner_pg_tx(
        tx,
        context_str.as_deref(),
        required_capabilities.as_ref(),
    )
    .await?;

    let inserted = sqlx::query(
        "INSERT INTO dispatch_outbox (
            dispatch_id, action, agent_id, card_id, title, required_capabilities, claim_owner
         )
         SELECT $1, 'notify', $2, $3, $4, required_capabilities, $5
           FROM task_dispatches
          WHERE id = $1
         ON CONFLICT DO NOTHING",
    )
    .bind(dispatch_id)
    .bind(agent_id)
    .bind(card_id)
    .bind(title)
    .bind(claim_owner.as_deref())
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!("insert postgres dispatch outbox for {dispatch_id}: {error}")
    })?;

    Ok(inserted.rows_affected() > 0)
}

async fn cancel_stale_review_decisions_pg_tx(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    card_id: &str,
) -> Result<usize> {
    let stale_ids = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = 'review-decision'
           AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!("load stale postgres review-decision dispatches for {card_id}: {error}")
    })?;

    let mut cancelled = 0usize;
    for stale_id in &stale_ids {
        cancelled += cancel_dispatch_and_reset_auto_queue_on_pg_tx(
            tx,
            stale_id,
            Some("superseded_by_new_review_decision"),
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    }

    Ok(cancelled)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_dispatch_attached_intents_pg(
    pool: &PgPool,
    card_id: &str,
    to_agent_id: &str,
    dispatch_id: &str,
    dispatch_type: &str,
    is_review_type: bool,
    old_status: &str,
    effective: &crate::pipeline::PipelineConfig,
    title: &str,
    context_str: &str,
    parent_dispatch_id: Option<&str>,
    chain_depth: i64,
    options: DispatchCreateOptions,
) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| anyhow::anyhow!("begin postgres dispatch {dispatch_id}: {error}"))?;

    let result = async {
        if dispatch_type == "review-decision" {
            let cancelled = cancel_stale_review_decisions_pg_tx(&mut tx, card_id).await?;
            if cancelled > 0 {
                tracing::info!(
                    "[dispatch] Cancelled {} stale review-decision(s) for card {} before creating new one",
                    cancelled,
                    card_id
                );
            }
        }

        apply_dispatch_attached_intents_on_pg_tx(
            &mut tx,
            card_id,
            to_agent_id,
            dispatch_id,
            dispatch_type,
            is_review_type,
            old_status,
            effective,
            title,
            context_str,
            parent_dispatch_id,
            chain_depth,
            options,
        )
        .await
    }
    .await;

    match result {
        Ok(()) => tx
            .commit()
            .await
            .map_err(|error| anyhow::anyhow!("commit postgres dispatch {dispatch_id}: {error}")),
        Err(error) => {
            let _ = tx.rollback().await;
            Err(error)
        }
    }
}

/// Transaction-local PG variant: does NOT manage its own transaction.
/// Caller must already have an open postgres transaction and commit/rollback
/// after this returns.
///
/// This exists for callers like review-automation handoff paths that need to
/// compose dispatch creation with surrounding PG updates in one atomic unit.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_dispatch_attached_intents_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    card_id: &str,
    to_agent_id: &str,
    dispatch_id: &str,
    dispatch_type: &str,
    is_review_type: bool,
    old_status: &str,
    effective: &crate::pipeline::PipelineConfig,
    title: &str,
    context_str: &str,
    parent_dispatch_id: Option<&str>,
    chain_depth: i64,
    options: DispatchCreateOptions,
) -> Result<()> {
    use crate::engine::transition::{
        self, CardState, GateSnapshot, TransitionContext, TransitionEvent, TransitionOutcome,
    };

    let kickoff_state = if !is_review_type && !options.sidecar_dispatch {
        Some(effective.kickoff_for(old_status).unwrap_or_else(|| {
            tracing::error!("Pipeline has no kickoff state — check pipeline configuration");
            effective.initial_state().to_string()
        }))
    } else {
        None
    };

    let ctx = TransitionContext {
        card: CardState {
            id: card_id.to_string(),
            status: old_status.to_string(),
            review_status: None,
            latest_dispatch_id: None,
        },
        pipeline: effective.clone(),
        gates: GateSnapshot::default(),
    };

    let decision = if options.sidecar_dispatch {
        transition::TransitionDecision {
            outcome: transition::TransitionOutcome::Allowed,
            intents: Vec::new(),
        }
    } else {
        transition::decide_transition(
            &ctx,
            &TransitionEvent::DispatchAttached {
                dispatch_id: dispatch_id.to_string(),
                dispatch_type: dispatch_type.to_string(),
                kickoff_state,
            },
        )
    };

    if let TransitionOutcome::Blocked(reason) = &decision.outcome {
        return Err(anyhow::anyhow!("{}", reason));
    }
    let required_capabilities = dispatch_required_capabilities(context_str, dispatch_type);

    if let Err(error) = sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
            parent_dispatch_id, chain_depth, required_capabilities, created_at, updated_at
        ) VALUES (
            $1, $2, $3, $4, 'pending', $5, $6, $7, $8, $9, NOW(), NOW()
        )",
    )
    .bind(dispatch_id)
    .bind(card_id)
    .bind(to_agent_id)
    .bind(dispatch_type)
    .bind(title)
    .bind(context_str)
    .bind(parent_dispatch_id)
    .bind(chain_depth)
    .bind(required_capabilities.as_ref())
    .execute(&mut **tx)
    .await
    {
        if matches!(dispatch_type, "review" | "review-decision" | "create-pr")
            && is_single_active_dispatch_violation_pg(&error)
        {
            return Err(anyhow::anyhow!(
                "{} already exists for card {} (concurrent race prevented by DB constraint)",
                dispatch_type,
                card_id
            ));
        }
        return Err(anyhow::anyhow!(
            "insert postgres dispatch {dispatch_id} for {card_id}: {error}"
        ));
    }

    record_dispatch_status_event_on_pg_tx(
        tx,
        dispatch_id,
        None,
        "pending",
        "create_dispatch",
        None,
    )
    .await?;
    if !options.skip_outbox {
        ensure_dispatch_notify_outbox_on_pg_tx(tx, dispatch_id, to_agent_id, card_id, title)
            .await?;
    }
    for intent in &decision.intents {
        crate::engine::transition_executor_pg::execute_activate_transition_intent_pg(tx, intent)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
    }
    Ok(())
}

/// Test-only sqlite connection-local variant.
///
/// Production transactional callers use `apply_dispatch_attached_intents_on_pg_tx`.
/// This exists only for sqlite-backed test fixtures that still exercise the
/// old connection-local transition plumbing.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[allow(clippy::too_many_arguments)]
fn apply_dispatch_attached_intents_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
    to_agent_id: &str,
    dispatch_id: &str,
    dispatch_type: &str,
    is_review_type: bool,
    old_status: &str,
    effective: &crate::pipeline::PipelineConfig,
    title: &str,
    context_str: &str,
    parent_dispatch_id: Option<&str>,
    chain_depth: i64,
    options: DispatchCreateOptions,
) -> Result<()> {
    use crate::engine::transition::{
        self, CardState, GateSnapshot, TransitionContext, TransitionEvent, TransitionOutcome,
    };

    let kickoff_state = if !is_review_type && !options.sidecar_dispatch {
        Some(effective.kickoff_for(old_status).unwrap_or_else(|| {
            tracing::error!("Pipeline has no kickoff state — check pipeline configuration");
            effective.initial_state().to_string()
        }))
    } else {
        None
    };

    let ctx = TransitionContext {
        card: CardState {
            id: card_id.to_string(),
            status: old_status.to_string(),
            review_status: None,
            latest_dispatch_id: None,
        },
        pipeline: effective.clone(),
        gates: GateSnapshot::default(),
    };

    let decision = if options.sidecar_dispatch {
        transition::TransitionDecision {
            outcome: transition::TransitionOutcome::Allowed,
            intents: Vec::new(),
        }
    } else {
        transition::decide_transition(
            &ctx,
            &TransitionEvent::DispatchAttached {
                dispatch_id: dispatch_id.to_string(),
                dispatch_type: dispatch_type.to_string(),
                kickoff_state,
            },
        )
    };

    if let TransitionOutcome::Blocked(reason) = &decision.outcome {
        return Err(anyhow::anyhow!("{}", reason));
    }

    // Caller owns the transaction. See the wrapper `apply_dispatch_attached_intents`
    // for the BEGIN/COMMIT-owning variant.
    if let Err(e) = conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
            parent_dispatch_id, chain_depth, created_at, updated_at
        )
         VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, ?8, datetime('now'), datetime('now'))",
        sqlite_test::params![
            dispatch_id,
            card_id,
            to_agent_id,
            dispatch_type,
            title,
            context_str,
            parent_dispatch_id,
            chain_depth
        ],
    ) {
        // #743: create-pr also has a partial unique index (kanban_card_id
        // filtered to status IN (pending, dispatched)) so its UNIQUE
        // violation needs the same soft-error path — the caller's dedup
        // retry will reuse the winner's dispatch.
        if matches!(dispatch_type, "review" | "review-decision" | "create-pr")
            && is_single_active_dispatch_violation(&e)
        {
            return Err(anyhow::anyhow!(
                "{} already exists for card {} (concurrent race prevented by DB constraint)",
                dispatch_type,
                card_id
            ));
        }
        return Err(e.into());
    }
    record_dispatch_status_event_on_conn(
        conn,
        dispatch_id,
        None,
        "pending",
        "create_dispatch",
        None,
    )?;
    if !options.skip_outbox {
        ensure_dispatch_notify_outbox_on_conn(conn, dispatch_id, to_agent_id, card_id, title)?;
    }
    for intent in &decision.intents {
        apply_sqlite_transition_intent_for_tests(conn, intent)?;
    }
    Ok(())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn apply_sqlite_transition_intent_for_tests(
    conn: &sqlite_test::Connection,
    intent: &crate::engine::transition::TransitionIntent,
) -> Result<()> {
    use crate::engine::transition::TransitionIntent;

    match intent {
        TransitionIntent::UpdateStatus { card_id, to, .. } => {
            conn.execute(
                "UPDATE kanban_cards SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
                sqlite_test::params![to, card_id],
            )?;
        }
        TransitionIntent::SetLatestDispatchId {
            card_id,
            dispatch_id,
        } => {
            conn.execute(
                "UPDATE kanban_cards SET latest_dispatch_id = ?1, updated_at = datetime('now') WHERE id = ?2",
                sqlite_test::params![dispatch_id, card_id],
            )?;
        }
        TransitionIntent::SetReviewStatus {
            card_id,
            review_status,
        } => {
            conn.execute(
                "UPDATE kanban_cards SET review_status = ?1, updated_at = datetime('now') WHERE id = ?2",
                sqlite_test::params![review_status, card_id],
            )?;
        }
        TransitionIntent::ApplyClock { card_id, clock, .. } => {
            if let Some(clock) = clock {
                let sql = if clock.mode.as_deref() == Some("coalesce") {
                    format!(
                        "UPDATE kanban_cards SET {field} = COALESCE({field}, datetime('now')), updated_at = datetime('now') WHERE id = ?1",
                        field = clock.set
                    )
                } else {
                    format!(
                        "UPDATE kanban_cards SET {field} = datetime('now'), updated_at = datetime('now') WHERE id = ?1",
                        field = clock.set
                    )
                };
                conn.execute(&sql, [card_id])?;
            }
        }
        TransitionIntent::ClearTerminalFields { card_id } => {
            conn.execute(
                "UPDATE kanban_cards SET review_status = NULL, suggestion_pending_at = NULL, \
                 review_entered_at = NULL, awaiting_dod_at = NULL, blocked_reason = NULL, \
                 review_round = NULL, deferred_dod_json = NULL, updated_at = datetime('now') WHERE id = ?1",
                [card_id],
            )?;
        }
        TransitionIntent::AuditLog {
            card_id,
            from,
            to,
            source,
            message,
        } => {
            crate::kanban::log_audit_on_conn(conn, card_id, from, to, source, message);
        }
        TransitionIntent::CancelDispatch { dispatch_id } => {
            crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(conn, dispatch_id, None)
                .ok();
        }
        TransitionIntent::SyncAutoQueue { .. } | TransitionIntent::SyncReviewState { .. } => {}
    }

    Ok(())
}

#[cfg(test)]
mod capability_routing_tests {
    use super::*;

    fn routing() -> crate::config::ClusterDispatchRoutingConfig {
        crate::config::ClusterDispatchRoutingConfig {
            default_preferred_labels: vec!["mac-book".to_string()],
            opt_out_dispatch_types: vec!["create-pr".to_string(), "github-sync".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn default_preferred_labels_are_injected_when_context_is_silent() {
        let required =
            dispatch_required_capabilities_from_routing(&json!({}), "implementation", &routing());

        assert_eq!(
            required,
            Some(json!({
                "preferred": {
                    "labels": ["mac-book"],
                }
            }))
        );
    }

    #[test]
    fn explicit_required_capabilities_are_preserved() {
        let required = dispatch_required_capabilities_from_routing(
            &json!({
                "required_capabilities": {
                    "required": {
                        "labels": ["mac-mini"],
                    }
                }
            }),
            "implementation",
            &routing(),
        );

        assert_eq!(
            required,
            Some(json!({
                "required": {
                    "labels": ["mac-mini"],
                }
            }))
        );
    }

    #[test]
    fn explicit_null_suppresses_default_injection() {
        let required = dispatch_required_capabilities_from_routing(
            &json!({"required_capabilities": null}),
            "implementation",
            &routing(),
        );

        assert_eq!(required, None);
    }

    #[test]
    fn opt_out_dispatch_types_do_not_get_defaults() {
        let required =
            dispatch_required_capabilities_from_routing(&json!({}), "create-pr", &routing());

        assert_eq!(required, None);
    }

    #[test]
    fn hard_required_detection_ignores_preferred_only_routes() {
        assert!(!has_hard_required_capabilities(
            &json!({"preferred": {"labels": ["linux"]}})
        ));
        assert!(!has_hard_required_capabilities(&json!({
            "required": {},
            "preferred": {"labels": ["linux"]}
        })));
        assert!(has_hard_required_capabilities(
            &json!({"labels": ["mac-book"]})
        ));
        assert!(has_hard_required_capabilities(&json!({
            "required": {"labels": ["mac-book"]},
            "preferred": {"labels": ["mac-mini"]}
        })));
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::apply_dispatch_attached_intents_on_pg_tx;
    use super::create_dispatch_core_with_id_and_options as create_dispatch_core_with_id_and_options_async;
    use super::*;

    use crate::dispatch::test_support::DispatchEnvOverride;
    use crate::pipeline::ClockConfig;
    use std::collections::HashMap;

    #[allow(clippy::too_many_arguments)]
    async fn apply_dispatch_attached_intents_pg<F>(
        tx: &mut sqlx::Transaction<'_, Postgres>,
        card_id: &str,
        to_agent_id: &str,
        dispatch_id: &str,
        dispatch_type: &str,
        is_review_type: bool,
        old_status: &str,
        effective: &crate::pipeline::PipelineConfig,
        title: &str,
        context_str: &str,
        parent_dispatch_id: Option<&str>,
        chain_depth: i64,
        options: DispatchCreateOptions,
        _is_single_active_violation_check: F,
    ) -> Result<()>
    where
        F: Fn(&sqlx::Error) -> bool,
    {
        apply_dispatch_attached_intents_on_pg_tx(
            tx,
            card_id,
            to_agent_id,
            dispatch_id,
            dispatch_type,
            is_review_type,
            old_status,
            effective,
            title,
            context_str,
            parent_dispatch_id,
            chain_depth,
            options,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_dispatch_core_with_id_and_options_pg(
        _db: &Db,
        pg_pool: Option<&PgPool>,
        kanban_card_id: &str,
        to_agent_id: &str,
        dispatch_id: &str,
        dispatch_type: &str,
        title: &str,
        context: &serde_json::Value,
        options: DispatchCreateOptions,
    ) -> Result<(String, String, bool)> {
        let pool = pg_pool.ok_or_else(|| anyhow::anyhow!("Postgres pool required"))?;
        create_dispatch_core_with_id_and_options_async(
            pool,
            dispatch_id,
            kanban_card_id,
            to_agent_id,
            dispatch_type,
            title,
            context,
            options,
        )
        .await
    }

    struct TestPostgresDb {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Option<Self> {
            let admin_url = postgres_admin_url();
            let database_name = format!(
                "agentdesk_dispatch_create_{}",
                uuid::Uuid::new_v4().simple()
            );
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            let admin_pool = match sqlx::PgPool::connect(&admin_url).await {
                Ok(pool) => pool,
                Err(error) => {
                    eprintln!(
                        "skipping postgres dispatch_create test: admin connect failed: {error}"
                    );
                    return None;
                }
            };
            if let Err(error) = sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
                .execute(&admin_pool)
                .await
            {
                eprintln!(
                    "skipping postgres dispatch_create test: create database failed: {error}"
                );
                admin_pool.close().await;
                return None;
            }
            admin_pool.close().await;
            Some(Self {
                admin_url,
                database_name,
                database_url,
            })
        }

        async fn migrate(&self) -> Option<sqlx::PgPool> {
            let pool = match sqlx::PgPool::connect(&self.database_url).await {
                Ok(pool) => pool,
                Err(error) => {
                    eprintln!("skipping postgres dispatch_create test: db connect failed: {error}");
                    return None;
                }
            };
            if let Err(error) = crate::db::postgres::migrate(&pool).await {
                eprintln!("skipping postgres dispatch_create test: migrate failed: {error}");
                pool.close().await;
                return None;
            }
            Some(pool)
        }

        async fn drop(self) {
            let Ok(admin_pool) = sqlx::PgPool::connect(&self.admin_url).await else {
                return;
            };
            let _ = sqlx::query(
                "SELECT pg_terminate_backend(pid)
                 FROM pg_stat_activity
                 WHERE datname = $1
                   AND pid <> pg_backend_pid()",
            )
            .bind(&self.database_name)
            .execute(&admin_pool)
            .await;
            let _ = sqlx::query(&format!(
                "DROP DATABASE IF EXISTS \"{}\"",
                self.database_name
            ))
            .execute(&admin_pool)
            .await;
            admin_pool.close().await;
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }
        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgres://{user}:{password}@{host}:{port}"),
            None => format!("postgres://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_url() -> String {
        if let Ok(url) = std::env::var("POSTGRES_TEST_ADMIN_URL") {
            let trimmed = url.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        format!("{}/postgres", postgres_base_database_url())
    }

    async fn pg_seed_card(
        pool: &PgPool,
        card_id: &str,
        channel_thread_map: Option<&str>,
        active_thread_id: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, channel_thread_map, active_thread_id
             ) VALUES (
                $1, 'Test Card', 'ready', $2::jsonb, $3
             )",
        )
        .bind(card_id)
        .bind(channel_thread_map)
        .bind(active_thread_id)
        .execute(pool)
        .await
        .expect("seed kanban_cards");
    }

    async fn pg_seed_agent(
        pool: &PgPool,
        agent_id: &str,
        discord_channel_id: Option<&str>,
        discord_channel_alt: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(agent_id)
        .bind(format!("Agent {agent_id}"))
        .bind(discord_channel_id)
        .bind(discord_channel_alt)
        .execute(pool)
        .await
        .expect("seed agents");
    }

    fn write_cluster_routing_config(labels: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create temp config dir");
        let mut config = crate::config::Config::default();
        config.cluster.dispatch_routing.default_preferred_labels =
            labels.iter().map(|label| (*label).to_string()).collect();
        crate::config::save_to_path(&dir.path().join("agentdesk.yaml"), &config)
            .expect("write cluster routing config");
        dir
    }

    async fn pg_seed_worker_node_with_capabilities(
        pool: &PgPool,
        instance_id: &str,
        labels: serde_json::Value,
        capabilities: serde_json::Value,
        heartbeat_age_secs: i64,
    ) {
        sqlx::query(
            "INSERT INTO worker_nodes (
                instance_id, hostname, process_id, role, effective_role, status,
                labels, capabilities, last_heartbeat_at, started_at, updated_at
             ) VALUES (
                $1, $2, 100, 'worker', 'worker', 'online',
                $3, $4,
                NOW() - ($5::BIGINT * INTERVAL '1 second'), NOW(), NOW()
             )",
        )
        .bind(instance_id)
        .bind(instance_id)
        .bind(labels)
        .bind(capabilities)
        .bind(heartbeat_age_secs)
        .execute(pool)
        .await
        .expect("seed worker_nodes");
    }

    async fn pg_seed_worker_node(pool: &PgPool, instance_id: &str, heartbeat_age_secs: i64) {
        pg_seed_worker_node_with_capabilities(
            pool,
            instance_id,
            json!([]),
            json!({}),
            heartbeat_age_secs,
        )
        .await;
    }

    async fn pg_seed_session(pool: &PgPool, session_key: &str, instance_id: Option<&str>) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO sessions (
                session_key, provider, status, instance_id, last_heartbeat
             ) VALUES (
                $1, 'codex', 'working', $2, NOW()
             )
             RETURNING id",
        )
        .bind(session_key)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .expect("seed sessions")
    }

    async fn pg_notify_claim_owner(pool: &PgPool, dispatch_id: &str) -> Option<String> {
        sqlx::query_scalar(
            "SELECT claim_owner
             FROM dispatch_outbox
             WHERE dispatch_id = $1
               AND action = 'notify'",
        )
        .bind(dispatch_id)
        .fetch_one(pool)
        .await
        .expect("load notify outbox claim_owner")
    }

    async fn pg_seed_dispatch(
        pool: &PgPool,
        dispatch_id: &str,
        card_id: &str,
        dispatch_type: &str,
        status: &str,
    ) {
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title
             ) VALUES (
                $1, $2, 'agent-seeded', $3, $4, 'Seeded Dispatch'
             )",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .bind(dispatch_type)
        .bind(status)
        .execute(pool)
        .await
        .expect("seed task_dispatches");
    }

    fn seed_sqlite_card_and_agent(db: &Db, card_id: &str, status: &str, agent_id: &str) {
        let conn = db.lock().expect("lock sqlite test db");
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES (?1, ?2, '111', '222')",
            sqlite_test::params![agent_id, format!("Agent {agent_id}")],
        )
        .expect("seed sqlite agent");
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES (?1, 'Test Card', ?2, ?3)",
            sqlite_test::params![card_id, status, agent_id],
        )
        .expect("seed sqlite card");
    }

    fn seed_sqlite_card_and_agent_provider(
        db: &Db,
        card_id: &str,
        status: &str,
        agent_id: &str,
        provider: &str,
    ) {
        let conn = db.lock().expect("lock sqlite test db");
        conn.execute(
            "INSERT INTO agents (
                id, name, provider, discord_channel_id, discord_channel_alt,
                discord_channel_cc, discord_channel_cdx
             ) VALUES (?1, ?2, ?3, '111', '222', '111', '222')",
            sqlite_test::params![agent_id, format!("Agent {agent_id}"), provider],
        )
        .expect("seed sqlite provider agent");
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES (?1, 'Test Card', ?2, ?3)",
            sqlite_test::params![card_id, status, agent_id],
        )
        .expect("seed sqlite card");
    }

    fn sqlite_dispatch_context(db: &Db, dispatch_id: &str) -> serde_json::Value {
        let conn = db.separate_conn().expect("open sqlite conn");
        let context: String = conn
            .query_row(
                "SELECT context FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .expect("load dispatch context");
        serde_json::from_str(&context).expect("dispatch context json")
    }

    #[test]
    fn create_review_dispatch_context_targets_opposite_explicit_implementer_provider() {
        let db = crate::db::test_db();
        seed_sqlite_card_and_agent_provider(
            &db,
            "card-review-counter-explicit",
            "ready",
            "agent-review-counter-explicit",
            "codex",
        );

        create_dispatch_record_with_id_sqlite_test(
            &db,
            "dispatch-review-counter-explicit",
            "card-review-counter-explicit",
            "agent-review-counter-explicit",
            "review",
            "Review explicit implementer",
            &json!({
                "review_mode": "noop_verification",
                "implementer_provider": "claude",
                "from_provider": "codex"
            }),
            DispatchCreateOptions {
                skip_outbox: true,
                sidecar_dispatch: false,
            },
        )
        .expect("create review dispatch");

        let context = sqlite_dispatch_context(&db, "dispatch-review-counter-explicit");
        assert_eq!(context["implementer_provider"], "claude");
        assert_eq!(context["from_provider"], "claude");
        assert_eq!(context["target_provider"], "codex");
        assert_eq!(
            context["counter_model_resolution_reason"],
            "explicit_implementer_provider:claude=>codex"
        );
    }

    #[test]
    fn create_review_dispatch_context_falls_back_to_agent_main_provider() {
        let db = crate::db::test_db();
        seed_sqlite_card_and_agent_provider(
            &db,
            "card-review-counter-fallback",
            "ready",
            "agent-review-counter-fallback",
            "codex",
        );

        create_dispatch_record_with_id_sqlite_test(
            &db,
            "dispatch-review-counter-fallback",
            "card-review-counter-fallback",
            "agent-review-counter-fallback",
            "review",
            "Review fallback",
            &json!({ "review_mode": "noop_verification" }),
            DispatchCreateOptions {
                skip_outbox: true,
                sidecar_dispatch: false,
            },
        )
        .expect("create review dispatch");

        let context = sqlite_dispatch_context(&db, "dispatch-review-counter-fallback");
        assert_eq!(context["from_provider"], "codex");
        assert_eq!(context["target_provider"], "claude");
        assert_eq!(
            context["counter_model_resolution_reason"],
            "agent_main_provider:codex=>claude"
        );
    }

    async fn pg_count_dispatches(pool: &PgPool, dispatch_id: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM task_dispatches
             WHERE id = $1",
        )
        .bind(dispatch_id)
        .fetch_one(pool)
        .await
        .expect("count task_dispatches")
    }

    async fn pg_count_notify_outbox(pool: &PgPool, dispatch_id: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM dispatch_outbox
             WHERE dispatch_id = $1
               AND action = 'notify'",
        )
        .bind(dispatch_id)
        .fetch_one(pool)
        .await
        .expect("count notify outbox")
    }

    async fn pg_card_status(pool: &PgPool, card_id: &str) -> String {
        sqlx::query_scalar(
            "SELECT status
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(card_id)
        .fetch_one(pool)
        .await
        .expect("load card status")
    }

    async fn pg_latest_dispatch_id(pool: &PgPool, card_id: &str) -> Option<String> {
        sqlx::query_scalar(
            "SELECT latest_dispatch_id
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(card_id)
        .fetch_one(pool)
        .await
        .expect("load latest dispatch id")
    }

    async fn pg_dispatch_status(pool: &PgPool, dispatch_id: &str) -> String {
        sqlx::query_scalar(
            "SELECT status
             FROM task_dispatches
             WHERE id = $1",
        )
        .bind(dispatch_id)
        .fetch_one(pool)
        .await
        .expect("load dispatch status")
    }

    async fn pg_default_pipeline(pool: &PgPool) -> crate::pipeline::PipelineConfig {
        crate::pipeline::ensure_loaded();
        crate::pipeline::resolve_for_card_pg(pool, None, None).await
    }

    fn invalid_clock_pipeline(
        base: &crate::pipeline::PipelineConfig,
        state: &str,
    ) -> crate::pipeline::PipelineConfig {
        let mut pipeline = base.clone();
        let mut clocks = HashMap::new();
        clocks.insert(
            state.to_string(),
            ClockConfig {
                set: "definitely_missing_dispatch_clock_column".to_string(),
                mode: None,
            },
        );
        pipeline.clocks = clocks;
        pipeline
    }

    #[tokio::test]
    async fn apply_dispatch_attached_intents_pg_happy_path_inserts_review_and_updates_card() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_agent(&pool, "agent-apply-happy", Some("111"), Some("222")).await;
        pg_seed_card(&pool, "card-apply-happy", None, None).await;
        let effective = pg_default_pipeline(&pool).await;

        let mut tx = pool.begin().await.expect("begin postgres tx");
        apply_dispatch_attached_intents_pg(
            &mut tx,
            "card-apply-happy",
            "agent-apply-happy",
            "dispatch-apply-happy",
            "review",
            true,
            "ready",
            &effective,
            "Happy path",
            &json!({}).to_string(),
            None,
            0,
            DispatchCreateOptions::default(),
            is_single_active_dispatch_violation_pg,
        )
        .await
        .expect("happy path dispatch attach");
        tx.commit().await.expect("commit postgres tx");

        assert_eq!(
            pg_count_dispatches(&pool, "dispatch-apply-happy").await,
            1,
            "task_dispatches row must be inserted"
        );
        assert_eq!(
            pg_count_notify_outbox(&pool, "dispatch-apply-happy").await,
            1,
            "notify outbox row must be inserted"
        );
        assert_eq!(
            pg_card_status(&pool, "card-apply-happy").await,
            "ready",
            "review dispatch must leave the card state unchanged"
        );
        assert_eq!(
            pg_latest_dispatch_id(&pool, "card-apply-happy")
                .await
                .as_deref(),
            Some("dispatch-apply-happy"),
            "latest_dispatch_id must track the new dispatch"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn apply_dispatch_attached_intents_pg_pins_notify_outbox_to_live_session_owner() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_agent(&pool, "agent-affinity-live", Some("111"), Some("222")).await;
        pg_seed_card(&pool, "card-affinity-live", None, None).await;
        pg_seed_worker_node(&pool, "mac-book-release", 0).await;
        let session_id =
            pg_seed_session(&pool, "session-affinity-live", Some("mac-book-release")).await;
        let effective = pg_default_pipeline(&pool).await;

        let mut tx = pool.begin().await.expect("begin postgres tx");
        apply_dispatch_attached_intents_pg(
            &mut tx,
            "card-affinity-live",
            "agent-affinity-live",
            "dispatch-affinity-live",
            "review",
            true,
            "ready",
            &effective,
            "Affinity live",
            &json!({"session_id": session_id}).to_string(),
            None,
            0,
            DispatchCreateOptions::default(),
            is_single_active_dispatch_violation_pg,
        )
        .await
        .expect("create session affinity dispatch");
        tx.commit().await.expect("commit postgres tx");

        assert_eq!(
            pg_notify_claim_owner(&pool, "dispatch-affinity-live")
                .await
                .as_deref(),
            Some("mac-book-release")
        );
        let wrong_node_claim =
            crate::services::dispatches::outbox_claiming::claim_pending_dispatch_outbox_batch_pg(
                &pool,
                "mac-mini-release",
            )
            .await;
        assert!(
            wrong_node_claim.is_empty(),
            "a different node must not claim an affinity-pinned outbox row"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn apply_dispatch_attached_intents_pg_keeps_notify_claim_owner_null_without_session() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_agent(&pool, "agent-affinity-none", Some("111"), Some("222")).await;
        pg_seed_card(&pool, "card-affinity-none", None, None).await;
        let effective = pg_default_pipeline(&pool).await;

        let mut tx = pool.begin().await.expect("begin postgres tx");
        apply_dispatch_attached_intents_pg(
            &mut tx,
            "card-affinity-none",
            "agent-affinity-none",
            "dispatch-affinity-none",
            "review",
            true,
            "ready",
            &effective,
            "Affinity none",
            &json!({}).to_string(),
            None,
            0,
            DispatchCreateOptions::default(),
            is_single_active_dispatch_violation_pg,
        )
        .await
        .expect("create dispatch without session affinity");
        tx.commit().await.expect("commit postgres tx");

        assert_eq!(
            pg_notify_claim_owner(&pool, "dispatch-affinity-none").await,
            None
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn apply_dispatch_attached_intents_pg_pins_default_preferred_label_order() {
        let _config_dir = write_cluster_routing_config(&["mac-book", "mac-mini"]);
        let config_path = _config_dir.path().join("agentdesk.yaml");
        let _env = DispatchEnvOverride::new(None, Some(config_path.to_str().unwrap()));

        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_agent(&pool, "agent-label-default", Some("111"), Some("222")).await;
        pg_seed_card(&pool, "card-label-default", None, None).await;
        pg_seed_worker_node_with_capabilities(
            &pool,
            "mac-mini-release",
            json!(["mac-mini"]),
            json!({"providers": ["codex"]}),
            0,
        )
        .await;
        pg_seed_worker_node_with_capabilities(
            &pool,
            "mac-book-release",
            json!(["mac-book"]),
            json!({"providers": ["codex"]}),
            5,
        )
        .await;
        let effective = pg_default_pipeline(&pool).await;

        let mut tx = pool.begin().await.expect("begin postgres tx");
        apply_dispatch_attached_intents_pg(
            &mut tx,
            "card-label-default",
            "agent-label-default",
            "dispatch-label-default",
            "review",
            true,
            "ready",
            &effective,
            "Label default",
            &json!({}).to_string(),
            None,
            0,
            DispatchCreateOptions::default(),
            is_single_active_dispatch_violation_pg,
        )
        .await
        .expect("create default label dispatch");
        tx.commit().await.expect("commit postgres tx");

        assert_eq!(
            pg_notify_claim_owner(&pool, "dispatch-label-default")
                .await
                .as_deref(),
            Some("mac-book-release"),
            "first configured preferred label must win even when the second label has a fresher heartbeat"
        );

        let wrong_node_claim =
            crate::services::dispatches::outbox_claiming::claim_pending_dispatch_outbox_batch_pg(
                &pool,
                "mac-mini-release",
            )
            .await;
        assert!(
            wrong_node_claim.is_empty(),
            "a non-selected label node must not claim the pinned notify row"
        );
        let selected_node_claim =
            crate::services::dispatches::outbox_claiming::claim_pending_dispatch_outbox_batch_pg(
                &pool,
                "mac-book-release",
            )
            .await;
        assert_eq!(selected_node_claim.len(), 1);
        assert_eq!(selected_node_claim[0].1, "dispatch-label-default");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn apply_dispatch_attached_intents_pg_falls_back_to_next_default_label_when_first_stale()
    {
        let _config_dir = write_cluster_routing_config(&["mac-book", "mac-mini"]);
        let config_path = _config_dir.path().join("agentdesk.yaml");
        let _env = DispatchEnvOverride::new(None, Some(config_path.to_str().unwrap()));

        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_agent(&pool, "agent-label-fallback", Some("111"), Some("222")).await;
        pg_seed_card(&pool, "card-label-fallback", None, None).await;
        pg_seed_worker_node_with_capabilities(
            &pool,
            "mac-book-release",
            json!(["mac-book"]),
            json!({"providers": ["codex"]}),
            600,
        )
        .await;
        pg_seed_worker_node_with_capabilities(
            &pool,
            "mac-mini-release",
            json!(["mac-mini"]),
            json!({"providers": ["codex"]}),
            0,
        )
        .await;
        let effective = pg_default_pipeline(&pool).await;

        let mut tx = pool.begin().await.expect("begin postgres tx");
        apply_dispatch_attached_intents_pg(
            &mut tx,
            "card-label-fallback",
            "agent-label-fallback",
            "dispatch-label-fallback",
            "review",
            true,
            "ready",
            &effective,
            "Label fallback",
            &json!({}).to_string(),
            None,
            0,
            DispatchCreateOptions::default(),
            is_single_active_dispatch_violation_pg,
        )
        .await
        .expect("create default label fallback dispatch");
        tx.commit().await.expect("commit postgres tx");

        assert_eq!(
            pg_notify_claim_owner(&pool, "dispatch-label-fallback")
                .await
                .as_deref(),
            Some("mac-mini-release")
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn apply_dispatch_attached_intents_pg_leaves_claim_owner_null_when_no_label_matches() {
        let _config_dir = write_cluster_routing_config(&["linux"]);
        let config_path = _config_dir.path().join("agentdesk.yaml");
        let _env = DispatchEnvOverride::new(None, Some(config_path.to_str().unwrap()));

        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_agent(&pool, "agent-label-none", Some("111"), Some("222")).await;
        pg_seed_card(&pool, "card-label-none", None, None).await;
        pg_seed_worker_node_with_capabilities(
            &pool,
            "mac-mini-release",
            json!(["mac-mini"]),
            json!({"providers": ["codex"]}),
            0,
        )
        .await;
        let effective = pg_default_pipeline(&pool).await;

        let mut tx = pool.begin().await.expect("begin postgres tx");
        apply_dispatch_attached_intents_pg(
            &mut tx,
            "card-label-none",
            "agent-label-none",
            "dispatch-label-none",
            "review",
            true,
            "ready",
            &effective,
            "Label none",
            &json!({}).to_string(),
            None,
            0,
            DispatchCreateOptions::default(),
            is_single_active_dispatch_violation_pg,
        )
        .await
        .expect("create no label match dispatch");
        tx.commit().await.expect("commit postgres tx");

        assert_eq!(
            pg_notify_claim_owner(&pool, "dispatch-label-none").await,
            None
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn apply_dispatch_attached_intents_pg_pins_explicit_required_label_match() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_agent(&pool, "agent-label-required", Some("111"), Some("222")).await;
        pg_seed_card(&pool, "card-label-required", None, None).await;
        pg_seed_worker_node_with_capabilities(
            &pool,
            "mac-book-release",
            json!(["mac-book"]),
            json!({"providers": ["codex"]}),
            0,
        )
        .await;
        pg_seed_worker_node_with_capabilities(
            &pool,
            "mac-mini-release",
            json!(["mac-mini"]),
            json!({"providers": ["codex"]}),
            5,
        )
        .await;
        let effective = pg_default_pipeline(&pool).await;

        let mut tx = pool.begin().await.expect("begin postgres tx");
        apply_dispatch_attached_intents_pg(
            &mut tx,
            "card-label-required",
            "agent-label-required",
            "dispatch-label-required",
            "review",
            true,
            "ready",
            &effective,
            "Label required",
            &json!({"required_capabilities": {"required": {"labels": ["mac-mini"]}}}).to_string(),
            None,
            0,
            DispatchCreateOptions::default(),
            is_single_active_dispatch_violation_pg,
        )
        .await
        .expect("create explicit required label dispatch");
        tx.commit().await.expect("commit postgres tx");

        assert_eq!(
            pg_notify_claim_owner(&pool, "dispatch-label-required")
                .await
                .as_deref(),
            Some("mac-mini-release")
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn apply_dispatch_attached_intents_pg_session_affinity_suppresses_label_routing() {
        let _config_dir = write_cluster_routing_config(&["mac-book"]);
        let config_path = _config_dir.path().join("agentdesk.yaml");
        let _env = DispatchEnvOverride::new(None, Some(config_path.to_str().unwrap()));

        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_agent(&pool, "agent-affinity-label", Some("111"), Some("222")).await;
        pg_seed_card(&pool, "card-affinity-label", None, None).await;
        pg_seed_worker_node_with_capabilities(
            &pool,
            "stale-session-owner",
            json!(["mac-mini"]),
            json!({"providers": ["codex"]}),
            600,
        )
        .await;
        pg_seed_worker_node_with_capabilities(
            &pool,
            "mac-book-release",
            json!(["mac-book"]),
            json!({"providers": ["codex"]}),
            0,
        )
        .await;
        let session_id = pg_seed_session(
            &pool,
            "session-affinity-suppresses-label",
            Some("stale-session-owner"),
        )
        .await;
        let effective = pg_default_pipeline(&pool).await;

        let mut tx = pool.begin().await.expect("begin postgres tx");
        apply_dispatch_attached_intents_pg(
            &mut tx,
            "card-affinity-label",
            "agent-affinity-label",
            "dispatch-affinity-label",
            "review",
            true,
            "ready",
            &effective,
            "Affinity suppresses labels",
            &json!({"session_id": session_id}).to_string(),
            None,
            0,
            DispatchCreateOptions::default(),
            is_single_active_dispatch_violation_pg,
        )
        .await
        .expect("create session affinity label dispatch");
        tx.commit().await.expect("commit postgres tx");

        assert_eq!(
            pg_notify_claim_owner(&pool, "dispatch-affinity-label").await,
            None,
            "session affinity must stay higher priority than label routing even when the session owner is stale"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn apply_dispatch_attached_intents_pg_nulls_stale_session_owner() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_agent(&pool, "agent-affinity-stale", Some("111"), Some("222")).await;
        pg_seed_card(&pool, "card-affinity-stale", None, None).await;
        pg_seed_worker_node(&pool, "stale-node", 600).await;
        let session_id = pg_seed_session(&pool, "session-affinity-stale", Some("stale-node")).await;
        let effective = pg_default_pipeline(&pool).await;

        let mut tx = pool.begin().await.expect("begin postgres tx");
        apply_dispatch_attached_intents_pg(
            &mut tx,
            "card-affinity-stale",
            "agent-affinity-stale",
            "dispatch-affinity-stale",
            "review",
            true,
            "ready",
            &effective,
            "Affinity stale",
            &json!({"session_id": session_id}).to_string(),
            None,
            0,
            DispatchCreateOptions::default(),
            is_single_active_dispatch_violation_pg,
        )
        .await
        .expect("create dispatch with stale session owner");
        tx.commit().await.expect("commit postgres tx");

        assert_eq!(
            pg_notify_claim_owner(&pool, "dispatch-affinity-stale").await,
            None
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn apply_dispatch_attached_intents_pg_pins_100_concurrent_session_dispatches() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_agent(&pool, "agent-affinity-load", Some("111"), Some("222")).await;
        pg_seed_worker_node(&pool, "mac-book-release", 0).await;
        let session_id =
            pg_seed_session(&pool, "session-affinity-load", Some("mac-book-release")).await;
        let effective = pg_default_pipeline(&pool).await;
        let context = json!({"session_id": session_id}).to_string();

        for idx in 0..100 {
            pg_seed_card(&pool, &format!("card-affinity-load-{idx}"), None, None).await;
        }

        let futures = (0..100).map(|idx| {
            let pool = pool.clone();
            let effective = effective.clone();
            let context = context.clone();
            async move {
                let card_id = format!("card-affinity-load-{idx}");
                let dispatch_id = format!("dispatch-affinity-load-{idx}");
                let mut tx = pool.begin().await.expect("begin postgres tx");
                apply_dispatch_attached_intents_on_pg_tx(
                    &mut tx,
                    &card_id,
                    "agent-affinity-load",
                    &dispatch_id,
                    "review",
                    true,
                    "ready",
                    &effective,
                    "Affinity load",
                    &context,
                    None,
                    0,
                    DispatchCreateOptions::default(),
                )
                .await
                .expect("create load dispatch");
                tx.commit().await.expect("commit postgres tx");
            }
        });
        futures::future::join_all(futures).await;

        let pinned: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM dispatch_outbox
             WHERE action = 'notify'
               AND claim_owner = 'mac-book-release'",
        )
        .fetch_one(&pool)
        .await
        .expect("count pinned outbox rows");
        assert_eq!(pinned, 100);

        let wrong_node_claim =
            crate::services::dispatches::outbox_claiming::claim_pending_dispatch_outbox_batch_pg(
                &pool,
                "mac-mini-release",
            )
            .await;
        assert!(
            wrong_node_claim.is_empty(),
            "different owner claim attempt count must stay at zero"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn apply_dispatch_attached_intents_pg_unique_race_surfaces_dedup_message() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_agent(&pool, "agent-apply-race", Some("111"), Some("222")).await;
        pg_seed_card(&pool, "card-apply-race", None, None).await;
        pg_seed_dispatch(
            &pool,
            "dispatch-apply-race-existing",
            "card-apply-race",
            "review",
            "pending",
        )
        .await;
        let effective = pg_default_pipeline(&pool).await;

        let mut tx = pool.begin().await.expect("begin postgres tx");
        let err = apply_dispatch_attached_intents_pg(
            &mut tx,
            "card-apply-race",
            "agent-apply-race",
            "dispatch-apply-race-new",
            "review",
            true,
            "ready",
            &effective,
            "Race loser",
            &json!({}).to_string(),
            None,
            0,
            DispatchCreateOptions::default(),
            is_single_active_dispatch_violation_pg,
        )
        .await
        .expect_err("unique race loser must surface dedup marker");
        assert!(
            err.to_string()
                .contains("concurrent race prevented by DB constraint"),
            "PG UNIQUE loser must preserve the dedup marker"
        );
        tx.rollback().await.expect("rollback postgres tx");
        assert_eq!(
            lookup_active_dispatch_id_pg(&pool, "card-apply-race", "review").await,
            Some("dispatch-apply-race-existing".to_string()),
            "lookup_active_dispatch_id_pg must still return the seeded winner"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn apply_dispatch_attached_intents_pg_rolls_back_mid_transaction_failures() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_agent(&pool, "agent-apply-rollback", Some("111"), Some("222")).await;
        pg_seed_card(&pool, "card-apply-rollback", None, None).await;
        let base_pipeline = pg_default_pipeline(&pool).await;
        let kickoff_state = base_pipeline
            .kickoff_for("ready")
            .unwrap_or_else(|| base_pipeline.initial_state().to_string());
        let failing_pipeline = invalid_clock_pipeline(&base_pipeline, &kickoff_state);

        let mut tx = pool.begin().await.expect("begin postgres tx");
        let err = apply_dispatch_attached_intents_pg(
            &mut tx,
            "card-apply-rollback",
            "agent-apply-rollback",
            "dispatch-apply-rollback",
            "implementation",
            false,
            "ready",
            &failing_pipeline,
            "Rollback path",
            &json!({}).to_string(),
            None,
            0,
            DispatchCreateOptions::default(),
            is_single_active_dispatch_violation_pg,
        )
        .await
        .expect_err("invalid clock column must abort the transaction");
        assert!(
            err.to_string()
                .contains("definitely_missing_dispatch_clock_column"),
            "the injected ApplyClock failure should be the surfaced error"
        );
        tx.rollback().await.expect("rollback postgres tx");
        assert_eq!(
            pg_count_dispatches(&pool, "dispatch-apply-rollback").await,
            0,
            "task_dispatches insert must roll back on later failure"
        );
        assert_eq!(
            pg_count_notify_outbox(&pool, "dispatch-apply-rollback").await,
            0,
            "notify outbox insert must roll back on later failure"
        );
        assert_eq!(
            pg_card_status(&pool, "card-apply-rollback").await,
            "ready",
            "card status must remain unchanged after rollback"
        );
        assert_eq!(
            pg_latest_dispatch_id(&pool, "card-apply-rollback").await,
            None,
            "latest_dispatch_id must remain unchanged after rollback"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn apply_dispatch_attached_intents_pg_skip_outbox_omits_notify_row() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_agent(&pool, "agent-apply-skip", Some("111"), Some("222")).await;
        pg_seed_card(&pool, "card-apply-skip", None, None).await;
        let effective = pg_default_pipeline(&pool).await;

        let mut tx = pool.begin().await.expect("begin postgres tx");
        apply_dispatch_attached_intents_pg(
            &mut tx,
            "card-apply-skip",
            "agent-apply-skip",
            "dispatch-apply-skip",
            "implementation",
            false,
            "ready",
            &effective,
            "Skip outbox",
            &json!({}).to_string(),
            None,
            0,
            DispatchCreateOptions {
                skip_outbox: true,
                ..Default::default()
            },
            is_single_active_dispatch_violation_pg,
        )
        .await
        .expect("skip_outbox dispatch attach");
        tx.commit().await.expect("commit postgres tx");

        assert_eq!(
            pg_count_dispatches(&pool, "dispatch-apply-skip").await,
            1,
            "task_dispatches row must still be inserted"
        );
        assert_eq!(
            pg_count_notify_outbox(&pool, "dispatch-apply-skip").await,
            0,
            "skip_outbox must suppress notify outbox insertion"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn create_dispatch_core_pg_cancels_stale_review_decision_siblings() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        let db = crate::db::test_db();
        seed_sqlite_card_and_agent(
            &db,
            "card-apply-review-decision",
            "review",
            "agent-apply-review-decision",
        );

        pg_seed_card(&pool, "card-apply-review-decision", None, None).await;
        sqlx::query(
            "UPDATE kanban_cards
             SET status = 'review'
             WHERE id = $1",
        )
        .bind("card-apply-review-decision")
        .execute(&pool)
        .await
        .expect("align postgres card status");
        pg_seed_agent(
            &pool,
            "agent-apply-review-decision",
            Some("111"),
            Some("222"),
        )
        .await;
        pg_seed_dispatch(
            &pool,
            "dispatch-apply-review-decision-old",
            "card-apply-review-decision",
            "review-decision",
            "pending",
        )
        .await;
        let (dispatch_id, old_status, reused) = create_dispatch_core_with_id_and_options_pg(
            &db,
            Some(&pool),
            "card-apply-review-decision",
            "agent-apply-review-decision",
            "dispatch-apply-review-decision-new",
            "review-decision",
            "Fresh review decision",
            &json!({}),
            DispatchCreateOptions::default(),
        )
        .await
        .expect("review-decision create should cancel stale sibling");

        assert_eq!(dispatch_id, "dispatch-apply-review-decision-new");
        assert_eq!(old_status, "review");
        assert!(!reused);

        assert_eq!(
            pg_dispatch_status(&pool, "dispatch-apply-review-decision-old").await,
            "cancelled",
            "stale review-decision sibling must be cancelled before the new insert commits"
        );
        assert_eq!(
            pg_dispatch_status(&pool, "dispatch-apply-review-decision-new").await,
            "pending",
            "new review-decision dispatch must be inserted"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn lookup_active_dispatch_id_pg_handles_empty_and_type_filtered_rows() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        assert_eq!(
            lookup_active_dispatch_id_pg(&pool, "card-missing", "implementation").await,
            None
        );

        pg_seed_card(&pool, "card-lookup-happy", None, None).await;
        pg_seed_dispatch(
            &pool,
            "dispatch-lookup-happy",
            "card-lookup-happy",
            "implementation",
            "pending",
        )
        .await;
        assert_eq!(
            lookup_active_dispatch_id_pg(&pool, "card-lookup-happy", "implementation").await,
            Some("dispatch-lookup-happy".to_string())
        );

        pg_seed_card(&pool, "card-lookup-mixed", None, None).await;
        pg_seed_dispatch(
            &pool,
            "dispatch-lookup-review",
            "card-lookup-mixed",
            "review",
            "pending",
        )
        .await;
        pg_seed_dispatch(
            &pool,
            "dispatch-lookup-impl",
            "card-lookup-mixed",
            "implementation",
            "pending",
        )
        .await;
        assert_eq!(
            lookup_active_dispatch_id_pg(&pool, "card-lookup-mixed", "review").await,
            Some("dispatch-lookup-review".to_string())
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn validate_dispatch_target_on_pg_matches_sqlite_errors() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        pg_seed_agent(&pool, "agent-valid", Some("111"), Some("222")).await;
        pg_seed_card(&pool, "card-valid", None, None).await;
        validate_dispatch_target_on_pg(&pool, "card-valid", "agent-valid", "implementation", None)
            .await
            .expect("happy-path validation");

        pg_seed_agent(&pool, "agent-missing-channel", None, None).await;
        pg_seed_card(&pool, "card-missing-channel", None, None).await;
        let err = validate_dispatch_target_on_pg(
            &pool,
            "card-missing-channel",
            "agent-missing-channel",
            "implementation",
            None,
        )
        .await
        .expect_err("missing primary channel must fail");
        assert_eq!(
            err.to_string(),
            "Cannot create implementation dispatch: agent 'agent-missing-channel' has no primary discord channel (card card-missing-channel)"
        );

        pg_seed_card(
            &pool,
            "card-invalid-thread",
            None,
            Some("thread-not-numeric"),
        )
        .await;
        let err = validate_dispatch_target_on_pg(
            &pool,
            "card-invalid-thread",
            "agent-valid",
            "implementation",
            None,
        )
        .await
        .expect_err("invalid cached thread id must fail");
        assert_eq!(
            err.to_string(),
            "Cannot create implementation dispatch: card 'card-invalid-thread' has invalid thread 'thread-not-numeric' for channel 111"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[path = "dispatch_create_relocated_tests.rs"]
mod relocated_tests;
