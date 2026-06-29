use super::{DispatchNotifyDeliveryResult, DispatchTransport};
use crate::db::dispatches::delivery_events::{
    DispatchDeliveryEventFinalize, DispatchDeliveryEventStatus,
    finalize_dispatch_delivery_event_conn, insert_reserved_dispatch_delivery_event_pg,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::time::Duration;

/// Backoff schedule for retrying each durable delivery-finalize write (#3861). A
/// transient DB blip during finalize must NOT leave a delivered message without a
/// durable dedup key, so we retry — and ultimately surface the error — instead of
/// the original `.ok()` swallow that left it to reservation expiry.
const DELIVERY_FINALIZE_RETRY_BACKOFFS: [Duration; 4] = [
    Duration::from_millis(100),
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_millis(1000),
];

// #3861 dedup-durability invariant. The `dispatch_notified:*` anchor written below
// is the timing-independent proof of a completed send that reconcile's
// notified-backfill (`recover_orphan_dispatch_notified_pg`) relies on. It is
// committed independently and BEFORE the reserving key is deleted, so after a
// successful send there is ALWAYS either an active `dispatch_reserving:*` key (a
// retry / outbox reclaim will converge) or a durable `dispatch_notified:*` key
// (reconcile backfills the ledger to `sent`, no resend) — never neither.
//
// SCOPE — what this PR closes vs. the irreducible residual (verified against the
// outbox worker + reconcile paths; do not overstate):
//   * CLOSED — transient finalize failure: a transient typed-finalize /
//     reserving-delete failure (or its swallowed `.ok()` predecessor) can no
//     longer strand a delivered message with no dedup proof. STEP 1 commits the
//     anchor first, independently, and reconcile backfills `sent` from it; a
//     bookkeeping failure is self-healing.
//   * CLOSED — slow/hanging send: a transport `send_dispatch` that ran longer
//     than the reservation TTL used to let an outbox reclaim observe an EXPIRED
//     reservation while the original worker was still in-flight, and re-send.
//     `reqwest::Client::new()` carries no request timeout, so that wall-clock was
//     unbounded. We now bound the whole send to `DISPATCH_SEND_DEADLINE_SECS`,
//     held strictly below the reservation TTL by the static assertion below, so
//     the original worker always settles (success or timeout-as-failure) before
//     its reservation can expire — the reclaim either sees the durable anchor or
//     an active reservation, never an in-flight-but-expired one.
//   * IRREDUCIBLE: the remaining duplicate paths are (i) a process crash in the
//     narrow window between the Discord HTTP send returning and the anchor INSERT
//     committing, (ii) a total DB outage spanning the entire reservation TTL (so
//     the anchor write, the outbox `mark_done`, AND reconcile all fail), and
//     (iii) at-least-once "delivered but the ack/response was lost" — a crashed
//     or deadline-timed-out send that actually reached Discord, which the retry /
//     reclaim then re-sends because nothing can prove the lost-ack delivery
//     happened. No design removes these: an external HTTP send and a local DB
//     commit cannot be made atomic, and a lost ack is indistinguishable from a
//     non-delivery. Anchor-FIRST ordering minimizes window (i) to one statement,
//     and a STEP 1 permanent failure is escalated as an operator-alertable
//     invariant breach so it is never silent.
//   * NOT a duplicate (verified): in an ALIVE process a STEP 1 anchor failure
//     still returns the transport's Ok, so the worker marks the outbox row `done`
//     (mark_outbox_done_pg) and it is never reclaimed — zero re-send; the typed
//     ledger merely settles to a cosmetic `failed` that reconcile cannot upgrade
//     without the anchor.
//
// The outbox reclaim backstop holds because the delivery reservation is acquired
// AFTER the outbox claim (reserved_until = t_deliv+TTL > t_outbox+TTL), so the
// earliest reclaim (claimed_at + stale) observes an active reservation and
// short-circuits as a duplicate — AS LONG AS the send itself settles before the
// reservation expires (the `DISPATCH_SEND_DEADLINE_SECS < TTL` assertion) and the
// outbox stale threshold is <= the reservation TTL. Both static assertions lock
// those orderings so the constants cannot silently drift apart.
const _: () = assert!(
    crate::db::dispatches::outbox::DISPATCH_OUTBOX_CLAIM_STALE_SECS
        <= DISPATCH_DELIVERY_RESERVATION_TTL_SECS,
    "outbox stale threshold must be <= dispatch delivery reservation TTL or a reclaim \
     could re-send before the notified dedup anchor becomes durable (#3861)"
);
const _: () = assert!(
    (DISPATCH_SEND_DEADLINE_SECS as i64) < DISPATCH_DELIVERY_RESERVATION_TTL_SECS,
    "dispatch transport send deadline must be < reservation TTL or a slow/hanging send \
     could outlive its reservation and let an outbox reclaim re-send it (#3861)"
);

/// Reservation TTL for `dispatch_reserving:*` guard keys and the typed
/// `reserved` ledger row (both written as `NOW() + INTERVAL '5 minutes'`).
/// Mirrored here as a constant so the #3861 dedup-durability invariant above can
/// be checked at compile time.
const DISPATCH_DELIVERY_RESERVATION_TTL_SECS: i64 = 300;

/// Hard wall-clock ceiling for a single `send_dispatch` transport call (#3861).
/// `reqwest::Client::new()` sets no request timeout, so without this a hung or
/// repeatedly rate-limited Discord HTTP send could outlive its 300s reservation
/// and let an outbox reclaim re-send it. Held strictly below the reservation TTL
/// (asserted above) with generous head-room over the realistic worst case (a
/// message POST plus a few bounded 429 backoffs of <=10s each), so legitimate
/// sends never trip it while pathological hangs are contained.
const DISPATCH_SEND_DEADLINE_SECS: u64 = 120;

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

    let send_result = send_dispatch_within_deadline(
        transport,
        db.cloned(),
        agent_id.to_string(),
        title.to_string(),
        card_id.to_string(),
        dispatch_id,
        Duration::from_secs(DISPATCH_SEND_DEADLINE_SECS),
    )
    .await;

    if let Err(finalize_error) =
        finalize_dispatch_delivery_guard(pg_pool, dispatch_id, send_result.as_ref()).await
    {
        // The error is no longer swallowed: finalize already retried each durable
        // write, and on a successful send we must NOT re-drive it from here
        // (returning Err would make the caller re-send an already-delivered
        // message). On a successful send the durable `notified` anchor is written
        // and committed FIRST, before anything else, so even when the later
        // bookkeeping fails the anchor (or, in its narrow pre-commit window, the
        // still-active reservation) keeps reconcile / outbox reclaim from
        // re-sending the same Discord message (#3861).
        if send_result.is_ok() {
            tracing::error!(
                dispatch_id,
                error = %finalize_error,
                "[dispatch] delivery finalize failed after retries; message was delivered but dedup ledger is not yet durable"
            );
        } else {
            tracing::warn!(
                dispatch_id,
                error = %finalize_error,
                "[dispatch] delivery finalize failed after retries for a failed send"
            );
        }
    }
    send_result
}

