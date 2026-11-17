use crate::domain::session_identity::SessionIdentity;
use crate::services::provider::ProviderKind;
use std::collections::HashSet;

pub(super) fn live_name(
    key: &str,
    provider: Option<&str>,
    live: &HashSet<String>,
) -> Option<(ProviderKind, String)> {
    let parsed = SessionIdentity::parse(key)?;
    let provider = match provider {
        Some(value) => ProviderKind::from_str(value)?,
        None => {
            crate::services::provider::parse_provider_and_channel_from_tmux_name(&parsed.tmux_name)?
                .0
        }
    };
    if !live.contains(&parsed.tmux_name)
        || parsed
            .provider_from_key
            .as_deref()
            .is_some_and(|value| value != provider.as_str())
    {
        return None;
    }
    Some((provider, parsed.tmux_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::cluster::session_matcher::{
        ChannelBinding, ChannelDirectory, match_session,
    };
    #[tokio::test]
    async fn key_promotion_cluster_directory_uses_stored_provider_and_actual_name_pg() {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let key = "claude/discord_0123456789abcdef/host:스마트홈-cc";
        sqlx::query("INSERT INTO sessions(session_key,provider,status,channel_id) VALUES($1,'claude','idle','81190401')").bind(key).execute(&pool).await.unwrap();
        let row: (String, String) =
            sqlx::query_as("SELECT session_key,provider FROM sessions WHERE channel_id='81190401'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let live = HashSet::from(["스마트홈-cc".to_owned()]);
        let (provider, name) = live_name(&row.0, Some(&row.1), &live).unwrap();
        let mut directory = ChannelDirectory::new();
        directory
            .insert_name(
                name,
                ChannelBinding {
                    channel_id: "81190401".into(),
                    agent_id: "agent".into(),
                    provider,
                    tmux_segment: None,
                },
            )
            .unwrap();
        assert!(match_session("스마트홈-cc", "claude", &directory).is_some());
        assert!(live_name(key, Some("codex"), &live).is_none());
        pool.close().await;
        db.drop().await;
    }
}
