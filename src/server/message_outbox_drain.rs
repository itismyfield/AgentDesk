use super::{
    Arc, PgPool, claim_pending_message_outbox_batch_pg, mark_message_outbox_failed_pg,
    mark_message_outbox_sent_pg, outbox_actionable_delivery, outbox_delivery_alert,
    release_session_for_terminal_outbox_failure, schedule_message_outbox_retry_pg,
};

const MESSAGE_OUTBOX_MAX_RETRY_COUNT: i64 = 4;
const MESSAGE_OUTBOX_RETRY_BACKOFF_SECS: [i64; 4] = [60, 300, 900, 3600];

/// A `message_outbox` row claimed (up to 10 per batch) for in-process Discord delivery.
#[derive(Clone, Debug)]
pub(super) struct PendingMessageOutboxRow {
    pub(super) id: i64,
    pub(super) target: String,
    pub(super) content: String,
    pub(super) bot: String,
    pub(super) source: String,
    pub(super) reason_code: Option<String>,
    pub(super) session_key: Option<String>,
    pub(super) retry_count: i64,
    pub(super) attachment:
        Result<Option<Arc<crate::services::message_outbox::StoredOutboxAttachment>>, &'static str>,
    pub(super) claim_owner: String,
    pub(super) claimed_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MessageOutboxFailureAction {
    Retry { retry_count: i64, backoff_secs: i64 },
    Fail { retry_count: i64 },
}

pub(super) fn is_terminal_turn_delivery_outbox_source(source: &str) -> bool {
    matches!(source, "headless_turn" | "turn_bridge_tmux_handoff")
}

#[cfg(test)]
pub(super) fn session_can_be_released_for_terminal_outbox_failure(
    session_key: Option<&str>,
    failed_outbox_session_key: Option<&str>,
    active_turn_delivery_outbox_id: Option<i64>,
    failed_outbox_id: i64,
) -> bool {
    let session_key_matches = match failed_outbox_session_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(failed_session_key) => session_key == Some(failed_session_key),
        None => true,
    };

    session_key_matches && active_turn_delivery_outbox_id == Some(failed_outbox_id)
}

pub(super) fn message_outbox_failure_action(retry_count: i64) -> MessageOutboxFailureAction {
    let next_retry_count = retry_count.saturating_add(1);
    if next_retry_count > MESSAGE_OUTBOX_MAX_RETRY_COUNT {
        return MessageOutboxFailureAction::Fail {
            retry_count: next_retry_count,
        };
    }

    let backoff_idx = (next_retry_count - 1) as usize;
    let backoff_secs = MESSAGE_OUTBOX_RETRY_BACKOFF_SECS
        .get(backoff_idx)
        .copied()
        .unwrap_or(3600);
    MessageOutboxFailureAction::Retry {
        retry_count: next_retry_count,
        backoff_secs,
    }
}

