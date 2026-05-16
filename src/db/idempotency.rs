//! Stripe-style idempotency-key storage (#2257 concern 5).
//!
//! Backed by the `idempotency_keys` table created in migration `0059`. Each
//! mutation endpoint that opts in calls [`claim`] before running the
//! business logic. The outcome enum tells the handler whether to (a) run
//! the work, (b) replay a cached response, (c) reject because another
//! caller is mid-flight with the same key, or (d) reject because the key
//! was reused with a different request body.
//!
//! Multi-node safe via the PG primary key (`scope`, `key`). The
//! [`record_response`] call stamps the row with the final status/body so
//! later callers replay verbatim. [`gc_expired`] removes rows past their
//! TTL and is intended to run on the existing tick loop.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Row};
use std::time::Duration;

/// Default time-to-live for an idempotency record. Stripe uses 24h; we
/// match that so callers can rely on familiar semantics. Set with
/// `IDEMPOTENCY_KEY_TTL` env if a deployment needs a different window.
pub const DEFAULT_IDEMPOTENCY_TTL: Duration = Duration::from_secs(60 * 60 * 24);

/// Outcome of a [`claim`] call.
#[derive(Debug)]
pub enum IdempotencyOutcome {
    /// Row was just inserted — caller owns the lifecycle and must call
    /// [`record_response`] once the business logic resolves.
    Created,
    /// A prior call already finished — replay the cached response.
    Replay {
        status: u16,
        body: Value,
        completed_at: DateTime<Utc>,
    },
    /// Another caller holds the key but has not finished yet. The right
    /// response at the route layer is `409 Conflict`.
    InFlight,
    /// The key was reused with a different request body. The right
    /// response at the route layer is `422 Unprocessable Entity`.
    FingerprintMismatch { stored_fingerprint: String },
}

/// Attempt to take ownership of `(scope, key)` for a new request.
///
/// On a fresh key the function inserts a row with `response_status = NULL`
/// (the "in-flight" sentinel) and returns [`IdempotencyOutcome::Created`].
/// The caller MUST eventually call [`record_response`] — otherwise the row
/// sits "in-flight" until its `expires_at` and the GC sweep removes it.
///
/// The function performs the insert + lookup in a single short tx so two
/// concurrent callers cannot both see an empty slot.
pub async fn claim(
    pool: &PgPool,
    scope: &str,
    key: &str,
    request_fingerprint: &str,
    caller: Option<&str>,
    ttl: Duration,
) -> Result<IdempotencyOutcome, sqlx::Error> {
    let expires_at =
        Utc::now() + chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::days(1));
    let mut tx = pool.begin().await?;

    let inserted = sqlx::query(
        "INSERT INTO idempotency_keys (scope, key, request_fingerprint, caller, expires_at)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (scope, key) DO NOTHING
         RETURNING scope",
    )
    .bind(scope)
    .bind(key)
    .bind(request_fingerprint)
    .bind(caller)
    .bind(expires_at)
    .fetch_optional(&mut *tx)
    .await?;

    if inserted.is_some() {
        tx.commit().await?;
        return Ok(IdempotencyOutcome::Created);
    }

    // Conflict path: load the existing row so we can decide between
    // replay / in-flight / mismatch.
    let row = sqlx::query(
        "SELECT request_fingerprint, response_status, response_body, completed_at
         FROM idempotency_keys
         WHERE scope = $1 AND key = $2",
    )
    .bind(scope)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?;

    tx.commit().await?;

    let Some(row) = row else {
        // Row vanished between the INSERT conflict and the SELECT (likely
        // GC-deleted). Treat the slot as free; caller will retry.
        return Ok(IdempotencyOutcome::InFlight);
    };

    let stored_fingerprint: String = row.try_get("request_fingerprint")?;
    if stored_fingerprint != request_fingerprint {
        return Ok(IdempotencyOutcome::FingerprintMismatch { stored_fingerprint });
    }

    let response_status: Option<i16> = row.try_get("response_status")?;
    match response_status {
        None => Ok(IdempotencyOutcome::InFlight),
        Some(status) => {
            let body: Option<Value> = row.try_get("response_body")?;
            let completed_at: Option<DateTime<Utc>> = row.try_get("completed_at")?;
            Ok(IdempotencyOutcome::Replay {
                status: status as u16,
                body: body.unwrap_or(Value::Null),
                completed_at: completed_at.unwrap_or_else(Utc::now),
            })
        }
    }
}

