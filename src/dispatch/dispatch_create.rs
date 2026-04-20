use anyhow::Result;
use serde_json::json;
use sqlx::PgPool;

use crate::db::Db;
use crate::db::agents::{
    resolve_agent_dispatch_channel_on_conn, resolve_agent_dispatch_channel_pg,
};
use crate::engine::PolicyEngine;

use super::dispatch_channel::{dispatch_uses_alt_channel, resolve_dispatch_channel_id};
use super::dispatch_context::{
    ReviewTargetTrust, TargetRepoSource, build_review_context, build_review_context_pg,
    dispatch_context_with_session_strategy, dispatch_context_worktree_target,
    inject_review_dispatch_identifiers, json_string_field, resolve_card_target_repo_ref,
    resolve_card_worktree, resolve_parent_dispatch_context,
};
use super::dispatch_status::{
    ensure_dispatch_notify_outbox_on_conn, record_dispatch_status_event_on_conn,
};
use super::{
    DispatchCreateOptions, cancel_dispatch_and_reset_auto_queue_on_conn, query_dispatch_row,
};

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

fn load_existing_thread_for_channel(
    conn: &libsql_rusqlite::Connection,
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
    let row = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT channel_thread_map::text, active_thread_id
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

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

    Ok(active_thread_id)
}

