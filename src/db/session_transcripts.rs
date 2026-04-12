use anyhow::{Result, anyhow};
use rusqlite::{Connection, Row, params};
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::db::session_agent_resolution::resolve_agent_id_for_session;

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

impl SessionTranscriptEventKind {
    fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Thinking => "thinking",
            Self::ToolUse => "tool_use",
            Self::ToolResult => "tool_result",
            Self::Result => "result",
            Self::Error => "error",
            Self::Task => "task",
            Self::System => "system",
        }
    }
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

impl SessionTranscriptEvent {
    fn search_text(&self) -> String {
        let mut parts = Vec::new();
        parts.push(self.kind.label().to_string());
        if let Some(tool_name) = self.tool_name.as_deref().map(str::trim)
            && !tool_name.is_empty()
        {
            parts.push(tool_name.to_string());
        }
        if let Some(summary) = self.summary.as_deref().map(str::trim)
            && !summary.is_empty()
        {
            parts.push(summary.to_string());
        }
        let content = self.content.trim();
        if !content.is_empty() {
            parts.push(content.to_string());
        }
        if self.is_error {
            parts.push("error".to_string());
        }
        parts.join("\n")
    }
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
    let events = normalize_events(entry.events);
    if user_message.is_empty() && assistant_message.is_empty() && events.is_empty() {
        return Ok(false);
    }

    let session_key = normalized_opt(entry.session_key);
    let channel_id = normalized_opt(entry.channel_id);
    let provider = normalized_opt(entry.provider);
    let dispatch_id = normalized_opt(entry.dispatch_id);
    let agent_id = resolve_agent_id_for_session(
        conn,
        entry.agent_id,
        session_key.as_deref(),
        None,
        None,
        dispatch_id.as_deref(),
    );
    let events_json = serde_json::to_string(&events)?;
    let search_document = build_search_document(user_message, assistant_message, &events);

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
            turn_id,
            session_key,
            channel_id,
            agent_id,
            provider,
            dispatch_id,
            user_message,
            assistant_message,
            events_json,
            entry.duration_ms,
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
         WHERE COALESCE(NULLIF(TRIM(st.agent_id), ''), NULLIF(TRIM(s.agent_id), '')) = ?1
            OR (
                COALESCE(NULLIF(TRIM(st.agent_id), ''), NULLIF(TRIM(s.agent_id), '')) IS NULL
                AND td.to_agent_id = ?1
            )
         ORDER BY st.created_at DESC, st.id DESC
         LIMIT ?2",
    )?;

    let rows = stmt.query_map(params![agent_id, limit], session_transcript_record_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn session_transcript_record_from_row(row: &Row<'_>) -> rusqlite::Result<SessionTranscriptRecord> {
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

#[cfg(test)]
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

#[cfg(test)]
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

fn build_search_document(
    user_message: &str,
    assistant_message: &str,
    events: &[SessionTranscriptEvent],
) -> String {
    let mut sections = Vec::new();
    if !user_message.is_empty() {
        sections.push(format!("user:\n{user_message}"));
    }
    if !assistant_message.is_empty() {
        sections.push(format!("assistant:\n{assistant_message}"));
    }
    for event in events {
        let text = event.search_text();
        if !text.trim().is_empty() {
            sections.push(text);
        }
    }
    sections.join("\n\n")
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

#[cfg(test)]
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
            let events = vec![SessionTranscriptEvent {
                kind: SessionTranscriptEventKind::Thinking,
                tool_name: None,
                summary: Some("FTS5 검색 설계".to_string()),
                content: "검색 전략을 정리합니다.".to_string(),
                status: Some("info".to_string()),
                is_error: false,
            }];
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
                    events: &events,
                    duration_ms: Some(9000),
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
