use super::{DispatchNotifyDeliveryResult, DispatchTransport};
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

    finalize_dispatch_delivery_guard(pg_pool, dispatch_id, send_result.is_ok()).await;
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
    Ok(result.rows_affected() > 0)
}

async fn finalize_dispatch_delivery_guard(
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
    success: bool,
) {
    let Some(pool) = pg_pool else {
        return;
    };
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
