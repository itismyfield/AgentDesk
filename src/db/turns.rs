use anyhow::{Result, anyhow};
use libsql_rusqlite::{Connection, params}; // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::db::Db;
use crate::db::session_agent_resolution::{
    resolve_agent_id_for_session, resolve_agent_id_for_session_pg,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnTokenUsage {
    pub input_tokens: u64,
    pub cache_create_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
}

impl TurnTokenUsage {
    pub fn total_input_tokens(self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_create_tokens)
            .saturating_add(self.cache_read_tokens)
    }
}

#[derive(Debug, Clone)]
pub struct PersistTurn<'a> {
    pub turn_id: &'a str,
    pub session_key: Option<&'a str>,
    pub thread_id: Option<&'a str>,
    pub thread_title: Option<&'a str>,
    pub channel_id: &'a str,
    pub agent_id: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub dispatch_id: Option<&'a str>,
    pub started_at: Option<&'a str>,
    pub finished_at: Option<&'a str>,
    pub duration_ms: Option<i64>,
    pub token_usage: TurnTokenUsage,
}

#[derive(Debug, Clone)]
pub struct PersistTurnOwned {
    pub turn_id: String,
    pub session_key: Option<String>,
    pub thread_id: Option<String>,
    pub thread_title: Option<String>,
    pub channel_id: String,
    pub agent_id: Option<String>,
    pub provider: Option<String>,
    pub session_id: Option<String>,
    pub dispatch_id: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub token_usage: TurnTokenUsage,
}

