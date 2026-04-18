use anyhow::Result;
use serde_json::json;

use crate::db::Db;
use crate::db::agents::resolve_agent_dispatch_channel_on_conn;
use crate::engine::PolicyEngine;

use super::dispatch_channel::{dispatch_uses_alt_channel, resolve_dispatch_channel_id};
use super::dispatch_context::{
    ReviewTargetTrust, build_review_context, dispatch_context_with_session_strategy,
    dispatch_context_worktree_target, inject_review_dispatch_identifiers,
    resolve_card_target_repo_ref, resolve_card_worktree, resolve_parent_dispatch_context,
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

    if let Some(json_str) = map_json.as_deref() {
        if !json_str.is_empty() && json_str != "{}" {
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
    }

    Ok(conn
        .query_row(
            "SELECT active_thread_id FROM kanban_cards WHERE id = ?1 AND active_thread_id IS NOT NULL",
            [card_id],
            |row| row.get(0),
        )
        .ok())
}

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

fn is_single_active_dispatch_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(_, Some(message))
            if message.contains("UNIQUE constraint failed")
                && message.contains("task_dispatches.kanban_card_id")
    )
}

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

    if let Some(thread_id) = load_existing_thread_for_channel(conn, card_id, channel_id)? {
        if thread_id.parse::<u64>().is_err() {
            return Err(anyhow::anyhow!(
                "Cannot create {} dispatch: card '{}' has invalid thread '{}' for channel {}",
                dispatch_type,
                card_id,
                thread_id,
                channel_id
            ));
        }
    }

    Ok(())
}

fn create_dispatch_core_internal(
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
        let existing_id = lookup_active_dispatch_id(&conn, kanban_card_id, dispatch_type);
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

    let mut context_with_session_strategy =
        dispatch_context_with_session_strategy(dispatch_type, context);
    let target_repo =
        resolve_card_target_repo_ref(db, kanban_card_id, Some(&context_with_session_strategy));
    if let Some(obj) = context_with_session_strategy.as_object_mut() {
        if let Some(target_repo) = target_repo.as_deref() {
            obj.entry("target_repo".to_string())
                .or_insert_with(|| json!(target_repo));
        }
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
        build_review_context(
            db,
            kanban_card_id,
            to_agent_id,
            &context_with_session_strategy,
            ReviewTargetTrust::Untrusted,
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
            // Phase-gate sidecars can operate on the recorded gate context alone.
            // Do not fail dispatch creation just because a repo_dir mapping is absent.
            None
        } else {
            resolve_card_worktree(db, kanban_card_id, Some(&context_with_session_strategy))?
                .map(|(wt_path, wt_branch, _)| (wt_path, Some(wt_branch)))
        };

        if let Some((wt_path, wt_branch)) = worktree_target {
            if let Ok(mut obj) =
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
            if let Some(existing_id) =
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
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        DispatchCreateOptions::default(),
    )
}

pub fn create_dispatch_core_with_options(
    db: &Db,
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
    create_dispatch_core_internal(
        db,
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
        engine,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
        DispatchCreateOptions::default(),
    )
}

pub fn create_dispatch_with_options(
    db: &Db,
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

/// Transaction-owning wrapper. Opens BEGIN/COMMIT around the on-conn variant.
///
/// Use this when the caller does not have an outer transaction. Callers that
/// need to compose dispatch creation into their own transaction (e.g.
/// `handoffCreatePr` in #743) should call
/// [`apply_dispatch_attached_intents_on_conn`] directly instead.
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

/// Connection-local variant: does NOT manage its own transaction. Caller must
/// have an open transaction on `conn` and commit/rollback after this returns.
///
/// This variant exists so bridge ops like `handoffCreatePr` (#743) can compose
/// dispatch creation with surrounding pr_tracking/kanban_cards updates in a
/// single atomic transaction.
pub(crate) fn apply_dispatch_attached_intents_on_conn(
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
        transition::execute_intent_on_conn(conn, intent)?;
    }
    Ok(())
}
