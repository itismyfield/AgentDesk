use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use rusqlite::OptionalExtension;
use serde_json::json;

pub(super) fn register_review_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let review_obj = Object::new(ctx.clone())?;

    let db_verdict = db;
    review_obj.set(
        "__getVerdictRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            review_get_verdict_raw(&db_verdict, &card_id)
        })?,
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
        })();
        "#,
    )?;

    Ok(())
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