impl PersistTurnOwned {
    pub fn as_borrowed(&self) -> PersistTurn<'_> {
        PersistTurn {
            turn_id: self.turn_id.as_str(),
            session_key: self.session_key.as_deref(),
            thread_id: self.thread_id.as_deref(),
            thread_title: self.thread_title.as_deref(),
            channel_id: self.channel_id.as_str(),
            agent_id: self.agent_id.as_deref(),
            provider: self.provider.as_deref(),
            session_id: self.session_id.as_deref(),
            dispatch_id: self.dispatch_id.as_deref(),
            started_at: self.started_at.as_deref(),
            finished_at: self.finished_at.as_deref(),
            duration_ms: self.duration_ms,
            token_usage: self.token_usage,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn upsert_turn(db: &Db, entry: PersistTurn<'_>) -> Result<()> {
    let mut conn = db
        .lock()
        .map_err(|e| anyhow!("db lock failed while persisting turn: {e}"))?;
    upsert_turn_on_conn(&mut conn, entry)
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn upsert_turn_owned(db: &Db, entry: &PersistTurnOwned) -> Result<()> {
    upsert_turn(db, entry.as_borrowed())
}

pub fn upsert_turn_owned_on_separate_conn(db: &Db, entry: &PersistTurnOwned) -> Result<()> {
    let mut conn = db
        .separate_conn()
        .map_err(|e| anyhow!("db separate_conn failed while persisting turn: {e}"))?;
    upsert_turn_on_conn(&mut conn, entry.as_borrowed())
}

pub async fn upsert_turn_owned_db(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    entry: &PersistTurnOwned,
) -> Result<()> {
    if let Some(pool) = pg_pool {
        return upsert_turn_owned_pg(pool, entry).await;
    }

    let db = db.ok_or_else(|| anyhow!("sqlite db is required when postgres pool is absent"))?;
    upsert_turn_owned_on_separate_conn(db, entry)
}

pub fn upsert_turn_on_conn(conn: &mut Connection, entry: PersistTurn<'_>) -> Result<()> {
    let turn_id = entry.turn_id.trim();
    if turn_id.is_empty() {
        return Err(anyhow!("turn_id is required"));
    }

    let channel_id = entry.channel_id.trim();
    if channel_id.is_empty() {
        return Err(anyhow!("channel_id is required"));
    }

    let session_key = normalized_opt(entry.session_key);
    let thread_id = normalized_opt(entry.thread_id);
    let thread_title = normalized_opt(entry.thread_title);
    let provider = normalized_opt(entry.provider);
    let session_id = normalized_opt(entry.session_id);
    let dispatch_id = normalized_opt(entry.dispatch_id);
    let started_at = normalized_opt(entry.started_at).unwrap_or_else(now_string);
    let finished_at = normalized_opt(entry.finished_at).unwrap_or_else(now_string);
    let agent_id = resolve_agent_id_for_session(
        conn,
        entry.agent_id,
        session_key.as_deref(),
        None,
        thread_id.as_deref(),
        dispatch_id.as_deref(),
    );

    conn.execute(
        "INSERT INTO turns (
            turn_id,
            session_key,
            thread_id,
            thread_title,
            channel_id,
            agent_id,
            provider,
            session_id,
            dispatch_id,
            started_at,
            finished_at,
            duration_ms,
            input_tokens,
            cache_create_tokens,
            cache_read_tokens,
            output_tokens
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT(turn_id) DO UPDATE SET
            session_key = COALESCE(excluded.session_key, turns.session_key),
            thread_id = COALESCE(excluded.thread_id, turns.thread_id),
            thread_title = COALESCE(excluded.thread_title, turns.thread_title),
            channel_id = excluded.channel_id,
            agent_id = COALESCE(excluded.agent_id, turns.agent_id),
            provider = COALESCE(excluded.provider, turns.provider),
            session_id = COALESCE(excluded.session_id, turns.session_id),
            dispatch_id = COALESCE(excluded.dispatch_id, turns.dispatch_id),
            started_at = COALESCE(excluded.started_at, turns.started_at),
            finished_at = COALESCE(excluded.finished_at, turns.finished_at),
            duration_ms = COALESCE(excluded.duration_ms, turns.duration_ms),
            input_tokens = CASE
                WHEN excluded.input_tokens > 0 THEN excluded.input_tokens
                ELSE turns.input_tokens
            END,
            cache_create_tokens = CASE
                WHEN excluded.cache_create_tokens > 0 THEN excluded.cache_create_tokens
                ELSE turns.cache_create_tokens
            END,
            cache_read_tokens = CASE
                WHEN excluded.cache_read_tokens > 0 THEN excluded.cache_read_tokens
                ELSE turns.cache_read_tokens
            END,
            output_tokens = CASE
                WHEN excluded.output_tokens > 0 THEN excluded.output_tokens
                ELSE turns.output_tokens
            END",
        params![
            turn_id,
            session_key,
            thread_id,
            thread_title,
            channel_id,
            agent_id,
            provider,
            session_id,
            dispatch_id,
            started_at,
            finished_at,
            entry.duration_ms,
            u64_to_i64(entry.token_usage.input_tokens),
            u64_to_i64(entry.token_usage.cache_create_tokens),
            u64_to_i64(entry.token_usage.cache_read_tokens),
            u64_to_i64(entry.token_usage.output_tokens),
        ],
    )?;

    Ok(())
}

async fn upsert_turn_owned_pg(pool: &PgPool, entry: &PersistTurnOwned) -> Result<()> {
    let turn_id = entry.turn_id.trim();
    if turn_id.is_empty() {
        return Err(anyhow!("turn_id is required"));
    }

    let channel_id = entry.channel_id.trim();
    if channel_id.is_empty() {
        return Err(anyhow!("channel_id is required"));
    }

    let session_key = normalized_opt(entry.session_key.as_deref());
    let thread_id = normalized_opt(entry.thread_id.as_deref());
    let thread_title = normalized_opt(entry.thread_title.as_deref());
    let provider = normalized_opt(entry.provider.as_deref());
    let session_id = normalized_opt(entry.session_id.as_deref());
    let dispatch_id = normalized_opt(entry.dispatch_id.as_deref());
    let started_at = normalized_opt(entry.started_at.as_deref()).unwrap_or_else(now_string);
    let finished_at = normalized_opt(entry.finished_at.as_deref()).unwrap_or_else(now_string);
    let agent_id = resolve_agent_id_for_session_pg(
        pool,
        entry.agent_id.as_deref(),
        session_key.as_deref(),
        None,
        thread_id.as_deref(),
        dispatch_id.as_deref(),
    )
    .await;

    sqlx::query(
        "INSERT INTO turns (
            turn_id,
            session_key,
            thread_id,
            thread_title,
            channel_id,
            agent_id,
            provider,
            session_id,
            dispatch_id,
            started_at,
            finished_at,
            duration_ms,
            input_tokens,
            cache_create_tokens,
            cache_read_tokens,
            output_tokens
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9,
            CAST($10 AS timestamptz),
            CAST($11 AS timestamptz),
            $12, $13, $14, $15, $16
         )
         ON CONFLICT(turn_id) DO UPDATE SET
            session_key = COALESCE(EXCLUDED.session_key, turns.session_key),
            thread_id = COALESCE(EXCLUDED.thread_id, turns.thread_id),
            thread_title = COALESCE(EXCLUDED.thread_title, turns.thread_title),
            channel_id = EXCLUDED.channel_id,
            agent_id = COALESCE(EXCLUDED.agent_id, turns.agent_id),
            provider = COALESCE(EXCLUDED.provider, turns.provider),
            session_id = COALESCE(EXCLUDED.session_id, turns.session_id),
            dispatch_id = COALESCE(EXCLUDED.dispatch_id, turns.dispatch_id),
            started_at = COALESCE(EXCLUDED.started_at, turns.started_at),
            finished_at = COALESCE(EXCLUDED.finished_at, turns.finished_at),
            duration_ms = COALESCE(EXCLUDED.duration_ms, turns.duration_ms),
            input_tokens = CASE
                WHEN EXCLUDED.input_tokens > 0 THEN EXCLUDED.input_tokens
                ELSE turns.input_tokens
            END,
            cache_create_tokens = CASE
                WHEN EXCLUDED.cache_create_tokens > 0 THEN EXCLUDED.cache_create_tokens
                ELSE turns.cache_create_tokens
            END,
            cache_read_tokens = CASE
                WHEN EXCLUDED.cache_read_tokens > 0 THEN EXCLUDED.cache_read_tokens
                ELSE turns.cache_read_tokens
            END,
            output_tokens = CASE
                WHEN EXCLUDED.output_tokens > 0 THEN EXCLUDED.output_tokens
                ELSE turns.output_tokens
            END",
    )
    .bind(turn_id)
    .bind(session_key)
    .bind(thread_id)
    .bind(thread_title)
    .bind(channel_id)
    .bind(agent_id)
    .bind(provider)
    .bind(session_id)
    .bind(dispatch_id)
    .bind(started_at)
    .bind(finished_at)
    .bind(entry.duration_ms)
    .bind(u64_to_i64(entry.token_usage.input_tokens))
    .bind(u64_to_i64(entry.token_usage.cache_create_tokens))
    .bind(u64_to_i64(entry.token_usage.cache_read_tokens))
    .bind(u64_to_i64(entry.token_usage.output_tokens))
    .execute(pool)
    .await
    .map_err(|e| anyhow!("persist postgres turn failed: {e}"))?;

    Ok(())
}

fn normalized_opt(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        PersistTurn, PersistTurnOwned, TurnTokenUsage, upsert_turn,
        upsert_turn_owned_on_separate_conn,
    };
    use crate::db::test_db;

    #[test]
    fn upsert_turn_persists_metadata_and_token_usage() {
        let db = test_db();
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO agents (id, name) VALUES ('agent-1', 'Agent One')",
                [],
            )
            .unwrap();

        upsert_turn(
            &db,
            PersistTurn {
                turn_id: "discord:1:2",
                session_key: Some("claude/token/host:adk-cdx"),
                thread_id: Some("200"),
                thread_title: Some("[AgentDesk] #558 token audit"),
                channel_id: "100",
                agent_id: Some("agent-1"),
                provider: Some("claude"),
                session_id: Some("session-abc"),
                dispatch_id: Some("dispatch-1"),
                started_at: Some("2026-04-14 10:00:00"),
                finished_at: Some("2026-04-14 10:00:12"),
                duration_ms: Some(12_000),
                token_usage: TurnTokenUsage {
                    input_tokens: 100,
                    cache_create_tokens: 20,
                    cache_read_tokens: 30,
                    output_tokens: 40,
                },
            },
        )
        .unwrap();

        let conn = db.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT thread_id, thread_title, channel_id, agent_id, provider, session_id,
                        dispatch_id, started_at, finished_at, duration_ms,
                        input_tokens, cache_create_tokens, cache_read_tokens, output_tokens
                 FROM turns WHERE turn_id = 'discord:1:2'",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, Option<i64>>(9)?,
                        row.get::<_, i64>(10)?,
                        row.get::<_, i64>(11)?,
                        row.get::<_, i64>(12)?,
                        row.get::<_, i64>(13)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(row.0.as_deref(), Some("200"));
        assert_eq!(row.1.as_deref(), Some("[AgentDesk] #558 token audit"));
        assert_eq!(row.2, "100");
        assert_eq!(row.3.as_deref(), Some("agent-1"));
        assert_eq!(row.4.as_deref(), Some("claude"));
        assert_eq!(row.5.as_deref(), Some("session-abc"));
        assert_eq!(row.6.as_deref(), Some("dispatch-1"));
        assert_eq!(row.7, "2026-04-14 10:00:00");
        assert_eq!(row.8, "2026-04-14 10:00:12");
        assert_eq!(row.9, Some(12_000));
        assert_eq!(row.10, 100);
        assert_eq!(row.11, 20);
        assert_eq!(row.12, 30);
        assert_eq!(row.13, 40);
    }

