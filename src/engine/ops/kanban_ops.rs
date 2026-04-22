use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use sqlx::{PgPool, Postgres, QueryBuilder, Row as SqlxRow};

// ── Kanban ops ────────────────────────────────────────────────────
//
// agentdesk.kanban.setStatus(cardId, newStatus, force?) — updates card status
// and fires appropriate hooks (OnCardTransition, OnCardTerminal, OnReviewEnter).
// This replaces direct SQL UPDATEs in policies to ensure hooks always fire.

pub(super) fn register_kanban_ops<'js>(
    ctx: &Ctx<'js>,
    db: Option<Db>,
    pg_pool: Option<PgPool>,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let kanban_obj = Object::new(ctx.clone())?;

    let db_set = db.clone();
    let pg_set = pg_pool.clone();
    kanban_obj.set(
        "__setStatusRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, new_status: String, force: Option<bool>| -> String {
                if let Some(pool) = pg_set.as_ref() {
                    return set_status_raw_pg(pool, &card_id, &new_status, force.unwrap_or(false));
                }
                let Some(db_set) = db_set.as_ref() else {
                    return r#"{"error":"sqlite backend is unavailable"}"#.to_string();
                };
                let conn = match db_set.separate_conn() {
                    Ok(c) => c,
                    Err(e) => return format!(r#"{{"error":"DB lock: {}"}}"#, e),
                };
                let force = force.unwrap_or(false);

                // Get current status + review round before any terminal cleanup runs.
                let (old_status, old_review_round): (String, Option<i64>) = match conn.query_row(
                    "SELECT status, review_round FROM kanban_cards WHERE id = ?1",
                    [&card_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                ) {
                    Ok(values) => values,
                    Err(_) => return r#"{"error":"card not found"}"#.to_string(),
                };

                if old_status == new_status {
                    return format!(r#"{{"ok":true,"changed":false,"status":"{}"}}"#, new_status);
                }

                // Pipeline-driven guard and clock fields (#106 P5)
                // Resolve effective pipeline for this card (repo + agent overrides)
                crate::pipeline::ensure_loaded();
                let repo_id: Option<String> = conn
                    .query_row("SELECT repo_id FROM kanban_cards WHERE id = ?1", [&card_id], |r| {
                        r.get(0)
                    })
                    .ok()
                    .flatten();
                let agent_id: Option<String> = conn
                    .query_row(
                        "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?1",
                        [&card_id],
                        |r| r.get(0),
                    )
                    .ok()
                    .flatten();
                let effective =
                    crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref());
                let pipeline = &effective;
                let transition_rule = pipeline.find_transition(&old_status, &new_status);

                // Guard: prevent reverting terminal cards
                if pipeline.is_terminal(&old_status) && old_status != new_status && !force {
                    return format!(
                        r#"{{"error":"cannot revert terminal card from {} to {}"}}"#,
                        old_status, new_status
                    );
                }

                // #228: Enforce review_verdict_pass gate on transitions to terminal states.
                // Only this specific gate is checked — other gates (has_active_dispatch,
                // review_rework) are used legitimately by policies and must not be blocked here.
                if pipeline.is_terminal(&new_status)
                    && !force
                    && let Some(t) = transition_rule
                {
                    let needs_review_pass = t.gates.iter().any(|g| {
                        pipeline
                            .gates
                            .get(g.as_str())
                            .is_some_and(|gc| gc.check.as_deref() == Some("review_verdict_pass"))
                    });
                    if needs_review_pass {
                        // Mirror kanban.rs:112-125 — check the LATEST completed review
                        // dispatch only, not any historical pass. A card with pass R1
                        // then rework R2 must not skip the current review round.
                        let latest_verdict: Option<String> = conn
                            .query_row(
                                "SELECT json_extract(result, '$.verdict') FROM task_dispatches \
                                 WHERE kanban_card_id = ?1 AND dispatch_type = 'review' \
                                 AND status = 'completed' \
                                 ORDER BY updated_at DESC LIMIT 1",
                                [&card_id],
                                |row| row.get(0),
                            )
                            .ok()
                            .flatten();
                        let has_pass =
                            matches!(latest_verdict.as_deref(), Some("pass") | Some("approved"));
                        if !has_pass {
                            return format!(
                                r#"{{"error":"gate blocked: review_verdict_pass — no review pass verdict","from":"{}","to":"{}"}}"#,
                                old_status, new_status
                            );
                        }
                    }
                }

                let mut active_dispatch_warning: Option<&'static str> = None;
                if let Some(t) = transition_rule {
                    let needs_active_dispatch = t.gates.iter().any(|g| {
                        pipeline
                            .gates
                            .get(g.as_str())
                            .is_some_and(|gc| gc.check.as_deref() == Some("has_active_dispatch"))
                    });
                    if needs_active_dispatch {
                        let has_active_dispatch: bool = conn
                            .query_row(
                                "SELECT COUNT(*) > 0 FROM task_dispatches \
                                 WHERE kanban_card_id = ?1 AND status IN ('pending', 'dispatched')",
                                [&card_id],
                                |row| row.get(0),
                            )
                            .unwrap_or(false);
                        if !has_active_dispatch {
                            active_dispatch_warning = Some(
                                "transition bypassed has_active_dispatch gate without an active dispatch",
                            );
                        }
                    }
                }

                // Clock fields from pipeline config
                let clock_extra = match pipeline.clock_for_state(&new_status) {
                    Some(clock) if clock.mode.as_deref() == Some("coalesce") => {
                        format!(", {} = COALESCE({}, datetime('now'))", clock.set, clock.set)
                    }
                    Some(clock) => format!(", {} = datetime('now')", clock.set),
                    None => String::new(),
                };
                // Terminal cleanup: clear review-related fields
                let terminal_cleanup = if pipeline.is_terminal(&new_status) {
                    ", review_status = NULL, suggestion_pending_at = NULL, review_entered_at = NULL, awaiting_dod_at = NULL, blocked_reason = NULL, review_round = NULL, deferred_dod_json = NULL"
                } else {
                    ""
                };
                let extra = format!("{clock_extra}{terminal_cleanup}");
                let sql = format!(
                    "UPDATE kanban_cards SET status = ?1, updated_at = datetime('now'){} WHERE id = ?2",
                    extra
                );
                if let Err(e) = conn.execute(&sql, libsql_rusqlite::params![new_status, card_id]) { // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
                    return format!(r#"{{"error":"UPDATE: {}"}}"#, e);
                }

                // Also update auto_queue_entries and cancel orphan dispatches if terminal
                if pipeline.is_terminal(&new_status) {
                    sync_auto_queue_terminal_on_conn(&conn, &card_id);
                    // Cancel active implementation/rework/review-decision dispatches
                    // so they don't remain orphaned after card reaches terminal.
                    let orphan_dispatches: Vec<String> = conn
                        .prepare(
                            "SELECT id FROM task_dispatches \
                             WHERE kanban_card_id = ?1 \
                             AND dispatch_type IN ('implementation', 'review-decision', 'rework') \
                             AND status IN ('pending', 'dispatched')",
                        )
                        .ok()
                        .and_then(|mut stmt| {
                            stmt.query_map([&*card_id], |row| row.get::<_, String>(0))
                                .ok()
                                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                        })
                        .unwrap_or_default();
                    for dispatch_id in &orphan_dispatches {
                        crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
                            &conn,
                            dispatch_id,
                            Some("auto_cancelled_on_terminal_card"),
                        )
                        .ok();
                    }
                }

                // #117/#158: Sync canonical review state via unified entrypoint
                let has_hooks = pipeline
                    .hooks_for_state(&new_status)
                    .is_some_and(|h| !h.on_enter.is_empty() || !h.on_exit.is_empty());
                let is_review_enter = pipeline
                    .hooks_for_state(&new_status)
                    .is_some_and(|h| h.on_enter.iter().any(|n| n == "OnReviewEnter"));
                if pipeline.is_terminal(&new_status) || !has_hooks {
                    let mut payload = serde_json::json!({"card_id": card_id, "state": "idle"});
                    if pipeline.is_terminal(&new_status)
                        && let Some(review_round) = old_review_round.filter(|value| *value > 0)
                    {
                        payload["review_round"] = serde_json::json!(review_round);
                    }
                    review_state_sync_on_conn(
                        &conn,
                        &payload.to_string(),
                    );
                } else if is_review_enter {
                    review_state_sync_on_conn(
                        &conn,
                        &serde_json::json!({"card_id": card_id, "state": "reviewing"}).to_string(),
                    );
                }

                let warning_json = active_dispatch_warning
                    .map(|warning| format!(r#","warning":"{}""#, warning))
                    .unwrap_or_default();
                format!(
                    r#"{{"ok":true,"changed":true,"from":"{}","to":"{}","card_id":"{}"{} }}"#,
                    old_status, new_status, card_id, warning_json
                )
            },
        )?,
    )?;

    let db_reopen = db.clone();
    let pg_reopen = pg_pool.clone();
    kanban_obj.set(
        "__reopenRaw",
        Function::new(ctx.clone(), move |card_id: String, new_status: String| -> String {
            let Some(db_reopen) = db_reopen.as_ref() else {
                return r#"{"error":"sqlite backend is unavailable"}"#.to_string();
            };
            let conn = match db_reopen.separate_conn() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"DB lock: {}"}}"#, e),
            };

            let old_status: String = match conn.query_row(
                "SELECT status FROM kanban_cards WHERE id = ?1",
                [&card_id],
                |row| row.get(0),
            ) {
                Ok(s) => s,
                Err(_) => return r#"{"error":"card not found"}"#.to_string(),
            };

            crate::pipeline::ensure_loaded();
            let repo_id: Option<String> = conn
                .query_row("SELECT repo_id FROM kanban_cards WHERE id = ?1", [&card_id], |r| {
                    r.get(0)
                })
                .ok()
                .flatten();
            let agent_id: Option<String> = conn
                .query_row(
                    "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?1",
                    [&card_id],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            let effective =
                crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref());
            let pipeline = &effective;

            if !pipeline.is_terminal(&old_status) {
                return format!(
                    r#"{{"error":"reopen requires terminal card (current: {})"}}"#,
                    old_status
                );
            }
            if pipeline.is_terminal(&new_status) {
                return format!(
                    r#"{{"error":"reopen target must be non-terminal (target: {})"}}"#,
                    new_status
                );
            }
            if old_status == new_status {
                return format!(r#"{{"ok":true,"changed":false,"status":"{}"}}"#, new_status);
            }

            let clock_extra = match pipeline.clock_for_state(&new_status) {
                Some(clock) if clock.mode.as_deref() == Some("coalesce") => {
                    format!(", {} = COALESCE({}, datetime('now'))", clock.set, clock.set)
                }
                Some(clock) => format!(", {} = datetime('now')", clock.set),
                None => String::new(),
            };

            let sql = format!(
                "UPDATE kanban_cards SET status = ?1, completed_at = NULL, updated_at = datetime('now'){} WHERE id = ?2",
                clock_extra
            );
            if let Err(e) = conn.execute(&sql, libsql_rusqlite::params![new_status, card_id]) { // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
                return format!(r#"{{"error":"UPDATE: {}"}}"#, e);
            }

            let entry_ids: Vec<String> = conn
                .prepare(
                    "SELECT id FROM auto_queue_entries
                     WHERE kanban_card_id = ?1 AND status = 'done'",
                )
                .ok()
                .and_then(|mut stmt| {
                    stmt.query_map([&card_id], |row| row.get::<_, String>(0))
                        .ok()
                        .map(|rows| rows.filter_map(|row| row.ok()).collect())
                })
                .unwrap_or_default();
            for entry_id in entry_ids {
                if let Err(error) = crate::db::auto_queue::update_entry_status_on_conn(
                    &conn,
                    &entry_id,
                    crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
                    "js_reopen",
                    &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                ) {
                    return format!(r#"{{"error":"{}"}}"#, error);
                }
            }

            crate::kanban::correct_tn_to_fn_on_reopen(db_reopen, pg_reopen.as_ref(), &card_id);

            let has_hooks = pipeline
                .hooks_for_state(&new_status)
                .is_some_and(|h| !h.on_enter.is_empty() || !h.on_exit.is_empty());
            let is_review_enter = pipeline
                .hooks_for_state(&new_status)
                .is_some_and(|h| h.on_enter.iter().any(|n| n == "OnReviewEnter"));
            if !has_hooks {
                review_state_sync_on_conn(
                    &conn,
                    &serde_json::json!({"card_id": card_id, "state": "idle"}).to_string(),
                );
            } else if is_review_enter {
                review_state_sync_on_conn(
                    &conn,
                    &serde_json::json!({"card_id": card_id, "state": "reviewing"}).to_string(),
                );
            }

            format!(
                r#"{{"ok":true,"changed":true,"from":"{}","to":"{}","card_id":"{}","reopened":true}}"#,
                old_status, new_status, card_id
            )
        })?,
    )?;

    let db_get = db.clone();
    let pg_get = pg_pool.clone();
    kanban_obj.set(
        "__getCardRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            if let Some(pool) = pg_get.as_ref() {
                return get_card_raw_pg(pool, &card_id);
            }
            let Some(db_get) = db_get.as_ref() else {
                return r#"{"error":"sqlite backend is unavailable"}"#.to_string();
            };
            let conn = match db_get.separate_conn() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
            };
            match conn.query_row(
                "SELECT id, status, assigned_agent_id, title, review_status, review_round, latest_dispatch_id FROM kanban_cards WHERE id = ?1",
                [&card_id],
                |row| {
                    Ok(serde_json::json!({
                        "id": row.get::<_, String>(0)?,
                        "status": row.get::<_, String>(1)?,
                        "assigned_agent_id": row.get::<_, Option<String>>(2)?,
                        "title": row.get::<_, Option<String>>(3)?,
                        "review_status": row.get::<_, Option<String>>(4)?,
                        "review_round": row.get::<_, Option<i64>>(5)?,
                        "latest_dispatch_id": row.get::<_, Option<String>>(6)?,
                    }))
                },
            ) {
                Ok(card) => card.to_string(),
                Err(_) => r#"{"error":"card not found"}"#.to_string(),
            }
        })?,
    )?;

    let db_clear_latest = db.clone();
    let pg_clear_latest = pg_pool.clone();
    kanban_obj.set(
        "__clearLatestDispatchRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, expected_dispatch_id: Option<String>| -> String {
                if let Some(pool) = pg_clear_latest.as_ref() {
                    return clear_latest_dispatch_raw_pg(
                        pool,
                        &card_id,
                        expected_dispatch_id.as_deref(),
                    );
                }
                let Some(db_clear_latest) = db_clear_latest.as_ref() else {
                    return r#"{"error":"sqlite backend is unavailable"}"#.to_string();
                };
                let conn = match db_clear_latest.separate_conn() {
                    Ok(c) => c,
                    Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
                };
                let current_latest: Option<String> = conn
                    .query_row(
                        "SELECT latest_dispatch_id FROM kanban_cards WHERE id = ?1",
                        [&card_id],
                        |row| row.get(0),
                    )
                    .ok()
                    .flatten();
                if let Some(expected) = expected_dispatch_id.as_deref() {
                    if current_latest.as_deref() != Some(expected) {
                        return r#"{"ok":true,"rows_affected":0,"skipped":"latest_mismatch"}"#.to_string();
                    }
                }
                match conn.execute(
                    "UPDATE kanban_cards SET latest_dispatch_id = NULL, updated_at = datetime('now') \
                     WHERE id = ?1 AND latest_dispatch_id IS NOT NULL",
                    [&card_id],
                ) {
                    Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
                    Err(e) => format!(r#"{{"error":"UPDATE: {}"}}"#, e),
                }
            },
        )?,
    )?;

    // #155: setReviewStatus — controlled path for review_status + clock updates.
    // Replaces direct SQL UPDATEs so the ExecuteSQL guard can block bare review_status writes.
    let db_review = db.clone();
    let pg_review = pg_pool.clone();
    kanban_obj.set(
        "__setReviewStatusRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, opts_json: String| -> String {
                if let Some(pool) = pg_review.as_ref() {
                    return set_review_status_raw_pg(pool, &card_id, &opts_json);
                }
                let opts: serde_json::Value = match serde_json::from_str(&opts_json) {
                    Ok(v) => v,
                    Err(e) => return format!(r#"{{"error":"bad opts: {}"}}"#, e),
                };
                let Some(db_review) = db_review.as_ref() else {
                    return r#"{"error":"sqlite backend is unavailable"}"#.to_string();
                };
                let conn = match db_review.separate_conn() {
                    Ok(c) => c,
                    Err(e) => return format!(r#"{{"error":"DB: {}"}}"#, e),
                };

                // Build dynamic SET clause
                let mut sets = vec!["updated_at = datetime('now')".to_string()];
                let mut params: Vec<Box<dyn libsql_rusqlite::types::ToSql>> = vec![]; // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.

                if let Some(rs) = opts.get("review_status") {
                    if rs.is_null() {
                        sets.push("review_status = NULL".to_string());
                    } else if let Some(s) = rs.as_str() {
                        params.push(Box::new(s.to_string()));
                        sets.push(format!("review_status = ?{}", params.len()));
                    }
                }
                if let Some(v) = opts.get("suggestion_pending_at") {
                    if v.is_null() {
                        sets.push("suggestion_pending_at = NULL".to_string());
                    } else if v.as_str() == Some("now") {
                        sets.push("suggestion_pending_at = datetime('now')".to_string());
                    }
                }
                if let Some(v) = opts.get("review_entered_at") {
                    if v.is_null() {
                        sets.push("review_entered_at = NULL".to_string());
                    } else if v.as_str() == Some("now") {
                        sets.push("review_entered_at = datetime('now')".to_string());
                    }
                }
                if let Some(v) = opts.get("awaiting_dod_at") {
                    if v.is_null() {
                        sets.push("awaiting_dod_at = NULL".to_string());
                    } else if v.as_str() == Some("now") {
                        sets.push("awaiting_dod_at = datetime('now')".to_string());
                    }
                }
                if let Some(v) = opts.get("blocked_reason") {
                    if v.is_null() {
                        sets.push("blocked_reason = NULL".to_string());
                    } else if let Some(s) = v.as_str() {
                        params.push(Box::new(s.to_string()));
                        sets.push(format!("blocked_reason = ?{}", params.len()));
                    }
                }

                // Optional terminal guard: only update if status != terminal
                let where_clause = if let Some(excl) = opts.get("exclude_status") {
                    if let Some(s) = excl.as_str() {
                        params.push(Box::new(s.to_string()));
                        params.push(Box::new(card_id.clone()));
                        format!(
                            "WHERE id = ?{} AND status != ?{}",
                            params.len(),
                            params.len() - 1
                        )
                    } else {
                        params.push(Box::new(card_id.clone()));
                        format!("WHERE id = ?{}", params.len())
                    }
                } else {
                    params.push(Box::new(card_id.clone()));
                    format!("WHERE id = ?{}", params.len())
                };

                let sql = format!(
                    "UPDATE kanban_cards SET {} {}",
                    sets.join(", "),
                    where_clause
                );
                let param_refs: Vec<&dyn libsql_rusqlite::types::ToSql> = // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
                    params.iter().map(|p| p.as_ref()).collect();
                if let Err(e) = conn.execute(&sql, param_refs.as_slice()) {
                    return format!(r#"{{"error":"UPDATE: {}"}}"#, e);
                }

                // #117/#158: Sync card_review_state via unified entrypoint
                if let Some(rs) = opts.get("review_status") {
                    let review_state = if rs.is_null() {
                        Some("idle")
                    } else {
                        rs.as_str()
                    };
                    if let Some(s) = review_state {
                        review_state_sync_on_conn(
                            &conn,
                            &serde_json::json!({"card_id": card_id, "state": s}).to_string(),
                        );
                    }
                }

                r#"{"ok":true}"#.to_string()
            },
        )?,
    )?;

    ad.set("kanban", kanban_obj)?;

    // JS wrapper that parses JSON and accumulates transitions for post-hook processing.
    // setStatus only updates the DB — transition hooks (OnCardTransition, OnReviewEnter,
    // OnCardTerminal) cannot fire from within a hook because the engine is not reentrant.
    // Instead, transitions are collected in __pendingTransitions and the Rust caller
    // processes them after the hook returns via drain_pending_transitions().
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            agentdesk.kanban.__pendingTransitions = [];
            agentdesk.kanban.setStatus = function(cardId, newStatus, force) {
                var result = JSON.parse(
                    agentdesk.kanban.__setStatusRaw(cardId, newStatus, !!force)
                );
                if (result.error) throw new Error(result.error);
                if (result.changed) {
                    agentdesk.kanban.__pendingTransitions.push({
                        card_id: result.card_id,
                        from: result.from,
                        to: result.to
                    });
                    if (result.warning) {
                        agentdesk.log.warn("[setStatus] " + result.card_id + " " + result.from + " -> " + result.to + " — " + result.warning);
                    }
                    agentdesk.log.info("[setStatus] " + result.card_id + " " + result.from + " -> " + result.to + " (pendingLen=" + agentdesk.kanban.__pendingTransitions.length + ")");
                } else {
                    agentdesk.log.info("[setStatus] " + cardId + " -> " + newStatus + " (no-change)");
                }
                return result;
            };
            agentdesk.kanban.reopen = function(cardId, newStatus) {
                var result = JSON.parse(agentdesk.kanban.__reopenRaw(cardId, newStatus));
                if (result.error) throw new Error(result.error);
                if (result.changed) {
                    agentdesk.kanban.__pendingTransitions.push({
                        card_id: result.card_id,
                        from: result.from,
                        to: result.to
                    });
                    agentdesk.log.info("[reopen] " + result.card_id + " " + result.from + " -> " + result.to + " (pendingLen=" + agentdesk.kanban.__pendingTransitions.length + ")");
                } else {
                    agentdesk.log.info("[reopen] " + cardId + " -> " + newStatus + " (no-change)");
                }
                return result;
            };
            agentdesk.kanban.getCard = function(cardId) {
                var result = JSON.parse(agentdesk.kanban.__getCardRaw(cardId));
                if (result.error) return null;
                return result;
            };
            agentdesk.kanban.clearLatestDispatch = function(cardId, expectedDispatchId) {
                var result = JSON.parse(
                    agentdesk.kanban.__clearLatestDispatchRaw(cardId, expectedDispatchId || null)
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.kanban.setReviewStatus = function(cardId, reviewStatus, opts) {
                var o = opts || {};
                o.review_status = reviewStatus;
                var result = JSON.parse(
                    agentdesk.kanban.__setReviewStatusRaw(cardId, JSON.stringify(o))
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
        })();
    "#,
    )?;

    Ok(())
}

fn set_status_raw_pg(pool: &PgPool, card_id: &str, new_status: &str, force: bool) -> String {
    let card_id = card_id.to_string();
    let new_status = new_status.to_string();
    match run_async_bridge_pg(pool, move |pool| async move {
        let mut tx = pool
            .begin()
            .await
            .map_err(|error| format!("open postgres kanban status transaction: {error}"))?;

        let row = sqlx::query(
            "SELECT status, title, metadata::text AS metadata, latest_dispatch_id, repo_id, assigned_agent_id, review_round
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(&card_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| format!("load postgres kanban card {card_id}: {error}"))?
        .ok_or_else(|| "card not found".to_string())?;

        let old_status: String = row
            .try_get("status")
            .map_err(|error| format!("decode old status for {card_id}: {error}"))?;
        let title: String = row
            .try_get("title")
            .map_err(|error| format!("decode title for {card_id}: {error}"))?;
        let metadata: Option<String> = row
            .try_get("metadata")
            .map_err(|error| format!("decode metadata for {card_id}: {error}"))?;
        let latest_dispatch_id: Option<String> = row
            .try_get("latest_dispatch_id")
            .map_err(|error| format!("decode latest_dispatch_id for {card_id}: {error}"))?;
        let repo_id: Option<String> = row
            .try_get("repo_id")
            .map_err(|error| format!("decode repo_id for {card_id}: {error}"))?;
        let assigned_agent_id: Option<String> = row
            .try_get("assigned_agent_id")
            .map_err(|error| format!("decode assigned_agent_id for {card_id}: {error}"))?;
        let old_review_round: Option<i64> = row
            .try_get("review_round")
            .map_err(|error| format!("decode review_round for {card_id}: {error}"))?;

        if old_status == new_status {
            return Ok(serde_json::json!({
                "ok": true,
                "changed": false,
                "status": new_status,
            }));
        }

        let latest_dispatch_status = if let Some(dispatch_id) = latest_dispatch_id.as_deref() {
            sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
                .bind(dispatch_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|error| format!("load latest dispatch status for {card_id}: {error}"))?
        } else {
            None
        };
        let effective =
            resolve_pipeline_with_pg(&pool, repo_id.as_deref(), assigned_agent_id.as_deref())
                .await?;
        let transition_rule = effective.find_transition(&old_status, &new_status);

        if effective.is_terminal(&old_status) && old_status != new_status && !force {
            return Err(format!(
                "cannot revert terminal card from {old_status} to {new_status}"
            ));
        }

        if effective.is_terminal(&new_status)
            && !force
            && let Some(t) = transition_rule
        {
            let needs_review_pass = t.gates.iter().any(|g| {
                effective
                    .gates
                    .get(g.as_str())
                    .is_some_and(|gc| gc.check.as_deref() == Some("review_verdict_pass"))
            });
            if needs_review_pass {
                let latest_verdict = sqlx::query_scalar::<_, String>(
                    "SELECT result ->> 'verdict'
                     FROM task_dispatches
                     WHERE kanban_card_id = $1
                       AND dispatch_type = 'review'
                       AND status = 'completed'
                     ORDER BY updated_at DESC
                     LIMIT 1",
                )
                .bind(&card_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|error| format!("load latest review verdict for {card_id}: {error}"))?;
                let has_pass = matches!(latest_verdict.as_deref(), Some("pass") | Some("approved"));
                if !has_pass {
                    return Err(format!(
                        "gate blocked: review_verdict_pass — no review pass verdict (from {old_status} to {new_status})"
                    ));
                }
            }
        }

        let mut active_dispatch_warning: Option<&'static str> = None;
        if let Some(t) = transition_rule {
            let needs_active_dispatch = t.gates.iter().any(|g| {
                effective
                    .gates
                    .get(g.as_str())
                    .is_some_and(|gc| gc.check.as_deref() == Some("has_active_dispatch"))
            });
            if needs_active_dispatch {
                let has_active_dispatch = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*)
                     FROM task_dispatches
                     WHERE kanban_card_id = $1
                       AND status IN ('pending', 'dispatched')",
                )
                .bind(&card_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| format!("load active dispatch count for {card_id}: {error}"))?
                    > 0;
                if !has_active_dispatch {
                    active_dispatch_warning = Some(
                        "transition bypassed has_active_dispatch gate without an active dispatch",
                    );
                }
            }
        }

        let clock_extra = match effective.clock_for_state(&new_status) {
            Some(clock) if clock.mode.as_deref() == Some("coalesce") => {
                format!(", {} = COALESCE({}, NOW())", clock.set, clock.set)
            }
            Some(clock) => format!(", {} = NOW()", clock.set),
            None => String::new(),
        };
        let terminal_cleanup = if effective.is_terminal(&new_status) {
            ", review_status = NULL, suggestion_pending_at = NULL, review_entered_at = NULL, awaiting_dod_at = NULL, blocked_reason = NULL, review_round = NULL, deferred_dod_json = NULL"
        } else {
            ""
        };
        let sql = format!(
            "UPDATE kanban_cards SET status = $1, updated_at = NOW(){}{} WHERE id = $2",
            clock_extra, terminal_cleanup
        );
        sqlx::query(&sql)
            .bind(&new_status)
            .bind(&card_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("update kanban card {card_id} status: {error}"))?;

        if effective.is_terminal(&new_status) {
            crate::github::sync::sync_auto_queue_terminal_on_pg(&mut tx, &card_id).await?;

            sqlx::query(
                "UPDATE task_dispatches
                 SET status = 'cancelled',
                     updated_at = NOW(),
                     completed_at = COALESCE(completed_at, NOW())
                 WHERE kanban_card_id = $1
                   AND dispatch_type IN ('implementation', 'review-decision', 'rework')
                   AND status IN ('pending', 'dispatched')",
            )
            .bind(&card_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!("cancel orphan dispatches for terminal card {card_id}: {error}")
            })?;
        }

        let has_hooks = effective
            .hooks_for_state(&new_status)
            .is_some_and(|h| !h.on_enter.is_empty() || !h.on_exit.is_empty());
        let is_review_enter = effective
            .hooks_for_state(&new_status)
            .is_some_and(|h| h.on_enter.iter().any(|n| n == "OnReviewEnter"));
        if effective.is_terminal(&new_status) || !has_hooks {
            crate::github::sync::sync_review_state_on_pg(&mut tx, &card_id, "idle").await?;
        } else if is_review_enter {
            crate::github::sync::sync_review_state_on_pg(&mut tx, &card_id, "reviewing").await?;
        }

        tx.commit().await.map_err(|error| {
            format!("commit postgres kanban status update for {card_id}: {error}")
        })?;

        let mut response = serde_json::json!({
            "ok": true,
            "changed": true,
            "from": old_status,
            "to": new_status,
            "card_id": card_id,
        });
        if let Some(warning) = active_dispatch_warning {
            response["warning"] = serde_json::json!(warning);
        }
        let _ = metadata;
        let _ = latest_dispatch_status;
        let _ = old_review_round;
        let _ = title;
        Ok(response)
    }) {
        Ok(response) => response.to_string(),
        Err(error) => serde_json::json!({ "error": error }).to_string(),
    }
}

