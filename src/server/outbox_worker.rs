//! Background drain for durable message outbox and detached terminal custody.

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
        if drain_message_outbox_batch_once(pg_pool.as_ref(), Some(&claim_owner), {
            let health_registry = health_registry.clone();
            let pg_pool = pg_pool.clone();
            move |row| {
                let health_registry = health_registry.clone();
                let pg_pool = pg_pool.clone();
                async move {
                    outbox_actionable_delivery::deliver(&health_registry, pg_pool.as_ref(), &row)
                        .await
                }
            }
        })
        .await
            == 0
        {
            // No work: increase interval (up to max)
            poll_interval = (poll_interval.mul_f64(1.5)).min(max_interval);
            continue;
        }
        // Work found: reset to fast polling
        poll_interval = Duration::from_millis(500);
    }
}
