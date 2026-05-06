use super::{DispatchNotifyDeliveryResult, DispatchTransport};
use crate::db::dispatches::delivery_events::{
    DispatchDeliveryEventFinalize, DispatchDeliveryEventStatus,
    finalize_dispatch_delivery_event_pg, insert_reserved_dispatch_delivery_event_pg,
};
use serde_json::{Value, json};
use sqlx::PgPool;

pub(crate) async fn send_dispatch_with_delivery_guard<T: DispatchTransport>(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    agent_id: &str,
    title: &str,
    card_id: &str,
    dispatch_id: &str,
    transport: &T,
) -> Result<DispatchNotifyDeliveryResult, String> {
    let pg_pool = pg_pool.or_else(|| transport.pg_pool());
    if !claim_dispatch_delivery_guard(pg_pool, dispatch_id).await? {
        return Ok(DispatchNotifyDeliveryResult::duplicate(
            dispatch_id,
            "dispatch delivery guard already recorded this semantic notify event",
        ));
    }

    let send_result = transport
        .send_dispatch(
            db.cloned(),
            agent_id.to_string(),
            title.to_string(),
            card_id.to_string(),
            dispatch_id.to_string(),
        )
        .await;

    finalize_dispatch_delivery_guard(pg_pool, dispatch_id, send_result.as_ref()).await;
    send_result
}

fn notified_key(dispatch_id: &str) -> String {
    format!("dispatch_notified:{dispatch_id}")
}

fn reserving_key(dispatch_id: &str) -> String {
    format!("dispatch_reserving:{dispatch_id}")
}

async fn claim_dispatch_delivery_guard(
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
) -> Result<bool, String> {
    let pool = pg_pool.ok_or_else(|| "delivery guard requires postgres pool".to_string())?;
    let notified: Option<i32> = sqlx::query_scalar("SELECT 1 FROM kv_meta WHERE key = $1 LIMIT 1")
        .bind(notified_key(dispatch_id))
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("check postgres delivery guard for {dispatch_id}: {error}"))?;
    if notified.is_some() {
        return Ok(false);
    }

    let result = sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO NOTHING",
    )
    .bind(reserving_key(dispatch_id))
    .bind(dispatch_id)
    .execute(pool)
    .await
    .map_err(|error| format!("claim postgres delivery guard for {dispatch_id}: {error}"))?;
    let claimed = result.rows_affected() > 0;
    if claimed {
        if let Err(error) =
            insert_reserved_dispatch_delivery_event_pg(pool, dispatch_id, None, None).await
        {
            tracing::warn!(
                dispatch_id,
                error = %error,
                "[dispatch] shadow dispatch_delivery_events reservation write failed"
            );
        }
    }
    Ok(claimed)
}

async fn finalize_dispatch_delivery_guard(
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
    send_result: Result<&DispatchNotifyDeliveryResult, &String>,
) {
    let Some(pool) = pg_pool else {
        return;
    };
    let success = send_result.is_ok();
    sqlx::query("DELETE FROM kv_meta WHERE key = $1")
        .bind(reserving_key(dispatch_id))
        .execute(pool)
        .await
        .ok();
    if success {
        sqlx::query(
            "INSERT INTO kv_meta (key, value)
             VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(notified_key(dispatch_id))
        .bind(dispatch_id)
        .execute(pool)
        .await
        .ok();
    }

    let finalize = dispatch_delivery_event_finalize_input(dispatch_id, send_result);
    if let Err(error) = finalize_dispatch_delivery_event_pg(pool, finalize).await {
        tracing::warn!(
            dispatch_id,
            error = %error,
            "[dispatch] shadow dispatch_delivery_events finalize write failed"
        );
    }
}

fn dispatch_delivery_event_finalize_input<'a>(
    dispatch_id: &'a str,
    send_result: Result<&'a DispatchNotifyDeliveryResult, &'a String>,
) -> DispatchDeliveryEventFinalize<'a> {
    match send_result {
        Ok(result) => DispatchDeliveryEventFinalize {
            dispatch_id,
            status: dispatch_delivery_event_status(result),
            target_channel_id: result.target_channel_id.as_deref(),
            target_thread_id: None,
            message_id: result
                .message_id
                .as_deref()
                .filter(|value| !value.trim().is_empty()),
            messages_json: dispatch_delivery_messages_json(result),
            fallback_kind: result.fallback_kind.as_deref(),
            error: None,
            result_json: dispatch_delivery_result_json(result),
        },
        Err(error) => DispatchDeliveryEventFinalize {
            dispatch_id,
            status: DispatchDeliveryEventStatus::Failed,
            target_channel_id: None,
            target_thread_id: None,
            message_id: None,
            messages_json: json!([]),
            fallback_kind: None,
            error: Some(error.as_str()),
            result_json: json!({
                "status": "failed",
                "dispatch_id": dispatch_id,
                "action": "notify",
                "detail": error,
            }),
        },
    }
}

