use crate::db::Db;
use anyhow::anyhow;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use rusqlite::{OptionalExtension, Row};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Default, Deserialize)]
struct CardListFilter {
    #[serde(default, alias = "repoId")]
    repo_id: Option<String>,
    #[serde(default, alias = "assignedAgentId")]
    assigned_agent_id: Option<String>,
    #[serde(default, alias = "githubIssueNumber")]
    github_issue_number: Option<i64>,
    #[serde(default, alias = "metadataPresent")]
    metadata_present: Option<bool>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    statuses: Option<Vec<String>>,
    #[serde(default)]
    unassigned: Option<bool>,
    #[serde(default)]
    limit: Option<usize>,
}

pub(super) fn register_card_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let cards_obj = Object::new(ctx.clone())?;

    let db_get = db.clone();
    cards_obj.set(
        "__getRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            card_get_raw(&db_get, &card_id)
        })?,
    )?;

    let db_list = db.clone();
    cards_obj.set(
        "__listRaw",
        Function::new(ctx.clone(), move |filter_json: String| -> String {
            card_list_raw(&db_list, &filter_json)
        })?,
    )?;

    let db_assign = db.clone();
    cards_obj.set(
        "__assignRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, agent_id: String| -> String {
                card_assign_raw(&db_assign, &card_id, &agent_id)
            },
        )?,
    )?;

    let db_priority = db;
    cards_obj.set(
        "__setPriorityRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, priority: String| -> String {
                card_set_priority_raw(&db_priority, &card_id, &priority)
            },
        )?,
    )?;

    ad.set("cards", cards_obj)?;

    ctx.eval::<(), _>(
        r#"
        (function() {
            var raw = agentdesk.cards;
            raw.get = function(cardId) {
                var result = JSON.parse(raw.__getRaw(cardId || ""));
                if (result.error) throw new Error(result.error);
                return result.card || null;
            };
            raw.list = function(filter) {
                var result = JSON.parse(raw.__listRaw(JSON.stringify(filter || {})));
                if (result.error) throw new Error(result.error);
                return result.cards || [];
            };
            raw.assign = function(cardId, agentId) {
                var result = JSON.parse(raw.__assignRaw(cardId || "", agentId || ""));
                if (result.error) throw new Error(result.error);
                return result;
            };
            raw.setPriority = function(cardId, priority) {
                var result = JSON.parse(raw.__setPriorityRaw(cardId || "", priority || ""));
                if (result.error) throw new Error(result.error);
                return result;
            };
        })();
        "#,
    )?;

    Ok(())
}

fn card_get_raw(db: &Db, card_id: &str) -> String {
    let result = (|| -> anyhow::Result<Value> {
        let conn = db.read_conn()?;
        let card = load_card_json_by_id_on_conn(&conn, card_id)?;
        Ok(json!({ "card": card }))
    })();

    json_result(result)
}

fn card_list_raw(db: &Db, filter_json: &str) -> String {
    let result = (|| -> anyhow::Result<Value> {
        let filter = if filter_json.trim().is_empty() {
            CardListFilter::default()
        } else {
            serde_json::from_str::<CardListFilter>(filter_json)
                .map_err(|e| anyhow!("invalid cards.list filter: {e}"))?
        };

        let conn = db.read_conn()?;
        let mut sql = format!("{} WHERE 1 = 1", card_select_sql());
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(status) = filter.status {
            params.push(Box::new(status));
            sql.push_str(&format!(" AND kc.status = ?{}", params.len()));
        }

        if let Some(statuses) = filter.statuses.filter(|items| !items.is_empty()) {
            let start = params.len() + 1;
            let placeholders: Vec<String> = statuses
                .iter()
                .enumerate()
                .map(|(idx, _)| format!("?{}", start + idx))
                .collect();
            for status in statuses {
                params.push(Box::new(status));
            }
            sql.push_str(&format!(" AND kc.status IN ({})", placeholders.join(", ")));
        }

        if let Some(repo_id) = filter.repo_id {
            params.push(Box::new(repo_id));
            sql.push_str(&format!(" AND kc.repo_id = ?{}", params.len()));
        }

        if let Some(agent_id) = filter.assigned_agent_id {
            params.push(Box::new(agent_id));
            sql.push_str(&format!(" AND kc.assigned_agent_id = ?{}", params.len()));
        }

        if let Some(unassigned) = filter.unassigned {
            if unassigned {
                sql.push_str(" AND kc.assigned_agent_id IS NULL");
            } else {
                sql.push_str(" AND kc.assigned_agent_id IS NOT NULL");
            }
        }

        if let Some(metadata_present) = filter.metadata_present {
            if metadata_present {
                sql.push_str(" AND kc.metadata IS NOT NULL");
            } else {
                sql.push_str(" AND kc.metadata IS NULL");
            }
        }

        if let Some(issue_number) = filter.github_issue_number {
            params.push(Box::new(issue_number));
            sql.push_str(&format!(" AND kc.github_issue_number = ?{}", params.len()));
        }

        sql.push_str(" ORDER BY kc.sort_order ASC, kc.updated_at DESC");
        if let Some(limit) = filter.limit {
            sql.push_str(&format!(" LIMIT {}", limit.min(200)));
        }

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|value| value.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), card_row_to_json)?;
        let cards: Vec<Value> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({ "cards": cards }))
    })();

    json_result(result)
}