pub(super) async fn drain_message_outbox_batch_once<F, Fut>(
    pg_pool: &PgPool,
    claim_owner: Option<&str>,
    mut deliver: F,
) -> usize
where
    F: FnMut(PendingMessageOutboxRow) -> Fut,
    Fut: std::future::Future<Output = (String, String)>,
{
    let pending =
        claim_pending_message_outbox_batch_pg(pg_pool, claim_owner.unwrap_or("message-outbox"))
            .await;
    if pending.is_empty() {
        return 0;
    }

    for row in &pending {
        // Re-check circuit authority under the claim lease: `revoke_on_fresh_vouch` misses rows
        // superseded after their claim. Fails closed: any error skips the row this cycle.
        use crate::services::message_outbox_circuit_authority::DeliveryFenceOutcome;
        match crate::services::message_outbox_circuit_authority::fence_claimed_delivery(
            pg_pool,
            row.id,
            &row.claim_owner,
            row.claimed_at,
        )
        .await
        {
            Ok(DeliveryFenceOutcome::Cleared) => {}
            Ok(DeliveryFenceOutcome::Fenced) => {
                tracing::info!(
                    outbox_id = row.id,
                    target = %row.target,
                    "[outbox] delivery fenced: circuit authority superseded before send"
                );
                continue;
            }
            Ok(DeliveryFenceOutcome::LeaseLost) => {
                tracing::warn!(
                    outbox_id = row.id,
                    claim_owner = %row.claim_owner,
                    "[outbox] delivery fence: stale lease, skipping send"
                );
                continue;
            }
            Err(error) => {
                tracing::warn!(
                    outbox_id = row.id,
                    %error,
                    "[outbox] delivery fence db error, deferring send this cycle"
                );
                continue;
            }
        }
        if crate::services::message_outbox::routine_incident::is_incident(
            &row.source,
            row.reason_code.as_deref(),
        ) {
            outbox_actionable_delivery::routine_incident::deliver(pg_pool, row, || {
                deliver(row.clone())
            })
            .await;
            continue;
        }
        let (status, err_text) = match &row.attachment {
            Err(error) => ("invalid_attachment".to_string(), error.to_string()),
            Ok(_) => deliver(row.clone()).await,
        };
        if status == "200 OK" {
            if mark_message_outbox_sent_pg(pg_pool, row.id, &row.claim_owner, row.claimed_at)
                .await
                .is_ok()
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::debug!(
                    "[{ts}] [outbox] ✅ delivered msg {} → {}",
                    row.id,
                    row.target
                );
            }
        } else {
            let error_text = format!("{status}: {err_text}");
            let action = outbox_actionable_delivery::failure_action(row, &status);
            match action {
                MessageOutboxFailureAction::Fail { retry_count } => {
                    let failed_update = mark_message_outbox_failed_pg(
                        pg_pool,
                        row.id,
                        &error_text,
                        retry_count,
                        &row.claim_owner,
                        row.claimed_at,
                    )
                    .await;
                    // Release only terminal turn-delivery rows, and only while the session
                    // still points at this row (no newer turn has taken the channel).
                    if failed_update.is_ok() && is_terminal_turn_delivery_outbox_source(&row.source)
                    {
                        if let Some(channel_id_str) = row.target.strip_prefix("channel:") {
                            let released_sessions =
                                match release_session_for_terminal_outbox_failure(
                                    pg_pool,
                                    channel_id_str,
                                    row.id,
                                )
                                .await
                                {
                                    Ok(count) => count,
                                    Err(error) => {
                                        tracing::warn!(
                                            "[outbox] failed to release session for channel {} after msg {} failure: {}",
                                            channel_id_str,
                                            row.id,
                                            error,
                                        );
                                        0
                                    }
                                };
                            tracing::warn!(
                                "[outbox] ❌ permanent delivery failure for channel {} (msg {}, released_sessions={}): {}",
                                channel_id_str,
                                row.id,
                                released_sessions,
                                error_text,
                            );
                        }
                    }
                    // After the release, so reporting never delays freeing the channel. The
                    // destination is never notified: it may be the failing target itself.
                    if failed_update.is_ok() {
                        outbox_delivery_alert::note_terminal_outbox_delivery_failure(
                            row,
                            &error_text,
                        );
                    }
                }
                MessageOutboxFailureAction::Retry {
                    retry_count,
                    backoff_secs,
                } => {
                    let _ = schedule_message_outbox_retry_pg(
                        pg_pool,
                        row.id,
                        &error_text,
                        retry_count,
                        backoff_secs,
                        &row.claim_owner,
                        row.claimed_at,
                    )
                    .await;
                }
            }
            tracing::warn!(
                "[outbox] ❌ msg {} → {} failed: {status} ({action:?})",
                row.id,
                row.target
            );
        }
    }

    pending.len()
}

pub(super) async fn dm_reply_retry_loop(pg_pool: Arc<PgPool>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
    interval.tick().await; // skip immediate first tick
    loop {
        interval.tick().await;
        crate::services::discord::retry_failed_dm_notifications(Some(pg_pool.as_ref())).await;
    }
}
