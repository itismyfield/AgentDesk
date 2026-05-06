use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub(crate) enum DispatchDeliveryEventStatus {
    Reserved,
    Sent,
    Fallback,
    Duplicate,
    Skipped,
    Failed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, sqlx::FromRow)]
pub(crate) struct DispatchDeliveryEvent {
    pub(crate) id: i64,
    pub(crate) dispatch_id: String,
    pub(crate) correlation_id: String,
    pub(crate) semantic_event_id: String,
    pub(crate) operation: String,
    pub(crate) target_kind: String,
    pub(crate) target_channel_id: Option<String>,
    pub(crate) target_thread_id: Option<String>,
    pub(crate) status: DispatchDeliveryEventStatus,
    pub(crate) attempt: i32,
    pub(crate) message_id: Option<String>,
    pub(crate) messages_json: Value,
    pub(crate) fallback_kind: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) result_json: Value,
    pub(crate) reserved_until: Option<DateTime<Utc>>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::{DispatchDeliveryEvent, DispatchDeliveryEventStatus};
    use chrono::{DateTime, Utc};
    use serde_json::json;

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
                "agentdesk_dispatch_delivery_events_{}",
                uuid::Uuid::new_v4().simple()
            );
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "dispatch delivery events tests",
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
                "dispatch delivery events tests",
            )
            .await
            .unwrap()
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "dispatch delivery events tests",
            )
            .await
            .unwrap();
        }
    }

    fn utc(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .expect("rfc3339 timestamp")
            .with_timezone(&Utc)
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

    #[test]
    fn dispatch_delivery_event_serde_roundtrips_snake_case_status() {
        let event = DispatchDeliveryEvent {
            id: 7,
            dispatch_id: "dispatch-serde".to_string(),
            correlation_id: "dispatch:dispatch-serde".to_string(),
            semantic_event_id: "dispatch:dispatch-serde:notify".to_string(),
            operation: "send".to_string(),
            target_kind: "channel".to_string(),
            target_channel_id: Some("1500000000000000000".to_string()),
            target_thread_id: Some("1500000000000000001".to_string()),
            status: DispatchDeliveryEventStatus::Fallback,
            attempt: 2,
            message_id: Some("1500000000000000002".to_string()),
            messages_json: json!([
                {
                    "channel_id": "1500000000000000000",
                    "message_id": "1500000000000000002"
                }
            ]),
            fallback_kind: Some("channel".to_string()),
            error: Some("thread archived".to_string()),
            result_json: json!({
                "kind": "fallback",
                "reason": "thread_archived"
            }),
            reserved_until: Some(utc("2026-05-06T08:00:00Z")),
            created_at: utc("2026-05-06T07:45:00Z"),
            updated_at: utc("2026-05-06T07:46:00Z"),
        };

        let value = serde_json::to_value(&event).expect("serialize delivery event");
        assert_eq!(value["status"], "fallback");

        let parsed: DispatchDeliveryEvent =
            serde_json::from_value(value).expect("deserialize delivery event");
        assert_eq!(parsed, event);
    }

    #[tokio::test]
    async fn dispatch_delivery_event_maps_to_postgres_roundtrip() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO task_dispatches (id, status, title)
             VALUES ($1, 'pending', 'Delivery event roundtrip')",
        )
        .bind("dispatch-roundtrip")
        .execute(&pool)
        .await
        .unwrap();

        let event: DispatchDeliveryEvent = sqlx::query_as(
            "INSERT INTO dispatch_delivery_events (
                dispatch_id,
                correlation_id,
                semantic_event_id,
                operation,
                target_kind,
                target_channel_id,
                target_thread_id,
                status,
                attempt,
                message_id,
                messages_json,
                fallback_kind,
                error,
                result_json,
                reserved_until
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                $11, $12, $13, $14, NOW() + INTERVAL '30 seconds'
             )
             RETURNING *",
        )
        .bind("dispatch-roundtrip")
        .bind("dispatch:dispatch-roundtrip")
        .bind("dispatch:dispatch-roundtrip:notify")
        .bind("send")
        .bind("channel")
        .bind("1500000000000000000")
        .bind("1500000000000000001")
        .bind(DispatchDeliveryEventStatus::Sent)
        .bind(1_i32)
        .bind("1500000000000000002")
        .bind(json!([
            {
                "channel_id": "1500000000000000000",
                "message_id": "1500000000000000002"
            }
        ]))
        .bind(Option::<String>::None)
        .bind(Option::<String>::None)
        .bind(json!({
            "kind": "sent",
            "message_id": "1500000000000000002"
        }))
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(event.dispatch_id, "dispatch-roundtrip");
        assert_eq!(event.status, DispatchDeliveryEventStatus::Sent);
        assert_eq!(event.attempt, 1);
        assert_eq!(event.message_id.as_deref(), Some("1500000000000000002"));
        assert_eq!(event.messages_json[0]["message_id"], "1500000000000000002");
        assert_eq!(event.result_json["kind"], "sent");

        let index_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT
               FROM pg_indexes
              WHERE schemaname = current_schema()
                AND tablename = 'dispatch_delivery_events'
                AND indexname IN (
                    'uq_dispatch_delivery_events_attempt',
                    'uq_dispatch_delivery_events_active',
                    'idx_dispatch_delivery_events_dispatch_created',
                    'idx_dispatch_delivery_events_status_created',
                    'idx_dispatch_delivery_events_reserved_until',
                    'idx_dispatch_delivery_events_message_id'
                )",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(index_count, 6);

        pool.close().await;
        pg_db.drop().await;
    }
}
