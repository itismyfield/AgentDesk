use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

use crate::config::AgentDef;
use crate::db::Db;
use crate::services::provider::ProviderKind;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentChannelBindings {
    pub provider: Option<String>,
    pub discord_channel_id: Option<String>,
    pub discord_channel_alt: Option<String>,
    pub discord_channel_cc: Option<String>,
    pub discord_channel_cdx: Option<String>,
}

impl AgentChannelBindings {
    pub fn primary_channel(&self) -> Option<String> {
        match self.provider.as_deref().and_then(ProviderKind::from_str) {
            Some(ProviderKind::Claude) => self
                .claude_channel()
                .or_else(|| self.codex_channel())
                .or_else(|| self.legacy_primary_channel()),
            Some(ProviderKind::Codex) => self
                .codex_channel()
                .or_else(|| self.claude_channel())
                .or_else(|| self.legacy_primary_channel()),
            Some(_) | None => self
                .legacy_primary_channel()
                .or_else(|| self.codex_channel())
                .or_else(|| self.claude_channel()),
        }
    }

    pub fn counter_model_channel(&self) -> Option<String> {
        let target = self
            .provider
            .as_deref()
            .and_then(ProviderKind::from_str)
            .unwrap_or(ProviderKind::Claude)
            .counterpart();
        self.channel_for_provider(Some(target.as_str()))
    }

    pub fn channel_for_provider(&self, provider: Option<&str>) -> Option<String> {
        match provider.and_then(ProviderKind::from_str) {
            Some(ProviderKind::Claude) => self.claude_channel(),
            Some(ProviderKind::Codex) => self.codex_channel(),
            _ => self.legacy_primary_channel(),
        }
    }

    pub fn all_channels(&self) -> Vec<String> {
        let mut channels = Vec::new();
        for value in [
            self.discord_channel_id.clone(),
            self.discord_channel_alt.clone(),
            self.discord_channel_cc.clone(),
            self.discord_channel_cdx.clone(),
        ] {
            if let Some(channel) = normalized_channel(value) {
                if !channels.contains(&channel) {
                    channels.push(channel);
                }
            }
        }
        channels
    }

    fn claude_channel(&self) -> Option<String> {
        normalized_channel(self.discord_channel_cc.clone())
            .or_else(|| normalized_channel(self.discord_channel_id.clone()))
    }

    fn codex_channel(&self) -> Option<String> {
        normalized_channel(self.discord_channel_cdx.clone())
            .or_else(|| normalized_channel(self.discord_channel_alt.clone()))
    }

    fn legacy_primary_channel(&self) -> Option<String> {
        normalized_channel(self.discord_channel_id.clone())
            .or_else(|| normalized_channel(self.discord_channel_cc.clone()))
    }
}