    #[test]
    fn upsert_turn_preserves_existing_token_usage_when_followup_has_zeros() {
        let db = test_db();
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO agents (id, name) VALUES ('agent-1', 'Agent One')",
                [],
            )
            .unwrap();

        upsert_turn(
            &db,
            PersistTurn {
                turn_id: "discord:1:2",
                session_key: None,
                thread_id: None,
                thread_title: None,
                channel_id: "100",
                agent_id: Some("agent-1"),
                provider: Some("claude"),
                session_id: Some("session-abc"),
                dispatch_id: None,
                started_at: Some("2026-04-14 10:00:00"),
                finished_at: Some("2026-04-14 10:00:12"),
                duration_ms: Some(12_000),
                token_usage: TurnTokenUsage {
                    input_tokens: 10,
                    cache_create_tokens: 2,
                    cache_read_tokens: 3,
                    output_tokens: 4,
                },
            },
        )
        .unwrap();

        upsert_turn(
            &db,
            PersistTurn {
                turn_id: "discord:1:2",
                session_key: Some("claude/token/host:adk-cdx"),
                thread_id: Some("200"),
                thread_title: Some("[AgentDesk] #558 token audit"),
                channel_id: "100",
                agent_id: None,
                provider: None,
                session_id: None,
                dispatch_id: Some("dispatch-1"),
                started_at: None,
                finished_at: Some("2026-04-14 10:00:13"),
                duration_ms: None,
                token_usage: TurnTokenUsage::default(),
            },
        )
        .unwrap();