fn get_card_raw_pg(pool: &PgPool, card_id: &str) -> String {
    let card_id = card_id.to_string();
    match run_async_bridge_pg(pool, move |pool| async move {
        let row = sqlx::query(
            "SELECT id, status, assigned_agent_id, title, review_status, review_round, latest_dispatch_id
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(&card_id)
        .fetch_optional(&pool)
        .await
        .map_err(|error| format!("load postgres kanban card {card_id}: {error}"))?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(serde_json::json!({
            "id": row.try_get::<String, _>("id").map_err(|error| format!("decode id for {card_id}: {error}"))?,
            "status": row.try_get::<String, _>("status").map_err(|error| format!("decode status for {card_id}: {error}"))?,
            "assigned_agent_id": row.try_get::<Option<String>, _>("assigned_agent_id").map_err(|error| format!("decode assigned_agent_id for {card_id}: {error}"))?,
            "title": row.try_get::<Option<String>, _>("title").map_err(|error| format!("decode title for {card_id}: {error}"))?,
            "review_status": row.try_get::<Option<String>, _>("review_status").map_err(|error| format!("decode review_status for {card_id}: {error}"))?,
            "review_round": row.try_get::<Option<i64>, _>("review_round").map_err(|error| format!("decode review_round for {card_id}: {error}"))?,
            "latest_dispatch_id": row.try_get::<Option<String>, _>("latest_dispatch_id").map_err(|error| format!("decode latest_dispatch_id for {card_id}: {error}"))?,
        })))
    }) {
        Ok(Some(card)) => card.to_string(),
        Ok(None) => r#"{"error":"card not found"}"#.to_string(),
        Err(error) => serde_json::json!({ "error": error }).to_string(),
    }
}

