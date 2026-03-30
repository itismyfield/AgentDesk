use anyhow::Result;
use serde_json::json;

use crate::db::Db;
use crate::engine::PolicyEngine;

/// Build the context JSON string for a review dispatch.
///
/// Injects `reviewed_commit`, `branch`, and provider info.
/// Prefers worktree branch (if found for this card's issue) over main HEAD.
fn build_review_context(
    db: &Db,
    kanban_card_id: &str,
    to_agent_id: &str,
    context: &serde_json::Value,
) -> Result<String> {
    let mut ctx_val = if context.is_object() {
        context.clone()
    } else {
        json!({})
    };
    if let Some(obj) = ctx_val.as_object_mut() {
        if !obj.contains_key("reviewed_commit") {
            let repo_dir =
                crate::services::platform::resolve_repo_dir().ok_or_else(|| {
                    anyhow::anyhow!("Cannot resolve repo dir; set AGENTDESK_REPO_DIR")
                })?;

            // #193: Try to find a worktree branch for this card's issue.
            // Without this, reviews always inspect main HEAD and miss
            // unmerged implementation commits, causing stale review loops.
            let issue_number: Option<i64> = db
                .separate_conn()
                .ok()
                .and_then(|conn| {
                    conn.query_row(
                        "SELECT github_issue_number FROM kanban_cards WHERE id = ?1",
                        [kanban_card_id],
                        |row| row.get(0),
                    )
                    .ok()
                })
                .flatten();

            let wt_info = issue_number
                .and_then(|num| crate::services::platform::find_worktree_for_issue(&repo_dir, num));

            if let Some(ref wt) = wt_info {
                obj.insert("reviewed_commit".to_string(), json!(wt.commit));
                obj.insert("branch".to_string(), json!(wt.branch));
                tracing::info!(
                    "[dispatch] Review dispatch for card {}: using worktree branch '{}' (commit {})",
                    kanban_card_id,
                    wt.branch,
                    &wt.commit[..8.min(wt.commit.len())]
                );
            } else {
                // Fall back: check previous review dispatch for persisted branch info
                let prev_branch: Option<(String, String)> =
                    db.separate_conn().ok().and_then(|conn| {
                        let ctx_str: Option<String> = conn
                            .query_row(
                                "SELECT context FROM task_dispatches \
                             WHERE kanban_card_id = ?1 AND dispatch_type = 'review' \
                             AND status IN ('completed', 'failed', 'cancelled') \
                             ORDER BY created_at DESC LIMIT 1",
                                [kanban_card_id],
                                |row| row.get(0),
                            )
                            .ok()
                            .flatten();
                        ctx_str.and_then(|s| {
                            let v: serde_json::Value = serde_json::from_str(&s).ok()?;
                            let b = v.get("branch")?.as_str()?.to_string();
                            let c = v.get("reviewed_commit")?.as_str()?.to_string();
                            Some((b, c))
                        })
                    });

                if let Some((branch, commit)) = prev_branch {
                    obj.insert("reviewed_commit".to_string(), json!(commit));
                    obj.insert("branch".to_string(), json!(branch));
                    tracing::info!(
                        "[dispatch] Review dispatch for card {}: reusing previous branch '{}'",
                        kanban_card_id,
                        branch
                    );
                } else if let Some(commit) = crate::services::platform::git_head_commit(&repo_dir) {
                    obj.insert("reviewed_commit".to_string(), json!(commit));
                }
            }
        }

        // Inject from_provider/target_provider for cross-provider review validation
        if !obj.contains_key("from_provider") || !obj.contains_key("target_provider") {
            if let Ok(conn) = db.separate_conn() {
                if let Ok((ch, alt)) = conn.query_row(
                    "SELECT discord_channel_id, discord_channel_alt FROM agents WHERE id = ?1",
                    [to_agent_id],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    },
                ) {
                    if !obj.contains_key("from_provider") {
                        if let Some(fp) = ch.as_deref().and_then(provider_from_channel_suffix) {
                            obj.insert("from_provider".to_string(), json!(fp));
                        }
                    }
                    if !obj.contains_key("target_provider") {
                        if let Some(tp) = alt.as_deref().and_then(provider_from_channel_suffix) {
                            obj.insert("target_provider".to_string(), json!(tp));
                        }
                    }
                }
            }
        }
    }
    Ok(serde_json::to_string(&ctx_val)?)
}

