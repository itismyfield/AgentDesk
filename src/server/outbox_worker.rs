//! Background drain for durable message outbox and detached terminal custody.

pub(super) mod claim;

use super::{drain_message_outbox_batch_once, outbox_actionable_delivery};
use crate::services::discord::health::HealthRegistry;
use sqlx::PgPool;
use std::sync::Arc;

pub(super) async fn message_outbox_loop(
    pg_pool: Arc<PgPool>,
    health_registry: Option<Arc<HealthRegistry>>,
) {
    use std::time::Duration;

    let Some(health_registry) = health_registry else {
        tracing::error!("[outbox] Health registry unavailable; message outbox worker stopped");
        return;
    };

    // Give Discord runtime bootstrap a brief head start before polling.
    tokio::time::sleep(Duration::from_secs(3)).await;
    tracing::info!("[outbox] Message outbox worker started (adaptive backoff 500ms-5s)");
    let claim_owner = format!(
        "message-outbox:{}:{}",
        std::env::var("HOSTNAME").unwrap_or_else(|_| "local".to_string()),
        std::process::id()
    );

    let mut poll_interval = Duration::from_millis(500);
    let max_interval = Duration::from_secs(5);
    // Periodic stale-row GC: prune old terminal rows so config rejections do not accumulate.
    let mut next_gc_at = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        tokio::time::sleep(poll_interval).await;

        crate::services::discord::terminal_delivery_custody::drain(&health_registry).await;

        // #3651: the message outbox drain delivers headless terminal responses
        // — foreground turns enqueue here and synchronously block on the row
        // becoming `sent` — so this loop is NOT backpressured. Yielding it under
        // pool pressure would delay (and risk later duplicate-recovery of)
        // user-visible delivery. Only genuinely low-priority chore loops gate on
        // `background_should_yield`.

        if std::time::Instant::now() >= next_gc_at {
            match crate::services::message_outbox::gc_stale_outbox_rows(pg_pool.as_ref()).await {
                Ok((held, failed, sent)) if held + failed + sent > 0 => {
                    tracing::info!(
                        held_pruned = held,
                        failed_pruned = failed,
                        sent_pruned = sent,
                        "[outbox] gc swept stale message_outbox rows"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!("[outbox] gc failed: {error}");
                }
            }
            next_gc_at = std::time::Instant::now() + Duration::from_secs(3600);
        }
        if drain_message_outbox_once(&pg_pool, &health_registry, &claim_owner).await == 0 {
            // No work: increase interval (up to max)
            poll_interval = (poll_interval.mul_f64(1.5)).min(max_interval);
            continue;
        }
        // Work found: reset to fast polling
        poll_interval = Duration::from_millis(500);
    }
}

/// One claim/deliver/settle cycle with the production delivery; returns the rows claimed.
pub(crate) async fn drain_message_outbox_once(
    pg_pool: &Arc<PgPool>,
    health_registry: &Arc<HealthRegistry>,
    claim_owner: &str,
) -> usize {
    drain_message_outbox_batch_once(pg_pool.as_ref(), Some(claim_owner), {
        let health_registry = health_registry.clone();
        let pg_pool = pg_pool.clone();
        move |row| {
            let health_registry = health_registry.clone();
            let pg_pool = pg_pool.clone();
            async move {
                outbox_actionable_delivery::deliver(&health_registry, pg_pool.as_ref(), &row).await
            }
        }
    })
    .await
}

impl super::PendingMessageOutboxRow {
    pub(super) fn delivery_ids(&self) -> (String, String) {
        let reason = self
            .reason_code
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("message");
        let session = self
            .session_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(self.target.as_str());
        (
            format!("message_outbox:{}:{reason}:{session}", self.source),
            format!("message_outbox:{}:deliver", self.id),
        )
    }
}

#[cfg(test)]
mod attachment_pg_tests;

#[cfg(test)]
mod session_notice_pg_tests;

#[cfg(test)]
pub(crate) mod origin_fixture {
    pub(crate) use super::session_notice_pg_tests::Discord;

    #[tokio::test]
    async fn output_origin_stampless_current_owner_outbox_pg() {
        use crate::services::cluster::intake_router_hook::owner_record::output::tests::fixture;
        use crate::services::message_outbox::{self, OutboxMessage};
        use std::sync::Arc;
        let (_db, pool, _origin) = fixture("72280700").await;
        let mut tx = pool.begin().await.unwrap();
        let id = message_outbox::enqueue_outbox_pg_on_tx(
            &mut tx,
            OutboxMessage {
                target: "channel:72280700",
                content: "legacy answer",
                bot: "notify",
                source: "headless_turn",
                reason_code: Some("headless.delivery"),
                session_key: None,
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let mock = Discord::new().await;
        let registry = mock.registry().await;
        let pool = Arc::new(pool);
        assert_eq!(
            super::drain_message_outbox_once(&pool, &registry, "stampless-consumer").await,
            1
        );
        let status: String = sqlx::query_scalar("SELECT status FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
        assert_eq!(status, "held");
        assert!(mock.posts().is_empty());
    }
}

#[cfg(test)]
mod guard_retry_pg_tests;
