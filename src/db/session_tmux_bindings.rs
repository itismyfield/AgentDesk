use sqlx::{PgPool, Row as SqlxRow};

use crate::services::discord::session_identity::tmux_name_from_session_key;
use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};

const LIVE_SESSION_BINDINGS_QUERY: &str =
    "SELECT agent_id, provider, channel_id, session_key, instance_id
         FROM sessions
         WHERE NULLIF(TRIM(channel_id), '') IS NOT NULL
           AND NULLIF(TRIM(session_key), '') IS NOT NULL
           AND LOWER(TRIM(COALESCE(status, ''))) IN (
             'connected',
             'turn_active',
             'awaiting_bg',
             'awaiting_user',
             'idle',
             'working'
           )
           AND last_heartbeat IS NOT NULL
           AND last_heartbeat > NOW() - INTERVAL '10 minutes'
         ORDER BY last_heartbeat DESC NULLS LAST,
                  created_at DESC NULLS LAST,
                  id DESC";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSessionTmuxBinding {
    pub agent_id: String,
    pub provider: ProviderKind,
    pub channel_id: String,
    pub session_name: String,
    pub tmux_segment: String,
}

pub async fn load_live_session_tmux_bindings_pg(
    pool: &PgPool,
    live_session_names: &std::collections::HashSet<String>,
    local_instance_id: Option<&str>,
) -> Result<Vec<LiveSessionTmuxBinding>, sqlx::Error> {
    let rows = sqlx::query(LIVE_SESSION_BINDINGS_QUERY)
        .fetch_all(pool)
        .await?;

    let mut bindings = Vec::new();
    let mut claimed_session_names = std::collections::HashSet::new();
    for row in rows {
        let agent_id: Option<String> = row.try_get("agent_id")?;
        let provider_hint: Option<String> = row.try_get("provider")?;
        let channel_id: Option<String> = row.try_get("channel_id")?;
        let session_key: Option<String> = row.try_get("session_key")?;
        let row_instance_id: Option<String> = row.try_get("instance_id")?;
        let Some(agent_id) = normalize_nonempty(agent_id.as_deref()) else {
            continue;
        };
        let Some(channel_id) = normalize_nonempty(channel_id.as_deref()) else {
            continue;
        };
        let Some(session_key) = session_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        if !session_row_matches_local_instance(row_instance_id.as_deref(), local_instance_id) {
            tracing::debug!(
                session_key,
                row_instance_id = row_instance_id.as_deref().unwrap_or("<none>"),
                local_instance_id = local_instance_id.unwrap_or("<none>"),
                "session binding ignored because it belongs to a non-local runtime"
            );
            continue;
        }
        let Some((session_name, provider, tmux_segment)) =
            live_tmux_identity_from_session_key(session_key, live_session_names)
        else {
            continue;
        };
        if !provider_hint_matches_session_provider(provider_hint.as_deref(), &provider) {
            tracing::debug!(
                session_key,
                provider_hint = provider_hint.as_deref().unwrap_or("<none>"),
                parsed_provider = ?provider,
                "session binding ignored because its provider hint disagrees with the tmux identity"
            );
            continue;
        }
        if !session_agent_matches_provider_channel(pool, &agent_id, &provider).await? {
            tracing::debug!(
                session_key,
                agent_id,
                provider = provider.as_str(),
                "session binding ignored because the owning agent no longer serves this provider"
            );
            continue;
        }
        if !claimed_session_names.insert(session_name.clone()) {
            tracing::debug!(
                session_name,
                channel_id,
                "session binding kept the newest live row for the tmux session"
            );
            continue;
        }
        bindings.push(LiveSessionTmuxBinding {
            agent_id,
            provider,
            channel_id,
            session_name,
            tmux_segment,
        });
    }
    Ok(bindings)
}

