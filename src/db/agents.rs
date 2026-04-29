use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
#[cfg(test)]
use rusqlite::{Connection, OptionalExtension};
use sqlx::{PgPool, Row as SqlxRow};

use crate::config::{AgentChannel, AgentDef};
#[cfg(test)]
use crate::db::Db;
use crate::services::provider::ProviderKind;

const LEGACY_AGENT_PREFIX: &str = "openclaw-";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentChannelBindings {
    pub provider: Option<String>,
    pub discord_channel_id: Option<String>,
    pub discord_channel_alt: Option<String>,
    pub discord_channel_cc: Option<String>,
    pub discord_channel_cdx: Option<String>,
}

impl AgentChannelBindings {
    fn configured_provider_kind(&self) -> Option<ProviderKind> {
        self.provider.as_deref().and_then(ProviderKind::from_str)
    }

    fn primary_provider_kind(&self) -> Option<ProviderKind> {
        self.configured_provider_kind()
            .or_else(ProviderKind::default_channel_provider)
    }

    pub(crate) fn resolved_primary_provider_kind(&self) -> Option<ProviderKind> {
        let configured_provider = self.primary_provider_kind()?;
        if self
            .provider_specific_channel(&configured_provider)
            .is_some()
        {
            return Some(configured_provider);
        }

        configured_provider
            .preferred_counterparts()
            .into_iter()
            .find(|provider| self.provider_specific_channel(provider).is_some())
    }

    fn provider_specific_channel(&self, provider: &ProviderKind) -> Option<String> {
        match provider {
            ProviderKind::Claude => self.claude_channel(),
            ProviderKind::Codex => self.codex_channel(),
            ProviderKind::OpenCode
                if self.configured_provider_kind() == Some(ProviderKind::OpenCode) =>
            {
                self.legacy_primary_channel()
            }
            _ => None,
        }
    }

    pub fn primary_channel(&self) -> Option<String> {
        if let Some(primary_provider) = self.resolved_primary_provider_kind() {
            if let Some(channel) = self.provider_specific_channel(&primary_provider) {
                return Some(channel);
            }
        }
        self.legacy_primary_channel()
            .or_else(|| self.codex_channel())
            .or_else(|| self.claude_channel())
    }

    pub fn counter_model_channel(&self) -> Option<String> {
        self.resolved_primary_provider_kind().and_then(|provider| {
            let primary_channel = self.provider_specific_channel(&provider)?;
            provider
                .preferred_counterparts()
                .into_iter()
                .find_map(|counterpart| self.provider_specific_channel(&counterpart))
                .filter(|channel| channel != &primary_channel)
        })
    }

    pub fn channel_for_provider(&self, provider: Option<&str>) -> Option<String> {
        match provider.and_then(ProviderKind::from_str) {
            Some(kind) => self
                .provider_specific_channel(&kind)
                .or_else(|| self.legacy_primary_channel()),
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

#[cfg(test)]
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

#[cfg(test)]
pub fn resolve_agent_primary_channel_on_conn(
    conn: &Connection,
    agent_id: &str,
) -> rusqlite::Result<Option<String>> {
    Ok(load_agent_channel_bindings(conn, agent_id)?.and_then(|b| b.primary_channel()))
}

#[cfg(test)]
pub fn resolve_agent_channel_for_provider_on_conn(
    conn: &Connection,
    agent_id: &str,
    provider: Option<&str>,
) -> rusqlite::Result<Option<String>> {
    Ok(load_agent_channel_bindings(conn, agent_id)?.and_then(|b| b.channel_for_provider(provider)))
}

#[cfg(test)]
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

pub async fn load_agent_channel_bindings_pg(
    pool: &PgPool,
    agent_id: &str,
) -> Result<Option<AgentChannelBindings>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
         FROM agents
         WHERE id = $1",
    )
    .bind(agent_id)
    .fetch_optional(pool)
    .await?;

    row.map(|row| {
        Ok(AgentChannelBindings {
            provider: row.try_get("provider")?,
            discord_channel_id: row.try_get("discord_channel_id")?,
            discord_channel_alt: row.try_get("discord_channel_alt")?,
            discord_channel_cc: row.try_get("discord_channel_cc")?,
            discord_channel_cdx: row.try_get("discord_channel_cdx")?,
        })
    })
    .transpose()
}

pub async fn load_all_agent_channel_bindings_pg(
    pool: &PgPool,
) -> Result<BTreeMap<String, AgentChannelBindings>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
         FROM agents",
    )
    .fetch_all(pool)
    .await?;

    let mut bindings = BTreeMap::new();
    for row in rows {
        let agent_id: String = row.try_get("id")?;
        bindings.insert(
            agent_id,
            AgentChannelBindings {
                provider: row.try_get("provider")?,
                discord_channel_id: row.try_get("discord_channel_id")?,
                discord_channel_alt: row.try_get("discord_channel_alt")?,
                discord_channel_cc: row.try_get("discord_channel_cc")?,
                discord_channel_cdx: row.try_get("discord_channel_cdx")?,
            },
        );
    }

    Ok(bindings)
}