/// Run a transport `send_dispatch` under a hard wall-clock `deadline`, mapping a
/// timeout to a transport error (#3861).
///
/// `reqwest::Client::new()` carries no request timeout, so a hung or repeatedly
/// rate-limited Discord HTTP send could otherwise outlive the delivery
/// reservation and let an outbox reclaim re-send it. Bounding the send below the
/// reservation TTL guarantees the original worker settles before its reservation
/// can expire. A timeout is surfaced as a normal failed send: it flows through
/// the failed-send finalize (no anchor; reserving key released so the dispatch
/// can be retried). The residual at-least-once exposure — a timed-out send that
/// actually reached Discord — is irreducible (a lost ack cannot be distinguished
/// from a non-delivery).
async fn send_dispatch_within_deadline<T: DispatchTransport>(
    transport: &T,
    db: Option<crate::db::Db>,
    agent_id: String,
    title: String,
    card_id: String,
    dispatch_id: &str,
    deadline: Duration,
) -> Result<DispatchNotifyDeliveryResult, String> {
    match tokio::time::timeout(
        deadline,
        transport.send_dispatch(db, agent_id, title, card_id, dispatch_id.to_string()),
    )
    .await
    {
        Ok(result) => result,
        Err(_elapsed) => Err(format!(
            "dispatch transport send for {dispatch_id} exceeded the {}s deadline \
             (bounded below the reservation TTL to prevent a slow-send reclaim duplicate)",
            deadline.as_secs()
        )),
    }
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
) -> Result<(), String> {
    let Some(pool) = pg_pool else {
        return Ok(());
    };

    // STEP 1 — durable dedup ANCHOR (success only), committed on its OWN
    // autocommit statement FIRST, before any other bookkeeping (#3861).
    //
    // The `dispatch_notified:*` key is the timing-independent proof of a completed
    // send that reconcile's notified-backfill (`recover_orphan_dispatch_notified_pg`)
    // relies on to converge the typed ledger to `sent` WITHOUT re-sending. Keeping
    // it independent of the bookkeeping transaction below means a failure isolated
    // to the reserving-delete or the typed-ledger finalize can never roll the
    // anchor back — which a single all-in-one transaction would. We still retry it
    // and surface the error instead of the original `.ok()` swallow.
    //
    // If the anchor STILL cannot be persisted after the bounded retries, the
    // message was already delivered but no durable dedup proof exists — the
    // operationally dangerous "delivered but not deduped" state. We escalate it as
    // an operator-alertable invariant breach (ERROR log + structured analytics
    // event) so it is never silent, then surface the error. The residual exposure
    // here (a reclaim re-sending only after the 5-minute reservation also expires
    // with no anchor) is the irreducible send↔durable-anchor window — see the
    // module-level invariant comment.
    if send_result.is_ok() {
        let mut attempt = 0usize;
        loop {
            match persist_dispatch_notified_anchor(pool, dispatch_id).await {
                Ok(()) => break,
                Err(error) => {
                    match delivery_finalize_backoff_or_fail(
                        dispatch_id,
                        "persist notified dedup anchor",
                        attempt,
                        error,
                    )
                    .await
                    {
                        Ok(()) => attempt += 1,
                        Err(terminal) => {
                            record_dispatch_notified_anchor_durability_breach(
                                dispatch_id,
                                &terminal,
                            );
                            return Err(terminal);
                        }
                    }
                }
            }
        }
    }

    // STEP 2 — atomic bookkeeping: delete the reserving guard key and finalize the
    // typed ledger row together. On a successful send the anchor above already
    // guarantees no duplicate, so a failure here is non-fatal for dedup (reconcile
    // backfills from the anchor); on a FAILED send there is no anchor, but the
    // reserving key is released so a retry can re-drive. Either way we retry and
    // surface the error rather than leaving it to reservation expiry.
    let mut attempt = 0usize;
    loop {
        match finalize_dispatch_delivery_bookkeeping_once(pool, dispatch_id, send_result).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                delivery_finalize_backoff_or_fail(
                    dispatch_id,
                    "finalize delivery bookkeeping",
                    attempt,
                    error,
                )
                .await?;
                attempt += 1;
            }
        }
    }
}