fn clear_latest_dispatch_raw_pg(
    pool: &PgPool,
    card_id: &str,
    expected_dispatch_id: Option<&str>,
) -> String {
    let card_id = card_id.to_string();
    let expected_dispatch_id = expected_dispatch_id.map(str::to_string);
    match run_async_bridge_pg(pool, move |pool| async move {
        let current_latest = sqlx::query_scalar::<_, Option<String>>(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = $1",
        )
        .bind(&card_id)
        .fetch_optional(&pool)
        .await
        .map_err(|error| format!("load latest dispatch for {card_id}: {error}"))?
        .flatten();
        if let Some(expected) = expected_dispatch_id.as_deref()
            && current_latest.as_deref() != Some(expected)
        {
            return Ok(serde_json::json!({
                "ok": true,
                "rows_affected": 0,
                "skipped": "latest_mismatch",
            }));
        }

        let rows_affected = sqlx::query(
            "UPDATE kanban_cards
             SET latest_dispatch_id = NULL,
                 updated_at = NOW()
             WHERE id = $1
               AND latest_dispatch_id IS NOT NULL",
        )
        .bind(&card_id)
        .execute(&pool)
        .await
        .map_err(|error| format!("clear latest dispatch for {card_id}: {error}"))?
        .rows_affected();
        Ok(serde_json::json!({
            "ok": true,
            "rows_affected": rows_affected,
        }))
    }) {
        Ok(response) => response.to_string(),
        Err(error) => serde_json::json!({ "error": error }).to_string(),
    }
}

