use anyhow::{Result, anyhow};
use libsql_rusqlite::{Connection, Row, params}; // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row as SqlxRow};

use crate::db::Db;
use crate::db::session_agent_resolution::{
    resolve_agent_id_for_session, resolve_agent_id_for_session_pg,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTranscriptEventKind {
    User,
    Assistant,
    Thinking,
    ToolUse,
    ToolResult,
    Result,
    Error,
    Task,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTranscriptEvent {
    pub kind: SessionTranscriptEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub struct PersistSessionTranscript<'a> {
    pub turn_id: &'a str,
    pub session_key: Option<&'a str>,
    pub channel_id: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub dispatch_id: Option<&'a str>,
    pub user_message: &'a str,
    pub assistant_message: &'a str,
    pub events: &'a [SessionTranscriptEvent],
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionTranscriptRecord {
    pub id: i64,
    pub turn_id: String,
    pub session_key: Option<String>,
    pub channel_id: Option<String>,
    pub agent_id: Option<String>,
    pub provider: Option<String>,
    pub dispatch_id: Option<String>,
    pub kanban_card_id: Option<String>,
    pub dispatch_title: Option<String>,
    pub card_title: Option<String>,
    pub github_issue_number: Option<i64>,
    pub user_message: String,
    pub assistant_message: String,
    pub events: Vec<SessionTranscriptEvent>,
    pub duration_ms: Option<i64>,
    pub created_at: String,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionTranscriptSearchHit {
    pub id: i64,
    pub turn_id: String,
    pub session_key: Option<String>,
    pub channel_id: Option<String>,
    pub agent_id: Option<String>,
    pub provider: Option<String>,
    pub dispatch_id: Option<String>,
    pub user_message: String,
    pub assistant_message: String,
    pub created_at: String,
    pub snippet: String,
    pub score: f64,
}

#[derive(Debug, Clone)]
struct PreparedSessionTranscript {
    turn_id: String,
    session_key: Option<String>,
    channel_id: Option<String>,
    agent_id: Option<String>,
    provider: Option<String>,
    dispatch_id: Option<String>,
    user_message: String,
    assistant_message: String,
    events_json: String,
    duration_ms: Option<i64>,
}

pub fn persist_turn(db: &Db, entry: PersistSessionTranscript<'_>) -> Result<bool> {
    let mut conn = db
        .lock()
        .map_err(|e| anyhow!("db lock failed while persisting transcript: {e}"))?;
    persist_turn_on_conn(&mut conn, entry)
}

pub async fn persist_turn_db(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    entry: PersistSessionTranscript<'_>,
) -> Result<bool> {
    let Some(pool) = pg_pool else {
        let db = db.ok_or_else(|| anyhow!("sqlite db is required when postgres pool is absent"))?;
        return persist_turn(db, entry);
    };

    let prepared = prepare_persist_entry_pg(pool, db, &entry).await?;
    let Some(prepared) = prepared else {
        return Ok(false);
    };

    persist_turn_pg(pool, &prepared).await?;
    Ok(true)
}

pub fn persist_turn_on_conn(
    conn: &mut Connection,
    entry: PersistSessionTranscript<'_>,
) -> Result<bool> {
    let Some(prepared) = prepare_persist_entry(conn, &entry)? else {
        return Ok(false);
    };

    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO session_transcripts (
            turn_id,
            session_key,
            channel_id,
            agent_id,
            provider,
            dispatch_id,
            user_message,
            assistant_message,
            events_json,
            duration_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ON CONFLICT(turn_id) DO UPDATE SET
            session_key = excluded.session_key,
            channel_id = excluded.channel_id,
            agent_id = COALESCE(excluded.agent_id, session_transcripts.agent_id),
            provider = excluded.provider,
            dispatch_id = excluded.dispatch_id,
            user_message = excluded.user_message,
            assistant_message = excluded.assistant_message,
            events_json = excluded.events_json,
            duration_ms = excluded.duration_ms",
        params![
            prepared.turn_id,
            prepared.session_key,
            prepared.channel_id,
            prepared.agent_id,
            prepared.provider,
            prepared.dispatch_id,
            prepared.user_message,
            prepared.assistant_message,
            prepared.events_json,
            prepared.duration_ms,
        ],
    )?;
    tx.commit()?;

    Ok(true)
}

async fn persist_turn_pg(pool: &PgPool, entry: &PreparedSessionTranscript) -> Result<()> {
    sqlx::query(
        "INSERT INTO session_transcripts (
            turn_id,
            session_key,
            channel_id,
            agent_id,
            provider,
            dispatch_id,
            user_message,
            assistant_message,
            events_json,
            duration_ms
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, CAST($9 AS jsonb), $10)
         ON CONFLICT (turn_id) DO UPDATE SET
            session_key = EXCLUDED.session_key,
            channel_id = EXCLUDED.channel_id,
            agent_id = COALESCE(EXCLUDED.agent_id, session_transcripts.agent_id),
            provider = EXCLUDED.provider,
            dispatch_id = EXCLUDED.dispatch_id,
            user_message = EXCLUDED.user_message,
            assistant_message = EXCLUDED.assistant_message,
            events_json = EXCLUDED.events_json,
            duration_ms = EXCLUDED.duration_ms",
    )
    .bind(&entry.turn_id)
    .bind(&entry.session_key)
    .bind(&entry.channel_id)
    .bind(&entry.agent_id)
    .bind(&entry.provider)
    .bind(&entry.dispatch_id)
    .bind(&entry.user_message)
    .bind(&entry.assistant_message)
    .bind(&entry.events_json)
    .bind(entry.duration_ms)
    .execute(pool)
    .await
    .map_err(|e| anyhow!("persist postgres transcript failed: {e}"))?;
    Ok(())
}

fn prepare_persist_entry_base(
    entry: &PersistSessionTranscript<'_>,
) -> Result<Option<PreparedSessionTranscript>> {
    let turn_id = entry.turn_id.trim();
    if turn_id.is_empty() {
        return Err(anyhow!("turn_id is required"));
    }

    let user_message = entry.user_message.trim();
    let assistant_message = entry.assistant_message.trim();
    let events = normalize_events(entry.events);
    if user_message.is_empty() && assistant_message.is_empty() && events.is_empty() {
        return Ok(None);
    }

    let events_json = serde_json::to_string(&events)?;

    Ok(Some(PreparedSessionTranscript {
        turn_id: turn_id.to_string(),
        session_key: normalized_opt(entry.session_key),
        channel_id: normalized_opt(entry.channel_id),
        agent_id: None,
        provider: normalized_opt(entry.provider),
        dispatch_id: normalized_opt(entry.dispatch_id),
        user_message: user_message.to_string(),
        assistant_message: assistant_message.to_string(),
        events_json,
        duration_ms: entry.duration_ms,
    }))
}

fn prepare_persist_entry(
    conn: &Connection,
    entry: &PersistSessionTranscript<'_>,
) -> Result<Option<PreparedSessionTranscript>> {
    let Some(mut prepared) = prepare_persist_entry_base(entry)? else {
        return Ok(None);
    };

    prepared.agent_id = resolve_agent_id_for_session(
        conn,
        entry.agent_id,
        prepared.session_key.as_deref(),
        None,
        None,
        prepared.dispatch_id.as_deref(),
    );

    Ok(Some(prepared))
}

async fn prepare_persist_entry_pg(
    pool: &PgPool,
    db: Option<&Db>,
    entry: &PersistSessionTranscript<'_>,
) -> Result<Option<PreparedSessionTranscript>> {
    let Some(mut prepared) = prepare_persist_entry_base(entry)? else {
        return Ok(None);
    };

    prepared.agent_id = resolve_agent_id_for_session_pg(
        pool,
        entry.agent_id,
        prepared.session_key.as_deref(),
        None,
        None,
        prepared.dispatch_id.as_deref(),
    )
    .await;

    if prepared.agent_id.is_none() {
        if let Some(db) = db {
            let conn = db
                .lock()
                .map_err(|e| anyhow!("db lock failed while preparing transcript fallback: {e}"))?;
            prepared.agent_id = resolve_agent_id_for_session(
                &conn,
                entry.agent_id,
                prepared.session_key.as_deref(),
                None,
                None,
                prepared.dispatch_id.as_deref(),
            );
        }
    }

    Ok(Some(prepared))
}

pub fn list_transcripts_for_agent(
    conn: &Connection,
    agent_id: &str,
    limit: usize,
) -> Result<Vec<SessionTranscriptRecord>> {
    let limit = limit.clamp(1, 100) as i64;
    let mut stmt = conn.prepare(
        "SELECT st.id,
                st.turn_id,
                st.session_key,
                st.channel_id,
                st.agent_id,
                st.provider,
                st.dispatch_id,
                td.kanban_card_id,
                td.title,
                kc.title,
                kc.github_issue_number,
                st.user_message,
                st.assistant_message,
                st.events_json,
                st.duration_ms,
                st.created_at
         FROM session_transcripts st
         LEFT JOIN sessions s
           ON s.session_key = st.session_key
         LEFT JOIN task_dispatches td
           ON td.id = st.dispatch_id
         LEFT JOIN kanban_cards kc
           ON kc.id = td.kanban_card_id
         WHERE COALESCE(NULLIF(TRIM(st.agent_id), ''), NULLIF(TRIM(s.agent_id), '')) = ?1
            OR (
                COALESCE(NULLIF(TRIM(st.agent_id), ''), NULLIF(TRIM(s.agent_id), '')) IS NULL
                AND td.to_agent_id = ?1
            )
         ORDER BY st.created_at DESC, st.id DESC
         LIMIT ?2",
    )?;

    let rows = stmt.query_map(params![agent_id, limit], session_transcript_record_from_row)?;
    Ok(rows.collect::<libsql_rusqlite::Result<Vec<_>>>()?) // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
}

pub async fn list_transcripts_for_agent_db(
    db: &Db,
    pg_pool: Option<&PgPool>,
    agent_id: &str,
    limit: usize,
) -> Result<Vec<SessionTranscriptRecord>> {
    let Some(pool) = pg_pool else {
        let conn = db
            .read_conn()
            .map_err(|e| anyhow!("db read lock failed while listing agent transcripts: {e}"))?;
        return list_transcripts_for_agent(&conn, agent_id, limit);
    };

    list_transcripts_for_agent_pg(pool, agent_id, limit).await
}

pub fn list_transcripts_for_card(
    conn: &Connection,
    card_id: &str,
    limit: usize,
) -> Result<Vec<SessionTranscriptRecord>> {
    let limit = limit.clamp(1, 100) as i64;
    let mut stmt = conn.prepare(
        "SELECT st.id,
                st.turn_id,
                st.session_key,
                st.channel_id,
                st.agent_id,
                st.provider,
                st.dispatch_id,
                td.kanban_card_id,
                td.title,
                kc.title,
                kc.github_issue_number,
                st.user_message,
                st.assistant_message,
                st.events_json,
                st.duration_ms,
                st.created_at
         FROM session_transcripts st
         JOIN task_dispatches td
           ON td.id = st.dispatch_id
         LEFT JOIN kanban_cards kc
           ON kc.id = td.kanban_card_id
         WHERE td.kanban_card_id = ?1
         ORDER BY st.created_at DESC, st.id DESC
         LIMIT ?2",
    )?;

    let rows = stmt.query_map(params![card_id, limit], session_transcript_record_from_row)?;
    Ok(rows.collect::<libsql_rusqlite::Result<Vec<_>>>()?) // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
}

pub async fn list_transcripts_for_card_db(
    db: &Db,
    pg_pool: Option<&PgPool>,
    card_id: &str,
    limit: usize,
) -> Result<Vec<SessionTranscriptRecord>> {
    let Some(pool) = pg_pool else {
        let conn = db
            .read_conn()
            .map_err(|e| anyhow!("db read lock failed while listing card transcripts: {e}"))?;
        return list_transcripts_for_card(&conn, card_id, limit);
    };

    list_transcripts_for_card_pg(pool, card_id, limit).await
}

pub fn dispatch_has_assistant_response_db(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
) -> Result<bool> {
    let Some(pool) = pg_pool else {
        let Some(db) = db else {
            return Ok(false);
        };
        let conn = db
            .read_conn()
            .map_err(|e| anyhow!("db read lock failed while checking transcript evidence: {e}"))?;
        return conn
            .query_row(
                "SELECT COUNT(*) > 0
                 FROM session_transcripts
                 WHERE dispatch_id = ?1
                   AND TRIM(assistant_message) <> ''",
                [dispatch_id],
                |row| row.get(0),
            )
            .map_err(|e| anyhow!("session transcript lookup failed: {e}"));
    };

    let dispatch_id = dispatch_id.to_string();
    run_pg_blocking(pool, move |pool| async move {
        dispatch_has_assistant_response_pg(&pool, &dispatch_id).await
    })
}

fn session_transcript_record_from_row(
    row: &Row<'_>,
) -> libsql_rusqlite::Result<SessionTranscriptRecord> {
    // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
    let events_json: Option<String> = row.get(13)?;
    Ok(SessionTranscriptRecord {
        id: row.get(0)?,
        turn_id: row.get(1)?,
        session_key: row.get(2)?,
        channel_id: row.get(3)?,
        agent_id: row.get(4)?,
        provider: row.get(5)?,
        dispatch_id: row.get(6)?,
        kanban_card_id: row.get(7)?,
        dispatch_title: row.get(8)?,
        card_title: row.get(9)?,
        github_issue_number: row.get(10)?,
        user_message: row.get(11)?,
        assistant_message: row.get(12)?,
        events: parse_events_json(events_json.as_deref()),
        duration_ms: row.get(14)?,
        created_at: row.get(15)?,
    })
}

async fn list_transcripts_for_agent_pg(
    pool: &PgPool,
    agent_id: &str,
    limit: usize,
) -> Result<Vec<SessionTranscriptRecord>> {
    let limit = limit.clamp(1, 100) as i64;
    let rows = sqlx::query(
        "SELECT st.id,
                st.turn_id,
                st.session_key,
                st.channel_id,
                st.agent_id,
                st.provider,
                st.dispatch_id,
                td.kanban_card_id,
                td.title,
                kc.title AS card_title,
                kc.github_issue_number,
                st.user_message,
                st.assistant_message,
                st.events_json::text AS events_json,
                st.duration_ms,
                to_char(st.created_at, 'YYYY-MM-DD HH24:MI:SS') AS created_at
         FROM session_transcripts st
         LEFT JOIN sessions s
           ON s.session_key = st.session_key
         LEFT JOIN task_dispatches td
           ON td.id = st.dispatch_id
         LEFT JOIN kanban_cards kc
           ON kc.id = td.kanban_card_id
         WHERE COALESCE(NULLIF(BTRIM(st.agent_id), ''), NULLIF(BTRIM(s.agent_id), '')) = $1
            OR (
                COALESCE(NULLIF(BTRIM(st.agent_id), ''), NULLIF(BTRIM(s.agent_id), '')) IS NULL
                AND td.to_agent_id = $1
            )
         ORDER BY st.created_at DESC, st.id DESC
         LIMIT $2",
    )
    .bind(agent_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow!("query agent transcripts failed: {e}"))?;

    rows.into_iter()
        .map(|row| session_transcript_record_from_pg_row(&row))
        .collect()
}

async fn list_transcripts_for_card_pg(
    pool: &PgPool,
    card_id: &str,
    limit: usize,
) -> Result<Vec<SessionTranscriptRecord>> {
    let limit = limit.clamp(1, 100) as i64;
    let rows = sqlx::query(
        "SELECT st.id,
                st.turn_id,
                st.session_key,
                st.channel_id,
                st.agent_id,
                st.provider,
                st.dispatch_id,
                td.kanban_card_id,
                td.title,
                kc.title AS card_title,
                kc.github_issue_number,
                st.user_message,
                st.assistant_message,
                st.events_json::text AS events_json,
                st.duration_ms,
                to_char(st.created_at, 'YYYY-MM-DD HH24:MI:SS') AS created_at
         FROM session_transcripts st
         JOIN task_dispatches td
           ON td.id = st.dispatch_id
         LEFT JOIN kanban_cards kc
           ON kc.id = td.kanban_card_id
         WHERE td.kanban_card_id = $1
         ORDER BY st.created_at DESC, st.id DESC
         LIMIT $2",
    )
    .bind(card_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow!("query card transcripts failed: {e}"))?;

    rows.into_iter()
        .map(|row| session_transcript_record_from_pg_row(&row))
        .collect()
}

async fn dispatch_has_assistant_response_pg(pool: &PgPool, dispatch_id: &str) -> Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT COUNT(*) > 0
         FROM session_transcripts
         WHERE dispatch_id = $1
           AND BTRIM(assistant_message) <> ''",
    )
    .bind(dispatch_id)
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow!("session transcript lookup failed: {e}"))
}