/// Decide whether to retry a failed durable finalize write. Sleeps the next
/// backoff and returns `Ok(())` (caller should retry), or returns the terminal
/// error string once the bounded schedule is exhausted — never swallowing it.
async fn delivery_finalize_backoff_or_fail(
    dispatch_id: &str,
    step: &str,
    attempt: usize,
    error: sqlx::Error,
) -> Result<(), String> {
    if attempt >= DELIVERY_FINALIZE_RETRY_BACKOFFS.len() {
        return Err(format!(
            "{step} for {dispatch_id} after {} attempts: {error}",
            attempt + 1
        ));
    }
    tracing::warn!(
        dispatch_id,
        step,
        attempt = attempt + 1,
        error = %error,
        "[dispatch] durable delivery finalize write failed; retrying"
    );
    tokio::time::sleep(DELIVERY_FINALIZE_RETRY_BACKOFFS[attempt]).await;
    Ok(())
}

/// Escalate a STEP 1 permanent failure: the transport already delivered the
/// message, but the durable `dispatch_notified:*` dedup anchor could not be
/// persisted after retries — "delivered but not deduped". Surface it as an
/// operator-alertable invariant breach (ERROR-level log + structured
/// `invariant_violation` analytics event) so the silent-but-dangerous state is
/// visible on dashboards/alerts (#3861).
fn record_dispatch_notified_anchor_durability_breach(dispatch_id: &str, detail: &str) {
    crate::services::observability::record_invariant_check(
        false,
        crate::services::observability::InvariantViolation {
            provider: None,
            channel_id: None,
            dispatch_id: Some(dispatch_id),
            session_key: None,
            turn_id: None,
            invariant: "dispatch_delivery_notified_anchor_durable",
            code_location: "src/services/dispatches/discord_delivery/guard.rs:finalize_dispatch_delivery_guard",
            message: "delivery succeeded but the dispatch_notified dedup anchor did not persist after retries",
            details: json!({ "dispatch_id": dispatch_id, "detail": detail }),
        },
    );
}