fn set_review_status_raw_pg(pool: &PgPool, card_id: &str, opts_json: &str) -> String {
    let card_id = card_id.to_string();
    let opts: serde_json::Value = match serde_json::from_str(opts_json) {
        Ok(value) => value,
        Err(error) => return format!(r#"{{"error":"bad opts: {}"}}"#, error),
    };

    match run_async_bridge_pg(pool, move |pool| async move {
        let mut tx = pool
            .begin()
            .await
            .map_err(|error| format!("open postgres review status transaction: {error}"))?;

        let mut query = QueryBuilder::<Postgres>::new("UPDATE kanban_cards SET updated_at = NOW()");
        if let Some(review_status) = opts.get("review_status") {
            if review_status.is_null() {
                query.push(", review_status = NULL");
            } else if let Some(status) = review_status.as_str() {
                query.push(", review_status = ");
                query.push_bind(status.to_string());
            }
        }
        if let Some(value) = opts.get("suggestion_pending_at") {
            if value.is_null() {
                query.push(", suggestion_pending_at = NULL");
            } else if value.as_str() == Some("now") {
                query.push(", suggestion_pending_at = NOW()");
            }
        }
        if let Some(value) = opts.get("review_entered_at") {
            if value.is_null() {
                query.push(", review_entered_at = NULL");
            } else if value.as_str() == Some("now") {
                query.push(", review_entered_at = NOW()");
            }
        }
        if let Some(value) = opts.get("awaiting_dod_at") {
            if value.is_null() {
                query.push(", awaiting_dod_at = NULL");
            } else if value.as_str() == Some("now") {
                query.push(", awaiting_dod_at = NOW()");
            }
        }
        if let Some(value) = opts.get("blocked_reason") {
            if value.is_null() {
                query.push(", blocked_reason = NULL");
            } else if let Some(reason) = value.as_str() {
                query.push(", blocked_reason = ");
                query.push_bind(reason.to_string());
            }
        }

        query.push(" WHERE id = ");
        query.push_bind(card_id.clone());
        if let Some(exclude_status) = opts.get("exclude_status").and_then(|value| value.as_str()) {
            query.push(" AND status != ");
            query.push_bind(exclude_status.to_string());
        }
        query
            .build()
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("update review status for {card_id}: {error}"))?;

        if let Some(review_status) = opts.get("review_status") {
            let review_state = if review_status.is_null() {
                Some("idle")
            } else {
                review_status.as_str()
            };
            if let Some(review_state) = review_state {
                crate::github::sync::sync_review_state_on_pg(&mut tx, &card_id, review_state)
                    .await?;
            }
        }

        tx.commit().await.map_err(|error| {
            format!("commit postgres review status update for {card_id}: {error}")
        })?;
        Ok(serde_json::json!({ "ok": true }))
    }) {
        Ok(response) => response.to_string(),
        Err(error) => serde_json::json!({ "error": error }).to_string(),
    }
}