pub async fn resolve_agent_primary_channel_pg(
    pool: &PgPool,
    agent_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    Ok(load_agent_channel_bindings_pg(pool, agent_id)
        .await?
        .and_then(|bindings| bindings.primary_channel()))
}

pub async fn resolve_agent_counter_model_channel_pg(
    pool: &PgPool,
    agent_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    Ok(load_agent_channel_bindings_pg(pool, agent_id)
        .await?
        .and_then(|bindings| bindings.counter_model_channel()))
}

pub async fn resolve_agent_channel_for_provider_pg(
    pool: &PgPool,
    agent_id: &str,
    provider: Option<&str>,
) -> Result<Option<String>, sqlx::Error> {
    Ok(load_agent_channel_bindings_pg(pool, agent_id)
        .await?
        .and_then(|bindings| bindings.channel_for_provider(provider)))
}

pub async fn resolve_agent_dispatch_channel_pg(
    pool: &PgPool,
    agent_id: &str,
    dispatch_type: Option<&str>,
) -> Result<Option<String>, sqlx::Error> {
    Ok(load_agent_channel_bindings_pg(pool, agent_id)
        .await?
        .and_then(|bindings| {
            if matches!(dispatch_type, Some("review" | "e2e-test" | "consultation")) {
                bindings.counter_model_channel()
            } else {
                bindings.primary_channel()
            }
        }))
}

/// Upsert agents from config into the agents table.
/// Only updates fields that come from config; leaves status/xp/skills untouched.
#[cfg(test)]
pub fn sync_agents_from_config(db: &Db, agents: &[AgentDef]) -> Result<usize> {
    let conn = db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock error: {e}"))?;

    let config_ids: HashSet<&str> = agents.iter().map(|a| a.id.as_str()).collect();

    for agent in agents {
        upsert_agent_from_config_sqlite(&conn, agent)?;
    }
    migrate_legacy_agent_aliases_sqlite(&conn, agents, &config_ids)?;

    // Remove DB agents that are no longer in yaml config
    {
        let mut stmt = conn.prepare("SELECT id FROM agents")?;
        let db_ids: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        for db_id in &db_ids {
            if !config_ids.contains(db_id.as_str()) {
                conn.execute("DELETE FROM agents WHERE id = ?1", [db_id])?;
                tracing::info!("[agent-sync] Removed agent '{db_id}' (not in yaml config)");
            }
        }
    }

    Ok(agents.len())
}

#[cfg(test)]
fn legacy_agent_alias(agent_id: &str) -> Option<String> {
    if agent_id.starts_with(LEGACY_AGENT_PREFIX) {
        return None;
    }
    Some(format!("{LEGACY_AGENT_PREFIX}{agent_id}"))
}

#[cfg(test)]
fn sqlite_agent_exists(conn: &Connection, agent_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM agents WHERE id = ?1)",
        [agent_id],
        |row| row.get(0),
    )?)
}

