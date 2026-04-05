use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Serialize;
use std::collections::HashSet;

use crate::config::AgentDef;
use crate::db::Db;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SyncAgentsResult {
    pub upserted: usize,
    pub pruned: usize,
    pub skipped_prune: usize,
}

/// Upsert agents from config into the agents table.
/// Only updates fields that come from config; leaves status/xp/skills untouched.
/// Agents that are no longer present in config are pruned when they are not
/// referenced by runtime records.
pub fn sync_agents_from_config(db: &Db, agents: &[AgentDef]) -> Result<SyncAgentsResult> {
    let mut conn = db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock error: {e}"))?;
    let tx = conn.transaction()?;
    let mut count = 0;

    for agent in agents {
        let discord_channel_id = agent.channels.get("claude").cloned();
        let discord_channel_alt = agent.channels.get("codex").cloned();

        tx.execute(
            "INSERT INTO agents (id, name, name_ko, provider, department, avatar_emoji, discord_channel_id, discord_channel_alt)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                name_ko = excluded.name_ko,
                provider = excluded.provider,
                department = excluded.department,
                avatar_emoji = excluded.avatar_emoji,
                discord_channel_id = excluded.discord_channel_id,
                discord_channel_alt = excluded.discord_channel_alt,
                updated_at = CURRENT_TIMESTAMP",
            rusqlite::params![
                agent.id,
                agent.name,
                agent.name_ko,
                agent.provider,
                agent.department,
                agent.avatar_emoji,
                discord_channel_id,
                discord_channel_alt,
            ],
        )?;
        count += 1;
    }

    let config_ids = agents
        .iter()
        .map(|agent| agent.id.as_str())
        .collect::<HashSet<_>>();
    let existing_ids = {
        let mut stmt = tx.prepare("SELECT id FROM agents")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }
        ids
    };

    let mut pruned = 0;
    let mut skipped_prune = 0;
    for agent_id in existing_ids {
        if config_ids.contains(agent_id.as_str()) {
            continue;
        }
        if has_runtime_references(&tx, &agent_id)? {
            skipped_prune += 1;
            continue;
        }
        tx.execute("DELETE FROM office_agents WHERE agent_id = ?1", [&agent_id])?;
        pruned += tx.execute("DELETE FROM agents WHERE id = ?1", [&agent_id])?;
    }

    tx.commit()?;

    Ok(SyncAgentsResult {
        upserted: count,
        pruned,
        skipped_prune,
    })
}