/// Core dispatch creation: DB operations only, no hooks fired.
///
/// - Inserts a record into `task_dispatches`
/// - Updates `kanban_cards.latest_dispatch_id` and sets status to "requested" (non-review)
/// - Returns `(dispatch_id, old_card_status)`
///
/// Caller is responsible for firing hooks after this returns.
///
/// Returns `(dispatch_id, old_card_status, reused)`.
/// When `reused` is true the returned ID belongs to an existing pending/dispatched
/// dispatch of the same type — no new row was inserted (#173 dedup).
pub fn create_dispatch_core(
    db: &Db,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<(String, String, bool)> {
    let dispatch_id = uuid::Uuid::new_v4().to_string();

    let context_str = if dispatch_type == "review" {
        build_review_context(db, kanban_card_id, to_agent_id, context)?
    } else {
        serde_json::to_string(context)?
    };

    // Use separate_conn to avoid blocking request handlers while
    // engine/onTick holds the main DB Mutex via QuickJS.
    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB conn error: {e}"))?;

    // Get current card status + repo/agent IDs for effective pipeline resolution
    let (old_status, card_repo_id, card_agent_id): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
            [kanban_card_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|e| anyhow::anyhow!("Card not found: {e}"))?;

    // Guard: prevent ALL dispatches for terminal cards (pipeline-driven).
    crate::pipeline::ensure_loaded();
    let effective =
        crate::pipeline::resolve_for_card(&conn, card_repo_id.as_deref(), card_agent_id.as_deref());
    let is_terminal = effective.is_terminal(&old_status);
    if is_terminal {
        return Err(anyhow::anyhow!(
            "Cannot create {} dispatch for terminal card {} (status: {}) — cannot revert terminal card",
            dispatch_type,
            kanban_card_id,
            old_status
        ));
    }

    // #173: Dedup — if same card already has a pending/dispatched dispatch of the SAME type,
    // return the existing dispatch_id idempotently instead of creating a duplicate.
    // review-decision handles its own dedup below (#116: cancel previous then insert).
    if dispatch_type != "review-decision" {
        let existing_id: Option<String> = conn
            .query_row(
                "SELECT id FROM task_dispatches \
                 WHERE kanban_card_id = ?1 AND dispatch_type = ?2 \
                 AND status IN ('pending', 'dispatched') LIMIT 1",
                rusqlite::params![kanban_card_id, dispatch_type],
                |row| row.get(0),
            )
            .ok();
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

    let is_review_type = dispatch_type == "review"
        || dispatch_type == "review-decision"
        || dispatch_type == "rework";

    // #116: Cancel any existing pending review-decision for this card before creating a new one.
    // Enforces the invariant: at most 1 pending/dispatched review-decision per card.
    if dispatch_type == "review-decision" {
        let cancelled = conn.execute(
            "UPDATE task_dispatches SET status = 'cancelled', result = '{\"reason\":\"superseded_by_new_review_decision\"}', updated_at = datetime('now') \
             WHERE kanban_card_id = ?1 AND dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched')",
            [kanban_card_id],
        ).unwrap_or(0);
        if cancelled > 0 {
            tracing::info!(
                "[dispatch] Cancelled {} stale review-decision(s) for card {} before creating new one",
                cancelled,
                kanban_card_id
            );
        }
    }

    // #155: Dispatch INSERT + card-state intents in a single transaction.
    // The dispatch row and card-state update must be atomic — if intents fail,
    // the dispatch row must also be rolled back to prevent orphaned dispatches.
    apply_dispatch_attached_intents(
        &conn,
        kanban_card_id,
        to_agent_id,
        &dispatch_id,
        dispatch_type,
        is_review_type,
        &old_status,
        &effective,
        title,
        &context_str,
    )?;

    Ok((dispatch_id, old_status, false))
}

/// Like `create_dispatch_core` but uses a pre-assigned dispatch ID (#121 intent model).
/// Called by the intent executor when processing CreateDispatch intents.
///
/// Returns `(dispatch_id, old_card_status, reused)` — see `create_dispatch_core` docs.
pub fn create_dispatch_core_with_id(
    db: &Db,
    dispatch_id: &str,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<(String, String, bool)> {
    let context_str = if dispatch_type == "review" {
        build_review_context(db, kanban_card_id, to_agent_id, context)?
    } else {
        serde_json::to_string(context)?
    };

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

    crate::pipeline::ensure_loaded();
    let effective =
        crate::pipeline::resolve_for_card(&conn, card_repo_id.as_deref(), card_agent_id.as_deref());
    let is_terminal = effective.is_terminal(&old_status);
    if is_terminal {
        return Err(anyhow::anyhow!(
            "Cannot create {} dispatch for terminal card {} (status: {}) — cannot revert terminal card",
            dispatch_type,
            kanban_card_id,
            old_status
        ));
    }

    // #173: Dedup guard (same logic as create_dispatch_core).
    if dispatch_type != "review-decision" {
        let existing_id: Option<String> = conn
            .query_row(
                "SELECT id FROM task_dispatches \
                 WHERE kanban_card_id = ?1 AND dispatch_type = ?2 \
                 AND status IN ('pending', 'dispatched') LIMIT 1",
                rusqlite::params![kanban_card_id, dispatch_type],
                |row| row.get(0),
            )
            .ok();
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

    let is_review_type = dispatch_type == "review"
        || dispatch_type == "review-decision"
        || dispatch_type == "rework";

    if dispatch_type == "review-decision" {
        let cancelled = conn.execute(
            "UPDATE task_dispatches SET status = 'cancelled', result = '{\"reason\":\"superseded_by_new_review_decision\"}', updated_at = datetime('now') \
             WHERE kanban_card_id = ?1 AND dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched')",
            [kanban_card_id],
        ).unwrap_or(0);
        if cancelled > 0 {
            tracing::info!(
                "[dispatch] Cancelled {} stale review-decision(s) for card {} before creating new one",
                cancelled,
                kanban_card_id
            );
        }
    }

    // #155: Dispatch INSERT + card-state intents in a single transaction
    apply_dispatch_attached_intents(
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
    )?;

    Ok((dispatch_id.to_string(), old_status, false))
}

/// Create a new dispatch for a kanban card.
///
/// - Delegates DB work to `create_dispatch_core`
/// - Fires `OnCardTransition` hook (old_status -> requested)
///
/// Returns the full dispatch row as JSON.
pub fn create_dispatch(
    db: &Db,
    engine: &PolicyEngine,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    let (dispatch_id, old_status, reused) = create_dispatch_core(
        db,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
    )?;

    // Read back the dispatch
    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB lock error: {e}"))?;
    let dispatch = query_dispatch_row(&conn, &dispatch_id)?;

    // #173: If dedup'd, skip hook firing — no new dispatch was created.
    if reused {
        let mut d = dispatch;
        // Signal to HTTP handler that this was a dedup'd response
        d["__reused"] = json!(true);
        return Ok(d);
    }

    // Fire pipeline-defined on_enter hooks for the kickoff state (#134).
    // Resolve kickoff state from card's effective pipeline (repo/agent overrides).
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

/// #155: Insert dispatch row + apply DispatchAttached transition intents atomically.
///
/// Both the `task_dispatches` INSERT and the card-state intents execute inside
/// a single transaction so that reducer failure rolls back the dispatch row too.
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
) -> Result<()> {
    use crate::engine::transition::{
        self, CardState, GateSnapshot, TransitionContext, TransitionEvent, TransitionOutcome,
    };

    let kickoff_state = if !is_review_type {
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

    let decision = transition::decide_transition(
        &ctx,
        &TransitionEvent::DispatchAttached {
            dispatch_id: dispatch_id.to_string(),
            dispatch_type: dispatch_type.to_string(),
            kickoff_state,
        },
    );

    if let TransitionOutcome::Blocked(reason) = &decision.outcome {
        return Err(anyhow::anyhow!("{}", reason));
    }

    conn.execute_batch("BEGIN")?;
    let exec_result = (|| -> anyhow::Result<()> {
        // Insert dispatch row inside the transaction (#155 review fix)
        if let Err(e) = conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, datetime('now'), datetime('now'))",
            rusqlite::params![dispatch_id, card_id, to_agent_id, dispatch_type, title, context_str],
        ) {
            if dispatch_type == "review-decision"
                && e.to_string().contains("UNIQUE constraint failed")
            {
                return Err(anyhow::anyhow!(
                    "review-decision already exists for card {} (concurrent race prevented by DB constraint)",
                    card_id
                ));
            }
            return Err(e.into());
        }
        for intent in &decision.intents {
            transition::execute_intent_on_conn(conn, intent)?;
        }
        Ok(())
    })();
    if let Err(e) = exec_result {
        conn.execute_batch("ROLLBACK").ok();
        return Err(e);
    }
    conn.execute_batch("COMMIT")?;

    Ok(())
}

/// Single authority for dispatch completion.
///
/// All dispatch completion paths — turn_bridge explicit, recovery, API PATCH,
/// session idle — MUST route through this function.  It performs:
///   1. DB status update  (task_dispatches → completed)
///   2. OnDispatchCompleted hook firing  (pipeline event hooks)
///   3. Side-effect draining  (intents, transitions, follow-up dispatches)
///   4. Safety-net re-fire of OnReviewEnter (#139)
pub fn finalize_dispatch(
    db: &Db,
    engine: &PolicyEngine,
    dispatch_id: &str,
    completion_source: &str,
    context: Option<&serde_json::Value>,
) -> Result<serde_json::Value> {
    let result = match context {
        Some(ctx) => {
            let mut merged = ctx.clone();
            if let Some(obj) = merged.as_object_mut() {
                obj.insert(
                    "completion_source".to_string(),
                    serde_json::Value::String(completion_source.to_string()),
                );
            }
            merged
        }
        None => json!({ "completion_source": completion_source }),
    };
    complete_dispatch_inner(db, engine, dispatch_id, &result)
}

/// #143: DB-only dispatch completion — marks status='completed' without firing hooks.
///
/// Used by specialized paths (review_verdict, pm-decision) that fire their own
/// domain-specific hooks instead of the generic OnDispatchCompleted.
/// Returns the number of rows updated (0 = already completed/cancelled/not found).
pub fn mark_dispatch_completed(
    db: &Db,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<usize> {
    let result_str = serde_json::to_string(result)?;
    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB conn error: {e}"))?;
    let changed = conn.execute(
        "UPDATE task_dispatches SET status = 'completed', result = ?1, updated_at = datetime('now') \
         WHERE id = ?2 AND status IN ('pending', 'dispatched')",
        rusqlite::params![result_str, dispatch_id],
    )?;
    Ok(changed)
}

/// Legacy wrapper — delegates to [`finalize_dispatch`] for callers that already
/// have a fully-formed result JSON (e.g. API PATCH handler).
pub fn complete_dispatch(
    db: &Db,
    engine: &PolicyEngine,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<serde_json::Value> {
    complete_dispatch_inner(db, engine, dispatch_id, result)
}

fn complete_dispatch_inner(
    db: &Db,
    engine: &PolicyEngine,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<serde_json::Value> {
    let result_str = serde_json::to_string(result)?;

    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB lock error: {e}"))?;

    let changed = conn.execute(
        "UPDATE task_dispatches SET status = 'completed', result = ?1, updated_at = datetime('now') \
         WHERE id = ?2 AND status IN ('pending', 'dispatched')",
        rusqlite::params![result_str, dispatch_id],
    )?;

    if changed == 0 {
        // Either not found, already completed, or cancelled — skip hook firing
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if exists {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⏭ complete_dispatch: {dispatch_id} already completed/cancelled, skipping hooks"
            );
            let dispatch = query_dispatch_row(&conn, dispatch_id)?;
            drop(conn);
            return Ok(dispatch);
        }
        return Err(anyhow::anyhow!("Dispatch not found: {dispatch_id}"));
    }

    let dispatch = query_dispatch_row(&conn, dispatch_id)?;

    let kanban_card_id: Option<String> = conn
        .query_row(
            "SELECT kanban_card_id FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok();

    // Capture card status BEFORE hooks fire (used for audit/logging if needed)
    let _old_status: String = kanban_card_id
        .as_ref()
        .and_then(|cid| {
            conn.query_row(
                "SELECT status FROM kanban_cards WHERE id = ?1",
                [cid],
                |row| row.get(0),
            )
            .ok()
        })
        .unwrap_or_default();

    // Capture max rowid before hooks fire — any dispatches created by hooks
    // (JS agentdesk.dispatch.create()) will have a higher rowid.
    let pre_hook_max_rowid: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(rowid), 0) FROM task_dispatches",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    drop(conn);

    // Fire event hooks for dispatch completion (#134 — pipeline-defined events)
    crate::kanban::fire_event_hooks(
        db,
        engine,
        "on_dispatch_completed",
        "OnDispatchCompleted",
        json!({
            "dispatch_id": dispatch_id,
            "kanban_card_id": kanban_card_id,
            "result": result,
        }),
    );

    // After OnDispatchCompleted, policies may have queued follow-up transitions
    // and dispatch intents (OnReviewEnter, retry dispatches, etc.).
    crate::kanban::drain_hook_side_effects(db, engine);

    // After all hooks and transitions drained, check for dispatches created by
    // OnDispatchCompleted hooks (e.g. pipeline.js, review-automation.js, timeouts.js)
    // that were NOT covered by fire_transition_hooks' notify_new_dispatches_after_hooks.
    // These are dispatches created outside any card transition context.
    notify_hook_created_dispatches(db, pre_hook_max_rowid);

    // #139: Safety net — if card transitioned to review but OnReviewEnter failed
    // to create a review dispatch (engine lock contention, JS error, etc.),
    // re-fire OnReviewEnter to guarantee review dispatch creation.
    {
        let needs_review_dispatch = db
            .lock()
            .ok()
            .map(|conn| {
                let (card_status, repo_id, agent_id): (Option<String>, Option<String>, Option<String>) = conn
                    .query_row(
                        "SELECT status, repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
                        [&kanban_card_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .unwrap_or((None, None, None));
                let has_review_dispatch: bool = conn
                    .query_row(
                        "SELECT COUNT(*) > 0 FROM task_dispatches \
                         WHERE kanban_card_id = ?1 AND dispatch_type IN ('review', 'review-decision') \
                         AND status IN ('pending', 'dispatched')",
                        [&kanban_card_id],
                        |row| row.get(0),
                    )
                    .unwrap_or(false);
                // Pipeline-driven: check if current state has OnReviewEnter hook (card's effective pipeline)
                let is_review_state = card_status.as_deref().map_or(false, |s| {
                    let eff = crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref());
                    eff.hooks_for_state(s)
                        .map_or(false, |h| h.on_enter.iter().any(|n| n == "OnReviewEnter"))
                });
                is_review_state && !has_review_dispatch
            })
            .unwrap_or(false);

        if needs_review_dispatch {
            let cid = kanban_card_id.as_deref().unwrap_or("unknown");
            tracing::warn!(
                "[dispatch] Card {} in review-like state but no review dispatch — re-firing OnReviewEnter (#139)",
                cid
            );
            let _ = engine.try_fire_hook_by_name("OnReviewEnter", json!({ "card_id": cid }));
            crate::kanban::drain_hook_side_effects(db, engine);
            notify_hook_created_dispatches(db, pre_hook_max_rowid);
        }
    }

    Ok(dispatch)
}

/// Send Discord notifications for any pending dispatches created after `pre_hook_max_rowid`.
/// Uses the `dispatch_notified` dedup guard in `send_dispatch_to_discord` to avoid
/// double-notifying dispatches already handled by `notify_new_dispatches_after_hooks`.
pub(crate) fn notify_hook_created_dispatches(db: &Db, pre_hook_max_rowid: i64) {
    let dispatches: Vec<(String, String, String, String)> = db
        .separate_conn()
        .ok()
        .map(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT td.id, td.to_agent_id, td.kanban_card_id, kc.title \
                     FROM task_dispatches td \
                     JOIN kanban_cards kc ON td.kanban_card_id = kc.id \
                     WHERE td.status = 'pending' \
                       AND td.rowid > ?1 \
                       AND NOT EXISTS (SELECT 1 FROM kv_meta WHERE key = 'dispatch_notified:' || td.id)",
                )
                .ok();
            stmt.as_mut()
                .and_then(|s| {
                    s.query_map(rusqlite::params![pre_hook_max_rowid], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })
                    .ok()
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default();

    if dispatches.is_empty() {
        return;
    }

    // #144: Queue via dispatch outbox instead of tokio::spawn.
    for (dispatch_id, agent_id, card_id, title) in dispatches {
        crate::server::routes::dispatches::queue_dispatch_notify(
            db,
            &dispatch_id,
            &agent_id,
            &card_id,
            &title,
        );
    }
}

/// Read a single dispatch row as JSON.
pub fn query_dispatch_row(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
) -> Result<serde_json::Value> {
    conn.query_row(
        "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type, status, title, context, result, parent_dispatch_id, chain_depth, created_at, updated_at, COALESCE(retry_count, 0)
         FROM task_dispatches WHERE id = ?1",
        [dispatch_id],
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "kanban_card_id": row.get::<_, Option<String>>(1)?,
                "from_agent_id": row.get::<_, Option<String>>(2)?,
                "to_agent_id": row.get::<_, Option<String>>(3)?,
                "dispatch_type": row.get::<_, Option<String>>(4)?,
                "status": row.get::<_, String>(5)?,
                "title": row.get::<_, Option<String>>(6)?,
                "context": row.get::<_, Option<String>>(7)?.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
                "result": row.get::<_, Option<String>>(8)?.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
                "parent_dispatch_id": row.get::<_, Option<String>>(9)?,
                "chain_depth": row.get::<_, i64>(10)?,
                "created_at": row.get::<_, String>(11)?,
                "updated_at": row.get::<_, String>(12)?,
                "retry_count": row.get::<_, i64>(13)?,
            }))
        },
    )
    .map_err(|e| anyhow::anyhow!("Dispatch query error: {e}"))
}

/// Check whether a dispatch belongs to an active unified-thread auto-queue run.
///
/// Returns `true` when:
/// - The dispatch's kanban card is part of an active/paused auto-queue run
/// - That run has `unified_thread_id IS NOT NULL`
/// - The run still has pending or dispatched entries remaining
///
/// When `true`, callers should **not** tear down the tmux session because the
/// same thread will be reused for subsequent queue entries.
///
/// Uses a standalone `rusqlite::Connection` opened from the runtime DB path
/// to avoid lock contention with the main `Db` mutex.
pub fn is_unified_thread_active(dispatch_id: &str) -> bool {
    let root = match crate::cli::agentdesk_runtime_root() {
        Some(r) => r,
        None => return false,
    };
    let db_path = root.join("data/agentdesk.sqlite");
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    // #145: Direct dispatch→entry→run lookup via auto_queue_entries.dispatch_id.
    // Eliminates kanban_card_id-based ambiguity when the same card is re-queued.
    let result: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 \
             FROM auto_queue_entries e \
             JOIN auto_queue_runs r ON e.run_id = r.id \
             WHERE e.run_id = ( \
                 SELECT e2.run_id FROM auto_queue_entries e2 \
                 WHERE e2.dispatch_id = ?1 \
                 ORDER BY CASE e2.status WHEN 'dispatched' THEN 0 WHEN 'pending' THEN 1 ELSE 2 END \
                 LIMIT 1 \
             ) \
             AND r.status IN ('active', 'paused') \
             AND e.status IN ('pending', 'dispatched') \
             AND r.unified_thread_id IS NOT NULL",
            [dispatch_id],
            |row| row.get(0),
        )
        .unwrap_or(false);
    result
}

/// Check whether a thread channel belongs to an active unified-thread auto-queue run.
///
/// Looks up `auto_queue_runs` by `unified_thread_channel_id` matching the
/// given Discord channel ID. Returns `true` when a matching active/paused run
/// still has pending or dispatched entries.
pub fn is_unified_thread_channel_active(channel_id: u64) -> bool {
    let root = match crate::cli::agentdesk_runtime_root() {
        Some(r) => r,
        None => return false,
    };
    let db_path = root.join("data/agentdesk.sqlite");
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let channel_str = channel_id.to_string();
    let result: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 \
             FROM auto_queue_entries e \
             JOIN auto_queue_runs r ON e.run_id = r.id \
             WHERE r.unified_thread_channel_id = ?1 \
             AND r.status IN ('active', 'paused') \
             AND e.status IN ('pending', 'dispatched') \
             AND r.unified_thread_id IS NOT NULL",
            [&channel_str],
            |row| row.get(0),
        )
        .unwrap_or(false);
    result
}

/// Extract thread channel ID from a channel name's `-t{15+digit}` suffix.
/// Pure parsing — no DB access. Used by both production guards and tests.
pub fn extract_thread_channel_id(channel_name: &str) -> Option<u64> {
    let pos = channel_name.rfind("-t")?;
    let suffix = &channel_name[pos + 2..];
    if suffix.len() >= 15 && suffix.chars().all(|c| c.is_ascii_digit()) {
        let id: u64 = suffix.parse().ok()?;
        if id == 0 { None } else { Some(id) }
    } else {
        None
    }
}

/// Check whether a channel name (from tmux session parsing) belongs to an active
/// unified-thread auto-queue run. Extracts the thread channel ID from the
/// `-t{15+digit}` suffix in the channel name.
pub fn is_unified_thread_channel_name_active(channel_name: &str) -> bool {
    let Some(thread_channel_id) = extract_thread_channel_id(channel_name) else {
        return false;
    };
    is_unified_thread_channel_active(thread_channel_id)
}

/// Drain `kill_unified_thread:*` kv_meta entries and return the channel names to kill.
/// Each entry is consumed (deleted from DB) on read.
pub fn drain_unified_thread_kill_signals() -> Vec<String> {
    let root = match crate::cli::agentdesk_runtime_root() {
        Some(r) => r,
        None => return vec![],
    };
    let db_path = root.join("data/agentdesk.sqlite");
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let mut stmt = match conn
        .prepare("SELECT key, value FROM kv_meta WHERE key LIKE 'kill_unified_thread:%'")
    {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let entries: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    let mut channels = Vec::new();
    for (key, _run_id) in &entries {
        if let Some(ch) = key.strip_prefix("kill_unified_thread:") {
            channels.push(ch.to_string());
        }
        conn.execute("DELETE FROM kv_meta WHERE key = ?1", [key])
            .ok();
    }
    channels
}

/// Determine provider from a Discord channel name suffix.
fn provider_from_channel_suffix(channel: &str) -> Option<&'static str> {
    if channel.ends_with("-cc") {
        Some("claude")
    } else if channel.ends_with("-cdx") {
        Some("codex")
    } else if channel.ends_with("-gm") {
        Some("gemini")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let config = crate::config::Config::default();
        PolicyEngine::new(&config, db.clone()).unwrap()
    }

    fn seed_card(db: &Db, card_id: &str, status: &str) {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at) VALUES (?1, 'Test Card', ?2, datetime('now'), datetime('now'))",
            rusqlite::params![card_id, status],
        )
        .unwrap();
    }

    #[test]
    fn create_dispatch_inserts_and_updates_card() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-1", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-1",
            "agent-1",
            "implementation",
            "Do the thing",
            &json!({"key": "value"}),
        )
        .unwrap();

        assert_eq!(dispatch["status"], "pending");
        assert_eq!(dispatch["kanban_card_id"], "card-1");
        assert_eq!(dispatch["to_agent_id"], "agent-1");
        assert_eq!(dispatch["dispatch_type"], "implementation");
        assert_eq!(dispatch["title"], "Do the thing");

        // Card should be updated
        let conn = db.separate_conn().unwrap();
        let (card_status, latest_dispatch_id): (String, String) = conn
            .query_row(
                "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(card_status, "requested");
        assert_eq!(latest_dispatch_id, dispatch["id"].as_str().unwrap());
    }

    #[test]
    fn create_dispatch_for_nonexistent_card_fails() {
        let db = test_db();
        let engine = test_engine(&db);

        let result = create_dispatch(
            &db,
            &engine,
            "nonexistent",
            "agent-1",
            "implementation",
            "title",
            &json!({}),
        );
        assert!(result.is_err());
    }

    #[test]
    fn complete_dispatch_updates_status() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-2", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-2",
            "agent-1",
            "implementation",
            "title",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        let completed =
            complete_dispatch(&db, &engine, &dispatch_id, &json!({"output": "done"})).unwrap();

        assert_eq!(completed["status"], "completed");
    }

    #[test]
    fn complete_dispatch_nonexistent_fails() {
        let db = test_db();
        let engine = test_engine(&db);

        let result = complete_dispatch(&db, &engine, "nonexistent", &json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn complete_dispatch_skips_cancelled() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-cancel", "review");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-cancel",
            "agent-1",
            "review-decision",
            "Decision",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        // Simulate dismiss: cancel the dispatch
        {
            let conn = db.separate_conn().unwrap();
            conn.execute(
                "UPDATE task_dispatches SET status = 'cancelled' WHERE id = ?1",
                [&dispatch_id],
            )
            .unwrap();
        }

        // Delayed completion attempt should NOT re-complete the cancelled dispatch
        let result = complete_dispatch(&db, &engine, &dispatch_id, &json!({"verdict": "pass"}));
        // Should return Ok (dispatch found) but status should remain cancelled
        assert!(result.is_ok());
        let returned = result.unwrap();
        assert_eq!(
            returned["status"], "cancelled",
            "cancelled dispatch must not be re-completed"
        );
    }

    #[test]
    fn provider_from_channel_suffix_supports_gemini() {
        assert_eq!(provider_from_channel_suffix("agent-cc"), Some("claude"));
        assert_eq!(provider_from_channel_suffix("agent-cdx"), Some("codex"));
        assert_eq!(provider_from_channel_suffix("agent-gm"), Some("gemini"));
        assert_eq!(provider_from_channel_suffix("agent"), None);
    }

    #[test]
    fn create_review_dispatch_for_done_card_rejected() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-done", "done");

        for dispatch_type in &["review", "review-decision", "rework"] {
            let result = create_dispatch(
                &db,
                &engine,
                "card-done",
                "agent-1",
                dispatch_type,
                "Should fail",
                &json!({}),
            );
            assert!(
                result.is_err(),
                "{} dispatch should not be created for done card",
                dispatch_type
            );
        }

        // All dispatch types for done cards should be rejected
        let result = create_dispatch(
            &db,
            &engine,
            "card-done",
            "agent-1",
            "implementation",
            "Reopen work",
            &json!({}),
        );
        assert!(
            result.is_err(),
            "implementation dispatch should be rejected for done card"
        );
    }

    #[test]
    fn create_dispatch_core_shares_invariants_with_create_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-core", "ready");

        // create_dispatch_core returns (dispatch_id, old_status, reused)
        let (dispatch_id, old_status, _reused) = create_dispatch_core(
            &db,
            "card-core",
            "agent-1",
            "implementation",
            "Core dispatch",
            &json!({"key": "value"}),
        )
        .unwrap();

        assert_eq!(old_status, "ready");

        let conn = db.separate_conn().unwrap();
        let (card_status, latest_dispatch_id): (String, String) = conn
            .query_row(
                "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-core'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(card_status, "requested");
        assert_eq!(latest_dispatch_id, dispatch_id);

        // Dispatch row exists
        let dispatch = query_dispatch_row(&conn, &dispatch_id).unwrap();
        assert_eq!(dispatch["status"], "pending");
        assert_eq!(dispatch["kanban_card_id"], "card-core");
        drop(conn);

        // create_dispatch delegates to core — verify same invariants
        seed_card(&db, "card-full", "ready");
        let full_dispatch = create_dispatch(
            &db,
            &engine,
            "card-full",
            "agent-1",
            "implementation",
            "Full dispatch",
            &json!({}),
        )
        .unwrap();
        assert_eq!(full_dispatch["status"], "pending");
    }

    #[test]
    fn create_dispatch_core_rejects_done_card() {
        let db = test_db();
        seed_card(&db, "card-done-core", "done");

        let result = create_dispatch_core(
            &db,
            "card-done-core",
            "agent-1",
            "implementation",
            "Should fail",
            &json!({}),
        );
        assert!(result.is_err(), "core should reject done card dispatch");
    }

    #[test]
    fn concurrent_dispatches_for_different_cards_have_distinct_ids() {
        // Regression: concurrent dispatches from different cards must not share
        // dispatch IDs or card state — each must be independently routable.
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-a", "ready");
        seed_card(&db, "card-b", "ready");

        let dispatch_a = create_dispatch(
            &db,
            &engine,
            "card-a",
            "agent-1",
            "implementation",
            "Task A",
            &json!({}),
        )
        .unwrap();

        let dispatch_b = create_dispatch(
            &db,
            &engine,
            "card-b",
            "agent-2",
            "implementation",
            "Task B",
            &json!({}),
        )
        .unwrap();

        let id_a = dispatch_a["id"].as_str().unwrap();
        let id_b = dispatch_b["id"].as_str().unwrap();
        assert_ne!(id_a, id_b, "dispatch IDs must be unique");
        assert_eq!(dispatch_a["kanban_card_id"], "card-a");
        assert_eq!(dispatch_b["kanban_card_id"], "card-b");

        // Each card's latest_dispatch_id points to its own dispatch
        let conn = db.separate_conn().unwrap();
        let latest_a: String = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let latest_b: String = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-b'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(latest_a, id_a);
        assert_eq!(latest_b, id_b);
        assert_ne!(latest_a, latest_b, "card dispatch IDs must not cross");
    }

    #[test]
    fn finalize_dispatch_sets_completion_source() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-fin", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-fin",
            "agent-1",
            "implementation",
            "Finalize test",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        let completed =
            finalize_dispatch(&db, &engine, &dispatch_id, "turn_bridge_explicit", None).unwrap();

        assert_eq!(completed["status"], "completed");
        // result is parsed JSON (query_dispatch_row parses it)
        assert_eq!(
            completed["result"]["completion_source"],
            "turn_bridge_explicit"
        );
    }

    #[test]
    fn finalize_dispatch_merges_context() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-ctx", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-ctx",
            "agent-1",
            "implementation",
            "Context test",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        let completed = finalize_dispatch(
            &db,
            &engine,
            &dispatch_id,
            "session_idle",
            Some(&json!({ "auto_completed": true })),
        )
        .unwrap();

        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["result"]["completion_source"], "session_idle");
        assert_eq!(completed["result"]["auto_completed"], true);
    }

    // ── #173 Dedup tests ─────────────────────────────────────────────

    #[test]
    fn dedup_same_card_same_type_returns_existing_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-dup", "ready");

        let d1 = create_dispatch(
            &db,
            &engine,
            "card-dup",
            "agent-1",
            "implementation",
            "First",
            &json!({}),
        )
        .unwrap();
        let id1 = d1["id"].as_str().unwrap();

        // Second call with same card + same type → should return existing
        let d2 = create_dispatch(
            &db,
            &engine,
            "card-dup",
            "agent-1",
            "implementation",
            "Second",
            &json!({}),
        )
        .unwrap();
        let id2 = d2["id"].as_str().unwrap();

        assert_eq!(id1, id2, "dedup must return existing dispatch_id");
        assert_eq!(d2["status"], "pending");

        // Only 1 row in DB
        let conn = db.separate_conn().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-dup' AND dispatch_type = 'implementation' \
                 AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "only one pending dispatch must exist");
    }

    #[test]
    fn dedup_same_card_different_type_allows_creation() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-diff", "review");

        // Create review dispatch
        let d1 = create_dispatch(
            &db,
            &engine,
            "card-diff",
            "agent-1",
            "review",
            "Review",
            &json!({}),
        )
        .unwrap();

        // Create review-decision for same card → different type, should succeed
        let d2 = create_dispatch(
            &db,
            &engine,
            "card-diff",
            "agent-1",
            "review-decision",
            "Decision",
            &json!({}),
        )
        .unwrap();

        assert_ne!(
            d1["id"].as_str().unwrap(),
            d2["id"].as_str().unwrap(),
            "different types must create distinct dispatches"
        );
    }

    #[test]
    fn dedup_completed_dispatch_allows_new_creation() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-reopen", "ready");

        let d1 = create_dispatch(
            &db,
            &engine,
            "card-reopen",
            "agent-1",
            "implementation",
            "First",
            &json!({}),
        )
        .unwrap();
        let id1 = d1["id"].as_str().unwrap().to_string();

        // Complete the first dispatch
        complete_dispatch(&db, &engine, &id1, &json!({"output": "done"})).unwrap();

        // New dispatch of same type → should succeed (old one is completed)
        let d2 = create_dispatch(
            &db,
            &engine,
            "card-reopen",
            "agent-1",
            "implementation",
            "Second",
            &json!({}),
        )
        .unwrap();

        assert_ne!(
            id1,
            d2["id"].as_str().unwrap(),
            "completed dispatch must not block new creation"
        );
    }

    #[test]
    fn dedup_core_returns_reused_flag() {
        let db = test_db();
        seed_card(&db, "card-flag", "ready");

        let (id1, _, reused1) = create_dispatch_core(
            &db,
            "card-flag",
            "agent-1",
            "implementation",
            "First",
            &json!({}),
        )
        .unwrap();
        assert!(!reused1, "first creation must not be reused");

        let (id2, _, reused2) = create_dispatch_core(
            &db,
            "card-flag",
            "agent-1",
            "implementation",
            "Second",
            &json!({}),
        )
        .unwrap();
        assert!(reused2, "duplicate must be flagged as reused");
        assert_eq!(id1, id2);
    }
}