fn normalized_channel(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub fn load_agent_channel_bindings(
    conn: &Connection,
    agent_id: &str,
) -> rusqlite::Result<Option<AgentChannelBindings>> {
    conn.query_row(
        "SELECT provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
         FROM agents WHERE id = ?1",
        [agent_id],
        |row| {
            Ok(AgentChannelBindings {
                provider: row.get(0)?,
                discord_channel_id: row.get(1)?,
                discord_channel_alt: row.get(2)?,
                discord_channel_cc: row.get(3)?,
                discord_channel_cdx: row.get(4)?,
            })
        },
    )
    .optional()
}

pub fn resolve_agent_primary_channel_on_conn(
    conn: &Connection,
    agent_id: &str,
) -> rusqlite::Result<Option<String>> {
    Ok(load_agent_channel_bindings(conn, agent_id)?.and_then(|b| b.primary_channel()))
}

pub fn resolve_agent_counter_model_channel_on_conn(
    conn: &Connection,
    agent_id: &str,
) -> rusqlite::Result<Option<String>> {
    Ok(load_agent_channel_bindings(conn, agent_id)?.and_then(|b| b.counter_model_channel()))
}

pub fn resolve_agent_channel_for_provider_on_conn(
    conn: &Connection,
    agent_id: &str,
    provider: Option<&str>,
) -> rusqlite::Result<Option<String>> {
    Ok(load_agent_channel_bindings(conn, agent_id)?.and_then(|b| b.channel_for_provider(provider)))
}

pub fn resolve_agent_dispatch_channel_on_conn(
    conn: &Connection,
    agent_id: &str,
    dispatch_type: Option<&str>,
) -> rusqlite::Result<Option<String>> {
    Ok(
        load_agent_channel_bindings(conn, agent_id)?.and_then(|bindings| {
            if matches!(dispatch_type, Some("review" | "e2e-test" | "consultation")) {
                bindings.counter_model_channel()
            } else {
                bindings.primary_channel()
            }
        }),
    )
}

/// Upsert agents from config into the agents table.
/// Only updates fields that come from config; leaves status/xp/skills untouched.
pub fn sync_agents_from_config(db: &Db, agents: &[AgentDef]) -> Result<usize> {
    let conn = db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock error: {e}"))?;
    let mut count = 0;

    for agent in agents {
        let discord_channel_cc = agent.channels.get("claude").cloned();
        let discord_channel_cdx = agent.channels.get("codex").cloned();
        let discord_channel_id = discord_channel_cc.clone();
        let discord_channel_alt = discord_channel_cdx.clone();

        conn.execute(
            "INSERT INTO agents (
                id, name, name_ko, provider, department, avatar_emoji,
                discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                name_ko = excluded.name_ko,
                provider = excluded.provider,
                department = excluded.department,
                avatar_emoji = excluded.avatar_emoji,
                discord_channel_id = excluded.discord_channel_id,
                discord_channel_alt = excluded.discord_channel_alt,
                discord_channel_cc = excluded.discord_channel_cc,
                discord_channel_cdx = excluded.discord_channel_cdx,
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
                discord_channel_cc,
                discord_channel_cdx,
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

        let cc: Option<String> = conn
            .query_row(
                "SELECT discord_channel_cc FROM agents WHERE id = 'ag-01'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cc, Some("111".into()));

        let cdx: Option<String> = conn
            .query_row(
                "SELECT discord_channel_cdx FROM agents WHERE id = 'ag-01'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cdx, Some("222".into()));
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

    #[test]
    fn resolve_primary_and_counter_model_channels_follow_provider_specific_columns() {
        let db = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (
                id, name, provider,
                discord_channel_id, discord_channel_alt,
                discord_channel_cc, discord_channel_cdx
            ) VALUES ('ag-01', 'Alpha', 'codex', 'legacy-cc', 'legacy-cdx', 'cc-chan', 'cdx-chan')",
            [],
        )
        .unwrap();

        let bindings = load_agent_channel_bindings(&conn, "ag-01")
            .unwrap()
            .expect("bindings");
        assert_eq!(bindings.primary_channel(), Some("cdx-chan".into()));
        assert_eq!(bindings.counter_model_channel(), Some("cc-chan".into()));
        assert_eq!(
            resolve_agent_dispatch_channel_on_conn(&conn, "ag-01", Some("review")).unwrap(),
            Some("cc-chan".into())
        );
        assert_eq!(
            resolve_agent_dispatch_channel_on_conn(&conn, "ag-01", Some("implementation")).unwrap(),
            Some("cdx-chan".into())
        );
    }

    #[test]
    fn single_provider_channel_falls_back_to_any_available() {
        let db = test_db();
        let conn = db.lock().unwrap();
        // Claude agent (DEFAULT provider) with only codex channels configured
        conn.execute(
            "INSERT INTO agents (
                id, name,
                discord_channel_id, discord_channel_alt,
                discord_channel_cc, discord_channel_cdx
            ) VALUES ('ag-02', 'Legacy', NULL, 'legacy-cdx', NULL, 'cdx-chan')",
            [],
        )
        .unwrap();

        let bindings = load_agent_channel_bindings(&conn, "ag-02")
            .unwrap()
            .expect("bindings");
        // Falls back to codex channel since no claude channel exists
        assert_eq!(bindings.primary_channel(), Some("cdx-chan".into()));
        assert_eq!(bindings.counter_model_channel(), Some("cdx-chan".into()));
        assert_eq!(
            resolve_agent_dispatch_channel_on_conn(&conn, "ag-02", Some("implementation")).unwrap(),
            Some("cdx-chan".into())
        );
        assert_eq!(
            resolve_agent_dispatch_channel_on_conn(&conn, "ag-02", Some("review")).unwrap(),
            Some("cdx-chan".into())
        );
    }

    #[test]
    fn single_channel_codex_agent_falls_back_to_claude_channel() {
        let db = test_db();
        let conn = db.lock().unwrap();
        // Codex agent with only a claude channel configured
        conn.execute(
            "INSERT INTO agents (
                id, name, provider,
                discord_channel_id, discord_channel_alt,
                discord_channel_cc, discord_channel_cdx
            ) VALUES ('ag-03', 'SingleCh', 'codex', 'cc-only', NULL, 'cc-only', NULL)",
            [],
        )
        .unwrap();

        let bindings = load_agent_channel_bindings(&conn, "ag-03")
            .unwrap()
            .expect("bindings");
        // Should fall back to claude channel instead of returning None
        assert_eq!(bindings.primary_channel(), Some("cc-only".into()));
        assert_eq!(
            resolve_agent_dispatch_channel_on_conn(&conn, "ag-03", Some("implementation")).unwrap(),
            Some("cc-only".into())
        );
    }

    #[test]
    fn non_claude_codex_provider_falls_back_to_any_channel() {
        let db = test_db();
        let conn = db.lock().unwrap();
        // Gemini agent with only discord_channel_cdx populated
        conn.execute(
            "INSERT INTO agents (
                id, name, provider,
                discord_channel_id, discord_channel_alt,
                discord_channel_cc, discord_channel_cdx
            ) VALUES ('ag-04', 'GeminiAgent', 'gemini', NULL, NULL, NULL, 'cdx-chan')",
            [],
        )
        .unwrap();

        let bindings = load_agent_channel_bindings(&conn, "ag-04")
            .unwrap()
            .expect("bindings");
        // Gemini hits Some(_) branch — should fall back to codex channel
        assert_eq!(bindings.primary_channel(), Some("cdx-chan".into()));
    }

    #[test]
    fn single_channel_claude_agent_counter_model_returns_none() {
        let db = test_db();
        let conn = db.lock().unwrap();
        // Claude agent with only cc channel — no codex channel at all
        conn.execute(
            "INSERT INTO agents (
                id, name, provider,
                discord_channel_id, discord_channel_alt,
                discord_channel_cc, discord_channel_cdx
            ) VALUES ('ag-05', 'SingleCC', 'claude', 'cc-only', NULL, 'cc-only', NULL)",
            [],
        )
        .unwrap();

        let bindings = load_agent_channel_bindings(&conn, "ag-05")
            .unwrap()
            .expect("bindings");
        assert_eq!(bindings.primary_channel(), Some("cc-only".into()));
        // counter_model returns None — no codex channel exists.
        // This triggers PM-decision in onReviewEnter instead of routing
        // to the same channel (which would strand the review).
        assert_eq!(bindings.counter_model_channel(), None);
        assert_eq!(
            resolve_agent_dispatch_channel_on_conn(&conn, "ag-05", Some("review")).unwrap(),
            None
        );
    }
}