#[cfg(test)]
fn upsert_agent_from_config_sqlite(conn: &Connection, agent: &AgentDef) -> Result<()> {
    let discord_channel_cc = agent
        .channels
        .claude
        .as_ref()
        .and_then(AgentChannel::target);
    let discord_channel_cdx = agent.channels.codex.as_ref().and_then(AgentChannel::target);
    // For providers without dedicated columns (gemini, opencode, qwen), store the
    // provider-specific channel in discord_channel_id so primary_channel() can find it.
    let provider_primary = match agent.provider.as_str() {
        "gemini" => agent
            .channels
            .gemini
            .as_ref()
            .and_then(AgentChannel::target),
        "opencode" => agent
            .channels
            .opencode
            .as_ref()
            .and_then(AgentChannel::target),
        "qwen" => agent.channels.qwen.as_ref().and_then(AgentChannel::target),
        _ => None,
    };
    let discord_channel_id = provider_primary.or_else(|| discord_channel_cc.clone());
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

    Ok(())
}

#[cfg(test)]
fn migrate_legacy_agent_aliases_sqlite(
    conn: &Connection,
    agents: &[AgentDef],
    config_ids: &HashSet<&str>,
) -> Result<()> {
    for agent in agents {
        let Some(legacy_id) = legacy_agent_alias(&agent.id) else {
            continue;
        };
        if config_ids.contains(legacy_id.as_str()) {
            tracing::info!(
                "[agent-sync] Preserving configured legacy agent '{}' while syncing '{}'",
                legacy_id,
                agent.id
            );
            continue;
        }

        if sqlite_agent_exists(conn, &legacy_id)? {
            copy_runtime_fields_from_legacy_sqlite(conn, &legacy_id, &agent.id)?;
        }
        move_legacy_agent_references_sqlite(conn, &legacy_id, &agent.id)?;

        if sqlite_agent_exists(conn, &legacy_id)? {
            conn.execute("DELETE FROM agents WHERE id = ?1", [&legacy_id])?;
            tracing::info!(
                "[agent-sync] Migrated legacy agent '{}' -> '{}'",
                legacy_id,
                agent.id
            );
        }
    }

    Ok(())
}

#[cfg(test)]
fn copy_runtime_fields_from_legacy_sqlite(
    conn: &Connection,
    legacy_id: &str,
    canonical_id: &str,
) -> Result<()> {
    if !sqlite_agent_exists(conn, legacy_id)? || !sqlite_agent_exists(conn, canonical_id)? {
        return Ok(());
    }

    conn.execute(
        "UPDATE agents
         SET status = (SELECT status FROM agents WHERE id = ?1),
             xp = (SELECT xp FROM agents WHERE id = ?1),
             skills = (SELECT skills FROM agents WHERE id = ?1),
             created_at = COALESCE((SELECT created_at FROM agents WHERE id = ?1), created_at),
             sprite_number = COALESCE((SELECT sprite_number FROM agents WHERE id = ?1), sprite_number),
             description = COALESCE((SELECT description FROM agents WHERE id = ?1), description),
             system_prompt = COALESCE((SELECT system_prompt FROM agents WHERE id = ?1), system_prompt),
             pipeline_config = COALESCE((SELECT pipeline_config FROM agents WHERE id = ?1), pipeline_config)
         WHERE id = ?2",
        rusqlite::params![legacy_id, canonical_id],
    )?;

    Ok(())
}

