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
        return duplicate_dispatch_delivery_result(pg_pool, dispatch_id).await;
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
    if dispatch_delivery_prior_delivery_pg(pool, dispatch_id)
        .await?
        .is_some()
    {
        return Ok(false);
    }

    recover_expired_dispatch_delivery_reservation_pg(pool, dispatch_id).await?;
    if has_active_dispatch_delivery_reservation_pg(pool, dispatch_id).await? {
        return Ok(false);
    }

    let notified: Option<i32> = sqlx::query_scalar("SELECT 1 FROM kv_meta WHERE key = $1 LIMIT 1")
        .bind(notified_key(dispatch_id))
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("check postgres delivery guard for {dispatch_id}: {error}"))?;
    if notified.is_some() {
        return Ok(false);
    }

    delete_expired_dispatch_reserving_marker_pg(pool, dispatch_id).await?;
    let result = sqlx::query(
        "INSERT INTO kv_meta (key, value, expires_at)
         VALUES ($1, $2, NOW() + INTERVAL '5 minutes')
         ON CONFLICT (key) DO NOTHING",
    )
    .bind(reserving_key(dispatch_id))
    .bind(dispatch_id)
    .execute(pool)
    .await
    .map_err(|error| format!("claim postgres delivery guard for {dispatch_id}: {error}"))?;
    let claimed = result.rows_affected() > 0;
    if claimed {
        match insert_reserved_dispatch_delivery_event_pg(pool, dispatch_id, None, None).await {
            Ok(true) => {}
            Ok(false) => {
                sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                    .bind(reserving_key(dispatch_id))
                    .execute(pool)
                    .await
                    .ok();
                return Ok(false);
            }
            Err(error) => {
                tracing::warn!(
                    dispatch_id,
                    error = %error,
                    "[dispatch] shadow dispatch_delivery_events reservation write failed"
                );
            }
        }
    }
    Ok(claimed)
}

async fn duplicate_dispatch_delivery_result(
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
) -> Result<DispatchNotifyDeliveryResult, String> {
    let mut result = DispatchNotifyDeliveryResult::duplicate(
        dispatch_id,
        "dispatch delivery guard already recorded this semantic notify event",
    );
    let Some(pool) = pg_pool else {
        return Ok(result);
    };
    if let Some(prior) = dispatch_delivery_prior_delivery_pg(pool, dispatch_id).await? {
        result.target_channel_id = prior.target_channel_id;
        result.message_id = prior.message_id;
        result.fallback_kind = prior.fallback_kind;
    }
    Ok(result)
}

struct PriorDispatchDelivery {
    target_channel_id: Option<String>,
    message_id: Option<String>,
    fallback_kind: Option<String>,
}

async fn dispatch_delivery_prior_delivery_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<Option<PriorDispatchDelivery>, String> {
    sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
        "SELECT target_channel_id, message_id, fallback_kind
           FROM dispatch_delivery_events
          WHERE dispatch_id = $1
            AND correlation_id = $2
            AND semantic_event_id = $3
            AND operation = 'send'
            AND target_kind = 'channel'
            AND status IN ('sent', 'fallback', 'skipped', 'duplicate')
          ORDER BY attempt DESC, updated_at DESC, id DESC
          LIMIT 1",
    )
    .bind(dispatch_id)
    .bind(format!("dispatch:{dispatch_id}"))
    .bind(format!("dispatch:{dispatch_id}:notify"))
    .fetch_optional(pool)
    .await
    .map(|row| {
        row.map(
            |(target_channel_id, message_id, fallback_kind)| PriorDispatchDelivery {
                target_channel_id,
                message_id,
                fallback_kind,
            },
        )
    })
    .map_err(|error| format!("load prior dispatch delivery event for {dispatch_id}: {error}"))
}

async fn recover_expired_dispatch_delivery_reservation_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE dispatch_delivery_events
            SET status = 'failed',
                error = COALESCE(error, 'delivery reservation expired before finalize'),
                result_json = CASE
                    WHEN result_json = '{}'::jsonb THEN jsonb_build_object(
                        'status', 'failed',
                        'dispatch_id', dispatch_id,
                        'action', 'notify',
                        'detail', 'delivery reservation expired before finalize'
                    )
                    ELSE result_json
                END,
                reserved_until = NULL,
                updated_at = NOW()
          WHERE dispatch_id = $1
            AND correlation_id = $2
            AND semantic_event_id = $3
            AND operation = 'send'
            AND target_kind = 'channel'
            AND status = 'reserved'
            AND reserved_until <= NOW()",
    )
    .bind(dispatch_id)
    .bind(format!("dispatch:{dispatch_id}"))
    .bind(format!("dispatch:{dispatch_id}:notify"))
    .execute(pool)
    .await
    .map(|_| ())
    .map_err(|error| {
        format!("recover expired dispatch delivery reservation for {dispatch_id}: {error}")
    })
}