fn card_assign_raw(db: &Db, card_id: &str, agent_id: &str) -> String {
    let result = (|| -> anyhow::Result<Value> {
        if card_id.trim().is_empty() {
            return Err(anyhow!("cards.assign requires card_id"));
        }
        if agent_id.trim().is_empty() {
            return Err(anyhow!("cards.assign requires agent_id"));
        }

        let conn = db.separate_conn()?;
        let agent_exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM agents WHERE id = ?1)",
            [agent_id],
            |row| row.get(0),
        )?;
        if !agent_exists {
            return Err(anyhow!("unknown agent: {agent_id}"));
        }

        let changed = conn.execute(
            "UPDATE kanban_cards SET assigned_agent_id = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![agent_id, card_id],
        )?;
        if changed == 0 {
            return Err(anyhow!("unknown card: {card_id}"));
        }

        let card = load_card_json_by_id_on_conn(&conn, card_id)?;
        Ok(json!({
            "ok": true,
            "changed": changed > 0,
            "card": card,
        }))
    })();

    json_result(result)
}

fn card_set_priority_raw(db: &Db, card_id: &str, priority: &str) -> String {
    let result = (|| -> anyhow::Result<Value> {
        if card_id.trim().is_empty() {
            return Err(anyhow!("cards.setPriority requires card_id"));
        }
        let normalized = priority.trim().to_lowercase();
        if !matches!(normalized.as_str(), "urgent" | "high" | "medium" | "low") {
            return Err(anyhow!(
                "invalid priority '{priority}' (expected urgent|high|medium|low)"
            ));
        }

        let conn = db.separate_conn()?;
        let changed = conn.execute(
            "UPDATE kanban_cards SET priority = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![normalized, card_id],
        )?;
        if changed == 0 {
            return Err(anyhow!("unknown card: {card_id}"));
        }

        let card = load_card_json_by_id_on_conn(&conn, card_id)?;
        Ok(json!({
            "ok": true,
            "changed": changed > 0,
            "card": card,
        }))
    })();

    json_result(result)
}

fn card_select_sql() -> &'static str {
    "SELECT \
        kc.id, kc.repo_id, kc.title, kc.status, kc.priority, kc.assigned_agent_id, \
        kc.github_issue_url, kc.github_issue_number, kc.latest_dispatch_id, kc.review_round, \
        kc.metadata, kc.created_at, kc.updated_at, kc.description, kc.blocked_reason, \
        kc.pipeline_stage_id, kc.review_notes, kc.review_status, kc.requested_at, \
        kc.owner_agent_id, kc.requester_agent_id, kc.parent_card_id, kc.depth, kc.sort_order, \
        kc.active_thread_id, kc.channel_thread_map, kc.suggestion_pending_at, kc.review_entered_at, \
        kc.awaiting_dod_at, kc.deferred_dod_json, kc.started_at, kc.completed_at \
     FROM kanban_cards kc"
}

fn load_card_json_by_id_on_conn(
    conn: &rusqlite::Connection,
    card_id: &str,
) -> anyhow::Result<Option<Value>> {
    let sql = format!("{} WHERE kc.id = ?1", card_select_sql());
    let mut stmt = conn.prepare(&sql)?;
    let card = stmt
        .query_row([card_id], card_row_to_json)
        .optional()
        .map_err(anyhow::Error::from)?;
    Ok(card)
}

fn card_row_to_json(row: &Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "repo_id": row.get::<_, Option<String>>(1)?,
        "title": row.get::<_, String>(2)?,
        "status": row.get::<_, String>(3)?,
        "priority": row.get::<_, Option<String>>(4)?,
        "assigned_agent_id": row.get::<_, Option<String>>(5)?,
        "github_issue_url": row.get::<_, Option<String>>(6)?,
        "github_issue_number": row.get::<_, Option<i64>>(7)?,
        "latest_dispatch_id": row.get::<_, Option<String>>(8)?,
        "review_round": row.get::<_, Option<i64>>(9)?,
        "metadata": parse_json_value(row.get::<_, Option<String>>(10)?),
        "created_at": row.get::<_, Option<String>>(11)?,
        "updated_at": row.get::<_, Option<String>>(12)?,
        "description": row.get::<_, Option<String>>(13)?,
        "blocked_reason": row.get::<_, Option<String>>(14)?,
        "pipeline_stage_id": row.get::<_, Option<String>>(15)?,
        "review_notes": row.get::<_, Option<String>>(16)?,
        "review_status": row.get::<_, Option<String>>(17)?,
        "requested_at": row.get::<_, Option<String>>(18)?,
        "owner_agent_id": row.get::<_, Option<String>>(19)?,
        "requester_agent_id": row.get::<_, Option<String>>(20)?,
        "parent_card_id": row.get::<_, Option<String>>(21)?,
        "depth": row.get::<_, Option<i64>>(22)?,
        "sort_order": row.get::<_, Option<i64>>(23)?,
        "active_thread_id": row.get::<_, Option<String>>(24)?,
        "channel_thread_map": parse_json_value(row.get::<_, Option<String>>(25)?),
        "suggestion_pending_at": row.get::<_, Option<String>>(26)?,
        "review_entered_at": row.get::<_, Option<String>>(27)?,
        "awaiting_dod_at": row.get::<_, Option<String>>(28)?,
        "deferred_dod_json": parse_json_value(row.get::<_, Option<String>>(29)?),
        "started_at": row.get::<_, Option<String>>(30)?,
        "completed_at": row.get::<_, Option<String>>(31)?,
    }))
}

fn parse_json_value(raw: Option<String>) -> Value {
    raw.and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

fn json_result(result: anyhow::Result<Value>) -> String {
    match result {
        Ok(value) => serde_json::to_string(&value)
            .unwrap_or_else(|e| format!(r#"{{"error":"serialize: {e}"}}"#)),
        Err(err) => json!({ "error": err.to_string() }).to_string(),
    }
}