        let conn = db.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT session_key, thread_id, thread_title, dispatch_id,
                        input_tokens, cache_create_tokens, cache_read_tokens, output_tokens
                 FROM turns WHERE turn_id = 'discord:1:2'",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(row.0.as_deref(), Some("claude/token/host:adk-cdx"));
        assert_eq!(row.1.as_deref(), Some("200"));
        assert_eq!(row.2.as_deref(), Some("[AgentDesk] #558 token audit"));
        assert_eq!(row.3.as_deref(), Some("dispatch-1"));
        assert_eq!(row.4, 10);
        assert_eq!(row.5, 2);
        assert_eq!(row.6, 3);
        assert_eq!(row.7, 4);
    }

    #[test]
    fn upsert_turn_owned_on_separate_conn_persists_turn_rows() {
        let db = test_db();
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO agents (id, name) VALUES ('agent-1', 'Agent One')",
                [],
            )
            .unwrap();

        upsert_turn_owned_on_separate_conn(
            &db,
            &PersistTurnOwned {
                turn_id: "discord:2:3".to_string(),
                session_key: Some("claude/token/host:adk-cdx".to_string()),
                thread_id: Some("201".to_string()),
                thread_title: Some("[AgentDesk] #593 turns persistence".to_string()),
                channel_id: "101".to_string(),
                agent_id: Some("agent-1".to_string()),
                provider: Some("claude".to_string()),
                session_id: Some("session-def".to_string()),
                dispatch_id: Some("dispatch-2".to_string()),
                started_at: Some("2026-04-15 07:00:00".to_string()),
                finished_at: Some("2026-04-15 07:00:08".to_string()),
                duration_ms: Some(8_000),
                token_usage: TurnTokenUsage {
                    input_tokens: 55,
                    cache_create_tokens: 5,
                    cache_read_tokens: 7,
                    output_tokens: 11,
                },
            },
        )
        .unwrap();

        let conn = db.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT thread_title, session_id, input_tokens, cache_create_tokens,
                        cache_read_tokens, output_tokens
                 FROM turns WHERE turn_id = 'discord:2:3'",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(row.0.as_deref(), Some("[AgentDesk] #593 turns persistence"));
        assert_eq!(row.1.as_deref(), Some("session-def"));
        assert_eq!(row.2, 55);
        assert_eq!(row.3, 5);
        assert_eq!(row.4, 7);
        assert_eq!(row.5, 11);
    }
}