async fn resolve_pipeline_with_pg(
    pool: &PgPool,
    repo_id: Option<&str>,
    agent_id: Option<&str>,
) -> Result<crate::pipeline::PipelineConfig, String> {
    crate::pipeline::ensure_loaded();

    let repo_override = if let Some(repo_id) = repo_id {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config FROM github_repos WHERE id = $1",
        )
        .bind(repo_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load repo pipeline override for {repo_id}: {error}"))?
        .flatten()
        .map(|json| crate::pipeline::parse_override(&json))
        .transpose()
        .map_err(|error| format!("parse repo pipeline override for {repo_id}: {error}"))?
        .flatten()
    } else {
        None
    };
    let agent_override = if let Some(agent_id) = agent_id {
        sqlx::query_scalar::<_, Option<String>>("SELECT pipeline_config FROM agents WHERE id = $1")
            .bind(agent_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| format!("load agent pipeline override for {agent_id}: {error}"))?
            .flatten()
            .map(|json| crate::pipeline::parse_override(&json))
            .transpose()
            .map_err(|error| format!("parse agent pipeline override for {agent_id}: {error}"))?
            .flatten()
    } else {
        None
    };

    Ok(crate::pipeline::resolve(
        repo_override.as_ref(),
        agent_override.as_ref(),
    ))
}