fn session_transcript_record_from_pg_row(
    row: &sqlx::postgres::PgRow,
) -> Result<SessionTranscriptRecord> {
    let events_json: Option<String> = row
        .try_get("events_json")
        .map_err(|e| anyhow!("read transcript events_json: {e}"))?;
    Ok(SessionTranscriptRecord {
        id: row
            .try_get("id")
            .map_err(|e| anyhow!("read transcript id: {e}"))?,
        turn_id: row
            .try_get("turn_id")
            .map_err(|e| anyhow!("read transcript turn_id: {e}"))?,
        session_key: row
            .try_get("session_key")
            .map_err(|e| anyhow!("read transcript session_key: {e}"))?,
        channel_id: row
            .try_get("channel_id")
            .map_err(|e| anyhow!("read transcript channel_id: {e}"))?,
        agent_id: row
            .try_get("agent_id")
            .map_err(|e| anyhow!("read transcript agent_id: {e}"))?,
        provider: row
            .try_get("provider")
            .map_err(|e| anyhow!("read transcript provider: {e}"))?,
        dispatch_id: row
            .try_get("dispatch_id")
            .map_err(|e| anyhow!("read transcript dispatch_id: {e}"))?,
        kanban_card_id: row
            .try_get("kanban_card_id")
            .map_err(|e| anyhow!("read transcript kanban_card_id: {e}"))?,
        dispatch_title: row
            .try_get("title")
            .map_err(|e| anyhow!("read transcript dispatch title: {e}"))?,
        card_title: row
            .try_get("card_title")
            .map_err(|e| anyhow!("read transcript card title: {e}"))?,
        github_issue_number: row
            .try_get("github_issue_number")
            .map_err(|e| anyhow!("read transcript github_issue_number: {e}"))?,
        user_message: row
            .try_get("user_message")
            .map_err(|e| anyhow!("read transcript user_message: {e}"))?,
        assistant_message: row
            .try_get("assistant_message")
            .map_err(|e| anyhow!("read transcript assistant_message: {e}"))?,
        events: parse_events_json(events_json.as_deref()),
        duration_ms: row
            .try_get("duration_ms")
            .map_err(|e| anyhow!("read transcript duration_ms: {e}"))?,
        created_at: row
            .try_get("created_at")
            .map_err(|e| anyhow!("read transcript created_at: {e}"))?,
    })
}

