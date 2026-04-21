use crate::db::Db;
use anyhow::anyhow;
use libsql_rusqlite::{OptionalExtension, Row, types::ToSql}; // TODO(#839): drop sqlite fallback once policy-engine tests move to PG fixtures.
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder, Row as SqlxRow};

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

pub(super) fn register_card_ops<'js>(
    ctx: &Ctx<'js>,
    db: Option<Db>,
    pg_pool: Option<PgPool>,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let cards_obj = Object::new(ctx.clone())?;

    let db_get = db.clone();
    let pg_get = pg_pool.clone();
    cards_obj.set(
        "__getRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            if let Some(pool) = pg_get.as_ref() {
                return card_get_raw_pg(pool, &card_id);
            }
            db_get
                .as_ref()
                .map(|db| card_get_raw(db, &card_id))
                .unwrap_or_else(|| json_result(Err(anyhow!("sqlite backend is unavailable"))))
        })?,
    )?;

    let db_list = db.clone();
    let pg_list = pg_pool.clone();
    cards_obj.set(
        "__listRaw",
        Function::new(ctx.clone(), move |filter_json: String| -> String {
            if let Some(pool) = pg_list.as_ref() {
                return card_list_raw_pg(pool, &filter_json);
            }
            db_list
                .as_ref()
                .map(|db| card_list_raw(db, &filter_json))
                .unwrap_or_else(|| json_result(Err(anyhow!("sqlite backend is unavailable"))))
        })?,
    )?;

    let db_assign = db.clone();
    let pg_assign = pg_pool.clone();
    cards_obj.set(
        "__assignRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, agent_id: String| -> String {
                if let Some(pool) = pg_assign.as_ref() {
                    return card_assign_raw_pg(pool, &card_id, &agent_id);
                }
                db_assign
                    .as_ref()
                    .map(|db| card_assign_raw(db, &card_id, &agent_id))
                    .unwrap_or_else(|| json_result(Err(anyhow!("sqlite backend is unavailable"))))
            },
        )?,
    )?;

    let db_priority = db;
    let pg_priority = pg_pool;
    cards_obj.set(
        "__setPriorityRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, priority: String| -> String {
                if let Some(pool) = pg_priority.as_ref() {
                    return card_set_priority_raw_pg(pool, &card_id, &priority);
                }
                db_priority
                    .as_ref()
                    .map(|db| card_set_priority_raw(db, &card_id, &priority))
                    .unwrap_or_else(|| json_result(Err(anyhow!("sqlite backend is unavailable"))))
            },
        )?,
    )?;

    ad.set("cards", cards_obj)?;

    ctx.eval::<(), _>(
        r#"
        (function() {
            agentdesk.cards.get = function(cardId) {
                var result = JSON.parse(agentdesk.cards.__getRaw(cardId || ""));
                if (result.error) throw new Error(result.error);
                return result.card || null;
            };
            agentdesk.cards.list = function(filter) {
                var result = JSON.parse(
                    agentdesk.cards.__listRaw(JSON.stringify(filter || {}))
                );
                if (result.error) throw new Error(result.error);
                return result.cards || [];
            };
            agentdesk.cards.assign = function(cardId, agentId) {
                var result = JSON.parse(
                    agentdesk.cards.__assignRaw(cardId || "", agentId || "")
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.cards.setPriority = function(cardId, priority) {
                var result = JSON.parse(
                    agentdesk.cards.__setPriorityRaw(cardId || "", priority || "")
                );
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
        let mut params: Vec<Box<dyn ToSql>> = Vec::new();

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

        let param_refs: Vec<&dyn ToSql> = params.iter().map(|value| value.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), card_row_to_json)?;
        let cards: Vec<Value> = rows.collect::<Result<Vec<_>, _>>()?;
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
            libsql_rusqlite::params![agent_id, card_id], // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
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
            libsql_rusqlite::params![normalized, card_id], // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
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

fn card_get_raw_pg(pool: &PgPool, card_id: &str) -> String {
    let card_id = card_id.to_string();
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let sql = format!("{} WHERE kc.id = $1", card_select_sql());
            let card = sqlx::query(&sql)
                .bind(&card_id)
                .fetch_optional(&bridge_pool)
                .await
                .map_err(|error| format!("load postgres card {card_id}: {error}"))?
                .map(|row| card_row_to_json_pg(&row))
                .transpose()?;
            Ok(json!({ "card": card }).to_string())
        },
        |error| json!({ "error": error }).to_string(),
    );

    match result {
        Ok(value) => value,
        Err(error_json) => error_json,
    }
}

fn card_list_raw_pg(pool: &PgPool, filter_json: &str) -> String {
    let filter = if filter_json.trim().is_empty() {
        CardListFilter::default()
    } else {
        match serde_json::from_str::<CardListFilter>(filter_json) {
            Ok(filter) => filter,
            Err(error) => {
                return json!({ "error": format!("invalid cards.list filter: {error}") })
                    .to_string();
            }
        }
    };

    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let mut query = QueryBuilder::<Postgres>::new(card_select_sql());
            query.push(" WHERE 1 = 1");

            if let Some(status) = filter.status.as_deref() {
                query.push(" AND kc.status = ");
                query.push_bind(status.to_string());
            }

            if let Some(statuses) = filter.statuses.clone().filter(|items| !items.is_empty()) {
                query.push(" AND kc.status IN (");
                let mut separated = query.separated(", ");
                for status in statuses {
                    separated.push_bind(status.to_string());
                }
                separated.push_unseparated(")");
            }

            if let Some(repo_id) = filter.repo_id.as_deref() {
                query.push(" AND kc.repo_id = ");
                query.push_bind(repo_id.to_string());
            }

            if let Some(agent_id) = filter.assigned_agent_id.as_deref() {
                query.push(" AND kc.assigned_agent_id = ");
                query.push_bind(agent_id.to_string());
            }

            if let Some(unassigned) = filter.unassigned {
                if unassigned {
                    query.push(" AND kc.assigned_agent_id IS NULL");
                } else {
                    query.push(" AND kc.assigned_agent_id IS NOT NULL");
                }
            }

            if let Some(metadata_present) = filter.metadata_present {
                if metadata_present {
                    query.push(" AND kc.metadata IS NOT NULL");
                } else {
                    query.push(" AND kc.metadata IS NULL");
                }
            }

            if let Some(issue_number) = filter.github_issue_number {
                query.push(" AND kc.github_issue_number = ");
                query.push_bind(issue_number);
            }

            query.push(" ORDER BY kc.sort_order ASC, kc.updated_at DESC");
            if let Some(limit) = filter.limit {
                query.push(" LIMIT ");
                query.push_bind(limit.min(200) as i64);
            }

            let rows = query
                .build()
                .fetch_all(&bridge_pool)
                .await
                .map_err(|error| format!("list postgres cards: {error}"))?;
            let mut cards = Vec::with_capacity(rows.len());
            for row in rows {
                cards.push(card_row_to_json_pg(&row)?);
            }
            Ok(json!({ "cards": cards }).to_string())
        },
        |error| json!({ "error": error }).to_string(),
    );

    match result {
        Ok(value) => value,
        Err(error_json) => error_json,
    }
}

fn card_assign_raw_pg(pool: &PgPool, card_id: &str, agent_id: &str) -> String {
    let card_id = card_id.to_string();
    let agent_id = agent_id.to_string();
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            if card_id.trim().is_empty() {
                return Err("cards.assign requires card_id".to_string());
            }
            if agent_id.trim().is_empty() {
                return Err("cards.assign requires agent_id".to_string());
            }

            let agent_exists =
                sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM agents WHERE id = $1)")
                    .bind(&agent_id)
                    .fetch_one(&bridge_pool)
                    .await
                    .map_err(|error| format!("load postgres agent {agent_id}: {error}"))?;
            if !agent_exists {
                return Err(format!("unknown agent: {agent_id}"));
            }

            let changed = sqlx::query(
                "UPDATE kanban_cards
                 SET assigned_agent_id = $1,
                     updated_at = NOW()
                 WHERE id = $2",
            )
            .bind(&agent_id)
            .bind(&card_id)
            .execute(&bridge_pool)
            .await
            .map_err(|error| format!("assign postgres card {card_id}: {error}"))?
            .rows_affected();
            if changed == 0 {
                return Err(format!("unknown card: {card_id}"));
            }

            let sql = format!("{} WHERE kc.id = $1", card_select_sql());
            let card = sqlx::query(&sql)
                .bind(&card_id)
                .fetch_one(&bridge_pool)
                .await
                .map_err(|error| format!("reload postgres card {card_id}: {error}"))?;
            Ok(json!({
                "ok": true,
                "changed": changed > 0,
                "card": card_row_to_json_pg(&card)?,
            })
            .to_string())
        },
        |error| json!({ "error": error }).to_string(),
    );

    match result {
        Ok(value) => value,
        Err(error_json) => error_json,
    }
}

