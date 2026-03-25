use anyhow::Result;

use crate::config::AgentDef;
use crate::db::Db;

/// Upsert agents from config into the agents table.
/// Only updates fields that come from config; leaves status/xp/skills untouched.
pub fn sync_agents_from_config(db: &Db, agents: &[AgentDef]) -> Result<usize> {
    let conn = db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock error: {e}"))?;
    let mut count = 0;

    for agent in agents {
        let discord_channel_id = agent.channels.get("claude").cloned();
        let discord_channel_alt = agent.channels.get("codex").cloned();

        conn.execute(
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

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

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

        let count = sync_agents_from_config(&db, &agents).unwrap();
        assert_eq!(count, 1);

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
        let count = sync_agents_from_config(&db, &[]).unwrap();
        assert_eq!(count, 0);
    }
}
