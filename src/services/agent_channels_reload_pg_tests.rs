//! A live default reload made while a binding query waits on its lock is what the query reads.
use crate::services::agent_channels::resolve_agent_primary_channel_pg;

fn child(name: &str, marker: &str) -> bool {
    if std::env::var_os(marker).is_some() {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(marker, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success()
            && stdout
                .lines()
                .filter(|line| line.starts_with("test result:"))
                .count()
                == 1
            && stdout
                .lines()
                .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

fn install_default(provider: &str) {
    let mut config = crate::config::Config::default();
    config.onboarding.default_provider = Some(provider.to_string());
    crate::config_live_reload::install(config);
}

#[cfg(unix)]
#[tokio::test]
async fn default_reload_during_binding_query_is_observed_after_await_pg() {
    if !child(
        "services::discord::host_key_derivation::ke_session_tests::agent_channels_reload_pg::default_reload_during_binding_query_is_observed_after_await_pg",
        "ADK_AGENT_CHANNELS_PG_CHILD",
    ) {
        return;
    }
    let (db, pool) = crate::services::discord::host_defer_gate::tests::postgres().await;
    sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_cc, discord_channel_cdx) VALUES ('live-default', 'live-default', NULL, 'cc', 'cdx')")
        .execute(&pool).await.unwrap();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE agents IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    install_default("claude");
    let pending = resolve_agent_primary_channel_pg(&pool, "live-default");
    tokio::pin!(pending);
    assert!(futures::poll!(&mut pending).is_pending());
    install_default("codex");
    lock.commit().await.unwrap();
    let channel = tokio::time::timeout(std::time::Duration::from_secs(15), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(channel.as_deref(), Some("cdx"));
    pool.close().await;
    db.drop().await;
}