fn run_async_bridge_pg<F, T>(
    pool: &PgPool,
    future_factory: impl FnOnce(PgPool) -> F + Send + 'static,
) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    crate::utils::async_bridge::block_on_pg_result(pool, future_factory, |error| error)
}

/// Rust implementation of card_review_state sync (#158).
/// Single entrypoint for all review-state mutations.
/// Used by both the JS bridge and Rust route handlers.
pub(super) fn review_state_sync(db: &Db, json_str: &str) -> String {
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"db error: {}"}}"#, e),
    };
    review_state_sync_on_conn(&conn, json_str)
}

pub(super) fn review_state_sync_with_backends(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    json_str: &str,
) -> String {
    if let Some(pool) = pg_pool {
        return review_state_sync_pg(pool, json_str);
    }
    let Some(db) = db else {
        return r#"{"error":"sqlite backend is unavailable"}"#.to_string();
    };
    review_state_sync(db, json_str)
}

/// Best-effort auto-queue cleanup for terminal cards.
///
/// When a card finishes, its active dispatch entry should become `done` and any
/// stale pending copies in active or paused runs should be skipped so they do
/// not block other runs.
pub(super) fn sync_auto_queue_terminal_on_conn(conn: &libsql_rusqlite::Connection, card_id: &str) {
    // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
    let dispatched_ids: Vec<String> = conn
        .prepare(
            "SELECT id FROM auto_queue_entries
             WHERE kanban_card_id = ?1 AND status = 'dispatched'",
        )
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([card_id], |row| row.get::<_, String>(0))
                .ok()
                .map(|rows| rows.filter_map(|row| row.ok()).collect())
        })
        .unwrap_or_default();
    for entry_id in dispatched_ids {
        if let Err(error) = crate::db::auto_queue::update_entry_status_on_conn(
            conn,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_DONE,
            "card_terminal",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        ) {
            tracing::warn!(
                "[auto-queue] failed to mark entry {} done during terminal sync: {}",
                entry_id,
                error
            );
        }
    }

    let pending_ids: Vec<String> = conn
        .prepare(
            "SELECT id FROM auto_queue_entries
             WHERE kanban_card_id = ?1
               AND status = 'pending'
               AND run_id IN (
                   SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
               )",
        )
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([card_id], |row| row.get::<_, String>(0))
                .ok()
                .map(|rows| rows.filter_map(|row| row.ok()).collect())
        })
        .unwrap_or_default();
    for entry_id in pending_ids {
        if let Err(error) = crate::db::auto_queue::update_entry_status_on_conn(
            conn,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
            "card_terminal_pending_cleanup",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        ) {
            tracing::warn!(
                "[auto-queue] failed to skip pending entry {} during terminal sync: {}",
                entry_id,
                error
            );
        }
    }
}

