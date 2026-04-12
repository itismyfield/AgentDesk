use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use rusqlite::OptionalExtension;
use serde_json::json;

pub(crate) const ADVANCE_REVIEW_ROUND_HINT_KEY: &str = "advance_review_round_on_next_review";

pub(super) fn register_review_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let review_obj = Object::new(ctx.clone())?;

    let db_verdict = db.clone();
    review_obj.set(
        "__getVerdictRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            review_get_verdict_raw(&db_verdict, &card_id)
        })?,
    )?;

    let db_entry = db.clone();
    review_obj.set(
        "__entryContextRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            review_entry_context_raw(&db_entry, &card_id)
        })?,
    )?;

    let db_record = db;
    review_obj.set(
        "__recordEntryRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, opts_json: String| -> String {
                review_record_entry_raw(&db_record, &card_id, &opts_json)
            },
        )?,
    )?;

    ad.set("review", review_obj)?;

    ctx.eval::<(), _>(
        r#"
        (function() {
            var review = agentdesk.review;
            review.getVerdict = function(cardId) {
                var result = JSON.parse(review.__getVerdictRaw(cardId || ""));
                if (result.error) throw new Error(result.error);
                return result.review || null;
            };
            review.entryContext = function(cardId) {
                var result = JSON.parse(review.__entryContextRaw(cardId || ""));
                if (result.error) throw new Error(result.error);
                return result.entry || null;
            };
            review.recordEntry = function(cardId, opts) {
                var result = JSON.parse(review.__recordEntryRaw(cardId || "", JSON.stringify(opts || {})));
                if (result.error) throw new Error(result.error);
                return result;
            };
        })();
        "#,
    )?;

    Ok(())
}

fn metadata_requests_review_round_advance(metadata_raw: Option<&str>) -> bool {
    metadata_raw
        .filter(|raw| !raw.trim().is_empty())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| value.as_object().cloned())
        .and_then(|metadata| metadata.get(ADVANCE_REVIEW_ROUND_HINT_KEY).cloned())
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn review_get_verdict_raw(db: &Db, card_id: &str) -> String {
    let result = (|| -> anyhow::Result<serde_json::Value> {
        let conn = db.read_conn()?;
        let review = conn
            .query_row(
                "SELECT kc.id, \
                        rs.review_round, rs.state, rs.pending_dispatch_id, rs.last_verdict, \
                        rs.last_decision, rs.decided_by, rs.decided_at, rs.review_entered_at, rs.updated_at, \
                        (SELECT json_extract(td.result, '$.verdict') \
                         FROM task_dispatches td \
                         WHERE td.kanban_card_id = kc.id \
                           AND td.dispatch_type = 'review' \
                           AND td.status = 'completed' \
                         ORDER BY COALESCE(td.completed_at, td.updated_at) DESC LIMIT 1) \
                 FROM kanban_cards kc \
                 LEFT JOIN card_review_state rs ON rs.card_id = kc.id \
                 WHERE kc.id = ?1",
                [card_id],
                |row| {
                    let state = row
                        .get::<_, Option<String>>(2)?
                        .unwrap_or_else(|| "idle".to_string());
                    let last_verdict = row.get::<_, Option<String>>(4)?;
                    let latest_dispatch_verdict = row.get::<_, Option<String>>(10)?;
                    let verdict = last_verdict
                        .clone()
                        .or(latest_dispatch_verdict.clone());
                    let source = if last_verdict.is_some() {
                        "review_state"
                    } else if latest_dispatch_verdict.is_some() {
                        "dispatch_result"
                    } else {
                        "none"
                    };

                    Ok(json!({
                        "card_id": row.get::<_, String>(0)?,
                        "review_round": row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        "state": state,
                        "verdict": verdict,
                        "pending_dispatch_id": row.get::<_, Option<String>>(3)?,
                        "decision": row.get::<_, Option<String>>(5)?,
                        "decided_by": row.get::<_, Option<String>>(6)?,
                        "decided_at": row.get::<_, Option<String>>(7)?,
                        "review_entered_at": row.get::<_, Option<String>>(8)?,
                        "updated_at": row.get::<_, Option<String>>(9)?,
                        "source": source,
                    }))
                },
            )
            .optional()?;

        Ok(json!({ "review": review }))
    })();

    match result {
        Ok(value) => value.to_string(),
        Err(err) => json!({ "error": err.to_string() }).to_string(),
    }
}