fn run_pg_blocking<T, F>(
    pool: &PgPool,
    future_factory: impl FnOnce(PgPool) -> F + Send + 'static,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    crate::utils::async_bridge::block_on_pg_result(pool, future_factory, |error| {
        anyhow!("build runtime for postgres transcript query failed: {error}")
    })
}

#[cfg(test)]
pub async fn search_transcripts_pg(
    pool: &PgPool,
    raw_query: &str,
    limit: usize,
) -> Result<(String, Vec<SessionTranscriptSearchHit>)> {
    let search_query = normalize_search_query(raw_query)
        .ok_or_else(|| anyhow!("query must contain a searchable term"))?;
    let limit = limit.clamp(1, 50) as i64;

    let rows = sqlx::query(
        "WITH search AS (
            SELECT websearch_to_tsquery('simple', $1) AS query
         )
         SELECT st.id,
                st.turn_id,
                st.session_key,
                st.channel_id,
                st.agent_id,
                st.provider,
                st.dispatch_id,
                st.user_message,
                st.assistant_message,
                to_char(st.created_at, 'YYYY-MM-DD HH24:MI:SS') AS created_at,
                COALESCE(
                    NULLIF(
                        ts_headline(
                            'simple',
                            concat_ws(
                                E'\\n\\n',
                                NULLIF(st.user_message, ''),
                                NULLIF(st.assistant_message, '')
                            ),
                            search.query,
                            'StartSel=<mark>, StopSel=</mark>, MaxFragments=2, MaxWords=18, MinWords=5, ShortWord=1, FragmentDelimiter=…'
                        ),
                        ''
                    ),
                    concat_ws(
                        E'\\n\\n',
                        NULLIF(st.user_message, ''),
                        NULLIF(st.assistant_message, '')
                    )
                ) AS snippet,
                ts_rank_cd(st.search_tsv, search.query)::float8 AS score
         FROM session_transcripts st
         CROSS JOIN search
         WHERE st.search_tsv @@ search.query
         ORDER BY score DESC, st.created_at DESC
         LIMIT $2",
    )
    .bind(&search_query)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow!("query transcript search failed: {e}"))?;

    let hits = rows
        .into_iter()
        .map(|row| {
            Ok(SessionTranscriptSearchHit {
                id: row
                    .try_get("id")
                    .map_err(|e| anyhow!("read transcript search id: {e}"))?,
                turn_id: row
                    .try_get("turn_id")
                    .map_err(|e| anyhow!("read transcript search turn_id: {e}"))?,
                session_key: row
                    .try_get("session_key")
                    .map_err(|e| anyhow!("read transcript search session_key: {e}"))?,
                channel_id: row
                    .try_get("channel_id")
                    .map_err(|e| anyhow!("read transcript search channel_id: {e}"))?,
                agent_id: row
                    .try_get("agent_id")
                    .map_err(|e| anyhow!("read transcript search agent_id: {e}"))?,
                provider: row
                    .try_get("provider")
                    .map_err(|e| anyhow!("read transcript search provider: {e}"))?,
                dispatch_id: row
                    .try_get("dispatch_id")
                    .map_err(|e| anyhow!("read transcript search dispatch_id: {e}"))?,
                user_message: row
                    .try_get("user_message")
                    .map_err(|e| anyhow!("read transcript search user_message: {e}"))?,
                assistant_message: row
                    .try_get("assistant_message")
                    .map_err(|e| anyhow!("read transcript search assistant_message: {e}"))?,
                created_at: row
                    .try_get("created_at")
                    .map_err(|e| anyhow!("read transcript search created_at: {e}"))?,
                snippet: row
                    .try_get::<Option<String>, _>("snippet")
                    .map_err(|e| anyhow!("read transcript search snippet: {e}"))?
                    .unwrap_or_default(),
                score: row
                    .try_get::<f64, _>("score")
                    .map_err(|e| anyhow!("read transcript search score: {e}"))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok((search_query, hits))
}

#[cfg(test)]
fn normalize_search_query(raw_query: &str) -> Option<String> {
    let normalized = raw_query.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn parse_events_json(raw: Option<&str>) -> Vec<SessionTranscriptEvent> {
    raw.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            serde_json::from_str::<Vec<SessionTranscriptEvent>>(trimmed).ok()
        }
    })
    .map(|events| normalize_events(&events))
    .unwrap_or_default()
}

fn normalize_events(events: &[SessionTranscriptEvent]) -> Vec<SessionTranscriptEvent> {
    events
        .iter()
        .filter_map(|event| {
            let mut normalized = event.clone();
            normalized.content = normalized.content.trim().to_string();
            normalized.summary = normalized
                .summary
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);
            normalized.tool_name = normalized
                .tool_name
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);
            normalized.status = normalized
                .status
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);

            if normalized.content.is_empty()
                && normalized.summary.is_none()
                && normalized.tool_name.is_none()
                && !matches!(
                    normalized.kind,
                    SessionTranscriptEventKind::Thinking
                        | SessionTranscriptEventKind::Result
                        | SessionTranscriptEventKind::Error
                        | SessionTranscriptEventKind::System
                )
            {
                return None;
            }

            Some(normalized)
        })
        .collect()
}