/// Commit the `dispatch_notified:*` dedup anchor on its own autocommit statement.
/// No expiry: this key is the permanent durable proof that the send landed.
async fn persist_dispatch_notified_anchor(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(notified_key(dispatch_id))
    .bind(dispatch_id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// One atomic attempt at the non-anchor bookkeeping: delete the reserving guard
/// key and finalize the typed ledger row inside a single transaction so they
/// commit together or not at all. On error the transaction rolls back, leaving
/// the still-active reservation (and, on success, the already-durable notified
/// anchor) untouched so a retry — or reconcile recovery — can converge without a
/// duplicate send.
async fn finalize_dispatch_delivery_bookkeeping_once(
    pool: &PgPool,
    dispatch_id: &str,
    send_result: Result<&DispatchNotifyDeliveryResult, &String>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    sqlx::query("DELETE FROM kv_meta WHERE key = $1")
        .bind(reserving_key(dispatch_id))
        .execute(&mut *tx)
        .await?;

    let finalize = dispatch_delivery_event_finalize_input(dispatch_id, send_result);
    finalize_dispatch_delivery_event_conn(&mut *tx, finalize).await?;

    tx.commit().await
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
        send_delay: Option<Duration>,
    }

    impl RecordingDispatchTransport {
        fn new(target_channel_id: &str, message_id: &str) -> Self {
            Self {
                calls: Arc::new(Mutex::new(0)),
                target_channel_id: target_channel_id.to_string(),
                message_id: message_id.to_string(),
                assert_reserving_during_send: None,
                send_delay: None,
            }
        }

        fn with_reservation_assertion(mut self, pool: PgPool) -> Self {
            self.assert_reserving_during_send = Some(pool);
            self
        }

        /// Make `send_dispatch` sleep before returning, to exercise the #3861
        /// send-deadline bound.
        fn with_send_delay(mut self, delay: Duration) -> Self {
            self.send_delay = Some(delay);
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
            if let Some(delay) = self.send_delay {
                tokio::time::sleep(delay).await;
            }
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

    #[tokio::test]
    async fn kv_meta_markers_remain_authoritative_when_typed_shadow_missing() {
        let pg_db = create_test_pg_db().await;
        let pool = pg_db.connect_and_migrate().await;

        let notified_dispatch_id = "dispatch-kv-notified-authority";
        seed_dispatch(&pool, notified_dispatch_id).await;
        sqlx::query("INSERT INTO kv_meta (key, value) VALUES ($1, $2)")
            .bind(notified_key(notified_dispatch_id))
            .bind(notified_dispatch_id)
            .execute(&pool)
            .await
            .unwrap();

        assert!(
            !claim_dispatch_delivery_guard(Some(&pool), notified_dispatch_id)
                .await
                .unwrap(),
            "kv_meta notified marker must block sends without a typed shadow row"
        );
        assert_eq!(delivery_event_count(&pool, notified_dispatch_id).await, 0);

        let reserving_dispatch_id = "dispatch-kv-reserving-authority";
        seed_dispatch(&pool, reserving_dispatch_id).await;
        sqlx::query(
            "INSERT INTO kv_meta (key, value, expires_at)
             VALUES ($1, $2, NOW() + INTERVAL '5 minutes')",
        )
        .bind(reserving_key(reserving_dispatch_id))
        .bind(reserving_dispatch_id)
        .execute(&pool)
        .await
        .unwrap();

        assert!(
            !claim_dispatch_delivery_guard(Some(&pool), reserving_dispatch_id)
                .await
                .unwrap(),
            "kv_meta reservation marker must block sends without a typed shadow row"
        );
        assert_eq!(
            kv_meta_count(&pool, &reserving_key(reserving_dispatch_id)).await,
            1
        );
        assert_eq!(delivery_event_count(&pool, reserving_dispatch_id).await, 0);

        pool.close().await;
        pg_db.drop().await;
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
        finalize_dispatch_delivery_guard(Some(&pool), dispatch_id, Ok(&result))
            .await
            .unwrap();

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
        finalize_dispatch_delivery_guard(Some(&pool), dispatch_id, Err(&error))
            .await
            .unwrap();

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
        finalize_dispatch_delivery_guard(Some(&pool), dispatch_id, Err(&first_error))
            .await
            .unwrap();

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
        finalize_dispatch_delivery_guard(Some(&pool), dispatch_id, Ok(&result))
            .await
            .unwrap();

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

    async fn delivery_event_status(pool: &PgPool, dispatch_id: &str) -> String {
        sqlx::query_scalar::<_, String>(
            "SELECT status
               FROM dispatch_delivery_events
              WHERE dispatch_id = $1
              ORDER BY attempt DESC
              LIMIT 1",
        )
        .bind(dispatch_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Inject a mid-sequence DB failure: a CHECK constraint that rejects the
    /// typed-ledger finalize's terminal `sent` status. BEGIN and the in-tx
    /// reserving-key DELETE still succeed, so the finalize statement specifically
    /// fails — exercising real transactional rollback rather than a `pool.begin()`
    /// failure that never executes a statement.
    async fn block_typed_sent_finalize(pool: &PgPool) {
        sqlx::query(
            "ALTER TABLE dispatch_delivery_events
                 ADD CONSTRAINT block_sent_3861 CHECK (status <> 'sent')",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn unblock_typed_sent_finalize(pool: &PgPool) {
        sqlx::query("ALTER TABLE dispatch_delivery_events DROP CONSTRAINT block_sent_3861")
            .execute(pool)
            .await
            .unwrap();
    }

    fn success_result(
        dispatch_id: &str,
        channel_id: &str,
        message_id: &str,
    ) -> DispatchNotifyDeliveryResult {
        DispatchNotifyDeliveryResult {
            status: "success".to_string(),
            dispatch_id: dispatch_id.to_string(),
            action: "notify".to_string(),
            correlation_id: Some(format!("dispatch:{dispatch_id}")),
            semantic_event_id: Some(format!("dispatch:{dispatch_id}:notify")),
            target_channel_id: Some(channel_id.to_string()),
            message_id: Some(message_id.to_string()),
            fallback_kind: None,
            detail: Some("sent".to_string()),
        }
    }

    /// #3861: the durable `notified` dedup ANCHOR is committed independently
    /// BEFORE the bookkeeping transaction, so a failure isolated to the
    /// typed-ledger finalize (a) surfaces the error (not swallowed), (b) leaves
    /// the bookkeeping atomic — the reserving key is NOT deleted (rolled back),
    /// and (c) does NOT roll back the anchor. This is the property a single
    /// all-in-one transaction would regress.
    #[tokio::test]
    async fn delivery_finalize_notified_anchor_survives_bookkeeping_failure() {
        let pg_db = create_test_pg_db().await;
        let pool = pg_db.connect_and_migrate().await;
        let dispatch_id = "dispatch-anchor-survives-bookkeeping-fail";
        seed_dispatch(&pool, dispatch_id).await;

        assert!(
            claim_dispatch_delivery_guard(Some(&pool), dispatch_id)
                .await
                .unwrap()
        );
        let result = success_result(dispatch_id, "1500000000000000030", "1500000000000000031");

        // The typed finalize statement (status -> 'sent') fails mid-transaction.
        block_typed_sent_finalize(&pool).await;
        let finalize =
            finalize_dispatch_delivery_guard(Some(&pool), dispatch_id, Ok(&result)).await;
        assert!(
            finalize.is_err(),
            "bookkeeping failure must surface the error, not swallow it"
        );

        // (a) anchor is durable despite the bookkeeping failure (independent commit).
        assert_eq!(kv_meta_count(&pool, &notified_key(dispatch_id)).await, 1);
        // (b) bookkeeping rolled back atomically: reserving key intact, row still reserved.
        assert_eq!(kv_meta_count(&pool, &reserving_key(dispatch_id)).await, 1);
        assert_eq!(delivery_event_count(&pool, dispatch_id).await, 1);
        assert_eq!(delivery_event_status(&pool, dispatch_id).await, "reserved");

        // Once the DB recovers, retrying the bookkeeping converges to `sent`.
        unblock_typed_sent_finalize(&pool).await;
        finalize_dispatch_delivery_guard(Some(&pool), dispatch_id, Ok(&result))
            .await
            .unwrap();
        assert_eq!(kv_meta_count(&pool, &reserving_key(dispatch_id)).await, 0);
        assert_eq!(kv_meta_count(&pool, &notified_key(dispatch_id)).await, 1);
        assert_eq!(delivery_event_status(&pool, dispatch_id).await, "sent");

        pool.close().await;
        pg_db.drop().await;
    }

    /// #3861 core regression guard, driven through the real delivery entry point
    /// (`send_dispatch_with_delivery_guard`) for BOTH the first processing and the
    /// reclaim redrive — so the mock transport actually sends, and we assert the
    /// total transport call count is exactly ONE (first send only, zero re-send).
    ///
    /// Flow: first processing sends once -> STEP 2 bookkeeping PERMANENTLY fails
    /// (CHECK constraint) while the STEP 1 anchor stays durable -> reservation
    /// expires -> reconcile backfills `sent` from the anchor -> stale-outbox
    /// reclaim re-drives the same entry point -> the guard short-circuits as a
    /// duplicate. A single all-in-one finalize transaction would have rolled the
    /// anchor back on the bookkeeping failure and re-sent on the reclaim.
    #[tokio::test]
    async fn delivery_first_send_not_resent_by_reclaim_after_permanent_bookkeeping_failure() {
        let pg_db = create_test_pg_db().await;
        let pool = pg_db.connect_and_migrate().await;
        let dispatch_id = "dispatch-worker-path-no-resend";
        seed_dispatch(&pool, dispatch_id).await;

        let transport =
            RecordingDispatchTransport::new("1500000000000000040", "1500000000000000041");

        // 1. FIRST worker processing through the real entry point. The transport
        //    sends (call #1); STEP 1 anchor commits; STEP 2 bookkeeping permanently
        //    fails against the CHECK constraint. The guard still returns the
        //    transport's success (it must not re-drive a delivered message).
        block_typed_sent_finalize(&pool).await;
        let first = send_dispatch_with_delivery_guard(
            None,
            Some(&pool),
            "agent-1",
            "Worker path first send",
            "card-worker-path",
            dispatch_id,
            &transport,
        )
        .await
        .unwrap();
        assert_eq!(first.status, "success");
        assert_eq!(
            transport.calls(),
            1,
            "first processing must send exactly once"
        );
        assert_eq!(
            kv_meta_count(&pool, &notified_key(dispatch_id)).await,
            1,
            "notified anchor must be durable even when bookkeeping fails"
        );
        assert_eq!(delivery_event_status(&pool, dispatch_id).await, "reserved");
        unblock_typed_sent_finalize(&pool).await;

        // 2. The reservation expires before any retry succeeds (DB was down long
        //    enough): age out both the kv guard key and the typed reservation.
        sqlx::query("UPDATE kv_meta SET expires_at = NOW() - INTERVAL '1 minute' WHERE key = $1")
            .bind(reserving_key(dispatch_id))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE dispatch_delivery_events
                SET reserved_until = NOW() - INTERVAL '1 minute'
              WHERE dispatch_id = $1 AND status = 'reserved'",
        )
        .bind(dispatch_id)
        .execute(&pool)
        .await
        .unwrap();

        // 3. Reconcile recovery runs: the notified anchor lets it backfill the
        //    typed ledger to `sent` instead of leaving a `failed` (= "not sent")
        //    row that a reclaim would re-send.
        crate::reconcile::reconcile_dispatch_delivery_events_pg(&pool)
            .await
            .unwrap();
        assert_eq!(kv_meta_count(&pool, &notified_key(dispatch_id)).await, 1);
        assert_eq!(delivery_event_status(&pool, dispatch_id).await, "sent");

        // 4. Stale-outbox reclaim re-drives the SAME entry point. The guard must
        //    short-circuit as a duplicate — the transport call count stays at ONE.
        let redrive = send_dispatch_with_delivery_guard(
            None,
            Some(&pool),
            "agent-1",
            "Worker path reclaim redrive",
            "card-worker-path",
            dispatch_id,
            &transport,
        )
        .await
        .unwrap();
        assert_eq!(
            transport.calls(),
            1,
            "reclaim redrive must NOT re-send: total transport sends stays exactly 1"
        );
        assert_eq!(redrive.status, "duplicate");
        // The message_id was lost with the failed bookkeeping and the anchor stores
        // only the dispatch_id, so recovery cannot reconstruct it — acceptable,
        // since the invariant under test is ZERO re-sends, not metadata fidelity.
        assert_eq!(redrive.message_id, None);

        pool.close().await;
        pg_db.drop().await;
    }

    /// #3861 point (b): a STEP 1 anchor permanent failure (delivered but no
    /// durable dedup proof) must SURFACE — the guard returns `Err` rather than
    /// swallowing it — so the operator-alert path
    /// (`record_dispatch_notified_anchor_durability_breach`) fires. Injected by a
    /// CHECK constraint that blocks the `dispatch_notified:*` anchor key only.
    #[tokio::test]
    async fn delivery_finalize_step1_anchor_permanent_failure_is_surfaced() {
        let pg_db = create_test_pg_db().await;
        let pool = pg_db.connect_and_migrate().await;
        let dispatch_id = "dispatch-step1-anchor-failure";
        seed_dispatch(&pool, dispatch_id).await;

        assert!(
            claim_dispatch_delivery_guard(Some(&pool), dispatch_id)
                .await
                .unwrap()
        );
        let result = success_result(dispatch_id, "1500000000000000050", "1500000000000000051");

        // Block ONLY the notified anchor key (the reserving key is unaffected, so
        // STEP 1 specifically fails).
        sqlx::query(
            "ALTER TABLE kv_meta
                 ADD CONSTRAINT block_notified_3861
                 CHECK (key NOT LIKE 'dispatch\\_notified:%')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let finalize =
            finalize_dispatch_delivery_guard(Some(&pool), dispatch_id, Ok(&result)).await;
        assert!(
            finalize.is_err(),
            "STEP 1 anchor permanent failure must be surfaced, not swallowed"
        );
        // Operator-alert path fired: a structured `invariant_violation` event for
        // this dispatch was emitted (the breach fn ran). Read immediately — there
        // is no await between the breach emit inside finalize and this read, so the
        // event cannot have been evicted by a parallel test.
        let breach = crate::services::observability::events::recent(64)
            .into_iter()
            .find(|event| {
                event.event_type == "invariant_violation"
                    && event.payload.get("invariant").and_then(|v| v.as_str())
                        == Some("dispatch_delivery_notified_anchor_durable")
                    && event.payload.to_string().contains(dispatch_id)
            });
        assert!(
            breach.is_some(),
            "STEP 1 anchor failure must emit an operator-alertable invariant_violation event"
        );
        // STEP 1 failed before STEP 2: anchor absent, reservation still intact.
        assert_eq!(kv_meta_count(&pool, &notified_key(dispatch_id)).await, 0);
        assert_eq!(kv_meta_count(&pool, &reserving_key(dispatch_id)).await, 1);
        assert_eq!(delivery_event_status(&pool, dispatch_id).await, "reserved");

        sqlx::query("ALTER TABLE kv_meta DROP CONSTRAINT block_notified_3861")
            .execute(&pool)
            .await
            .unwrap();

        pool.close().await;
        pg_db.drop().await;
    }

    /// #3861 P0: a transport send that outlives its deadline must be bounded and
    /// mapped to a failed send (so it flows through the failed-send finalize and
    /// releases the reservation), instead of hanging past the reservation TTL and
    /// letting an outbox reclaim re-send it. A send within the deadline passes
    /// through unchanged.
    #[tokio::test]
    async fn dispatch_send_deadline_bounds_a_hanging_transport() {
        let slow = RecordingDispatchTransport::new("1500000000000000060", "1500000000000000061")
            .with_send_delay(Duration::from_secs(30));
        let timed_out = send_dispatch_within_deadline(
            &slow,
            None,
            "agent-1".to_string(),
            "Slow send".to_string(),
            "card-slow-send".to_string(),
            "dispatch-slow-send",
            Duration::from_millis(50),
        )
        .await;
        let error = timed_out.expect_err("a send exceeding the deadline must fail, not hang");
        assert!(
            error.contains("deadline"),
            "timeout error should name the deadline: {error}"
        );

        let fast = RecordingDispatchTransport::new("1500000000000000062", "1500000000000000063");
        let ok = send_dispatch_within_deadline(
            &fast,
            None,
            "agent-1".to_string(),
            "Fast send".to_string(),
            "card-fast-send".to_string(),
            "dispatch-fast-send",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(ok.status, "success");
        assert_eq!(fast.calls(), 1);
    }

    /// The #3861 send-deadline constant must stay strictly below the reservation
    /// TTL (also enforced by the module-level `const _` assertion).
    #[test]
    fn dispatch_send_deadline_is_below_reservation_ttl() {
        assert!((DISPATCH_SEND_DEADLINE_SECS as i64) < DISPATCH_DELIVERY_RESERVATION_TTL_SECS);
    }
}