fn review_entry_context_raw(db: &Db, card_id: &str) -> String {
    let result = (|| -> anyhow::Result<serde_json::Value> {
        if card_id.trim().is_empty() {
            return Err(anyhow::anyhow!("review.entryContext requires card_id"));
        }

        let conn = db.read_conn()?;
        let entry = conn
            .query_row(
                "SELECT COALESCE(kc.review_round, 0), \
                        (SELECT COUNT(*) FROM task_dispatches td \
                         WHERE td.kanban_card_id = kc.id \
                           AND td.dispatch_type IN ('implementation', 'rework') \
                           AND td.status = 'completed'), \
                        (SELECT MAX(COALESCE(td.completed_at, td.updated_at)) FROM task_dispatches td \
                         WHERE td.kanban_card_id = kc.id \
                           AND td.dispatch_type IN ('implementation', 'rework') \
                           AND td.status = 'completed'), \
                        (SELECT MAX(COALESCE(td.completed_at, td.updated_at)) FROM task_dispatches td \
                         WHERE td.kanban_card_id = kc.id \
                           AND td.dispatch_type = 'review' \
                           AND td.status = 'completed'), \
                        kc.metadata \
                 FROM kanban_cards kc \
                 WHERE kc.id = ?1",
                [card_id],
                |row| {
                    let current_round = row.get::<_, i64>(0)?;
                    let completed_work_count = row.get::<_, i64>(1)?;
                    let latest_work_completed_at = row.get::<_, Option<String>>(2)?;
                    let latest_review_completed_at = row.get::<_, Option<String>>(3)?;
                    let should_advance_round = current_round == 0
                        || completed_work_count > current_round
                        || matches!(
                            (
                                latest_work_completed_at.as_deref(),
                                latest_review_completed_at.as_deref()
                            ),
                            (Some(work), Some(review)) if work > review
                        )
                        || metadata_requests_review_round_advance(
                            row.get::<_, Option<String>>(4)?.as_deref(),
                        );
                    let next_round = if should_advance_round {
                        current_round + 1
                    } else {
                        current_round
                    };
                    Ok(json!({
                        "card_id": card_id,
                        "current_round": current_round,
                        "completed_work_count": completed_work_count,
                        "should_advance_round": should_advance_round,
                        "next_round": next_round,
                    }))
                },
            )
            .optional()?;

        Ok(json!({ "entry": entry }))
    })();

    match result {
        Ok(value) => value.to_string(),
        Err(err) => json!({ "error": err.to_string() }).to_string(),
    }
}

fn review_record_entry_raw(db: &Db, card_id: &str, opts_json: &str) -> String {
    let result = (|| -> anyhow::Result<serde_json::Value> {
        if card_id.trim().is_empty() {
            return Err(anyhow::anyhow!("review.recordEntry requires card_id"));
        }

        let opts: serde_json::Value = if opts_json.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(opts_json)
                .map_err(|e| anyhow::anyhow!("invalid review.recordEntry opts: {e}"))?
        };

        let conn = db.separate_conn()?;
        let review_round = opts.get("review_round").and_then(|value| value.as_i64());
        let exclude_status = opts
            .get("exclude_status")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty());

        let changed = match (review_round, exclude_status) {
            (Some(round), Some(status)) => conn.execute(
                "UPDATE kanban_cards SET review_round = ?1, updated_at = datetime('now') \
                 WHERE id = ?2 AND status != ?3",
                rusqlite::params![round, card_id, status],
            )?,
            (Some(round), None) => conn.execute(
                "UPDATE kanban_cards SET review_round = ?1, updated_at = datetime('now') \
                 WHERE id = ?2",
                rusqlite::params![round, card_id],
            )?,
            (None, Some(status)) => conn.execute(
                "UPDATE kanban_cards SET updated_at = datetime('now') \
                 WHERE id = ?1 AND status != ?2",
                rusqlite::params![card_id, status],
            )?,
            (None, None) => conn.execute(
                "UPDATE kanban_cards SET updated_at = datetime('now') \
                 WHERE id = ?1",
                rusqlite::params![card_id],
            )?,
        };

        clear_review_round_advance_hint_on_conn(&conn, card_id)?;

        Ok(json!({
            "ok": true,
            "rows_affected": changed,
            "changed": changed > 0,
        }))
    })();

    match result {
        Ok(value) => value.to_string(),
        Err(err) => json!({ "error": err.to_string() }).to_string(),
    }
}

fn clear_review_round_advance_hint_on_conn(
    conn: &rusqlite::Connection,
    card_id: &str,
) -> rusqlite::Result<()> {
    let metadata_raw: Option<String> = conn
        .query_row(
            "SELECT metadata FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let Some(raw) = metadata_raw.filter(|value| !value.trim().is_empty()) else {
        return Ok(());
    };
    let Ok(mut metadata) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Ok(());
    };
    let Some(object) = metadata.as_object_mut() else {
        return Ok(());
    };
    if object.remove(ADVANCE_REVIEW_ROUND_HINT_KEY).is_none() {
        return Ok(());
    }

    let stored_metadata = if object.is_empty() {
        None
    } else {
        Some(metadata.to_string())
    };
    conn.execute(
        "UPDATE kanban_cards SET metadata = ?1, updated_at = datetime('now') WHERE id = ?2",
        rusqlite::params![stored_metadata, card_id],
    )?;
    Ok(())
}
