//! Report legacy home overrides that have not been seeded into the shared authority.
use super::HealthRegistry;
use crate::db::channel_home_preferences as homes;
use crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

pub(super) struct Issue {
    provider: String,
    channel: u64,
    reason: &'static str,
}

pub(crate) async fn inspect_at_boot(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    pool: Option<&PgPool>,
    restored: &[(ChannelId, String)],
) {
    let mut issues = Vec::new();
    for (channel, legacy_instance) in restored {
        let identity = OwnerIdentity::new(provider.as_str(), &channel.get().to_string());
        let stored = match pool {
            Some(pool) => homes::read(pool, &identity).await.map(|row| row.is_some()),
            None => Err(sqlx::Error::PoolClosed),
        };
        let reason = match stored {
            Ok(true) => continue,
            Ok(false) => "channel_home_seed_missing",
            Err(error) => {
                tracing::warn!(%error, provider=provider.as_str(), channel=channel.get(), "channel home seed check failed");
                "channel_home_seed_check_failed"
            }
        };
        tracing::warn!(provider=provider.as_str(), channel=channel.get(), %legacy_instance, reason, "legacy channel home override has no confirmed shared preference");
        issues.push(Issue {
            provider: provider.as_str().into(),
            channel: channel.get(),
            reason,
        });
    }
    let mut published = registry.channel_home_seed_issues.lock().await;
    published.retain(|issue| issue.provider != provider.as_str());
    published.extend(issues);
}

pub(super) async fn health_reasons(registry: &HealthRegistry) -> Vec<String> {
    registry
        .channel_home_seed_issues
        .lock()
        .await
        .iter()
        .map(|issue| format!("{}:{}:{}", issue.reason, issue.provider, issue.channel))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use uuid::Uuid;

    #[tokio::test]
    async fn owner_home_seed_boot_reports_missing_without_import_and_reset_survives_restart_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let registry = HealthRegistry::new();
        let restored = [(ChannelId::new(7336920), "old-local-choice".to_string())];
        let id = OwnerIdentity::new("claude", "7336920");
        inspect_at_boot(&registry, &ProviderKind::Claude, Some(&pool), &restored).await;
        assert_eq!(health_reasons(&registry).await, ["channel_home_seed_missing:claude:7336920"]);
        assert!(homes::read(&pool, &id).await.unwrap().is_none());
        let health = super::super::build_health_snapshot(&registry).await;
        let json = serde_json::to_value(health).unwrap();
        assert!(json["degraded_reasons"].as_array().unwrap().iter().any(|reason| reason == "channel_home_seed_missing:claude:7336920"));
        homes::write(&pool, &id, 0, Uuid::new_v4(), Some("shared-choice")).await.unwrap();
        inspect_at_boot(&registry, &ProviderKind::Claude, Some(&pool), &restored).await;
        assert!(health_reasons(&registry).await.is_empty());
        let reset_op = Uuid::new_v4();
        homes::write(&pool, &id, 1, reset_op, None).await.unwrap();
        for _ in 0..2 {
            let restarted = HealthRegistry::new();
            inspect_at_boot(&restarted, &ProviderKind::Claude, Some(&pool), &restored).await;
            assert!(health_reasons(&restarted).await.is_empty());
            let row = homes::read(&pool, &id).await.unwrap().unwrap();
            assert_eq!((row.mode.as_str(), row.home_instance_id, row.revision, row.operation_id), ("inherit", None, 2, reset_op));
        }
        pool.close().await;
    }

    #[tokio::test]
    async fn owner_home_seed_failed_read_is_not_an_empty_success_pg() {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        pool.close().await;
        let registry = HealthRegistry::new();
        let restored = [(ChannelId::new(7336921), "local".into())];
        inspect_at_boot(&registry, &ProviderKind::Codex, Some(&pool), &restored).await;
        assert_eq!(health_reasons(&registry).await, ["channel_home_seed_check_failed:codex:7336921"]);
        inspect_at_boot(&registry, &ProviderKind::Codex, None, &restored).await;
        assert_eq!(health_reasons(&registry).await.len(), 1);
    }
}
