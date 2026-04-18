use libsql_rusqlite::Connection;

use crate::db::agents::AgentChannelBindings;
use crate::services::provider::parse_provider_and_channel_from_tmux_name;

pub(crate) fn parse_thread_channel_name(channel_name: &str) -> Option<(&str, &str)> {
    let pos = channel_name.rfind("-t")?;
    let suffix = &channel_name[pos + 2..];
    if suffix.len() >= 15 && suffix.chars().all(|c| c.is_ascii_digit()) {
        Some((&channel_name[..pos], suffix))
    } else {
        None
    }
}

pub(crate) fn parse_channel_name_from_session_key(session_key: &str) -> Option<String> {
    // Session keys can be plain `host:tmux-name` or namespaced
    // `provider/token-hash/host:tmux-name`; the tmux session name is always the final segment.
    let (_, tmux_name) = session_key.rsplit_once(':')?;
    let (_, channel_name) = parse_provider_and_channel_from_tmux_name(tmux_name)?;
    Some(channel_name)
}

pub(crate) fn parse_thread_channel_id_from_session_key(session_key: &str) -> Option<String> {
    parse_channel_name_from_session_key(session_key).and_then(|channel_name| {
        parse_thread_channel_name(&channel_name).map(|(_, thread_id)| thread_id.to_string())
    })
}

pub(crate) fn normalize_thread_channel_id(thread_channel_id: Option<&str>) -> Option<String> {
    let trimmed = thread_channel_id?.trim();
    if trimmed.len() < 15 || !trimmed.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(trimmed.to_string())
}

fn normalize_nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn resolve_known_agent_id(conn: &Connection, agent_id: Option<&str>) -> Option<String> {
    let agent_id = normalize_nonempty(agent_id)?;
    let exists = conn
        .query_row(
            "SELECT 1 FROM agents WHERE id = ?1 LIMIT 1",
            [agent_id.as_str()],
            |_| Ok(()),
        )
        .is_ok();
    exists.then_some(agent_id)
}

fn resolve_agent_id_from_channel_name(conn: &Connection, channel_name: &str) -> Option<String> {
    if channel_name.is_empty() {
        return None;
    }

    conn.query_row(
        "SELECT id FROM agents
         WHERE discord_channel_id = ?1 OR discord_channel_alt = ?1
            OR discord_channel_cc = ?1 OR discord_channel_cdx = ?1",
        [channel_name],
        |row| row.get(0),
    )
    .ok()
    .or_else(|| {
        let mut stmt = conn
            .prepare(
                "SELECT id, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
                 FROM agents",
            )
            .ok()?;
        let mut rows = stmt.query([]).ok()?;
        while let Ok(Some(row)) = rows.next() {
            let id: String = row.get(0).ok()?;
            let bindings = AgentChannelBindings {
                provider: row.get(1).ok()?,
                discord_channel_id: row.get(2).ok()?,
                discord_channel_alt: row.get(3).ok()?,
                discord_channel_cc: row.get(4).ok()?,
                discord_channel_cdx: row.get(5).ok()?,
            };
            if bindings
                .all_channels()
                .iter()
                .any(|channel| channel_name.contains(channel))
            {
                return Some(id);
            }
        }
        None
    })
}

