use anyhow::{Result, anyhow};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::db::Db;

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
}

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

pub fn persist_turn(db: &Db, entry: PersistSessionTranscript<'_>) -> Result<bool> {
    let mut conn = db
        .lock()
        .map_err(|e| anyhow!("db lock failed while persisting transcript: {e}"))?;
    persist_turn_on_conn(&mut conn, entry)
}

pub fn persist_turn_on_conn(
    conn: &mut Connection,
    entry: PersistSessionTranscript<'_>,
) -> Result<bool> {
    let turn_id = entry.turn_id.trim();
    if turn_id.is_empty() {
        return Err(anyhow!("turn_id is required"));
    }

    let user_message = entry.user_message.trim();
    let assistant_message = entry.assistant_message.trim();
    if user_message.is_empty() && assistant_message.is_empty() {
        return Ok(false);
    }

    let session_key = normalized_opt(entry.session_key);
    let channel_id = normalized_opt(entry.channel_id);
    let provider = normalized_opt(entry.provider);
    let dispatch_id = normalized_opt(entry.dispatch_id);
    let agent_id = normalized_opt(entry.agent_id).or_else(|| {
        session_key.as_deref().and_then(|session_key| {
            conn.query_row(
                "SELECT agent_id FROM sessions WHERE session_key = ?1",
                [session_key],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        })
    });
    let search_document = build_search_document(user_message, assistant_message);

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
            assistant_message
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(turn_id) DO UPDATE SET
            session_key = excluded.session_key,
            channel_id = excluded.channel_id,
            agent_id = COALESCE(excluded.agent_id, session_transcripts.agent_id),
            provider = excluded.provider,
            dispatch_id = excluded.dispatch_id,
            user_message = excluded.user_message,
            assistant_message = excluded.assistant_message",
        params![
            turn_id,
            session_key,
            channel_id,
            agent_id,
            provider,
            dispatch_id,
            user_message,
            assistant_message,
        ],
    )?;

    let row_id: i64 = tx.query_row(
        "SELECT id FROM session_transcripts WHERE turn_id = ?1",
        [turn_id],
        |row| row.get(0),
    )?;

    tx.execute(
        "DELETE FROM session_transcripts_fts WHERE session_transcript_id = ?1",
        [row_id],
    )?;
    tx.execute(
        "INSERT INTO session_transcripts_fts (session_transcript_id, content)
         VALUES (?1, ?2)",
        params![row_id, search_document],
    )?;
    tx.commit()?;

    Ok(true)
}

pub fn search_transcripts(
    conn: &Connection,
    raw_query: &str,
    limit: usize,
) -> Result<(String, Vec<SessionTranscriptSearchHit>)> {
    let match_query = build_match_query(raw_query)
        .ok_or_else(|| anyhow!("query must contain a searchable term"))?;
    let limit = limit.clamp(1, 50) as i64;

    let mut stmt = conn.prepare(
        "SELECT st.id,
                st.turn_id,
                st.session_key,
                st.channel_id,
                st.agent_id,
                st.provider,
                st.dispatch_id,
                st.user_message,
                st.assistant_message,
                st.created_at,
                snippet(session_transcripts_fts, 1, '<mark>', '</mark>', '…', 18) AS snippet,
                bm25(session_transcripts_fts) AS score
         FROM session_transcripts_fts
         JOIN session_transcripts st
           ON st.id = session_transcripts_fts.session_transcript_id
         WHERE session_transcripts_fts MATCH ?1
         ORDER BY score ASC, st.created_at DESC
         LIMIT ?2",
    )?;

    let rows = stmt.query_map(params![match_query.as_str(), limit], |row| {
        Ok(SessionTranscriptSearchHit {
            id: row.get(0)?,
            turn_id: row.get(1)?,
            session_key: row.get(2)?,
            channel_id: row.get(3)?,
            agent_id: row.get(4)?,
            provider: row.get(5)?,
            dispatch_id: row.get(6)?,
            user_message: row.get(7)?,
            assistant_message: row.get(8)?,
            created_at: row.get(9)?,
            snippet: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
            score: row.get::<_, Option<f64>>(11)?.unwrap_or_default(),
        })
    })?;

    let hits = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((match_query, hits))
}

pub fn build_match_query(raw_query: &str) -> Option<String> {
    let terms: Vec<String> = sanitize_match_terms(raw_query)
        .into_iter()
        .map(|term| format!("{term}*"))
        .collect();

    if !terms.is_empty() {
        return Some(terms.join(" AND "));
    }

    None
}

fn build_search_document(user_message: &str, assistant_message: &str) -> String {
    match (user_message.is_empty(), assistant_message.is_empty()) {
        (false, false) => format!("user:\n{user_message}\n\nassistant:\n{assistant_message}"),
        (false, true) => format!("user:\n{user_message}"),
        (true, false) => format!("assistant:\n{assistant_message}"),
        (true, true) => String::new(),
    }
}

fn sanitize_match_terms(raw: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();

    for ch in raw.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch);
        } else if !current.is_empty() {
            terms.push(std::mem::take(&mut current));
        }
    }

    if !current.is_empty() {
        terms.push(current);
    }

    terms
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

    #[test]
    fn build_match_query_sanitizes_symbols() {
        assert_eq!(
            build_match_query("FTS5 #239 session-search"),
            Some("FTS5* AND 239* AND session* AND search*".to_string())
        );
    }

    #[test]
    fn build_match_query_supports_korean_terms() {
        assert_eq!(
            build_match_query("세션 검색"),
            Some("세션* AND 검색*".to_string())
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
            },
        )
        .unwrap();

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
            },
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_transcripts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);

        let assistant_message: String = conn
            .query_row(
                "SELECT assistant_message FROM session_transcripts WHERE turn_id = 'discord:1:2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(assistant_message, "new answer");
    }

    #[test]
    fn search_transcripts_returns_matching_turns() {
        let db = crate::db::test_db();
        {
            let mut conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name) VALUES ('agent-2', 'Agent 2')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions (session_key, agent_id, provider, status, created_at)
                 VALUES ('host:tmux-2', 'agent-2', 'claude', 'idle', datetime('now'))",
                [],
            )
            .unwrap();
            persist_turn_on_conn(
                &mut conn,
                PersistSessionTranscript {
                    turn_id: "discord:2:3",
                    session_key: Some("host:tmux-2"),
                    channel_id: Some("2"),
                    agent_id: None,
                    provider: Some("claude"),
                    dispatch_id: Some("dispatch-2"),
                    user_message: "FTS5 검색 구현 상태 알려줘",
                    assistant_message: "session transcript 검색 API를 추가했습니다.",
                },
            )
            .unwrap();
        }

        let conn = db.read_conn().unwrap();
        let (_match_query, hits) = search_transcripts(&conn, "FTS5 검색", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].agent_id.as_deref(), Some("agent-2"));
        assert!(hits[0].snippet.contains("FTS5") || hits[0].snippet.contains("검색"));
    }
}