fn has_runtime_references(tx: &rusqlite::Transaction<'_>, agent_id: &str) -> Result<bool> {
    const TABLE_CHECKS: &[&str] = &[
        "SELECT 1 FROM kanban_cards WHERE assigned_agent_id = ?1 LIMIT 1",
        "SELECT 1 FROM kanban_cards WHERE owner_agent_id = ?1 LIMIT 1",
        "SELECT 1 FROM kanban_cards WHERE requester_agent_id = ?1 LIMIT 1",
        "SELECT 1 FROM task_dispatches WHERE from_agent_id = ?1 LIMIT 1",
        "SELECT 1 FROM task_dispatches WHERE to_agent_id = ?1 LIMIT 1",
        "SELECT 1 FROM sessions WHERE agent_id = ?1 LIMIT 1",
        "SELECT 1 FROM meeting_transcripts WHERE speaker_agent_id = ?1 LIMIT 1",
        "SELECT 1 FROM skill_usage WHERE agent_id = ?1 LIMIT 1",
        "SELECT 1 FROM github_repos WHERE default_agent_id = ?1 LIMIT 1",
        "SELECT 1 FROM pipeline_stages WHERE agent_override_id = ?1 LIMIT 1",
    ];

    for query in TABLE_CHECKS {
        let found = tx
            .query_row(query, [agent_id], |row| row.get::<_, i64>(0))
            .optional()?;
        if found.is_some() {
            return Ok(true);
        }
    }

    let message_found = tx
        .query_row(
            "SELECT 1
             FROM messages
             WHERE (sender_type = 'agent' AND sender_id = ?1)
                OR (receiver_type = 'agent' AND receiver_id = ?1)
             LIMIT 1",
            [agent_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(message_found.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    #[test]
    fn sync_inserts_new_agents() {
        let db = test_db();
        let agents = vec![AgentDef {
            id: "ag-01".into(),
            name: "Alpha".into(),
            name_ko: Some("알파".into()),
            provider: "claude".into(),
            channels: {
                let mut m = HashMap::new();
                m.insert("claude".into(), "111".into());
                m.insert("codex".into(), "222".into());
                m
            },
            department: Some("eng".into()),
            avatar_emoji: Some("🤖".into()),
        }];

        let result = sync_agents_from_config(&db, &agents).unwrap();
        assert_eq!(result.upserted, 1);
        assert_eq!(result.pruned, 0);

        let conn = db.lock().unwrap();
        let name: String = conn
            .query_row("SELECT name FROM agents WHERE id = 'ag-01'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(name, "Alpha");

        let ch: Option<String> = conn
            .query_row(
                "SELECT discord_channel_id FROM agents WHERE id = 'ag-01'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ch, Some("111".into()));

        let alt: Option<String> = conn
            .query_row(
                "SELECT discord_channel_alt FROM agents WHERE id = 'ag-01'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(alt, Some("222".into()));
    }

    #[test]
    fn sync_upserts_existing_agents() {
        let db = test_db();

        let agents_v1 = vec![AgentDef {
            id: "ag-01".into(),
            name: "Alpha".into(),
            name_ko: None,
            provider: "claude".into(),
            channels: HashMap::new(),
            department: None,
            avatar_emoji: None,
        }];
        sync_agents_from_config(&db, &agents_v1).unwrap();

        let agents_v2 = vec![AgentDef {
            id: "ag-01".into(),
            name: "Alpha-v2".into(),
            name_ko: Some("알파v2".into()),
            provider: "codex".into(),
            channels: {
                let mut m = HashMap::new();
                m.insert("claude".into(), "333".into());
                m
            },
            department: Some("design".into()),
            avatar_emoji: Some("🎨".into()),
        }];
        sync_agents_from_config(&db, &agents_v2).unwrap();

        let conn = db.lock().unwrap();
        let (name, provider): (String, String) = conn
            .query_row(
                "SELECT name, provider FROM agents WHERE id = 'ag-01'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(name, "Alpha-v2");
        assert_eq!(provider, "codex");
    }

    #[test]
    fn sync_empty_agents_is_noop() {
        let db = test_db();
        let result = sync_agents_from_config(&db, &[]).unwrap();
        assert_eq!(result.upserted, 0);
        assert_eq!(result.pruned, 0);
    }

    #[test]
    fn sync_prunes_db_only_agents_missing_from_config() {
        let db = test_db();
        let agent_id = "db-only-agent";
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider) VALUES (?1, 'Juno QA', 'claude')",
            [agent_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO office_agents (office_id, agent_id) VALUES ('office-1', ?1)",
            [agent_id],
        )
        .unwrap();
        drop(conn);

        let result = sync_agents_from_config(&db, &[]).unwrap();
        assert_eq!(
            result,
            SyncAgentsResult {
                upserted: 0,
                pruned: 1,
                skipped_prune: 0,
            }
        );

        let conn = db.lock().unwrap();
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agents WHERE id = ?1",
                [agent_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
        let office_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM office_agents WHERE agent_id = ?1",
                [agent_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(office_rows, 0);
    }

    #[test]
    fn sync_keeps_referenced_message_agents_missing_from_config() {
        let db = test_db();
        let agent_id = "legacy-agent";
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider) VALUES (?1, 'Legacy', 'claude')",
            [agent_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (sender_type, sender_id, receiver_type, receiver_id, content) VALUES ('agent', ?1, 'user', 'u-1', 'hello')",
            [agent_id],
        )
        .unwrap();
        drop(conn);

        let result = sync_agents_from_config(&db, &[]).unwrap();
        assert_eq!(result.pruned, 0);
        assert_eq!(result.skipped_prune, 1);

        let conn = db.lock().unwrap();
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agents WHERE id = ?1",
                [agent_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn sync_keeps_referenced_session_agents_missing_from_config() {
        let db = test_db();
        let agent_id = "session-agent";
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider) VALUES (?1, 'Mina Dev', 'codex')",
            [agent_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (session_key, agent_id, provider) VALUES ('sess-1', ?1, 'codex')",
            [agent_id],
        )
        .unwrap();
        drop(conn);

        let result = sync_agents_from_config(&db, &[]).unwrap();
        assert_eq!(result.pruned, 0);
        assert_eq!(result.skipped_prune, 1);

        let conn = db.lock().unwrap();
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agents WHERE id = ?1",
                [agent_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 1);
    }
}
