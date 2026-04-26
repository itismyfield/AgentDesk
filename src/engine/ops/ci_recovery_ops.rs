use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde_json::json;
use sqlx::{PgPool, Row as SqlxRow};

// ── CI-recovery typed facade (#1007) ─────────────────────────────────
//
// Replaces raw `agentdesk.db.query/execute` mutations in policies/ci-recovery.js
// with domain-intent entrypoints.
//
//   agentdesk.ciRecovery.setBlockedReason(cardId, reason | null)
//     → UPDATE kanban_cards SET blocked_reason = ? WHERE id = ?
//
//   agentdesk.ciRecovery.getCardStatus(cardId) → { status: "…" } | null
//     → SELECT status FROM kanban_cards WHERE id = ?
//
//   agentdesk.ciRecovery.getReworkCardInfo(cardId)
//     → { assigned_agent_id, title, github_issue_number } | null
//     → SELECT assigned_agent_id, title, github_issue_number FROM kanban_cards WHERE id = ?
//
//   agentdesk.ciRecovery.listWaitingForCi() → [{ id, blocked_reason }, …]
//     → SELECT p.card_id, c.blocked_reason FROM pr_tracking p
//       JOIN kanban_cards c ON c.id = p.card_id WHERE p.state = 'wait-ci'

pub(super) fn register_ci_recovery_ops<'js>(
    ctx: &Ctx<'js>,
    db: Option<Db>,
    pg_pool: Option<PgPool>,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let obj = Object::new(ctx.clone())?;

    // setBlockedReason
    let db_sbr = db.clone();
    let pg_sbr = pg_pool.clone();
    obj.set(
        "__setBlockedReasonRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, reason: Option<String>| -> String {
                set_blocked_reason_raw(
                    db_sbr.as_ref(),
                    pg_sbr.as_ref(),
                    &card_id,
                    reason.as_deref(),
                )
            },
        )?,
    )?;

    // getCardStatus
    let db_gcs = db.clone();
    let pg_gcs = pg_pool.clone();
    obj.set(
        "__getCardStatusRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            get_card_status_raw(db_gcs.as_ref(), pg_gcs.as_ref(), &card_id)
        })?,
    )?;

    // getReworkCardInfo
    let db_grc = db.clone();
    let pg_grc = pg_pool.clone();
    obj.set(
        "__getReworkCardInfoRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            get_rework_card_info_raw(db_grc.as_ref(), pg_grc.as_ref(), &card_id)
        })?,
    )?;

    // listWaitingForCi
    let db_lwc = db.clone();
    let pg_lwc = pg_pool.clone();
    obj.set(
        "__listWaitingForCiRaw",
        Function::new(ctx.clone(), move || -> String {
            list_waiting_for_ci_raw(db_lwc.as_ref(), pg_lwc.as_ref())
        })?,
    )?;

    ad.set("ciRecovery", obj)?;

    // JS wrappers
    ctx.eval::<(), _>(
        r#"
        (function() {
            agentdesk.ciRecovery.setBlockedReason = function(cardId, reason) {
                var result = JSON.parse(
                    agentdesk.ciRecovery.__setBlockedReasonRaw(cardId, reason == null ? null : String(reason))
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.ciRecovery.getCardStatus = function(cardId) {
                var result = JSON.parse(agentdesk.ciRecovery.__getCardStatusRaw(cardId));
                if (result.error) return null;
                if (!result.found) return null;
                return { status: result.status };
            };
            agentdesk.ciRecovery.getReworkCardInfo = function(cardId) {
                var result = JSON.parse(agentdesk.ciRecovery.__getReworkCardInfoRaw(cardId));
                if (result.error) return null;
                if (!result.found) return null;
                return {
                    assigned_agent_id: result.assigned_agent_id,
                    title: result.title,
                    github_issue_number: result.github_issue_number
                };
            };
            agentdesk.ciRecovery.listWaitingForCi = function() {
                var result = JSON.parse(agentdesk.ciRecovery.__listWaitingForCiRaw());
                if (result.error) throw new Error(result.error);
                return result.rows || [];
            };
        })();
        "#,
    )?;

    Ok(())
}

fn set_blocked_reason_raw(
    _db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    card_id: &str,
    reason: Option<&str>,
) -> String {
    tracing::debug!(
        target: "policy.ci_recovery",
        card_id = %card_id,
        reason = reason.unwrap_or("<null>"),
        "ciRecovery.setBlockedReason"
    );

    if let Some(pool) = pg_pool {
        return set_blocked_reason_pg(pool, card_id, reason);
    }
    json!({ "error": "sqlite backend is unavailable" }).to_string()
}

fn set_blocked_reason_pg(pool: &PgPool, card_id: &str, reason: Option<&str>) -> String {
    let card_id = card_id.to_string();
    let reason = reason.map(|s| s.to_string());
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let rows_affected = sqlx::query(
                "UPDATE kanban_cards SET blocked_reason = $1, updated_at = NOW() WHERE id = $2",
            )
            .bind(reason)
            .bind(&card_id)
            .execute(&bridge_pool)
            .await
            .map_err(|error| {
                format!("update postgres kanban_cards blocked_reason {card_id}: {error}")
            })?
            .rows_affected();
            Ok(json!({ "ok": true, "rows_affected": rows_affected }).to_string())
        },
        |error| json!({ "error": error }).to_string(),
    ) {
        Ok(result) => result,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

fn get_card_status_raw(_db: Option<&Db>, pg_pool: Option<&PgPool>, card_id: &str) -> String {
    if let Some(pool) = pg_pool {
        return get_card_status_pg(pool, card_id);
    }
    json!({ "error": "sqlite backend is unavailable" }).to_string()
}

fn get_card_status_pg(pool: &PgPool, card_id: &str) -> String {
    let card_id = card_id.to_string();
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let status: Option<String> =
                sqlx::query_scalar::<_, String>("SELECT status FROM kanban_cards WHERE id = $1")
                    .bind(&card_id)
                    .fetch_optional(&bridge_pool)
                    .await
                    .map_err(|error| {
                        format!("load postgres kanban_cards status {card_id}: {error}")
                    })?;
            Ok(match status {
                Some(s) => json!({ "found": true, "status": s }).to_string(),
                None => json!({ "found": false }).to_string(),
            })
        },
        |error| json!({ "error": error }).to_string(),
    ) {
        Ok(result) => result,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

fn get_rework_card_info_raw(_db: Option<&Db>, pg_pool: Option<&PgPool>, card_id: &str) -> String {
    if let Some(pool) = pg_pool {
        return get_rework_card_info_pg(pool, card_id);
    }
    json!({ "error": "sqlite backend is unavailable" }).to_string()
}

fn get_rework_card_info_pg(pool: &PgPool, card_id: &str) -> String {
    let card_id = card_id.to_string();
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let row = sqlx::query(
                "SELECT assigned_agent_id, title, github_issue_number
                 FROM kanban_cards WHERE id = $1",
            )
            .bind(&card_id)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|error| {
                format!("load postgres kanban_cards rework info {card_id}: {error}")
            })?;
            Ok(match row {
                Some(row) => {
                    let agent: Option<String> = row
                        .try_get("assigned_agent_id")
                        .map_err(|e| format!("decode assigned_agent_id: {e}"))?;
                    let title: Option<String> = row
                        .try_get("title")
                        .map_err(|e| format!("decode title: {e}"))?;
                    let issue: Option<i64> = row
                        .try_get("github_issue_number")
                        .map_err(|e| format!("decode github_issue_number: {e}"))?;
                    json!({
                        "found": true,
                        "assigned_agent_id": agent,
                        "title": title,
                        "github_issue_number": issue
                    })
                    .to_string()
                }
                None => json!({ "found": false }).to_string(),
            })
        },
        |error| json!({ "error": error }).to_string(),
    ) {
        Ok(result) => result,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}

