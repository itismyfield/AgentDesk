use anyhow::Result;
use serde_json::json;
use sqlx::{PgPool, Postgres, Row as SqlxRow};

use crate::db::Db;
#[cfg(test)]
use crate::db::agents::resolve_agent_dispatch_channel_on_conn;
use crate::db::agents::resolve_agent_dispatch_channel_pg;
use crate::engine::PolicyEngine;

use super::dispatch_channel::{dispatch_uses_alt_channel, resolve_dispatch_channel_id};
use super::dispatch_context::{
    ReviewTargetTrust, TargetRepoSource, build_review_context,
    dispatch_context_with_session_strategy, dispatch_context_worktree_target,
    inject_review_dispatch_identifiers, json_string_field, resolve_card_target_repo_ref,
    resolve_card_worktree, resolve_parent_dispatch_context,
};
#[cfg(test)]
use super::dispatch_context::{
    build_review_context_sqlite_test, inject_review_dispatch_identifiers_sqlite_test,
    resolve_card_target_repo_ref_sqlite_test, resolve_card_worktree_sqlite_test,
    resolve_parent_dispatch_context_sqlite_test,
};
#[cfg(test)]
use super::dispatch_status::{
    ensure_dispatch_notify_outbox_on_conn, record_dispatch_status_event_on_conn,
};
use super::{
    DispatchCreateOptions, cancel_dispatch_and_reset_auto_queue_on_pg_tx, summarize_dispatch_result,
};
#[cfg(test)]
use super::{cancel_dispatch_and_reset_auto_queue_on_conn, query_dispatch_row};

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