/// Stamp the slot with the final response. Must be called for any slot
/// that returned [`IdempotencyOutcome::Created`] from [`claim`] so later
/// callers with the same key can replay.
pub async fn record_response(
    pool: &PgPool,
    scope: &str,
    key: &str,
    status: u16,
    body: &Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE idempotency_keys
            SET response_status = $1,
                response_body   = $2,
                completed_at    = NOW()
          WHERE scope = $3 AND key = $4",
    )
    .bind(status as i16)
    .bind(body)
    .bind(scope)
    .bind(key)
    .execute(pool)
    .await?;
    Ok(())
}

/// Best-effort cleanup of slots whose handler died before calling
/// [`record_response`]. Without this an interrupted request would
/// permanently occupy the slot until `expires_at`. Callers can invoke
/// this when they hit a code path that wants to release ownership
/// (e.g. validation failed before any business mutation ran).
pub async fn release_unclaimed(pool: &PgPool, scope: &str, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM idempotency_keys
          WHERE scope = $1
            AND key   = $2
            AND response_status IS NULL",
    )
    .bind(scope)
    .bind(key)
    .execute(pool)
    .await?;
    Ok(())
}

/// Sweep expired rows. Intended for the existing OnTick5min tick loop.
/// Returns the number of rows deleted so the caller can emit a metric.
pub async fn gc_expired(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM idempotency_keys WHERE expires_at < NOW()")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Compute a request fingerprint that's stable across byte-identical
/// requests but tolerates JSON whitespace differences. Falls back to the
/// raw byte form when JSON parsing fails (e.g. non-JSON bodies).
pub fn fingerprint_request(method: &str, path: &str, body: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let normalized_body = serde_json::from_slice::<Value>(body)
        .ok()
        .map(|value| value.to_string())
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());

    let mut hasher = Sha256::new();
    hasher.update(method.as_bytes());
    hasher.update(b"\n");
    hasher.update(path.as_bytes());
    hasher.update(b"\n");
    hasher.update(normalized_body.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_across_whitespace() {
        let a = fingerprint_request("POST", "/api/foo", b"{\"phase\":1}");
        let b = fingerprint_request("POST", "/api/foo", b"{ \"phase\": 1 }");
        assert_eq!(a, b, "JSON whitespace must not change the fingerprint");
    }

    #[test]
    fn fingerprint_differs_when_body_changes() {
        let a = fingerprint_request("POST", "/api/foo", b"{\"phase\":1}");
        let b = fingerprint_request("POST", "/api/foo", b"{\"phase\":2}");
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_differs_when_path_changes() {
        let a = fingerprint_request("POST", "/api/foo", b"{}");
        let b = fingerprint_request("POST", "/api/bar", b"{}");
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_handles_non_json_body() {
        // Non-JSON bodies must not panic; they fall back to raw bytes.
        let a = fingerprint_request("POST", "/api/foo", b"plain text");
        let b = fingerprint_request("POST", "/api/foo", b"plain text");
        assert_eq!(a, b);
    }

    mod pg_integration {
        use super::super::*;
        use crate::db::auto_queue::test_support::TestPostgresDb;
        use serde_json::json;

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn claim_then_record_then_reclaim_replays_cached_response() {
            let pg_db = TestPostgresDb::create().await;
            let pool = pg_db.connect_and_migrate().await;

            // First claim — should win the slot.
            let first = claim(
                &pool,
                "test-scope",
                "key-1",
                "fingerprint-A",
                Some("agent:operator-1"),
                DEFAULT_IDEMPOTENCY_TTL,
            )
            .await
            .expect("first claim");
            assert!(matches!(first, IdempotencyOutcome::Created));

            // Record the response so subsequent claims replay.
            record_response(&pool, "test-scope", "key-1", 200, &json!({"ok": true}))
                .await
                .expect("record_response");

            // Second claim with the SAME fingerprint → Replay path.
            let second = claim(
                &pool,
                "test-scope",
                "key-1",
                "fingerprint-A",
                Some("agent:operator-2"),
                DEFAULT_IDEMPOTENCY_TTL,
            )
            .await
            .expect("second claim");
            match second {
                IdempotencyOutcome::Replay { status, body, .. } => {
                    assert_eq!(status, 200);
                    assert_eq!(body, json!({"ok": true}));
                }
                other => panic!("expected Replay, got {other:?}"),
            }

            pool.close().await;
            pg_db.drop().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn claim_with_inflight_slot_reports_in_flight() {
            let pg_db = TestPostgresDb::create().await;
            let pool = pg_db.connect_and_migrate().await;

            // Slot 1: claim and DO NOT record_response — simulates a
            // concurrent in-flight request.
            let first = claim(
                &pool,
                "test-scope",
                "key-inflight",
                "fingerprint-A",
                None,
                DEFAULT_IDEMPOTENCY_TTL,
            )
            .await
            .expect("first claim");
            assert!(matches!(first, IdempotencyOutcome::Created));

            // Slot 2: same key, same fingerprint, no response yet → InFlight.
            let second = claim(
                &pool,
                "test-scope",
                "key-inflight",
                "fingerprint-A",
                None,
                DEFAULT_IDEMPOTENCY_TTL,
            )
            .await
            .expect("second claim");
            assert!(matches!(second, IdempotencyOutcome::InFlight));

            pool.close().await;
            pg_db.drop().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn claim_with_different_fingerprint_reports_mismatch() {
            let pg_db = TestPostgresDb::create().await;
            let pool = pg_db.connect_and_migrate().await;

            let first = claim(
                &pool,
                "test-scope",
                "key-mismatch",
                "fingerprint-A",
                None,
                DEFAULT_IDEMPOTENCY_TTL,
            )
            .await
            .expect("first claim");
            assert!(matches!(first, IdempotencyOutcome::Created));

            let second = claim(
                &pool,
                "test-scope",
                "key-mismatch",
                "fingerprint-B",
                None,
                DEFAULT_IDEMPOTENCY_TTL,
            )
            .await
            .expect("second claim");
            match second {
                IdempotencyOutcome::FingerprintMismatch { stored_fingerprint } => {
                    assert_eq!(stored_fingerprint, "fingerprint-A");
                }
                other => panic!("expected FingerprintMismatch, got {other:?}"),
            }

            pool.close().await;
            pg_db.drop().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn gc_expired_removes_only_past_due_rows() {
            let pg_db = TestPostgresDb::create().await;
            let pool = pg_db.connect_and_migrate().await;

            // Claim with a 1-hour TTL — should NOT be GC'd.
            claim(
                &pool,
                "test-scope",
                "key-keep",
                "fingerprint-fresh",
                None,
                std::time::Duration::from_secs(3600),
            )
            .await
            .expect("claim fresh");

            // Manually backdate one row to simulate expiry.
            sqlx::query(
                "INSERT INTO idempotency_keys (scope, key, request_fingerprint, expires_at)
                 VALUES ('test-scope', 'key-old', 'fingerprint-old', NOW() - INTERVAL '1 hour')",
            )
            .execute(&pool)
            .await
            .expect("insert expired row");

            let swept = gc_expired(&pool).await.expect("gc_expired");
            assert_eq!(swept, 1, "GC must delete only the past-due row");

            // Fresh row still resolves on a new claim of the same key.
            let again = claim(
                &pool,
                "test-scope",
                "key-keep",
                "fingerprint-fresh",
                None,
                DEFAULT_IDEMPOTENCY_TTL,
            )
            .await
            .expect("re-claim fresh");
            assert!(matches!(again, IdempotencyOutcome::InFlight));

            pool.close().await;
            pg_db.drop().await;
        }
    }
}