fn list_waiting_for_ci_raw(_db: Option<&Db>, pg_pool: Option<&PgPool>) -> String {
    if let Some(pool) = pg_pool {
        return list_waiting_for_ci_pg(pool);
    }
    json!({ "error": "sqlite backend is unavailable" }).to_string()
}

fn list_waiting_for_ci_pg(pool: &PgPool) -> String {
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let rows = sqlx::query(
                "SELECT p.card_id AS id, c.blocked_reason AS blocked_reason
                 FROM pr_tracking p
                 JOIN kanban_cards c ON c.id = p.card_id
                 WHERE p.state = 'wait-ci'",
            )
            .fetch_all(&bridge_pool)
            .await
            .map_err(|error| format!("list postgres wait-ci cards: {error}"))?;

            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                let id: String = row.try_get("id").map_err(|e| format!("decode id: {e}"))?;
                let blocked_reason: Option<String> = row
                    .try_get("blocked_reason")
                    .map_err(|e| format!("decode blocked_reason: {e}"))?;
                out.push(json!({ "id": id, "blocked_reason": blocked_reason }));
            }
            Ok(json!({ "rows": out }).to_string())
        },
        |error| json!({ "error": error }).to_string(),
    ) {
        Ok(result) => result,
        Err(raw) => crate::engine::ops::ensure_js_error_json(raw),
    }
}