fn dispatch_delivery_event_status(
    result: &DispatchNotifyDeliveryResult,
) -> DispatchDeliveryEventStatus {
    match result.status.as_str() {
        "fallback" => DispatchDeliveryEventStatus::Fallback,
        "duplicate" => DispatchDeliveryEventStatus::Duplicate,
        "permanent_failure" => DispatchDeliveryEventStatus::Failed,
        "success" if result.detail.as_deref().is_some_and(is_skip_detail) => {
            DispatchDeliveryEventStatus::Skipped
        }
        _ => DispatchDeliveryEventStatus::Sent,
    }
}

fn is_skip_detail(detail: &str) -> bool {
    detail
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("skipped")
}

fn dispatch_delivery_messages_json(result: &DispatchNotifyDeliveryResult) -> Value {
    let Some(message_id) = result
        .message_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return json!([]);
    };
    match result.target_channel_id.as_deref() {
        Some(channel_id) if !channel_id.trim().is_empty() => {
            json!([{"channel_id": channel_id, "message_id": message_id}])
        }
        _ => json!([{"message_id": message_id}]),
    }
}

fn dispatch_delivery_result_json(result: &DispatchNotifyDeliveryResult) -> Value {
    serde_json::to_value(result).unwrap_or_else(|_| {
        json!({
            "status": &result.status,
            "dispatch_id": &result.dispatch_id,
            "action": &result.action,
            "detail": &result.detail,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    struct TestPostgresDb {
        _lock: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let lock = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_database_url();
            let database_name = format!(
                "agentdesk_dispatch_delivery_guard_{}",
                uuid::Uuid::new_v4().simple()
            );
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "dispatch delivery guard tests",
            )
            .await
            .unwrap();

            Self {
                _lock: lock,
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "dispatch delivery guard tests",
            )
            .await
            .unwrap()
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "dispatch delivery guard tests",
            )
            .await
            .unwrap();
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    async fn seed_dispatch(pool: &PgPool, dispatch_id: &str) {
        sqlx::query(
            "INSERT INTO task_dispatches (id, status, title)
             VALUES ($1, 'pending', 'Delivery guard test')",
        )
        .bind(dispatch_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn kv_meta_count(pool: &PgPool, key: &str) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM kv_meta WHERE key = $1")
            .bind(key)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn delivery_event_count(pool: &PgPool, dispatch_id: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
               FROM dispatch_delivery_events
              WHERE dispatch_id = $1",
        )
        .bind(dispatch_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[test]
    fn duplicate_result_carries_dispatch_idempotency_keys() {
        let result = DispatchNotifyDeliveryResult::duplicate(
            "dispatch-1517",
            "dispatch delivery guard already recorded this semantic notify event",
        );

        assert_eq!(result.status, "duplicate");
        assert_eq!(result.dispatch_id, "dispatch-1517");
        assert_eq!(result.action, "notify");
        assert_eq!(
            result.correlation_id.as_deref(),
            Some("dispatch:dispatch-1517")
        );
        assert_eq!(
            result.semantic_event_id.as_deref(),
            Some("dispatch:dispatch-1517:notify")
        );
    }

    #[test]
    fn delivery_guard_keys_are_stable() {
        assert_eq!(
            notified_key("dispatch-1517"),
            "dispatch_notified:dispatch-1517"
        );
        assert_eq!(
            reserving_key("dispatch-1517"),
            "dispatch_reserving:dispatch-1517"
        );
    }

    #[test]
    fn delivery_result_status_maps_to_event_status() {
        let skipped = DispatchNotifyDeliveryResult::success(
            "dispatch-skip",
            "notify",
            "skipped non-deliverable status",
        );
        assert_eq!(
            dispatch_delivery_event_status(&skipped),
            DispatchDeliveryEventStatus::Skipped
        );

        let duplicate = DispatchNotifyDeliveryResult::duplicate("dispatch-dupe", "already sent");
        assert_eq!(
            dispatch_delivery_event_status(&duplicate),
            DispatchDeliveryEventStatus::Duplicate
        );

        let mut fallback =
            DispatchNotifyDeliveryResult::success("dispatch-fallback", "notify", "minimal sent");
        fallback.status = "fallback".to_string();
        assert_eq!(
            dispatch_delivery_event_status(&fallback),
            DispatchDeliveryEventStatus::Fallback
        );
    }

    #[tokio::test]
    async fn claim_delivery_guard_shadow_writes_one_reserved_event() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let dispatch_id = "dispatch-shadow-reserved";
        seed_dispatch(&pool, dispatch_id).await;

        assert!(
            claim_dispatch_delivery_guard(Some(&pool), dispatch_id)
                .await
                .unwrap()
        );
        assert!(
            !claim_dispatch_delivery_guard(Some(&pool), dispatch_id)
                .await
                .unwrap()
        );

        assert_eq!(
            kv_meta_count(&pool, &reserving_key(dispatch_id)).await,
            1,
            "kv_meta reservation remains authoritative"
        );
        assert_eq!(delivery_event_count(&pool, dispatch_id).await, 1);

        let (status, reserved_until): (String, Option<chrono::DateTime<chrono::Utc>>) =
            sqlx::query_as(
                "SELECT status, reserved_until
                   FROM dispatch_delivery_events
                  WHERE dispatch_id = $1",
            )
            .bind(dispatch_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "reserved");
        assert!(reserved_until.is_some());

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn finalize_delivery_guard_shadow_updates_sent_event_and_kv_meta() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let dispatch_id = "dispatch-shadow-sent";
        seed_dispatch(&pool, dispatch_id).await;
        assert!(
            claim_dispatch_delivery_guard(Some(&pool), dispatch_id)
                .await
                .unwrap()
        );

        let result = DispatchNotifyDeliveryResult {
            status: "success".to_string(),
            dispatch_id: dispatch_id.to_string(),
            action: "notify".to_string(),
            correlation_id: Some(format!("dispatch:{dispatch_id}")),
            semantic_event_id: Some(format!("dispatch:{dispatch_id}:notify")),
            target_channel_id: Some("1500000000000000000".to_string()),
            message_id: Some("1500000000000000001".to_string()),
            fallback_kind: None,
            detail: Some("sent".to_string()),
        };
        finalize_dispatch_delivery_guard(Some(&pool), dispatch_id, Ok(&result)).await;

        assert_eq!(kv_meta_count(&pool, &reserving_key(dispatch_id)).await, 0);
        assert_eq!(kv_meta_count(&pool, &notified_key(dispatch_id)).await, 1);
        assert_eq!(delivery_event_count(&pool, dispatch_id).await, 1);

        let (
            status,
            target_channel_id,
            message_id,
            messages_json,
            error,
            result_json,
            reserved_until,
        ): (
            String,
            Option<String>,
            Option<String>,
            Value,
            Option<String>,
            Value,
            Option<chrono::DateTime<chrono::Utc>>,
        ) = sqlx::query_as(
            "SELECT status, target_channel_id, message_id, messages_json,
                    error, result_json, reserved_until
               FROM dispatch_delivery_events
              WHERE dispatch_id = $1",
        )
        .bind(dispatch_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(status, "sent");
        assert_eq!(target_channel_id.as_deref(), Some("1500000000000000000"));
        assert_eq!(message_id.as_deref(), Some("1500000000000000001"));
        assert_eq!(messages_json[0]["message_id"], "1500000000000000001");
        assert!(error.is_none());
        assert_eq!(result_json["status"], "success");
        assert!(reserved_until.is_none());

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn finalize_delivery_guard_shadow_updates_failed_event_without_notified_marker() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let dispatch_id = "dispatch-shadow-failed";
        seed_dispatch(&pool, dispatch_id).await;
        assert!(
            claim_dispatch_delivery_guard(Some(&pool), dispatch_id)
                .await
                .unwrap()
        );

        let error = "discord transport failed".to_string();
        finalize_dispatch_delivery_guard(Some(&pool), dispatch_id, Err(&error)).await;

        assert_eq!(kv_meta_count(&pool, &reserving_key(dispatch_id)).await, 0);
        assert_eq!(kv_meta_count(&pool, &notified_key(dispatch_id)).await, 0);
        assert_eq!(delivery_event_count(&pool, dispatch_id).await, 1);

        let (status, stored_error, result_json): (String, Option<String>, Value) = sqlx::query_as(
            "SELECT status, error, result_json
                   FROM dispatch_delivery_events
                  WHERE dispatch_id = $1",
        )
        .bind(dispatch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "failed");
        assert_eq!(stored_error.as_deref(), Some("discord transport failed"));
        assert_eq!(result_json["status"], "failed");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn failed_delivery_retry_shadow_writes_next_attempt_without_changing_kv_meta() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let dispatch_id = "dispatch-shadow-retry";
        seed_dispatch(&pool, dispatch_id).await;

        assert!(
            claim_dispatch_delivery_guard(Some(&pool), dispatch_id)
                .await
                .unwrap()
        );
        let first_error = "first discord transport failure".to_string();
        finalize_dispatch_delivery_guard(Some(&pool), dispatch_id, Err(&first_error)).await;

        assert_eq!(kv_meta_count(&pool, &reserving_key(dispatch_id)).await, 0);
        assert_eq!(kv_meta_count(&pool, &notified_key(dispatch_id)).await, 0);
        assert!(
            claim_dispatch_delivery_guard(Some(&pool), dispatch_id)
                .await
                .unwrap(),
            "failed terminal rows must not block the authoritative kv_meta retry"
        );

        let result = DispatchNotifyDeliveryResult {
            status: "success".to_string(),
            dispatch_id: dispatch_id.to_string(),
            action: "notify".to_string(),
            correlation_id: Some(format!("dispatch:{dispatch_id}")),
            semantic_event_id: Some(format!("dispatch:{dispatch_id}:notify")),
            target_channel_id: Some("1500000000000000002".to_string()),
            message_id: Some("1500000000000000003".to_string()),
            fallback_kind: None,
            detail: Some("sent after retry".to_string()),
        };
        finalize_dispatch_delivery_guard(Some(&pool), dispatch_id, Ok(&result)).await;

        assert_eq!(kv_meta_count(&pool, &reserving_key(dispatch_id)).await, 0);
        assert_eq!(kv_meta_count(&pool, &notified_key(dispatch_id)).await, 1);
        assert_eq!(delivery_event_count(&pool, dispatch_id).await, 2);

        let rows: Vec<(String, i32, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT status, attempt, error, message_id
               FROM dispatch_delivery_events
              WHERE dispatch_id = $1
              ORDER BY attempt",
        )
        .bind(dispatch_id)
        .fetch_all(&pool)
        .await
        .unwrap();

        assert_eq!(
            rows,
            vec![
                (
                    "failed".to_string(),
                    1,
                    Some("first discord transport failure".to_string()),
                    None
                ),
                (
                    "sent".to_string(),
                    2,
                    None,
                    Some("1500000000000000003".to_string())
                ),
            ]
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