#[cfg(test)]
fn move_legacy_agent_references_sqlite(
    conn: &Connection,
    legacy_id: &str,
    canonical_id: &str,
) -> Result<()> {
    for sql in [
        "UPDATE github_repos SET default_agent_id = ?1 WHERE default_agent_id = ?2",
        "UPDATE pipeline_stages SET agent_override_id = ?1 WHERE agent_override_id = ?2",
        "UPDATE kanban_cards SET assigned_agent_id = ?1 WHERE assigned_agent_id = ?2",
        "UPDATE kanban_cards SET owner_agent_id = ?1 WHERE owner_agent_id = ?2",
        "UPDATE kanban_cards SET requester_agent_id = ?1 WHERE requester_agent_id = ?2",
        "UPDATE task_dispatches SET from_agent_id = ?1 WHERE from_agent_id = ?2",
        "UPDATE task_dispatches SET to_agent_id = ?1 WHERE to_agent_id = ?2",
        "UPDATE sessions SET agent_id = ?1 WHERE agent_id = ?2",
        "UPDATE meeting_transcripts SET speaker_agent_id = ?1 WHERE speaker_agent_id = ?2",
        "UPDATE skill_usage SET agent_id = ?1 WHERE agent_id = ?2",
        "UPDATE turns SET agent_id = ?1 WHERE agent_id = ?2",
        "UPDATE dispatch_outbox SET agent_id = ?1 WHERE agent_id = ?2",
        "UPDATE auto_queue_runs SET agent_id = ?1 WHERE agent_id = ?2",
        "UPDATE auto_queue_entries SET agent_id = ?1 WHERE agent_id = ?2",
        "UPDATE api_friction_events SET agent_id = ?1 WHERE agent_id = ?2",
        "UPDATE session_transcripts SET agent_id = ?1 WHERE agent_id = ?2",
        "UPDATE memento_feedback_turn_stats SET agent_id = ?1 WHERE agent_id = ?2",
    ] {
        conn.execute(sql, rusqlite::params![canonical_id, legacy_id])?;
    }

    conn.execute(
        "INSERT OR IGNORE INTO office_agents (office_id, agent_id, department_id, joined_at)
         SELECT office_id, ?1, department_id, joined_at
           FROM office_agents
          WHERE agent_id = ?2",
        rusqlite::params![canonical_id, legacy_id],
    )?;
    conn.execute("DELETE FROM office_agents WHERE agent_id = ?1", [legacy_id])?;

    conn.execute(
        "INSERT OR IGNORE INTO auto_queue_slots (
            agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map, created_at, updated_at
         )
         SELECT ?1, slot_index, assigned_run_id, assigned_thread_group, thread_id_map, created_at, updated_at
           FROM auto_queue_slots
          WHERE agent_id = ?2",
        rusqlite::params![canonical_id, legacy_id],
    )?;
    conn.execute(
        "DELETE FROM auto_queue_slots WHERE agent_id = ?1",
        [legacy_id],
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentChannels;

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
            channels: AgentChannels {
                claude: Some("111".into()),
                codex: Some("222".into()),
                gemini: None,
                opencode: None,
                qwen: None,
            },
            keywords: Vec::new(),
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
    fn sync_prefers_provider_primary_channel_over_claude_fallback() {
        let db = test_db();
        let agents = vec![AgentDef {
            id: "opencode-01".into(),
            name: "OpenCode".into(),
            name_ko: None,
            provider: "opencode".into(),
            channels: AgentChannels {
                claude: Some("claude-review".into()),
                opencode: Some("opencode-primary".into()),
                ..Default::default()
            },
            keywords: Vec::new(),
            department: Some("eng".into()),
            avatar_emoji: None,
        }];

        sync_agents_from_config(&db, &agents).unwrap();

        let conn = db.lock().unwrap();
        let (primary, claude): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT discord_channel_id, discord_channel_cc FROM agents WHERE id = 'opencode-01'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(primary.as_deref(), Some("opencode-primary"));
        assert_eq!(claude.as_deref(), Some("claude-review"));
    }

    #[test]
    fn sync_upserts_existing_agents() {
        let db = test_db();

        let agents_v1 = vec![AgentDef {
            id: "ag-01".into(),
            name: "Alpha".into(),
            name_ko: None,
            provider: "claude".into(),
            channels: AgentChannels::default(),
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
        }];
        sync_agents_from_config(&db, &agents_v1).unwrap();

        let agents_v2 = vec![AgentDef {
            id: "ag-01".into(),
            name: "Alpha-v2".into(),
            name_ko: Some("알파v2".into()),
            provider: "codex".into(),
            channels: AgentChannels {
                claude: Some("333".into()),
                codex: None,
                gemini: None,
                opencode: None,
                qwen: None,
            },
            keywords: Vec::new(),
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
    fn sync_migrates_legacy_openclaw_agent_ids_and_references() {
        let db = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (
                id, name, provider, status, xp, sprite_number, description, system_prompt, pipeline_config
             ) VALUES (?1, ?2, 'codex', 'working', 42, 7, 'legacy-desc', 'legacy-prompt', '{\"k\":1}')",
            rusqlite::params!["openclaw-maker", "Legacy Maker"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO github_repos (id, default_agent_id) VALUES ('owner/repo', 'openclaw-maker')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, assigned_agent_id) VALUES ('card-1', 'Card', 'openclaw-maker')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type, status, title)
             VALUES ('dispatch-1', 'card-1', 'openclaw-maker', 'openclaw-maker', 'implementation', 'pending', 'Dispatch')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (session_key, agent_id, status) VALUES ('sess-1', 'openclaw-maker', 'turn_active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meetings (id, title, status) VALUES ('meeting-1', 'Meeting', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meeting_transcripts (meeting_id, seq, round, speaker_agent_id, speaker_name, content)
             VALUES ('meeting-1', 1, 1, 'openclaw-maker', 'Maker', 'hello')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO office_agents (office_id, agent_id, department_id) VALUES ('office-1', 'openclaw-maker', 'engineering')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) VALUES ('run-1', 'owner/repo', 'openclaw-maker', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id) VALUES ('entry-1', 'run-1', 'card-1', 'openclaw-maker')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, assigned_run_id) VALUES ('openclaw-maker', 0, 'run-1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_transcripts (turn_id, agent_id, user_message, assistant_message) VALUES ('turn-1', 'openclaw-maker', 'u', 'a')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO turns (turn_id, channel_id, agent_id, provider, started_at, finished_at)
             VALUES ('turn-row-1', 'channel-1', 'openclaw-maker', 'codex', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();

        drop(conn);

        let agents = vec![AgentDef {
            id: "maker".into(),
            name: "Maker".into(),
            name_ko: Some("뚝딱이".into()),
            provider: "codex".into(),
            channels: AgentChannels {
                codex: Some("maker-cdx".into()),
                ..Default::default()
            },
            keywords: Vec::new(),
            department: Some("engineering".into()),
            avatar_emoji: Some("🛠️".into()),
        }];
        sync_agents_from_config(&db, &agents).unwrap();

        let conn = db.lock().unwrap();
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM agents ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|row| row.ok())
            .collect();
        assert_eq!(ids, vec!["maker".to_string()]);

        let (status, xp, sprite_number, description, system_prompt, pipeline_config): (
            String,
            i64,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT status, xp, sprite_number, description, system_prompt, pipeline_config
                 FROM agents
                 WHERE id = 'maker'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(status, "working");
        assert_eq!(xp, 42);
        assert_eq!(sprite_number, Some(7));
        assert_eq!(description.as_deref(), Some("legacy-desc"));
        assert_eq!(system_prompt.as_deref(), Some("legacy-prompt"));
        assert_eq!(pipeline_config.as_deref(), Some("{\"k\":1}"));

        for (sql, expected) in [
            (
                "SELECT default_agent_id FROM github_repos WHERE id = 'owner/repo'",
                Some("maker".to_string()),
            ),
            (
                "SELECT assigned_agent_id FROM kanban_cards WHERE id = 'card-1'",
                Some("maker".to_string()),
            ),
            (
                "SELECT to_agent_id FROM task_dispatches WHERE id = 'dispatch-1'",
                Some("maker".to_string()),
            ),
            (
                "SELECT agent_id FROM sessions WHERE session_key = 'sess-1'",
                Some("maker".to_string()),
            ),
            (
                "SELECT speaker_agent_id FROM meeting_transcripts WHERE meeting_id = 'meeting-1'",
                Some("maker".to_string()),
            ),
            (
                "SELECT agent_id FROM office_agents WHERE office_id = 'office-1'",
                Some("maker".to_string()),
            ),
            (
                "SELECT agent_id FROM auto_queue_runs WHERE id = 'run-1'",
                Some("maker".to_string()),
            ),
            (
                "SELECT agent_id FROM auto_queue_entries WHERE id = 'entry-1'",
                Some("maker".to_string()),
            ),
            (
                "SELECT agent_id FROM auto_queue_slots WHERE slot_index = 0",
                Some("maker".to_string()),
            ),
            (
                "SELECT agent_id FROM session_transcripts WHERE turn_id = 'turn-1'",
                Some("maker".to_string()),
            ),
            (
                "SELECT agent_id FROM turns WHERE turn_id = 'turn-row-1'",
                Some("maker".to_string()),
            ),
        ] {
            let actual: Option<String> = conn.query_row(sql, [], |row| row.get(0)).unwrap();
            assert_eq!(actual, expected, "query `{sql}`");
        }
    }

    #[test]
    fn sync_preserves_configured_legacy_openclaw_agent_id() {
        let db = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (
                id, name, provider, status, xp
             ) VALUES (?1, ?2, 'codex', 'working', 42)",
            rusqlite::params!["openclaw-maker", "Configured Legacy Maker"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO github_repos (id, default_agent_id) VALUES ('owner/repo', 'openclaw-maker')",
            [],
        )
        .unwrap();
        drop(conn);

        let agents = vec![
            AgentDef {
                id: "maker".into(),
                name: "Maker".into(),
                name_ko: None,
                provider: "codex".into(),
                channels: AgentChannels {
                    codex: Some("maker-cdx".into()),
                    ..Default::default()
                },
                keywords: Vec::new(),
                department: Some("engineering".into()),
                avatar_emoji: None,
            },
            AgentDef {
                id: "openclaw-maker".into(),
                name: "Legacy Maker".into(),
                name_ko: None,
                provider: "codex".into(),
                channels: AgentChannels {
                    codex: Some("legacy-cdx".into()),
                    ..Default::default()
                },
                keywords: Vec::new(),
                department: Some("legacy".into()),
                avatar_emoji: None,
            },
        ];
        sync_agents_from_config(&db, &agents).unwrap();

        let conn = db.lock().unwrap();
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM agents ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|row| row.ok())
            .collect();
        assert_eq!(ids, vec!["maker".to_string(), "openclaw-maker".to_string()]);

        let (status, xp): (String, i64) = conn
            .query_row(
                "SELECT status, xp FROM agents WHERE id = 'openclaw-maker'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "working");
        assert_eq!(xp, 42);

        let github_default: Option<String> = conn
            .query_row(
                "SELECT default_agent_id FROM github_repos WHERE id = 'owner/repo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(github_default.as_deref(), Some("openclaw-maker"));
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
    fn single_provider_channel_falls_back_to_available_primary_without_counter_model() {
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
        assert_eq!(bindings.counter_model_channel(), None);
        assert_eq!(
            resolve_agent_dispatch_channel_on_conn(&conn, "ag-02", Some("implementation")).unwrap(),
            Some("cdx-chan".into())
        );
        assert_eq!(
            resolve_agent_dispatch_channel_on_conn(&conn, "ag-02", Some("review")).unwrap(),
            None
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
        assert_eq!(bindings.counter_model_channel(), None);
        assert_eq!(
            resolve_agent_dispatch_channel_on_conn(&conn, "ag-03", Some("implementation")).unwrap(),
            Some("cc-only".into())
        );
        assert_eq!(
            resolve_agent_dispatch_channel_on_conn(&conn, "ag-03", Some("review")).unwrap(),
            None
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
        assert_eq!(bindings.counter_model_channel(), None);
    }

    #[test]
    fn non_claude_codex_provider_without_second_channel_has_no_counter_model() {
        let db = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (
                id, name, provider,
                discord_channel_id, discord_channel_alt,
                discord_channel_cc, discord_channel_cdx
            ) VALUES ('ag-04b', 'QwenAgent', 'qwen', 'cc-legacy', NULL, 'cc-chan', NULL)",
            [],
        )
        .unwrap();

        let bindings = load_agent_channel_bindings(&conn, "ag-04b")
            .unwrap()
            .expect("bindings");
        assert_eq!(bindings.primary_channel(), Some("cc-chan".into()));
        assert_eq!(bindings.counter_model_channel(), None);
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
        // counter_model returns None — no alternate provider channel exists.
        // Review automation now auto-approves instead of routing the review
        // back to the same provider/channel.
        assert_eq!(bindings.counter_model_channel(), None);
        assert_eq!(
            resolve_agent_dispatch_channel_on_conn(&conn, "ag-05", Some("review")).unwrap(),
            None
        );
    }

    #[test]
    fn opencode_legacy_primary_is_not_counter_model_for_claude_agent() {
        let db = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (
                id, name, provider,
                discord_channel_id, discord_channel_alt,
                discord_channel_cc, discord_channel_cdx
            ) VALUES ('ag-06', 'ClaudeOnly', 'claude', 'legacy-primary', NULL, 'cc-chan', NULL)",
            [],
        )
        .unwrap();

        let bindings = load_agent_channel_bindings(&conn, "ag-06")
            .unwrap()
            .expect("bindings");
        assert_eq!(bindings.primary_channel(), Some("cc-chan".into()));
        assert_eq!(bindings.counter_model_channel(), None);
    }
}