async fn session_agent_matches_provider_channel(
    pool: &PgPool,
    agent_id: &str,
    provider: &ProviderKind,
) -> Result<bool, sqlx::Error> {
    Ok(
        crate::db::agents::load_agent_channel_bindings_pg(pool, agent_id)
            .await?
            .is_some_and(|bindings| {
                bindings.resolved_primary_provider_kind().as_ref() == Some(provider)
                    || bindings
                        .channel_for_provider(Some(provider.as_str()))
                        .is_some()
            }),
    )
}

fn live_tmux_identity_from_session_key(
    session_key: &str,
    live_session_names: &std::collections::HashSet<String>,
) -> Option<(String, ProviderKind, String)> {
    let session_name = tmux_name_from_session_key(session_key)?;
    if !live_session_names.contains(&session_name) {
        tracing::debug!(
            session_key,
            session_name,
            "session binding ignored because its tmux session is not live"
        );
        return None;
    }
    let (provider, tmux_segment) = parse_provider_and_channel_from_tmux_name(&session_name)?;
    Some((session_name, provider, tmux_segment))
}

fn provider_hint_matches_session_provider(
    provider_hint: Option<&str>,
    parsed_provider: &ProviderKind,
) -> bool {
    match provider_hint
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(ProviderKind::from_str)
    {
        Some(hint) => hint == *parsed_provider,
        None => true,
    }
}

fn session_row_matches_local_instance(
    row_instance_id: Option<&str>,
    local_instance_id: Option<&str>,
) -> bool {
    let Some(local) = normalize_instance_id(local_instance_id) else {
        return true;
    };
    let Some(row) = normalize_instance_id(row_instance_id) else {
        return false;
    };
    row == local
        || matches!(
            (default_instance_hostname(row), default_instance_hostname(local)),
            (Some(row_host), Some(local_host)) if row_host == local_host
        )
}

fn normalize_instance_id(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn default_instance_hostname(value: &str) -> Option<&str> {
    let (hostname, pid) = value.rsplit_once('-')?;
    if hostname.is_empty() || pid.is_empty() || !pid.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some(hostname)
}

fn normalize_nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_identity_keeps_dm_channel_segment_only_for_a_live_pane() {
        let session_name = ProviderKind::Claude.build_tmux_session_name("dm-343742347365974026");
        let live = std::collections::HashSet::from([session_name.clone()]);

        assert_eq!(
            live_tmux_identity_from_session_key(
                &format!("claude/test-token/mac-mini:{session_name}"),
                &live,
            ),
            Some((
                session_name.clone(),
                ProviderKind::Claude,
                "dm-343742347365974026".to_string(),
            ))
        );
        assert!(
            live_tmux_identity_from_session_key(
                "claude/test-token/mac-mini:AgentDesk-claude-dm-999",
                &live,
            )
            .is_none(),
            "a stale DB row must not claim a different or dead DM pane"
        );
    }

    #[test]
    fn restart_accepts_same_host_pid_rotation_but_not_foreign_hosts() {
        assert!(session_row_matches_local_instance(
            Some("mac-mini-111"),
            Some("mac-mini-222")
        ));
        assert!(!session_row_matches_local_instance(
            Some("mac-book-111"),
            Some("mac-mini-222")
        ));
        assert!(!session_row_matches_local_instance(
            None,
            Some("mac-mini-222")
        ));
    }

    #[test]
    fn live_binding_query_requires_restart_safe_rows() {
        assert!(LIVE_SESSION_BINDINGS_QUERY.contains("agent_id"));
        assert!(LIVE_SESSION_BINDINGS_QUERY.contains("channel_id"));
        assert!(LIVE_SESSION_BINDINGS_QUERY.contains("session_key"));
        assert!(LIVE_SESSION_BINDINGS_QUERY.contains("instance_id"));
        assert!(LIVE_SESSION_BINDINGS_QUERY.contains("last_heartbeat IS NOT NULL"));
        assert!(
            LIVE_SESSION_BINDINGS_QUERY.contains("last_heartbeat > NOW() - INTERVAL '10 minutes'")
        );
        assert!(!LIVE_SESSION_BINDINGS_QUERY.contains("'disconnected'"));
        assert!(!LIVE_SESSION_BINDINGS_QUERY.contains("'aborted'"));
    }
}