fn normalized_opt(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
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
            let database_name = format!("agentdesk_pg_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            let admin_pool = match sqlx::PgPool::connect(&admin_url).await {
                Ok(pool) => pool,
                Err(error) => {
                    eprintln!("skipping postgres transcript test: admin connect failed: {error}");
                    return None;
                }
            };
            if let Err(error) = sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
                .execute(&admin_pool)
                .await
            {
                eprintln!("skipping postgres transcript test: create database failed: {error}");
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

        async fn migrate(&self) -> Option<PgPool> {
            let pool = match sqlx::PgPool::connect(&self.database_url).await {
                Ok(pool) => pool,
                Err(error) => {
                    eprintln!("skipping postgres transcript test: db connect failed: {error}");
                    return None;
                }
            };
            if let Err(error) = crate::db::postgres::migrate(&pool).await {
                eprintln!("skipping postgres transcript test: migrate failed: {error}");
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

    #[test]
    fn normalize_search_query_trims_and_collapses_whitespace() {
        assert_eq!(
            normalize_search_query("  FTS5   #239  session-search  "),
            Some("FTS5 #239 session-search".to_string())
        );
    }

    #[test]
    fn normalize_search_query_supports_korean_terms() {
        assert_eq!(
            normalize_search_query("세션 검색"),
            Some("세션 검색".to_string())
        );
    }

    #[test]
    fn persist_turn_upserts_same_turn_id() {
        let db = crate::db::test_db();
        let mut conn = db.lock().unwrap();

        persist_turn_on_conn(
            &mut conn,
            PersistSessionTranscript {
                turn_id: "discord:1:2",
                session_key: Some("host:tmux-1"),
                channel_id: Some("1"),
                agent_id: Some("agent-1"),
                provider: Some("codex"),
                dispatch_id: Some("dispatch-1"),
                user_message: "old question",
                assistant_message: "old answer",
                events: &[],
                duration_ms: None,
            },
        )
        .unwrap();

        let new_events = vec![SessionTranscriptEvent {
            kind: SessionTranscriptEventKind::ToolUse,
            tool_name: Some("Read".to_string()),
            summary: Some("src/config.rs".to_string()),
            content: "{\"file_path\":\"src/config.rs\"}".to_string(),
            status: Some("success".to_string()),
            is_error: false,
        }];

        persist_turn_on_conn(
            &mut conn,
            PersistSessionTranscript {
                turn_id: "discord:1:2",
                session_key: Some("host:tmux-1"),
                channel_id: Some("1"),
                agent_id: Some("agent-1"),
                provider: Some("codex"),
                dispatch_id: Some("dispatch-1"),
                user_message: "new question",
                assistant_message: "new answer",
                events: &new_events,
                duration_ms: Some(3210),
            },
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_transcripts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);

        let (assistant_message, events_json, duration_ms): (String, String, Option<i64>) = conn
            .query_row(
                "SELECT assistant_message, events_json, duration_ms
                 FROM session_transcripts
                 WHERE turn_id = 'discord:1:2'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(assistant_message, "new answer");
        assert!(events_json.contains("src/config.rs"));
        assert_eq!(duration_ms, Some(3210));
    }

    #[tokio::test]
    async fn session_transcripts_search_uses_tsvector() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        let db = crate::db::test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name) VALUES ('agent-2', 'Agent Two')",
                [],
            )
            .unwrap();
        }
        let events = vec![SessionTranscriptEvent {
            kind: SessionTranscriptEventKind::Thinking,
            tool_name: None,
            summary: Some("FTS5 검색 설계".to_string()),
            content: "검색 전략을 정리합니다.".to_string(),
            status: Some("info".to_string()),
            is_error: false,
        }];
        persist_turn_db(
            Some(&db),
            Some(&pool),
            PersistSessionTranscript {
                turn_id: "discord:2:3",
                session_key: Some("host:tmux-2"),
                channel_id: Some("2"),
                agent_id: Some("agent-2"),
                provider: Some("claude"),
                dispatch_id: Some("dispatch-2"),
                user_message: "FTS5 검색 구현 상태 알려줘",
                assistant_message: "session transcript 검색 API를 추가했습니다.",
                events: &events,
                duration_ms: Some(9000),
            },
        )
        .await
        .unwrap();

        let (_search_query, hits) = search_transcripts_pg(&pool, "FTS5 검색", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].agent_id.as_deref(), Some("agent-2"));
        assert!(hits[0].snippet.contains("FTS5") || hits[0].snippet.contains("검색"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn persist_turn_resolves_agent_from_session_context() {
        let db = crate::db::test_db();
        let mut conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name) VALUES ('agent-session', 'Agent Session')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (session_key, agent_id, provider, status, created_at)
             VALUES ('host:tmux-session', 'agent-session', 'claude', 'idle', datetime('now'))",
            [],
        )
        .unwrap();

        persist_turn_on_conn(
            &mut conn,
            PersistSessionTranscript {
                turn_id: "discord:session:1",
                session_key: Some("host:tmux-session"),
                channel_id: Some("session"),
                agent_id: None,
                provider: Some("claude"),
                dispatch_id: None,
                user_message: "question",
                assistant_message: "answer",
                events: &[],
                duration_ms: None,
            },
        )
        .unwrap();

        let agent_id: Option<String> = conn
            .query_row(
                "SELECT agent_id FROM session_transcripts WHERE turn_id = 'discord:session:1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(agent_id.as_deref(), Some("agent-session"));
    }

    #[test]
    fn list_transcripts_for_agent_falls_back_to_session_agent_id() {
        let db = crate::db::test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name) VALUES ('agent-fallback', 'Agent Fallback')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions (session_key, agent_id, provider, status, created_at)
                 VALUES ('host:tmux-fallback', 'agent-fallback', 'codex', 'idle', datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO session_transcripts (
                    turn_id, session_key, channel_id, agent_id, provider, dispatch_id, user_message, assistant_message, events_json
                 ) VALUES (
                    'discord:fallback:1', 'host:tmux-fallback', 'fallback', NULL, 'codex', NULL, 'legacy user', 'legacy assistant', '[]'
                 )",
                [],
            )
            .unwrap();
        }

        let conn = db.read_conn().unwrap();
        let transcripts = list_transcripts_for_agent(&conn, "agent-fallback", 10).unwrap();
        assert_eq!(transcripts.len(), 1);
        assert_eq!(transcripts[0].turn_id, "discord:fallback:1");
    }

    #[test]
    fn list_transcripts_for_card_returns_parsed_events() {
        let db = crate::db::test_db();
        {
            let mut conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name) VALUES ('agent-card', 'Agent Card')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, github_issue_number, created_at, updated_at)
                 VALUES ('card-1', 'Card 1', 'in_progress', 101, datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
                 ) VALUES (
                    'dispatch-card-1', 'card-1', 'agent-card', 'implementation', 'completed',
                    'Card Dispatch', datetime('now'), datetime('now')
                 )",
                [],
            )
            .unwrap();

            let events = vec![
                SessionTranscriptEvent {
                    kind: SessionTranscriptEventKind::ToolUse,
                    tool_name: Some("Bash".to_string()),
                    summary: Some("cargo test".to_string()),
                    content: "cargo test".to_string(),
                    status: Some("running".to_string()),
                    is_error: false,
                },
                SessionTranscriptEvent {
                    kind: SessionTranscriptEventKind::Result,
                    tool_name: None,
                    summary: Some("done".to_string()),
                    content: "done".to_string(),
                    status: Some("success".to_string()),
                    is_error: false,
                },
            ];
            persist_turn_on_conn(
                &mut conn,
                PersistSessionTranscript {
                    turn_id: "discord:3:4",
                    session_key: Some("host:tmux-3"),
                    channel_id: Some("3"),
                    agent_id: Some("agent-card"),
                    provider: Some("codex"),
                    dispatch_id: Some("dispatch-card-1"),
                    user_message: "Run tests",
                    assistant_message: "Tests completed",
                    events: &events,
                    duration_ms: Some(12000),
                },
            )
            .unwrap();
        }

        let conn = db.read_conn().unwrap();
        let transcripts = list_transcripts_for_card(&conn, "card-1", 10).unwrap();
        assert_eq!(transcripts.len(), 1);
        assert_eq!(
            transcripts[0].dispatch_title.as_deref(),
            Some("Card Dispatch")
        );
        assert_eq!(transcripts[0].card_title.as_deref(), Some("Card 1"));
        assert_eq!(transcripts[0].github_issue_number, Some(101));
        assert_eq!(transcripts[0].kanban_card_id.as_deref(), Some("card-1"));
        assert_eq!(transcripts[0].events.len(), 2);
        assert_eq!(transcripts[0].duration_ms, Some(12000));
    }
}
