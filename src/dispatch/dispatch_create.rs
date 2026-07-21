use anyhow::Result;
use serde_json::json;
use sqlx::{PgPool, Postgres, Row};

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
    sandbox_preflight_card_disables_external_side_effects,
};
use super::dispatch_query::query_dispatch_row_pg;
use super::{DispatchCreateOptions, cancel_dispatch_and_reset_auto_queue_on_pg_tx};

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

async fn lookup_active_dispatch_id_pg(
    pool: &PgPool,
    card_id: &str,
    dispatch_type: &str,
) -> Option<String> {
    // #2045 Finding 15 (P3): pull up to 2 rows so we can surface a warning
    // when more than one active dispatch of the same `(card_id,
    // dispatch_type)` exists. UNIQUE constraints cover `review` / `review-
    // decision` / `create-pr` but not `implementation`/`rework`; if a partial
    // index ever drifts (migration race, supervisor bug), the dedup helper
    // used to silently pick one of the rows and hide the inconsistency.
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = $2
           AND status IN ('pending', 'dispatched')
         ORDER BY created_at DESC, id DESC
         LIMIT 2",
    )
    .bind(card_id)
    .bind(dispatch_type)
    .fetch_all(pool)
    .await
    .ok()?;

    if rows.len() > 1 {
        tracing::warn!(
            card_id,
            dispatch_type,
            duplicate_dispatch_ids = ?rows,
            "[dispatch] multiple active dispatches of the same type — dedup will reuse newest but state is inconsistent"
        );
    }

    rows.into_iter().next()
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

    // #2045 Finding 11 (P2): if the caller is requesting a sidecar dispatch
    // (phase-gate / phase-side run / supervisor diagnostic), do NOT silently
    // reuse a non-sidecar dispatch of the same type. The caller-supplied
    // context for sidecars differs in ways the old reuse path would drop
    // (extra `phase_gate` config, different required_capabilities, etc.),
    // which would make the caller think a new sidecar exists while in
    // reality they got the mainline implementation row back.
    if dispatch_type != "review-decision"
        && !options.sidecar_dispatch
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
    let sandbox_preflight_without_external_side_effects =
        sandbox_preflight_card_disables_external_side_effects(pg_pool, kanban_card_id).await;
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
            if sandbox_preflight_without_external_side_effects {
                None
            } else {
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
            }
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
    // #3605 (T2): the broader "skip kickoff" set — review-family plus inert
    // side-paths (consultation, scope-assessment). A side-path must NOT pass a
    // kickoff_state into the transition so the card stays in `requested`. Shared
    // with transition.rs::decide_dispatch_attached and phase_gate via
    // dispatch_type_skips_kickoff; the only consumer is the kickoff_state gate
    // below. (Variable name kept as `is_review_type` to avoid churn in the
    // downstream helper signatures it feeds.)
    let is_review_type = crate::dispatch::dispatch_type_skips_kickoff(dispatch_type);
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
        sandbox_preflight_without_external_side_effects,
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

#[allow(clippy::too_many_arguments)]
fn create_dispatch_with_options_pg_backed(
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
        engine,
        kanban_card_id,
        &old_status,
        &kickoff_owned,
    );

    Ok(dispatch)
}

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
    sandbox_preflight_without_external_side_effects: bool,
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
            sandbox_preflight_without_external_side_effects,
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
    sandbox_preflight_without_external_side_effects: bool,
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
    if !options.skip_outbox && !sandbox_preflight_without_external_side_effects {
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