async fn has_active_dispatch_delivery_reservation_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<bool, String> {
    sqlx::query_scalar::<_, i32>(
        "SELECT 1
           FROM dispatch_delivery_events
          WHERE dispatch_id = $1
            AND correlation_id = $2
            AND semantic_event_id = $3
            AND operation = 'send'
            AND target_kind = 'channel'
            AND status = 'reserved'
            AND (reserved_until IS NULL OR reserved_until > NOW())
          LIMIT 1",
    )
    .bind(dispatch_id)
    .bind(format!("dispatch:{dispatch_id}"))
    .bind(format!("dispatch:{dispatch_id}:notify"))
    .fetch_optional(pool)
    .await
    .map(|row| row.is_some())
    .map_err(|error| {
        format!("check active dispatch delivery reservation for {dispatch_id}: {error}")
    })
}

async fn delete_expired_dispatch_reserving_marker_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<(), String> {
    sqlx::query(
        "DELETE FROM kv_meta WHERE key = $1 AND expires_at IS NOT NULL AND expires_at <= NOW()",
    )
    .bind(reserving_key(dispatch_id))
    .execute(pool)
    .await
    .map(|_| ())
    .map_err(|error| format!("delete expired postgres delivery guard for {dispatch_id}: {error}"))
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
    use super::super::ReviewFollowupKind;
    use super::*;
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    async fn create_test_pg_db() -> crate::dispatch::test_support::DispatchPostgresTestDb {
        crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_dispatch_delivery_guard",
            "dispatch delivery guard tests",
        )
        .await
    }

    async fn seed_dispatch(pool: &PgPool, dispatch_id: &str) {
        crate::dispatch::test_support::seed_pg_dispatch(pool, dispatch_id, "Delivery guard test")
            .await;
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

    #[derive(Clone)]
    struct RecordingDispatchTransport {
        calls: Arc<Mutex<usize>>,
        target_channel_id: String,
        message_id: String,
        assert_reserving_during_send: Option<PgPool>,
    }

    impl RecordingDispatchTransport {
        fn new(target_channel_id: &str, message_id: &str) -> Self {
            Self {
                calls: Arc::new(Mutex::new(0)),
                target_channel_id: target_channel_id.to_string(),
                message_id: message_id.to_string(),
                assert_reserving_during_send: None,
            }
        }

        fn with_reservation_assertion(mut self, pool: PgPool) -> Self {
            self.assert_reserving_during_send = Some(pool);
            self
        }

        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    impl DispatchTransport for RecordingDispatchTransport {
        async fn send_dispatch(
            &self,
            _db: Option<crate::db::Db>,
            _agent_id: String,
            _title: String,
            _card_id: String,
            dispatch_id: String,
        ) -> Result<DispatchNotifyDeliveryResult, String> {
            *self.calls.lock().unwrap() += 1;
            if let Some(pool) = self.assert_reserving_during_send.as_ref() {
                assert_eq!(
                    kv_meta_count(pool, &reserving_key(&dispatch_id)).await,
                    1,
                    "kv_meta reservation must be renewed before transport sends"
                );
            }
            let mut result =
                DispatchNotifyDeliveryResult::success(&dispatch_id, "notify", "mock sent");
            result.correlation_id = Some(format!("dispatch:{dispatch_id}"));
            result.semantic_event_id = Some(format!("dispatch:{dispatch_id}:notify"));
            result.target_channel_id = Some(self.target_channel_id.clone());
            result.message_id = Some(self.message_id.clone());
            Ok(result)
        }

        async fn send_review_followup(
            &self,
            _db: Option<crate::db::Db>,
            _review_dispatch_id: String,
            _card_id: String,
            _channel_id_num: u64,
            _message: String,
            _kind: ReviewFollowupKind,
        ) -> Result<(), String> {
            Ok(())
        }
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
        let pg_db = create_test_pg_db().await;
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
        let pg_db = create_test_pg_db().await;
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

        let reconcile = crate::reconcile::dispatch_delivery_event_reconcile_report_pg(&pool)
            .await
            .unwrap();
        assert_eq!(
            reconcile.stats.mismatch_count, 0,
            "dual-write delivery guard happy path must reconcile cleanly"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn finalize_delivery_guard_shadow_updates_failed_event_without_notified_marker() {
        let pg_db = create_test_pg_db().await;
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
        let pg_db = create_test_pg_db().await;
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

    #[tokio::test]
    async fn expired_reserved_delivery_recovers_with_new_attempt_and_single_transport_send() {
        let pg_db = create_test_pg_db().await;
        let pool = pg_db.connect_and_migrate().await;
        let dispatch_id = "dispatch-expired-reserved-recovery";
        seed_dispatch(&pool, dispatch_id).await;

        sqlx::query(
            "INSERT INTO dispatch_delivery_events (
                dispatch_id, correlation_id, semantic_event_id, operation, target_kind,
                status, attempt, result_json, reserved_until
             ) VALUES (
                $1, $2, $3, 'send', 'channel', 'reserved', 1, '{}'::jsonb,
                NOW() - INTERVAL '1 minute'
             )",
        )
        .bind(dispatch_id)
        .bind(format!("dispatch:{dispatch_id}"))
        .bind(format!("dispatch:{dispatch_id}:notify"))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kv_meta (key, value, expires_at)
             VALUES ($1, $2, NOW() - INTERVAL '1 minute')",
        )
        .bind(reserving_key(dispatch_id))
        .bind(dispatch_id)
        .execute(&pool)
        .await
        .unwrap();

        let transport =
            RecordingDispatchTransport::new("1500000000000000010", "1500000000000000011")
                .with_reservation_assertion(pool.clone());
        let result = send_dispatch_with_delivery_guard(
            None,
            Some(&pool),
            "agent-1",
            "Expired reservation",
            "card-expired-reserved",
            dispatch_id,
            &transport,
        )
        .await
        .unwrap();

        assert_eq!(result.status, "success");
        assert_eq!(transport.calls(), 1);
        assert_eq!(kv_meta_count(&pool, &reserving_key(dispatch_id)).await, 0);
        assert_eq!(kv_meta_count(&pool, &notified_key(dispatch_id)).await, 1);

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
                    Some("delivery reservation expired before finalize".to_string()),
                    None
                ),
                (
                    "sent".to_string(),
                    2,
                    None,
                    Some("1500000000000000011".to_string())
                ),
            ]
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn duplicate_delivery_replay_returns_prior_message_metadata_without_resend() {
        let pg_db = create_test_pg_db().await;
        let pool = pg_db.connect_and_migrate().await;
        let dispatch_id = "dispatch-duplicate-replay";
        seed_dispatch(&pool, dispatch_id).await;

        let transport =
            RecordingDispatchTransport::new("1500000000000000020", "1500000000000000021");
        let first = send_dispatch_with_delivery_guard(
            None,
            Some(&pool),
            "agent-1",
            "Duplicate replay",
            "card-duplicate-replay",
            dispatch_id,
            &transport,
        )
        .await
        .unwrap();
        assert_eq!(first.status, "success");

        let second = send_dispatch_with_delivery_guard(
            None,
            Some(&pool),
            "agent-1",
            "Duplicate replay",
            "card-duplicate-replay",
            dispatch_id,
            &transport,
        )
        .await
        .unwrap();

        assert_eq!(transport.calls(), 1);
        assert_eq!(second.status, "duplicate");
        assert_eq!(
            second.correlation_id.as_deref(),
            Some("dispatch:dispatch-duplicate-replay")
        );
        assert_eq!(
            second.semantic_event_id.as_deref(),
            Some("dispatch:dispatch-duplicate-replay:notify")
        );
        assert_eq!(
            second.target_channel_id.as_deref(),
            Some("1500000000000000020")
        );
        assert_eq!(second.message_id.as_deref(), Some("1500000000000000021"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn active_dispatch_delivery_unique_key_allows_one_concurrent_reserved_row() {
        let pg_db = create_test_pg_db().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(2).await;
        let dispatch_id = "dispatch-active-unique";
        seed_dispatch(&pool, dispatch_id).await;

        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let pool = pool.clone();
            let barrier = barrier.clone();
            let dispatch_id = dispatch_id.to_string();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                insert_reserved_dispatch_delivery_event_pg(&pool, &dispatch_id, None, None)
                    .await
                    .unwrap()
            }));
        }

        let mut inserted = 0;
        for task in tasks {
            if task.await.unwrap() {
                inserted += 1;
            }
        }
        assert_eq!(inserted, 1);
        assert_eq!(delivery_event_count(&pool, dispatch_id).await, 1);

        let (status, attempt): (String, i32) = sqlx::query_as(
            "SELECT status, attempt
               FROM dispatch_delivery_events
              WHERE dispatch_id = $1",
        )
        .bind(dispatch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "reserved");
        assert_eq!(attempt, 1);

        pool.close().await;
        pg_db.drop().await;
    }
}