#[cfg(test)]
fn load_existing_thread_for_channel(
    conn: &rusqlite::Connection,
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

#[cfg(test)]
fn lookup_active_dispatch_id(
    conn: &rusqlite::Connection,
    card_id: &str,
    dispatch_type: &str,
) -> Option<String> {
    conn.query_row(
        "SELECT id FROM task_dispatches \
         WHERE kanban_card_id = ?1 AND dispatch_type = ?2 \
         AND status IN ('pending', 'dispatched') \
         ORDER BY rowid DESC LIMIT 1",
        rusqlite::params![card_id, dispatch_type],
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

#[cfg(test)]
fn is_single_active_dispatch_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(_, Some(message))
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

#[cfg(test)]
fn validate_dispatch_target_on_conn(
    conn: &rusqlite::Connection,
    card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
) -> Result<()> {
    let channel_role = if dispatch_uses_alt_channel(dispatch_type) {
        "counter-model"
    } else {
        "primary"
    };

    let channel_value: Option<String> =
        resolve_agent_dispatch_channel_on_conn(conn, to_agent_id, Some(dispatch_type))
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
) -> Result<()> {
    let channel_role = if dispatch_uses_alt_channel(dispatch_type) {
        "counter-model"
    } else {
        "primary"
    };

    let channel_value: Option<String> =
        resolve_agent_dispatch_channel_pg(pool, to_agent_id, Some(dispatch_type))
            .await
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

fn parse_dispatch_json_text_pg(raw: Option<&str>) -> Option<serde_json::Value> {
    raw.and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
}

pub(crate) async fn query_dispatch_row_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<serde_json::Value> {
    let row = sqlx::query(
        "SELECT
            id,
            kanban_card_id,
            from_agent_id,
            to_agent_id,
            dispatch_type,
            status,
            title,
            context,
            result,
            parent_dispatch_id,
            COALESCE(chain_depth, 0)::bigint AS chain_depth,
            created_at::text AS created_at,
            updated_at::text AS updated_at,
            completed_at::text AS completed_at,
            COALESCE(retry_count, 0)::bigint AS retry_count
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?
    .ok_or_else(|| anyhow::anyhow!("Dispatch query error: Query returned no rows"))?;

    let status = row
        .try_get::<String, _>("status")
        .map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?;
    let updated_at = row
        .try_get::<String, _>("updated_at")
        .map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?;
    let dispatch_type = row
        .try_get::<Option<String>, _>("dispatch_type")
        .map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?;
    let context_raw = row
        .try_get::<Option<String>, _>("context")
        .map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?;
    let result_raw = row
        .try_get::<Option<String>, _>("result")
        .map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?;
    let context = parse_dispatch_json_text_pg(context_raw.as_deref());
    let result = parse_dispatch_json_text_pg(result_raw.as_deref());
    let result_summary = summarize_dispatch_result(
        dispatch_type.as_deref(),
        Some(status.as_str()),
        result.as_ref(),
        context.as_ref(),
    );
    let completed_at = row
        .try_get::<Option<String>, _>("completed_at")
        .map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?
        .or_else(|| (status == "completed").then(|| updated_at.clone()));

    Ok(json!({
        "id": row.try_get::<String, _>("id").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "kanban_card_id": row.try_get::<Option<String>, _>("kanban_card_id").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "from_agent_id": row.try_get::<Option<String>, _>("from_agent_id").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "to_agent_id": row.try_get::<Option<String>, _>("to_agent_id").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "dispatch_type": dispatch_type,
        "status": status,
        "title": row.try_get::<Option<String>, _>("title").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "context": context,
        "result": result,
        "result_summary": result_summary,
        "parent_dispatch_id": row.try_get::<Option<String>, _>("parent_dispatch_id").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "chain_depth": row.try_get::<i64, _>("chain_depth").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "created_at": row.try_get::<String, _>("created_at").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "updated_at": updated_at,
        "completed_at": completed_at,
        "retry_count": row.try_get::<i64, _>("retry_count").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
    }))
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

    validate_dispatch_target_on_pg(pg_pool, kanban_card_id, to_agent_id, dispatch_type).await?;
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
            Some((wt_path, wt_branch))
        } else if phase_gate_sidecar {
            None
        } else {
            resolve_card_worktree(
                pg_pool,
                kanban_card_id,
                Some(&context_with_session_strategy),
            )
            .await?
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

#[cfg(test)]
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

#[cfg(test)]
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
    validate_dispatch_target_on_conn(&conn, kanban_card_id, to_agent_id, dispatch_type)?;
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

    #[cfg(test)]
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
    #[cfg(not(test))]
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

#[cfg(test)]
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
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn apply_dispatch_attached_intents(
    conn: &rusqlite::Connection,
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
    let dispatch_status = sqlx::query_scalar::<_, String>(
        "SELECT status
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| anyhow::anyhow!("load postgres dispatch status {dispatch_id}: {error}"))?;

    if matches!(
        dispatch_status.as_deref(),
        Some("completed") | Some("failed") | Some("cancelled")
    ) {
        return Ok(false);
    }

    let inserted = sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title)
         VALUES ($1, 'notify', $2, $3, $4)
         ON CONFLICT DO NOTHING",
    )
    .bind(dispatch_id)
    .bind(agent_id)
    .bind(card_id)
    .bind(title)
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

    if let Err(error) = sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
            parent_dispatch_id, chain_depth, created_at, updated_at
        ) VALUES (
            $1, $2, $3, $4, 'pending', $5, $6, $7, $8, NOW(), NOW()
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
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn apply_dispatch_attached_intents_on_conn(
    conn: &rusqlite::Connection,
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
        rusqlite::params![
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

#[cfg(test)]
fn apply_sqlite_transition_intent_for_tests(
    conn: &rusqlite::Connection,
    intent: &crate::engine::transition::TransitionIntent,
) -> Result<()> {
    use crate::engine::transition::TransitionIntent;

    match intent {
        TransitionIntent::UpdateStatus { card_id, to, .. } => {
            conn.execute(
                "UPDATE kanban_cards SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
                rusqlite::params![to, card_id],
            )?;
        }
        TransitionIntent::SetLatestDispatchId {
            card_id,
            dispatch_id,
        } => {
            conn.execute(
                "UPDATE kanban_cards SET latest_dispatch_id = ?1, updated_at = datetime('now') WHERE id = ?2",
                rusqlite::params![dispatch_id, card_id],
            )?;
        }
        TransitionIntent::SetReviewStatus {
            card_id,
            review_status,
        } => {
            conn.execute(
                "UPDATE kanban_cards SET review_status = ?1, updated_at = datetime('now') WHERE id = ?2",
                rusqlite::params![review_status, card_id],
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
mod tests {
    use super::apply_dispatch_attached_intents_on_pg_tx;
    use super::create_dispatch_core_with_id_and_options as create_dispatch_core_with_id_and_options_async;
    use super::*;

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
            rusqlite::params![agent_id, format!("Agent {agent_id}")],
        )
        .expect("seed sqlite agent");
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES (?1, 'Test Card', ?2, ?3)",
            rusqlite::params![card_id, status, agent_id],
        )
        .expect("seed sqlite card");
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
        validate_dispatch_target_on_pg(&pool, "card-valid", "agent-valid", "implementation")
            .await
            .expect("happy-path validation");

        pg_seed_agent(&pool, "agent-missing-channel", None, None).await;
        pg_seed_card(&pool, "card-missing-channel", None, None).await;
        let err = validate_dispatch_target_on_pg(
            &pool,
            "card-missing-channel",
            "agent-missing-channel",
            "implementation",
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