fn card_set_priority_raw_pg(pool: &PgPool, card_id: &str, priority: &str) -> String {
    let card_id = card_id.to_string();
    let priority = priority.to_string();
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            if card_id.trim().is_empty() {
                return Err("cards.setPriority requires card_id".to_string());
            }
            let normalized = priority.trim().to_lowercase();
            if !matches!(normalized.as_str(), "urgent" | "high" | "medium" | "low") {
                return Err(format!(
                    "invalid priority '{priority}' (expected urgent|high|medium|low)"
                ));
            }

            let changed = sqlx::query(
                "UPDATE kanban_cards
                 SET priority = $1,
                     updated_at = NOW()
                 WHERE id = $2",
            )
            .bind(&normalized)
            .bind(&card_id)
            .execute(&bridge_pool)
            .await
            .map_err(|error| format!("update postgres card priority {card_id}: {error}"))?
            .rows_affected();
            if changed == 0 {
                return Err(format!("unknown card: {card_id}"));
            }

            let sql = format!("{} WHERE kc.id = $1", card_select_sql());
            let card = sqlx::query(&sql)
                .bind(&card_id)
                .fetch_one(&bridge_pool)
                .await
                .map_err(|error| format!("reload postgres card {card_id}: {error}"))?;
            Ok(json!({
                "ok": true,
                "changed": changed > 0,
                "card": card_row_to_json_pg(&card)?,
            })
            .to_string())
        },
        |error| json!({ "error": error }).to_string(),
    );

    match result {
        Ok(value) => value,
        Err(error_json) => error_json,
    }
}