fn lookup_active_dispatch_id(
    conn: &libsql_rusqlite::Connection,
    card_id: &str,
    dispatch_type: &str,
) -> Option<String> {
    conn.query_row(
        "SELECT id FROM task_dispatches \
         WHERE kanban_card_id = ?1 AND dispatch_type = ?2 \
         AND status IN ('pending', 'dispatched') \
         ORDER BY rowid DESC LIMIT 1",
        libsql_rusqlite::params![card_id, dispatch_type],
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

fn is_single_active_dispatch_violation(error: &libsql_rusqlite::Error) -> bool {
    matches!(
        error,
        libsql_rusqlite::Error::SqliteFailure(_, Some(message))
            if message.contains("UNIQUE constraint failed")
                && message.contains("task_dispatches.kanban_card_id")
    )
}

fn validate_dispatch_target_on_conn(
    conn: &libsql_rusqlite::Connection,
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

#[allow(clippy::too_many_arguments)]
fn create_dispatch_core_internal(
    db: &Db,
    pg_pool: Option<&PgPool>,
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
    if let Some(pool) = pg_pool {
        let card_id = kanban_card_id.to_string();
        let target_agent_id = to_agent_id.to_string();
        let dispatch_type_owned = dispatch_type.to_string();
        // #846: keep dispatch creation synchronous for now and bridge into the
        // PG leaf helpers. Fully promoting the public create_dispatch stack to
        // async would spill across a much larger caller surface; #850 can
        // remove this bridge once the rest of the stack is PG-native.
        block_on_dispatch_pg(pool, move |pool| async move {
            validate_dispatch_target_on_pg(&pool, &card_id, &target_agent_id, &dispatch_type_owned)
                .await
        })?;
    } else {
        validate_dispatch_target_on_conn(&conn, kanban_card_id, to_agent_id, dispatch_type)?;
    }
    if dispatch_context_requests_sidecar(context) {
        options.sidecar_dispatch = true;
    }

    crate::pipeline::ensure_loaded();
    let effective = if let Some(pool) = pg_pool {
        let repo_id = card_repo_id.clone();
        let agent_id = card_agent_id.clone();
        block_on_dispatch_pg(pool, move |pool| async move {
            Ok(
                crate::pipeline::resolve_for_card_pg(
                    &pool,
                    repo_id.as_deref(),
                    agent_id.as_deref(),
                )
                .await,
            )
        })?
    } else {
        crate::pipeline::resolve_for_card(&conn, card_repo_id.as_deref(), card_agent_id.as_deref())
    };
    let is_terminal = effective.is_terminal(&old_status);
    if is_terminal && !options.sidecar_dispatch {
        return Err(anyhow::anyhow!(
            "Cannot create {} dispatch for terminal card {} (status: {}) — cannot revert terminal card",
            dispatch_type,
            kanban_card_id,
            old_status
        ));
    }

    if dispatch_type != "review-decision" {
        let existing_id = if let Some(pool) = pg_pool {
            let card_id = kanban_card_id.to_string();
            let dispatch_type_owned = dispatch_type.to_string();
            block_on_dispatch_pg(pool, move |pool| async move {
                Ok(lookup_active_dispatch_id_pg(&pool, &card_id, &dispatch_type_owned).await)
            })?
        } else {
            lookup_active_dispatch_id(&conn, kanban_card_id, dispatch_type)
        };
        if let Some(eid) = existing_id {
            tracing::info!(
                "DEDUP: reusing existing dispatch {} for card {} type {}",
                eid,
                kanban_card_id,
                dispatch_type
            );
            return Ok((eid, old_status, true));
        }
    }

    let (parent_dispatch_id, chain_depth) =
        resolve_parent_dispatch_context(&conn, kanban_card_id, context)?;

    // #762 (A): Capture whether the caller explicitly supplied target_repo
    // BEFORE we inject our card-scoped fallback below. Downstream
    // `build_review_context` needs this provenance signal to decide whether
    // an unrecoverable external target_repo can safely fall back to
    // card-scoped recovery. Inferring from the post-injection context would
    // make every dispatch look caller-supplied.
    let caller_target_repo_source = if json_string_field(context, "target_repo").is_some() {
        TargetRepoSource::CallerSupplied
    } else {
        TargetRepoSource::CardScopeDefault
    };
    let mut context_with_session_strategy =
        dispatch_context_with_session_strategy(dispatch_type, context);
    let target_repo =
        resolve_card_target_repo_ref(db, kanban_card_id, Some(&context_with_session_strategy));
    if let Some(target_repo) = target_repo.as_deref()
        && let Some(obj) = context_with_session_strategy.as_object_mut()
    {
        obj.entry("target_repo".to_string())
            .or_insert_with(|| json!(target_repo));
    }
    let context_str = if dispatch_type == "review" {
        // #761 (Codex round-2): `create_dispatch_core_internal` is the single
        // funnel for every review dispatch that originates from the public
        // HTTP API (POST /api/dispatches → dispatch_service::create_dispatch
        // → here) as well as from JS policies
        // (`agentdesk.dispatch.create(..., "review", ...)`). Neither of those
        // call sites is entitled to pre-seed review-target fields, so this
        // path is ALWAYS untrusted. Internal tests or future Rust callers that
        // need to pre-populate review-target fields must NOT go through
        // `create_dispatch*` — they must call `build_review_context` directly
        // with `ReviewTargetTrust::Trusted`.
        if let Some(pool) = pg_pool {
            let card_id = kanban_card_id.to_string();
            let target_agent_id = to_agent_id.to_string();
            let review_context = context_with_session_strategy.clone();
            block_on_dispatch_pg(pool, move |pool| async move {
                build_review_context_pg(
                    &pool,
                    &card_id,
                    &target_agent_id,
                    &review_context,
                    ReviewTargetTrust::Untrusted,
                    caller_target_repo_source,
                )
                .await
            })?
        } else {
            build_review_context(
                db,
                kanban_card_id,
                to_agent_id,
                &context_with_session_strategy,
                ReviewTargetTrust::Untrusted,
                caller_target_repo_source,
            )?
        }
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
            // Phase-gate sidecars can operate on the recorded gate context alone.
            // Do not fail dispatch creation just because a repo_dir mapping is absent.
            None
        } else {
            resolve_card_worktree(db, kanban_card_id, Some(&context_with_session_strategy))?
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
            inject_review_dispatch_identifiers(db, kanban_card_id, dispatch_type, &mut obj);
            base = serde_json::to_string(&serde_json::Value::Object(obj)).unwrap_or(base);
        }
        base
    };
    let is_review_type = dispatch_type == "review"
        || dispatch_type == "review-decision"
        || dispatch_type == "rework"
        || dispatch_type == "consultation";

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

    let attach_result = apply_dispatch_attached_intents(
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
    );

    if let Err(e) = attach_result {
        // #743: include "create-pr" in the UNIQUE-race dedup recovery path.
        // The commit 1 partial unique index idx_single_active_create_pr makes
        // parallel create-pr inserts race the same way review/review-decision
        // already does — the loser should reuse the winner's dispatch rather
        // than surface a hard error.
        if matches!(dispatch_type, "review" | "review-decision" | "create-pr")
            && e.to_string()
                .contains("concurrent race prevented by DB constraint")
        {
            let existing_id = if let Some(pool) = pg_pool {
                let card_id = kanban_card_id.to_string();
                let dispatch_type_owned = dispatch_type.to_string();
                block_on_dispatch_pg(pool, move |pool| async move {
                    Ok(lookup_active_dispatch_id_pg(&pool, &card_id, &dispatch_type_owned).await)
                })?
            } else {
                lookup_active_dispatch_id(&conn, kanban_card_id, dispatch_type)
            };
            if let Some(existing_id) = existing_id {
                tracing::info!(
                    "DEDUP: reusing existing dispatch {} for card {} type {} after UNIQUE race",
                    existing_id,
                    kanban_card_id,
                    dispatch_type
                );
                return Ok((existing_id, old_status, true));
            }
        }
        return Err(e);
    }

    Ok((dispatch_id.to_string(), old_status, false))
}

pub fn create_dispatch_core(
    db: &Db,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<(String, String, bool)> {
    create_dispatch_core_with_options(
        db,
        None,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        DispatchCreateOptions::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn create_dispatch_core_with_options(
    db: &Db,
    pg_pool: Option<&PgPool>,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
    options: DispatchCreateOptions,
) -> Result<(String, String, bool)> {
    let dispatch_id = uuid::Uuid::new_v4().to_string();
    create_dispatch_core_internal(
        db,
        pg_pool,
        &dispatch_id,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        options,
    )
}

pub fn create_dispatch_core_with_id(
    db: &Db,
    dispatch_id: &str,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<(String, String, bool)> {
    create_dispatch_core_with_id_and_options(
        db,
        dispatch_id,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        DispatchCreateOptions::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn create_dispatch_core_with_id_and_options(
    db: &Db,
    dispatch_id: &str,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
    options: DispatchCreateOptions,
) -> Result<(String, String, bool)> {
    create_dispatch_core_with_id_and_options_pg(
        db,
        None,
        dispatch_id,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        options,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn create_dispatch_core_with_id_and_options_pg(
    db: &Db,
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
    options: DispatchCreateOptions,
) -> Result<(String, String, bool)> {
    create_dispatch_core_internal(
        db,
        pg_pool,
        dispatch_id,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        options,
    )
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
    let (dispatch_id, old_status, reused) = create_dispatch_core_with_options(
        db,
        pg_pool,
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
    let effective = if let Some(pool) = pg_pool {
        let repo_id = card_repo_id.clone();
        let agent_id = card_agent_id.clone();
        block_on_dispatch_pg(pool, move |pool| async move {
            Ok(
                crate::pipeline::resolve_for_card_pg(
                    &pool,
                    repo_id.as_deref(),
                    agent_id.as_deref(),
                )
                .await,
            )
        })?
    } else {
        crate::pipeline::resolve_for_card(&conn, card_repo_id.as_deref(), card_agent_id.as_deref())
    };
    drop(conn);
    let kickoff_owned = effective.kickoff_for(&old_status).unwrap_or_else(|| {
        tracing::error!("Pipeline has no kickoff state for hook firing");
        effective.initial_state().to_string()
    });
    crate::kanban::fire_state_hooks(db, engine, kanban_card_id, &old_status, &kickoff_owned);

    Ok(dispatch)
}

/// Transaction-owning wrapper. Opens BEGIN/COMMIT around the on-conn variant.
///
/// Use this when the caller does not have an outer transaction. Callers that
/// need to compose dispatch creation into their own transaction (e.g.
/// `handoffCreatePr` in #743) should call
/// [`apply_dispatch_attached_intents_on_conn`] directly instead.
#[allow(clippy::too_many_arguments)]
fn apply_dispatch_attached_intents(
    conn: &libsql_rusqlite::Connection,
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

/// Connection-local variant: does NOT manage its own transaction. Caller must
/// have an open transaction on `conn` and commit/rollback after this returns.
///
/// This variant exists so bridge ops like `handoffCreatePr` (#743) can compose
/// dispatch creation with surrounding pr_tracking/kanban_cards updates in a
/// single atomic transaction.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_dispatch_attached_intents_on_conn(
    conn: &libsql_rusqlite::Connection,
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
        libsql_rusqlite::params![
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
        transition::execute_intent_on_conn(conn, intent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