/// Skip live auto-queue entries for a card after PMD explicitly backs the card out.
///
/// Only active/paused runs are touched. Generated or future runs stay intact so
/// PMD can intentionally re-queue the card later after fixing prerequisites.
pub(super) fn skip_live_auto_queue_entries_for_card_on_conn(
    conn: &libsql_rusqlite::Connection, // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
    card_id: &str,
) -> libsql_rusqlite::Result<usize> {
    // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
    let mut stmt = conn.prepare(
        "SELECT id FROM auto_queue_entries
         WHERE kanban_card_id = ?1
           AND status IN ('pending', 'dispatched')
           AND run_id IN (SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused'))",
    )?;
    let entry_ids: Vec<String> = stmt
        .query_map([card_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);

    let mut changed = 0usize;
    for entry_id in entry_ids {
        if crate::db::auto_queue::update_entry_status_on_conn(
            conn,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
            "force_transition_cleanup",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        )
        .map_err(|error| match error {
            crate::db::auto_queue::EntryStatusUpdateError::Sql(sql) => sql,
            other => libsql_rusqlite::Error::ToSqlConversionFailure(Box::new(
                // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
                std::io::Error::other(other.to_string()),
            )),
        })?
        .changed
        {
            changed += 1;
        }
    }

    Ok(changed)
}

/// Same as `review_state_sync` but operates on an already-acquired connection.
/// Use this inside transactions or when a lock is already held (#158).
pub(super) fn review_state_sync_on_conn(
    conn: &libsql_rusqlite::Connection, // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
    json_str: &str,
) -> String {
    let params: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => return format!(r#"{{"error":"invalid JSON: {}"}}"#, e),
    };

    let card_id = params["card_id"].as_str().unwrap_or("");
    let state = params["state"].as_str().unwrap_or("");
    if card_id.is_empty() || state.is_empty() {
        return r#"{"error":"card_id and state are required"}"#.to_string();
    }

    // Special case: clear_verdict only NULLs last_verdict without changing state
    if state == "clear_verdict" {
        let result = conn.execute(
            "UPDATE card_review_state SET last_verdict = NULL, updated_at = datetime('now') WHERE card_id = ?1",
            libsql_rusqlite::params![card_id], // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
        );
        return match result {
            Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
            Err(e) => format!(r#"{{"error":"sql error: {}"}}"#, e),
        };
    }

    // Build dynamic SET clause based on provided fields
    let review_round = params["review_round"].as_i64();
    let last_verdict = params["last_verdict"].as_str();
    let last_decision = params["last_decision"].as_str();
    let pending_dispatch_id = params["pending_dispatch_id"].as_str();
    let approach_change_round = params["approach_change_round"].as_i64();
    let session_reset_round = params["session_reset_round"].as_i64();
    let review_entered_at = params["review_entered_at"].as_str();

    // UPSERT: INSERT OR REPLACE with all fields
    let result = conn.execute(
        "INSERT INTO card_review_state (card_id, state, review_round, last_verdict, last_decision, pending_dispatch_id, approach_change_round, session_reset_round, review_entered_at, updated_at) \
         VALUES (?1, ?2, COALESCE(?3, (SELECT COALESCE(review_round, 0) FROM kanban_cards WHERE id = ?1), 0), ?4, ?5, ?6, ?7, ?8, COALESCE(?9, CASE WHEN ?2 = 'reviewing' THEN datetime('now') ELSE NULL END), datetime('now')) \
         ON CONFLICT(card_id) DO UPDATE SET \
         state = ?2, \
         review_round = COALESCE(?3, review_round), \
         last_verdict = COALESCE(?4, last_verdict), \
         last_decision = COALESCE(?5, last_decision), \
         pending_dispatch_id = CASE \
             WHEN ?6 IS NOT NULL THEN ?6 \
             WHEN ?2 = 'suggestion_pending' THEN pending_dispatch_id \
             ELSE NULL \
         END, \
         approach_change_round = COALESCE(?7, approach_change_round), \
         session_reset_round = COALESCE(?8, session_reset_round), \
         review_entered_at = COALESCE(?9, CASE WHEN ?2 = 'reviewing' THEN datetime('now') ELSE review_entered_at END), \
         updated_at = datetime('now')",
        libsql_rusqlite::params![ // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
            card_id,
            state,
            review_round,
            last_verdict,
            last_decision,
            pending_dispatch_id,
            approach_change_round,
            session_reset_round,
            review_entered_at,
        ],
    );

    match result {
        Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
        Err(e) => format!(r#"{{"error":"sql error: {}"}}"#, e),
    }
}

fn review_state_sync_pg(pool: &PgPool, json_str: &str) -> String {
    let params: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => return format!(r#"{{"error":"invalid JSON: {}"}}"#, e),
    };

    let card_id = params["card_id"].as_str().unwrap_or("");
    let state = params["state"].as_str().unwrap_or("");
    if card_id.is_empty() || state.is_empty() {
        return r#"{"error":"card_id and state are required"}"#.to_string();
    }

    let card_id = card_id.to_string();
    let state = state.to_string();
    let review_round = params["review_round"].as_i64();
    let last_verdict = params["last_verdict"].as_str().map(str::to_string);
    let last_decision = params["last_decision"].as_str().map(str::to_string);
    let pending_dispatch_id = params["pending_dispatch_id"].as_str().map(str::to_string);
    let approach_change_round = params["approach_change_round"].as_i64();
    let session_reset_round = params["session_reset_round"].as_i64();
    let review_entered_at = params["review_entered_at"].as_str().map(str::to_string);

    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            if state == "clear_verdict" {
                let rows_affected = sqlx::query(
                    "UPDATE card_review_state
                     SET last_verdict = NULL,
                         updated_at = NOW()
                     WHERE card_id = $1",
                )
                .bind(&card_id)
                .execute(&bridge_pool)
                .await
                .map_err(|error| format!("clear postgres review verdict for {card_id}: {error}"))?
                .rows_affected();
                return Ok(format!(r#"{{"ok":true,"rows_affected":{rows_affected}}}"#));
            }

            let rows_affected = sqlx::query(
                "INSERT INTO card_review_state (
                    card_id,
                    state,
                    review_round,
                    last_verdict,
                    last_decision,
                    pending_dispatch_id,
                    approach_change_round,
                    session_reset_round,
                    review_entered_at,
                    updated_at
                 ) VALUES (
                    $1,
                    $2,
                    COALESCE(
                        $3,
                        (SELECT COALESCE(review_round, 0)::BIGINT FROM kanban_cards WHERE id = $1),
                        0
                    ),
                    $4,
                    $5,
                    $6,
                    $7,
                    $8,
                    COALESCE($9, CASE WHEN $2 = 'reviewing' THEN NOW()::TEXT ELSE NULL END),
                    NOW()
                 )
                 ON CONFLICT(card_id) DO UPDATE SET
                    state = EXCLUDED.state,
                    review_round = COALESCE(EXCLUDED.review_round, card_review_state.review_round),
                    last_verdict = COALESCE(EXCLUDED.last_verdict, card_review_state.last_verdict),
                    last_decision = COALESCE(EXCLUDED.last_decision, card_review_state.last_decision),
                    pending_dispatch_id = CASE
                        WHEN EXCLUDED.pending_dispatch_id IS NOT NULL THEN EXCLUDED.pending_dispatch_id
                        WHEN EXCLUDED.state = 'suggestion_pending' THEN card_review_state.pending_dispatch_id
                        ELSE NULL
                    END,
                    approach_change_round = COALESCE(
                        EXCLUDED.approach_change_round,
                        card_review_state.approach_change_round
                    ),
                    session_reset_round = COALESCE(
                        EXCLUDED.session_reset_round,
                        card_review_state.session_reset_round
                    ),
                    review_entered_at = COALESCE(
                        EXCLUDED.review_entered_at,
                        CASE
                            WHEN EXCLUDED.state = 'reviewing' THEN NOW()::TEXT
                            ELSE card_review_state.review_entered_at
                        END
                    ),
                    updated_at = NOW()",
            )
            .bind(&card_id)
            .bind(&state)
            .bind(review_round)
            .bind(last_verdict)
            .bind(last_decision)
            .bind(pending_dispatch_id)
            .bind(approach_change_round)
            .bind(session_reset_round)
            .bind(review_entered_at)
            .execute(&bridge_pool)
            .await
            .map_err(|error| format!("upsert postgres review state for {card_id}: {error}"))?
            .rows_affected();
            Ok(format!(r#"{{"ok":true,"rows_affected":{rows_affected}}}"#))
        },
        |error| format!(r#"{{"error":"{}"}}"#, error),
    );

    match result {
        Ok(value) => value,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}
