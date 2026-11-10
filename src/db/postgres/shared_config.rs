//! Shared-configuration persistence primitives; startup policy belongs to settings.

use sqlx::PgPool;

pub(crate) async fn shared_config_initialized(pool: &PgPool) -> Result<bool, String> {
    sqlx::query_scalar(
        "SELECT count(*) = 2 FROM kv_meta WHERE key IN ('server_port', 'runtime-config')",
    )
    .fetch_one(pool)
    .await
    .map_err(|error| format!("check hub-owned shared configuration: {error}"))
}

pub(crate) async fn upsert_kv_meta(pool: &PgPool, key: &str, value: &str) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE
         SET value = EXCLUDED.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await
    .map_err(|error| format!("upsert kv_meta {key}: {error}"))?;
    Ok(())
}

pub(crate) async fn put_kv_meta_if_absent(
    pool: &PgPool,
    key: &str,
    value: &str,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO NOTHING",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await
    .map_err(|error| format!("seed kv_meta {key}: {error}"))?;
    Ok(())
}

pub(crate) async fn delete_kv_meta(pool: &PgPool, key: &str) -> Result<(), String> {
    sqlx::query("DELETE FROM kv_meta WHERE key = $1")
        .bind(key)
        .execute(pool)
        .await
        .map_err(|error| format!("delete retired kv_meta {key}: {error}"))?;
    Ok(())
}

pub(crate) async fn delete_kv_meta_prefix(pool: &PgPool, prefix: &str) -> Result<(), String> {
    sqlx::query("DELETE FROM kv_meta WHERE starts_with(key, $1)")
        .bind(prefix)
        .execute(pool)
        .await
        .map_err(|error| format!("delete retired kv_meta prefix {prefix}: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, services::settings::startup::startup_reseed};

    #[tokio::test]
    async fn startup_reseed_removes_retired_keys_and_literal_prefixes_pg() {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let retired = [
            "merge_automation_enabled",
            "merge_strategy",
            "merge_strategy_mode",
            "merge_allowed_authors",
            "maxReviewRounds",
            "maxChainDepth",
            "inProgressStaleMin",
            "requestedAckTimeoutMin",
            "long_turn_alert_interval_min",
            "health_port",
            "test_tier_write",
            "kanban_manager_channel_id",
            "merge_strategy_mode:card:one",
            "merge_strategy_mode:card:two",
            "merge_strategy_mode:card:",
            "escalation_thread:one",
            "escalation_thread:two",
            "escalation_thread:",
        ];
        let retained = [
            "max_review_rounds",
            "merge_strategy_mode:cards:keep",
            "merge_strategy_mode:card",
            "merge_strategy_mode_extra",
            "escalation_other:keep",
            "escalation_thread",
            "mergeXstrategyXmode:card:keep",
            "escalationXthread:keep",
        ];
        for key in retired.iter().chain(&retained) {
            upsert_kv_meta(&pool, key, "7").await.unwrap();
        }
        let mut config = Config::default();
        config.review.max_rounds = Some(7);
        config.kanban.manager_channel_id = Some("1234567890123456789".into());
        for _ in 0..2 {
            startup_reseed(&pool, &config).await.unwrap();
            for key in retired {
                let value: Option<String> =
                    sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = $1")
                        .bind(key)
                        .fetch_optional(&pool)
                        .await
                        .unwrap();
                assert_eq!(value, None, "retired key {key} must be deleted at boot");
            }
            for key in retained {
                let value: String = sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = $1")
                    .bind(key)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                assert_eq!(value, "7", "adjacent/live key {key} must survive boot");
            }
        }
        pool.close().await;
        db.drop().await;
    }
}