fn load_card_json_by_id_on_conn(
    conn: &libsql_rusqlite::Connection, // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
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

fn card_row_to_json(row: &Row<'_>) -> libsql_rusqlite::Result<Value> {
    // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
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

fn card_row_to_json_pg(row: &sqlx::postgres::PgRow) -> Result<Value, String> {
    Ok(json!({
        "id": row.try_get::<String, _>("id").map_err(|error| format!("decode id: {error}"))?,
        "repo_id": row.try_get::<Option<String>, _>("repo_id").map_err(|error| format!("decode repo_id: {error}"))?,
        "title": row.try_get::<String, _>("title").map_err(|error| format!("decode title: {error}"))?,
        "status": row.try_get::<String, _>("status").map_err(|error| format!("decode status: {error}"))?,
        "priority": row.try_get::<Option<String>, _>("priority").map_err(|error| format!("decode priority: {error}"))?,
        "assigned_agent_id": row.try_get::<Option<String>, _>("assigned_agent_id").map_err(|error| format!("decode assigned_agent_id: {error}"))?,
        "github_issue_url": row.try_get::<Option<String>, _>("github_issue_url").map_err(|error| format!("decode github_issue_url: {error}"))?,
        "github_issue_number": row.try_get::<Option<i64>, _>("github_issue_number").map_err(|error| format!("decode github_issue_number: {error}"))?,
        "latest_dispatch_id": row.try_get::<Option<String>, _>("latest_dispatch_id").map_err(|error| format!("decode latest_dispatch_id: {error}"))?,
        "review_round": row.try_get::<Option<i64>, _>("review_round").map_err(|error| format!("decode review_round: {error}"))?,
        "metadata": parse_json_value(row.try_get::<Option<String>, _>("metadata").map_err(|error| format!("decode metadata: {error}"))?),
        "created_at": row.try_get::<Option<String>, _>("created_at").map_err(|error| format!("decode created_at: {error}"))?,
        "updated_at": row.try_get::<Option<String>, _>("updated_at").map_err(|error| format!("decode updated_at: {error}"))?,
        "description": row.try_get::<Option<String>, _>("description").map_err(|error| format!("decode description: {error}"))?,
        "blocked_reason": row.try_get::<Option<String>, _>("blocked_reason").map_err(|error| format!("decode blocked_reason: {error}"))?,
        "pipeline_stage_id": row.try_get::<Option<String>, _>("pipeline_stage_id").map_err(|error| format!("decode pipeline_stage_id: {error}"))?,
        "review_notes": row.try_get::<Option<String>, _>("review_notes").map_err(|error| format!("decode review_notes: {error}"))?,
        "review_status": row.try_get::<Option<String>, _>("review_status").map_err(|error| format!("decode review_status: {error}"))?,
        "requested_at": row.try_get::<Option<String>, _>("requested_at").map_err(|error| format!("decode requested_at: {error}"))?,
        "owner_agent_id": row.try_get::<Option<String>, _>("owner_agent_id").map_err(|error| format!("decode owner_agent_id: {error}"))?,
        "requester_agent_id": row.try_get::<Option<String>, _>("requester_agent_id").map_err(|error| format!("decode requester_agent_id: {error}"))?,
        "parent_card_id": row.try_get::<Option<String>, _>("parent_card_id").map_err(|error| format!("decode parent_card_id: {error}"))?,
        "depth": row.try_get::<Option<i64>, _>("depth").map_err(|error| format!("decode depth: {error}"))?,
        "sort_order": row.try_get::<Option<i64>, _>("sort_order").map_err(|error| format!("decode sort_order: {error}"))?,
        "active_thread_id": row.try_get::<Option<String>, _>("active_thread_id").map_err(|error| format!("decode active_thread_id: {error}"))?,
        "channel_thread_map": parse_json_value(row.try_get::<Option<String>, _>("channel_thread_map").map_err(|error| format!("decode channel_thread_map: {error}"))?),
        "suggestion_pending_at": row.try_get::<Option<String>, _>("suggestion_pending_at").map_err(|error| format!("decode suggestion_pending_at: {error}"))?,
        "review_entered_at": row.try_get::<Option<String>, _>("review_entered_at").map_err(|error| format!("decode review_entered_at: {error}"))?,
        "awaiting_dod_at": row.try_get::<Option<String>, _>("awaiting_dod_at").map_err(|error| format!("decode awaiting_dod_at: {error}"))?,
        "deferred_dod_json": parse_json_value(row.try_get::<Option<String>, _>("deferred_dod_json").map_err(|error| format!("decode deferred_dod_json: {error}"))?),
        "started_at": row.try_get::<Option<String>, _>("started_at").map_err(|error| format!("decode started_at: {error}"))?,
        "completed_at": row.try_get::<Option<String>, _>("completed_at").map_err(|error| format!("decode completed_at: {error}"))?,
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