fn resolve_agent_id_from_dispatch_id(conn: &Connection, dispatch_id: &str) -> Option<String> {
    let agent_id: Option<String> = conn
        .query_row(
            "SELECT to_agent_id FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    resolve_known_agent_id(conn, agent_id.as_deref())
}

fn resolve_agent_id_from_thread_channel_id(
    conn: &Connection,
    thread_channel_id: &str,
) -> Option<String> {
    let thread_channel_id = normalize_thread_channel_id(Some(thread_channel_id))?;
    resolve_agent_id_from_channel_name(conn, &thread_channel_id)
        .or_else(|| {
            let agent_id: Option<String> = conn
                .query_row(
                    "SELECT to_agent_id
                     FROM task_dispatches
                     WHERE thread_id = ?1
                       AND NULLIF(TRIM(to_agent_id), '') IS NOT NULL
                     ORDER BY datetime(created_at) DESC
                     LIMIT 1",
                    [thread_channel_id.as_str()],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            resolve_known_agent_id(conn, agent_id.as_deref())
        })
        .or_else(|| {
            let agent_id: Option<String> = conn
                .query_row(
                    "SELECT assigned_agent_id
                     FROM kanban_cards
                     WHERE active_thread_id = ?1
                       AND NULLIF(TRIM(assigned_agent_id), '') IS NOT NULL
                     ORDER BY datetime(updated_at) DESC
                     LIMIT 1",
                    [thread_channel_id.as_str()],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            resolve_known_agent_id(conn, agent_id.as_deref())
        })
}

fn load_session_context(
    conn: &Connection,
    session_key: &str,
) -> Option<(Option<String>, Option<String>, Option<String>)> {
    conn.query_row(
        "SELECT agent_id, thread_channel_id, active_dispatch_id
         FROM sessions
         WHERE session_key = ?1",
        [session_key],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .ok()
}

pub(crate) fn resolve_agent_id_for_session(
    conn: &Connection,
    explicit_agent_id: Option<&str>,
    session_key: Option<&str>,
    session_name: Option<&str>,
    thread_channel_id: Option<&str>,
    dispatch_id: Option<&str>,
) -> Option<String> {
    if let Some(agent_id) = resolve_known_agent_id(conn, explicit_agent_id) {
        return Some(agent_id);
    }

    let session_key = session_key.map(str::trim).filter(|value| !value.is_empty());
    let session_name = session_name
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let session_key_channel_name = session_key.and_then(parse_channel_name_from_session_key);
    let session_key_thread_channel_id =
        session_key.and_then(parse_thread_channel_id_from_session_key);
    let provided_thread_channel_id = normalize_thread_channel_id(thread_channel_id);

    let (session_agent_id, session_thread_channel_id, session_dispatch_id) = session_key
        .and_then(|value| load_session_context(conn, value))
        .unwrap_or((None, None, None));
    let session_agent_id = normalize_nonempty(session_agent_id.as_deref());
    let session_thread_channel_id =
        normalize_thread_channel_id(session_thread_channel_id.as_deref());
    let session_dispatch_id = normalize_nonempty(session_dispatch_id.as_deref());

    if let Some(agent_id) = session_agent_id {
        return Some(agent_id);
    }

    for channel_name in [session_name, session_key_channel_name.as_deref()]
        .into_iter()
        .flatten()
    {
        let channel_name = parse_thread_channel_name(channel_name)
            .map(|(parent, _)| parent)
            .unwrap_or(channel_name);
        if let Some(agent_id) = resolve_agent_id_from_channel_name(conn, channel_name) {
            return Some(agent_id);
        }
    }

    for dispatch_id in [dispatch_id, session_dispatch_id.as_deref()]
        .into_iter()
        .flatten()
    {
        if let Some(agent_id) = resolve_agent_id_from_dispatch_id(conn, dispatch_id) {
            return Some(agent_id);
        }
    }

    for thread_channel_id in [
        provided_thread_channel_id.as_deref(),
        session_key_thread_channel_id.as_deref(),
        session_thread_channel_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(agent_id) = resolve_agent_id_from_thread_channel_id(conn, thread_channel_id) {
            return Some(agent_id);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::parse_channel_name_from_session_key;
    use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};

    #[test]
    fn parse_channel_name_from_session_key_keeps_legacy_host_prefix_behavior() {
        let tmux_name = ProviderKind::Codex.build_tmux_session_name("adk-cdx");
        let session_key = format!("mac-mini:{tmux_name}");
        assert_eq!(
            parse_channel_name_from_session_key(&session_key).as_deref(),
            Some("adk-cdx")
        );
    }

    #[test]
    fn parse_channel_name_from_session_key_supports_namespaced_session_keys() {
        let tmux_name = ProviderKind::Codex
            .build_tmux_session_name("project-skillmanager-extremely-verbose-channel-cdx");
        let (_, expected_channel_name) = parse_provider_and_channel_from_tmux_name(&tmux_name)
            .expect("tmux session should parse");
        let session_key = format!("codex/hash123/mac-mini:{tmux_name}");
        assert_eq!(
            parse_channel_name_from_session_key(&session_key).as_deref(),
            Some(expected_channel_name.as_str())
        );
    }
}
