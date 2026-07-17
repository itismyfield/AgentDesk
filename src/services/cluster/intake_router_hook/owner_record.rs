//! Dormant generation-fenced owner-CAS primitives (#4538 PR-A).
//!
//! These helpers are the durable placement-owner authority for intake
//! routing: a channel's `(provider, raw_channel_id)` identity maps to at most
//! one `active` owner row, ownership moves forward through monotonic
//! generations, and every authoritative outbox write is fenced on
//! `(owner_instance_id, generation)`. The design is `voice_turn_link`
//! (migration 0060) generalized to a `(provider, raw_channel_id)` identity;
//! see the #4538 v3.1 design (§3.2–§3.10) and migration 0094.
//!
//! PR-A ships them DORMANT: no production caller resolves ownership, routes
//! intake, or dispatches through these functions. They are validated by the
//! test module below only. Reader flip, admission wiring, and the fence
//! rollout gate land in PR-B/PR-C (#4548 handoff). The whole module is
//! `#![allow(dead_code)]` for exactly that reason — every item is reachable
//! only from `#[cfg(test)]` until the activation slice calls it.

// reason: PR-A owner-CAS primitives are intentionally uncalled by production
// until #4538 PR-C activates the authority; exercised by the PG test module
// only. Remove this allow when the activation slice wires a live caller.
#![allow(dead_code)]

use crate::db::intake_outbox::{InsertPendingPayload, IntakeOutboxRow};
use sqlx::{PgPool, Postgres, Transaction};
use std::future::Future;

/// Canonical intake ownership identity. Both fields are stored normalized
/// (§3.10): `provider = lower(btrim())`, `raw_channel_id = btrim()`. The
/// advisory-lock key is derived from the same normalized bytes so the DB
/// WHERE identity and the app-side serialization lock always agree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwnerIdentity {
    provider: String,
    raw_channel_id: String,
}

impl OwnerIdentity {
    pub(crate) fn new(provider: &str, raw_channel_id: &str) -> Self {
        Self {
            provider: provider.trim().to_lowercase(),
            raw_channel_id: raw_channel_id.trim().to_string(),
        }
    }

    pub(crate) fn provider(&self) -> &str {
        &self.provider
    }

    pub(crate) fn raw_channel_id(&self) -> &str {
        &self.raw_channel_id
    }

    /// Deterministic per-channel serialization key for `pg_advisory_xact_lock`
    /// (§3.10). `acquire`, `transfer`, operator-retry, and adoption all take
    /// this same key so a channel's ownership transitions run serially. FNV-1a
    /// over a fixed byte encoding keeps the value stable across binaries and
    /// platforms during a rolling deploy (pinned by
    /// `advisory_lock_key_is_stable`).
    pub(crate) fn advisory_key(&self) -> i64 {
        advisory_lock_key(&self.provider, &self.raw_channel_id)
    }
}

/// FNV-1a 64-bit over `domain-tag \0 provider \0 raw_channel_id`, reinterpreted
/// as `i64`. Inputs must already be normalized (`OwnerIdentity` guarantees it).
/// The domain tag prevents collisions with other advisory-lock users in the
/// same database.
fn advisory_lock_key(norm_provider: &str, norm_raw_channel_id: &str) -> i64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash: u64 = FNV_OFFSET_BASIS;
    let mut absorb = |bytes: &[u8]| {
        for &byte in bytes {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    };

    absorb(b"intake_session_owner\0");
    absorb(norm_provider.as_bytes());
    absorb(b"\0");
    absorb(norm_raw_channel_id.as_bytes());

    hash as i64
}

/// Idempotency key for an admission attempt (§3.8). Ambiguous-commit retries
/// of the same `(provider, channel, user_msg, attempt_no)` reproduce the same
/// key, so the unique index `intake_outbox_idempotency_key_uq` dedups them and
/// the admission SAVEPOINT resolves the collision as an idempotent hit. The
/// `\u{1f}` (unit separator) join keeps the components unambiguous.
pub(crate) fn idempotency_key(
    provider: &str,
    raw_channel_id: &str,
    user_message_id: &str,
    attempt_no: i32,
) -> String {
    [
        provider.trim().to_lowercase(),
        raw_channel_id.trim().to_string(),
        user_message_id.to_string(),
        attempt_no.to_string(),
    ]
    .join("\u{1f}")
}

/// Snapshot of the latest (highest-generation) owner row for an identity.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub(crate) struct OwnerRecordSnapshot {
    pub owner_instance_id: String,
    pub generation: i64,
    pub status: String,
}

/// Result of `acquire_owner_in_tx` (§3.3.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AcquireOutcome {
    /// Ownership could not be determined safely (e.g. the index-guarded
    /// impossible multi-active state). Caller rolls back and fails closed.
    Blocked { reason: String },
    /// This instance owns the channel at `generation` (freshly acquired,
    /// reused, reclaimed from a stale owner, or re-acquired after
    /// superseded/released). Caller may proceed with local admission.
    AcquiredLocal { generation: i64 },
    /// A live foreign instance owns the channel at `generation`. No owner row
    /// was written; caller forwards admission to `owner_instance_id`.
    ObservedForeign {
        owner_instance_id: String,
        generation: i64,
    },
}

/// Result of `transfer_owner_in_tx` (§3.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TransferOutcome {
    /// CAS succeeded: `expected_owner@expected_generation` was superseded and a
    /// new `active` row was inserted for `target_owner`. Caller COMMITs.
    Transferred { new_generation: i64 },
    /// The observed owner/generation/status did not match the CAS expectation,
    /// or an atomicity guard tripped. Caller MUST ROLL BACK the whole tx
    /// (a partial supersede may be pending and must be undone — §3.4 Fix1).
    CasConflict,
    /// No owner row exists, or the latest is `released`: there is nothing to
    /// transfer. Caller ROLLs BACK.
    ChannelClosed,
}

/// Result of `adopt_owner_from_session_in_tx` (§3.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AdoptOutcome {
    Adopted {
        generation: i64,
    },
    AlreadyOwned {
        owner_instance_id: String,
        generation: i64,
    },
}

/// Local vs forwarded admission (§3.5.2). Drives `admission_kind` and the
/// initial outbox status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdmissionKind {
    /// The leader is both owner and executor: claim/accept collapse into the
    /// initial `spawned` state.
    Local,
    /// A foreign owner will claim + execute: the row starts `pending`.
    Forwarded,
}

impl AdmissionKind {
    fn as_str(self) -> &'static str {
        match self {
            AdmissionKind::Local => "local",
            AdmissionKind::Forwarded => "forwarded",
        }
    }

    fn initial_status(self) -> &'static str {
        match self {
            AdmissionKind::Local => "spawned",
            AdmissionKind::Forwarded => "pending",
        }
    }
}

/// Result of `insert_admission_savepoint` (§3.3.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AdmissionOutcome {
    /// A fresh outbox row was inserted.
    Inserted { outbox_id: i64 },
    /// The idempotency key already existed (ambiguous-commit retry): the prior
    /// row is returned unchanged.
    IdempotentHit { outbox_id: i64 },
    /// An open route for the same channel + user message already exists.
    SkippedDuplicate { existing_outbox_id: i64 },
    /// A DIFFERENT open route already owns the channel; admission is deferred.
    DeferredOpenRoute {
        existing_outbox_id: i64,
        existing_target_instance_id: String,
    },
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// Latest (highest-generation, status-agnostic) owner row for an identity, or
/// `None` when the channel has never been owned.
pub(crate) async fn read_latest_owner_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
) -> Result<Option<OwnerRecordSnapshot>, sqlx::Error> {
    sqlx::query_as::<_, OwnerRecordSnapshot>(
        "SELECT owner_instance_id, generation, status
           FROM intake_session_owners
          WHERE provider = $1 AND raw_channel_id = $2
          ORDER BY generation DESC
          LIMIT 1",
    )
    .bind(id.provider())
    .bind(id.raw_channel_id())
    .fetch_optional(&mut **tx)
    .await
}

// ---------------------------------------------------------------------------
// Acquire (§3.3.1)
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct LatestOwnerAndCount {
    owner_instance_id: String,
    generation: i64,
    status: String,
    active_count: i64,
}

/// Observe-or-acquire ownership for `id`, keying on the latest generation
/// (§3.3.1). The caller MUST already hold `pg_advisory_xact_lock(id.advisory_key())`
/// on `tx` so this runs serially against transfer/reclaim for the same channel.
/// `is_instance_live` reports whether a foreign owner instance is still live
/// (the leader's node-registry liveness in production; a fixture in tests). The
/// local instance is always treated as live.
///
/// Writes an owner row only on the `AcquiredLocal` reclaim / re-acquire /
/// fresh paths; `ObservedForeign` and same-owner reuse write nothing.
pub(crate) async fn acquire_owner_in_tx<F>(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
    self_instance_id: &str,
    is_instance_live: F,
) -> Result<AcquireOutcome, sqlx::Error>
where
    F: Fn(&str) -> bool,
{
    let latest = sqlx::query_as::<_, LatestOwnerAndCount>(
        "SELECT o.owner_instance_id, o.generation, o.status,
                (SELECT COUNT(*) FROM intake_session_owners a
                  WHERE a.provider = $1 AND a.raw_channel_id = $2 AND a.status = 'active')
                    AS active_count
           FROM intake_session_owners o
          WHERE o.provider = $1 AND o.raw_channel_id = $2
          ORDER BY o.generation DESC
          LIMIT 1",
    )
    .bind(id.provider())
    .bind(id.raw_channel_id())
    .fetch_optional(&mut **tx)
    .await?;

    let Some(latest) = latest else {
        // No history: this instance takes generation 0.
        insert_owner_row(tx, id, self_instance_id, 0, false).await?;
        return Ok(AcquireOutcome::AcquiredLocal { generation: 0 });
    };

    // Index-guarded impossibility: `iso_unique_active` forbids >1 active row
    // per identity. If it is ever violated (external corruption), fail closed
    // rather than pick an arbitrary owner.
    if latest.active_count > 1 {
        return Ok(AcquireOutcome::Blocked {
            reason: format!(
                "multiple active owner rows for ({}, {})",
                id.provider(),
                id.raw_channel_id()
            ),
        });
    }

    match latest.status.as_str() {
        "active" if latest.owner_instance_id == self_instance_id => {
            // Already ours — reuse without writing.
            Ok(AcquireOutcome::AcquiredLocal {
                generation: latest.generation,
            })
        }
        "active" => {
            if is_instance_live(latest.owner_instance_id.as_str()) {
                // Live foreign owner: forward, write nothing.
                Ok(AcquireOutcome::ObservedForeign {
                    owner_instance_id: latest.owner_instance_id,
                    generation: latest.generation,
                })
            } else {
                // Stale foreign owner: reclaim by superseding then advancing.
                let superseded =
                    supersede_active_owner(tx, id, &latest.owner_instance_id, latest.generation)
                        .await?;
                if superseded != 1 {
                    // Lost the race under the advisory lock's absence — refuse.
                    return Ok(AcquireOutcome::Blocked {
                        reason: "stale-owner supersede raced".to_string(),
                    });
                }
                let new_generation = latest.generation + 1;
                insert_owner_row(tx, id, self_instance_id, new_generation, false).await?;
                Ok(AcquireOutcome::AcquiredLocal {
                    generation: new_generation,
                })
            }
        }
        "superseded" | "released" => {
            // Channel is unowned but has history: advance without resetting the
            // generation counter.
            let new_generation = latest.generation + 1;
            insert_owner_row(tx, id, self_instance_id, new_generation, false).await?;
            Ok(AcquireOutcome::AcquiredLocal {
                generation: new_generation,
            })
        }
        other => Ok(AcquireOutcome::Blocked {
            reason: format!("unexpected owner status '{other}'"),
        }),
    }
}

/// Insert a fresh `active` owner row. Relies on `iso_unique_active` +
/// `iso_unique_generation` as the schema backstop.
async fn insert_owner_row(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
    owner_instance_id: &str,
    generation: i64,
    adopted_from_session: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO intake_session_owners
             (provider, raw_channel_id, owner_instance_id, generation, status, adopted_from_session)
         VALUES ($1, $2, $3, $4, 'active', $5)",
    )
    .bind(id.provider())
    .bind(id.raw_channel_id())
    .bind(owner_instance_id)
    .bind(generation)
    .bind(adopted_from_session)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Supersede the current `active` row for `(id, owner, generation)`. Returns
/// `rows_affected` so callers can enforce the `== 1` CAS guard.
async fn supersede_active_owner(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
    owner_instance_id: &str,
    generation: i64,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE intake_session_owners
            SET status = 'superseded', updated_at = NOW()
          WHERE provider = $1 AND raw_channel_id = $2 AND status = 'active'
            AND owner_instance_id = $3 AND generation = $4",
    )
    .bind(id.provider())
    .bind(id.raw_channel_id())
    .bind(owner_instance_id)
    .bind(generation)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected())
}

// ---------------------------------------------------------------------------
// Adoption (§3.6)
// ---------------------------------------------------------------------------

/// Adopt a live `sessions`-observed instance as the generation-0 owner when no
/// owner row exists yet. The caller determines liveness via
/// `resolve_session_owner` and MUST hold the channel advisory lock. If a
/// history row already exists, adoption is refused (`AlreadyOwned`) — the owner
/// registry, once seeded, is authoritative over `sessions`.
pub(crate) async fn adopt_owner_from_session_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
    live_owner_instance_id: &str,
) -> Result<AdoptOutcome, sqlx::Error> {
    if let Some(existing) = read_latest_owner_in_tx(tx, id).await? {
        return Ok(AdoptOutcome::AlreadyOwned {
            owner_instance_id: existing.owner_instance_id,
            generation: existing.generation,
        });
    }
    insert_owner_row(tx, id, live_owner_instance_id, 0, true).await?;
    Ok(AdoptOutcome::Adopted { generation: 0 })
}

// ---------------------------------------------------------------------------
// Transfer CAS (§3.4)
// ---------------------------------------------------------------------------

/// Three-way live→live ownership transfer CAS (§3.4). The caller MUST hold
/// `pg_advisory_xact_lock(id.advisory_key())` on `tx`, and MUST COMMIT only on
/// `Transferred`; on `CasConflict`/`ChannelClosed` it MUST ROLL BACK the whole
/// transaction (a supersede may already be pending and has to be undone — Fix1).
///
/// Atomicity guards (Fix1): both the supersede AND the new-generation INSERT
/// require `rows_affected == 1`. An `ON CONFLICT DO NOTHING` that inserts 0 rows
/// (the target generation already exists — concurrent contention) returns
/// `CasConflict` so the caller's rollback restores the prior `active` owner,
/// rather than committing a channel with a superseded-but-not-replaced owner.
pub(crate) async fn transfer_owner_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
    expected_owner: &str,
    expected_generation: i64,
    target_owner: &str,
) -> Result<TransferOutcome, sqlx::Error> {
    transfer_owner_with_test_seams(
        tx,
        id,
        expected_owner,
        expected_generation,
        target_owner,
        |_| async {},
        || async {},
    )
    .await
}

async fn transfer_owner_with_test_seams<FRead, FReadFuture, FWrite, FWriteFuture>(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
    expected_owner: &str,
    expected_generation: i64,
    target_owner: &str,
    after_read: FRead,
    before_successor_insert: FWrite,
) -> Result<TransferOutcome, sqlx::Error>
where
    FRead: FnOnce(Option<&OwnerRecordSnapshot>) -> FReadFuture,
    FReadFuture: Future<Output = ()>,
    FWrite: FnOnce() -> FWriteFuture,
    FWriteFuture: Future<Output = ()>,
{
    let latest = read_latest_owner_in_tx(tx, id).await?;
    after_read(latest.as_ref()).await;

    let Some(latest) = latest else {
        return Ok(TransferOutcome::ChannelClosed);
    };
    if latest.status == "released" {
        return Ok(TransferOutcome::ChannelClosed);
    }
    if latest.status != "active"
        || latest.owner_instance_id != expected_owner
        || latest.generation != expected_generation
    {
        return Ok(TransferOutcome::CasConflict);
    }

    // Supersede the current active owner. rows_affected must be exactly 1.
    let superseded = supersede_active_owner(tx, id, expected_owner, expected_generation).await?;
    if superseded != 1 {
        return Ok(TransferOutcome::CasConflict);
    }
    before_successor_insert().await;

    // Insert the successor generation. ON CONFLICT DO NOTHING guards against a
    // racing insert of the same generation; a 0-row insert is a conflict, so
    // the caller's rollback undoes the supersede above (Fix1 atomic restore).
    //
    // Reachability: with the advisory-lock contract held, this insert targets
    // `latest.generation + 1` (the max generation was just read), which is
    // always free, so `rows_affected` is 1 here — the != 1 branch is
    // defense-in-depth for a contract violation / out-of-band writer and is
    // unreachable in-helper (its SQL semantics are pinned by
    // `transfer_insert_guard_sql_maps_conflict_to_cas_conflict`).
    let inserted = sqlx::query(
        "INSERT INTO intake_session_owners
             (provider, raw_channel_id, owner_instance_id, generation, status)
         VALUES ($1, $2, $3, $4, 'active')
         ON CONFLICT (provider, raw_channel_id, generation) DO NOTHING",
    )
    .bind(id.provider())
    .bind(id.raw_channel_id())
    .bind(target_owner)
    .bind(expected_generation + 1)
    .execute(&mut **tx)
    .await?;
    if inserted.rows_affected() != 1 {
        return Ok(TransferOutcome::CasConflict);
    }

    Ok(TransferOutcome::Transferred {
        new_generation: expected_generation + 1,
    })
}

// ---------------------------------------------------------------------------
// Admission INSERT under SAVEPOINT (§3.3.2, Fix4)
// ---------------------------------------------------------------------------

const ADMISSION_INSERT_SQL: &str = r#"
INSERT INTO intake_outbox (
    target_instance_id, forwarded_by_instance_id, required_labels,
    channel_id, user_msg_id, request_owner_id, request_owner_name,
    user_text, reply_context, has_reply_boundary, dm_hint, turn_kind,
    merge_consecutive, reply_to_user_message, defer_watcher_resume,
    wait_for_completion, preserve_on_cancel, agent_id, provider,
    owner_instance_id, owner_generation, admission_kind, idempotency_key,
    status, attempt_no, parent_outbox_id
) VALUES (
    $1, $2, $3,
    $4, $5, $6, $7,
    $8, $9, $10, $11, $12,
    $13, $14, $15,
    $16, $17, $18, $19,
    $20, $21, $22, $23,
    $24, $25, $26
)
RETURNING id
"#;

#[derive(sqlx::FromRow)]
struct OpenRouteRow {
    id: i64,
    target_instance_id: String,
    user_msg_id: String,
}

/// Insert an owner-stamped admission row wrapped in a SAVEPOINT (§3.3.2, Fix4).
///
/// The open-route unique index is channel-only while the advisory key is
/// `(provider, raw_channel_id)`, so same-channel multi-provider writers are not
/// serialized and one INSERT can hit a 23505. PostgreSQL aborts the whole
/// transaction on a constraint violation (25P02), so a naive "INSERT then
/// re-evaluate in the same tx" fails. Wrapping the INSERT in `SAVEPOINT
/// admission` lets a 23505 roll back only the savepoint, keeping `tx` alive for
/// the constraint-specific re-evaluation.
///
/// `target_instance_id` is stamped as the outbox target (the owner: self for
/// local, the foreign owner for forwarded). The caller MUST hold the channel
/// advisory lock so the acquire/observe decision and this stamp are atomic.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_admission_savepoint(
    tx: &mut Transaction<'_, Postgres>,
    payload: &InsertPendingPayload,
    kind: AdmissionKind,
    owner_instance_id: &str,
    owner_generation: i64,
    target_instance_id: &str,
    idempotency_key: &str,
    attempt_no: i32,
) -> Result<AdmissionOutcome, sqlx::Error> {
    sqlx::query("SAVEPOINT admission")
        .execute(&mut **tx)
        .await?;

    let insert_result = sqlx::query_scalar::<_, i64>(ADMISSION_INSERT_SQL)
        .bind(target_instance_id)
        .bind(&payload.forwarded_by_instance_id)
        .bind(&payload.required_labels)
        .bind(&payload.channel_id)
        .bind(&payload.user_msg_id)
        .bind(&payload.request_owner_id)
        .bind(payload.request_owner_name.as_deref())
        .bind(&payload.user_text)
        .bind(payload.reply_context.as_deref())
        .bind(payload.has_reply_boundary)
        .bind(payload.dm_hint)
        .bind(&payload.turn_kind)
        .bind(payload.merge_consecutive)
        .bind(payload.reply_to_user_message)
        .bind(payload.defer_watcher_resume)
        .bind(payload.wait_for_completion)
        .bind(payload.preserve_on_cancel)
        .bind(&payload.agent_id)
        .bind(&payload.provider)
        .bind(owner_instance_id)
        .bind(owner_generation)
        .bind(kind.as_str())
        .bind(idempotency_key)
        .bind(kind.initial_status())
        .bind(attempt_no)
        .bind(None::<i64>)
        .fetch_one(&mut **tx)
        .await;

    let insert_error = match insert_result {
        Ok(outbox_id) => {
            sqlx::query("RELEASE SAVEPOINT admission")
                .execute(&mut **tx)
                .await?;
            return Ok(AdmissionOutcome::Inserted { outbox_id });
        }
        Err(error) => error,
    };

    // Only 23505 (unique violation) is recoverable in-tx; anything else is a
    // genuine failure the caller must see.
    let Some(constraint) = unique_violation_constraint(&insert_error) else {
        return Err(insert_error);
    };

    // Undo just the failed INSERT; `tx` stays alive (Fix4 — no 25P02).
    sqlx::query("ROLLBACK TO SAVEPOINT admission")
        .execute(&mut **tx)
        .await?;

    match constraint.as_str() {
        "intake_outbox_one_open_route_per_channel" => {
            match existing_open_route_in_tx(tx, &payload.channel_id).await? {
                Some(route) if route.user_msg_id == payload.user_msg_id => {
                    Ok(AdmissionOutcome::SkippedDuplicate {
                        existing_outbox_id: route.id,
                    })
                }
                Some(route) => Ok(AdmissionOutcome::DeferredOpenRoute {
                    existing_outbox_id: route.id,
                    existing_target_instance_id: route.target_instance_id,
                }),
                // The open route vanished between INSERT and re-check (rare
                // concurrent terminalize); surface the original error.
                None => Err(insert_error),
            }
        }
        "intake_outbox_idempotency_key_uq" => {
            match lookup_outbox_id_by_idempotency_in_tx(tx, idempotency_key).await? {
                Some(existing) => Ok(AdmissionOutcome::IdempotentHit {
                    outbox_id: existing,
                }),
                None => Err(insert_error),
            }
        }
        "intake_outbox_unique_message_attempt" => {
            match lookup_outbox_id_by_attempt_in_tx(
                tx,
                &payload.channel_id,
                &payload.user_msg_id,
                attempt_no,
            )
            .await?
            {
                Some(existing) => Ok(AdmissionOutcome::SkippedDuplicate {
                    existing_outbox_id: existing,
                }),
                None => Err(insert_error),
            }
        }
        _ => Err(insert_error),
    }
}

/// The single open-route row for a channel (`pending`/`claimed`/`accepted`/
/// `spawned`), if any. The channel-only open-route unique index guarantees at
/// most one.
async fn existing_open_route_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    channel_id: &str,
) -> Result<Option<OpenRouteRow>, sqlx::Error> {
    sqlx::query_as::<_, OpenRouteRow>(
        "SELECT id, target_instance_id, user_msg_id
           FROM intake_outbox
          WHERE channel_id = $1
            AND status IN ('pending', 'claimed', 'accepted', 'spawned')
          ORDER BY created_at ASC
          LIMIT 1",
    )
    .bind(channel_id)
    .fetch_optional(&mut **tx)
    .await
}

async fn lookup_outbox_id_by_idempotency_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    idempotency_key: &str,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT id FROM intake_outbox WHERE idempotency_key = $1 LIMIT 1")
        .bind(idempotency_key)
        .fetch_optional(&mut **tx)
        .await
}

async fn lookup_outbox_id_by_attempt_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    channel_id: &str,
    user_msg_id: &str,
    attempt_no: i32,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT id FROM intake_outbox
          WHERE channel_id = $1 AND user_msg_id = $2 AND attempt_no = $3
          LIMIT 1",
    )
    .bind(channel_id)
    .bind(user_msg_id)
    .bind(attempt_no)
    .fetch_optional(&mut **tx)
    .await
}

fn unique_violation_constraint(error: &sqlx::Error) -> Option<String> {
    let db_error = error.as_database_error()?;
    if db_error.code().as_deref() != Some("23505") {
        return None;
    }
    db_error.constraint().map(str::to_string)
}

// ---------------------------------------------------------------------------
// Claim with double owner fence (§3.5.1, Fix2)
// ---------------------------------------------------------------------------

/// Candidate selection: an oldest `pending`, forwarded row targeted at + stamped
/// for `claimer_instance_id`, whose stamped `(owner, generation)` still matches
/// the channel's `active` owner. Returns the row id AND its channel_id (the
/// claim needs the channel to take that channel's advisory lock before
/// promoting — P1-B). `$1` = claimer instance, `$2` = provider.
const CLAIM_CANDIDATE_SQL: &str = r#"
SELECT io.id, io.channel_id FROM intake_outbox io
 WHERE io.target_instance_id = $1
   AND io.status = 'pending'
   AND io.provider = $2
   AND io.admission_kind = 'forwarded'
   AND io.owner_instance_id = $1
   AND EXISTS (
        SELECT 1 FROM intake_session_owners o
         WHERE o.provider = io.provider
           AND o.raw_channel_id = io.channel_id
           AND o.status = 'active'
           AND o.owner_instance_id = $1
           AND o.generation = io.owner_generation)
 ORDER BY io.created_at ASC
 LIMIT 1
 FOR UPDATE OF io SKIP LOCKED
"#;

/// Row shape returned by `CLAIM_CANDIDATE_SQL`.
#[derive(sqlx::FromRow)]
struct ClaimCandidate {
    id: i64,
    channel_id: String,
}

/// Promotion: re-checks the SAME owner fence in the UPDATE WHERE. This is the
/// second (defense-in-depth) fence; the primary linearization is the channel
/// advisory lock the claim takes before this UPDATE (P1-B), which mutually
/// excludes an in-flight `transfer`. `$1` = row id, `$2` = claimer instance,
/// `$3` = claim token. A 0-row UPDATE means ownership moved: no claim.
const CLAIM_PROMOTE_SQL: &str = r#"
UPDATE intake_outbox AS io
   SET status = 'claimed', claim_owner = $3, claimed_at = NOW()
 WHERE io.id = $1
   AND io.status = 'pending'
   AND io.target_instance_id = $2
   AND io.owner_instance_id = $2
   AND EXISTS (
        SELECT 1 FROM intake_session_owners o
         WHERE o.provider = io.provider
           AND o.raw_channel_id = io.channel_id
           AND o.status = 'active'
           AND o.owner_instance_id = $2
           AND o.generation = io.owner_generation)
 RETURNING *
"#;

/// Fenced worker-side claim (§3.5.1, P1-B linearization). Node identity (who) is
/// `claimer_instance_id` (target + stamped owner + current active owner, triple
/// checked plus the generation fence); the lease token (which restart) is
/// `claim_token`, stored in `claim_owner`.
///
/// **Linearization with transfer (P1-B):** the owner-fence EXISTS alone is NOT
/// sufficient. Under READ COMMITTED the promote UPDATE evaluates its owner
/// EXISTS against the statement-start snapshot, so a `transfer` that commits
/// while the UPDATE runs (the claim holds only the outbox row lock, not the
/// owner row) would be invisible — the UPDATE could stamp a claim on a
/// now-superseded owner and let a stale node double-execute. To prevent that,
/// once a candidate is found the claim takes the SAME channel advisory lock
/// (`OwnerIdentity::advisory_key`) that `transfer` holds for its whole tx, so
/// claim and transfer for a channel are mutually exclusive. After acquiring the
/// lock the owner state is re-read fresh (the promote fence), so
/// "owner the claim saw == owner at commit" holds.
///
/// Returns `Ok(None)` when no eligible row exists, or when ownership moved
/// before/while the lock was acquired (promote fence 0 rows) — the row stays
/// `pending` for the stale-claim sweep / current-owner re-stamp to reconcile.
pub(crate) async fn claim_pending_for_target_fenced(
    pool: &PgPool,
    claimer_instance_id: &str,
    claim_token: &str,
    provider: &str,
) -> Result<Option<IntakeOutboxRow>, sqlx::Error> {
    claim_pending_after_candidate_fenced(
        pool,
        claimer_instance_id,
        claim_token,
        provider,
        |_| async {},
    )
    .await
}

async fn claim_pending_after_candidate_fenced<F, Fut>(
    pool: &PgPool,
    claimer_instance_id: &str,
    claim_token: &str,
    provider: &str,
    after_candidate: F,
) -> Result<Option<IntakeOutboxRow>, sqlx::Error>
where
    F: FnOnce(&ClaimCandidate) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut tx = pool.begin().await?;

    let candidate: Option<ClaimCandidate> = sqlx::query_as(CLAIM_CANDIDATE_SQL)
        .bind(claimer_instance_id)
        .bind(provider)
        .fetch_optional(&mut *tx)
        .await?;

    let Some(candidate) = candidate else {
        tx.commit().await?;
        return Ok(None);
    };
    after_candidate(&candidate).await;

    // Serialize with transfer for this channel BEFORE the fenced promote. Blocks
    // until any in-flight transfer commits and releases its advisory lock; held
    // until this tx commits so no transfer can interleave the promote (P1-B).
    let advisory_key = OwnerIdentity::new(provider, &candidate.channel_id).advisory_key();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(advisory_key)
        .execute(&mut *tx)
        .await?;

    // fetch_optional yields None when the fenced UPDATE affects 0 rows: the
    // owner changed (a transfer we just waited on), so the claim is void.
    let row: Option<IntakeOutboxRow> = sqlx::query_as(CLAIM_PROMOTE_SQL)
        .bind(candidate.id)
        .bind(claimer_instance_id)
        .bind(claim_token)
        .fetch_optional(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(row)
}

// ---------------------------------------------------------------------------
// Stale-claim sweep with owner fence (§3.5.3, Fix3)
// ---------------------------------------------------------------------------

/// Re-arm stale `claimed` rows to `pending`, but ONLY when the row's stamped
/// `(owner, generation)` is still the channel's `active` owner (Fix3). The
/// correlated columns are qualified with the outer `io` alias and normalized
/// (`lower(btrim(io.provider))`, `btrim(io.channel_id)`); leaving them
/// unqualified would bind to the inner `o` table and degrade the fence into a
/// tautology, letting another provider's active generation on the same raw
/// channel wrongly resurrect a stale/orphaned claim. Fence-failing rows
/// (superseded / transferred / legacy NULL) are intentionally left `claimed`
/// for the activation-phase park/terminalize path. `$1` = stale-after seconds.
const SWEEP_STALE_CLAIMS_SQL: &str = r#"
UPDATE intake_outbox AS io
   SET status = 'pending', claim_owner = NULL, claimed_at = NULL
 WHERE io.status = 'claimed'
   AND io.claimed_at < NOW() - ($1::BIGINT * INTERVAL '1 second')
   AND EXISTS (
        SELECT 1 FROM intake_session_owners o
         WHERE o.provider       = lower(btrim(io.provider))
           AND o.raw_channel_id = btrim(io.channel_id)
           AND o.status = 'active'
           AND o.owner_instance_id = io.owner_instance_id
           AND o.generation        = io.owner_generation)
"#;

/// Re-arm fence-valid stale claims (§3.5.3). Returns the number of rows reset.
/// Fence-failing stale rows are NOT re-armed here.
pub(crate) async fn sweep_stale_pre_accept_claims_fenced(
    pool: &PgPool,
    stale_after_secs: i64,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(SWEEP_STALE_CLAIMS_SQL)
        .bind(stale_after_secs.max(1))
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use serde_json::json;

    // -- fixtures -----------------------------------------------------------

    fn identity(provider: &str, chan: &str) -> OwnerIdentity {
        OwnerIdentity::new(provider, chan)
    }

    async fn insert_owner(
        pool: &PgPool,
        provider: &str,
        chan: &str,
        owner: &str,
        generation: i64,
        status: &str,
    ) {
        sqlx::query(
            "INSERT INTO intake_session_owners
                 (provider, raw_channel_id, owner_instance_id, generation, status)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(provider)
        .bind(chan)
        .bind(owner)
        .bind(generation)
        .bind(status)
        .execute(pool)
        .await
        .expect("seed owner row");
    }

    async fn active_owner(pool: &PgPool, provider: &str, chan: &str) -> Option<(String, i64)> {
        sqlx::query_as::<_, (String, i64)>(
            "SELECT owner_instance_id, generation FROM intake_session_owners
              WHERE provider = $1 AND raw_channel_id = $2 AND status = 'active'",
        )
        .bind(provider)
        .bind(chan)
        .fetch_optional(pool)
        .await
        .expect("read active owner")
    }

    async fn count_active(pool: &PgPool, provider: &str, chan: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT FROM intake_session_owners
              WHERE provider = $1 AND raw_channel_id = $2 AND status = 'active'",
        )
        .bind(provider)
        .bind(chan)
        .fetch_one(pool)
        .await
        .expect("count active")
    }

    /// Seed an intake_outbox row with explicit owner stamp + claim state.
    /// `claimed_secs_ago = None` leaves `claimed_at` NULL.
    #[allow(clippy::too_many_arguments)]
    async fn insert_outbox(
        pool: &PgPool,
        channel: &str,
        user_msg: &str,
        provider: &str,
        status: &str,
        admission_kind: &str,
        owner: Option<&str>,
        owner_generation: Option<i64>,
        claim_owner: Option<&str>,
        claimed_secs_ago: Option<i64>,
    ) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO intake_outbox (
                 target_instance_id, forwarded_by_instance_id, required_labels,
                 channel_id, user_msg_id, request_owner_id, user_text, turn_kind,
                 agent_id, provider, status, attempt_no,
                 admission_kind, owner_instance_id, owner_generation,
                 claim_owner, claimed_at
             ) VALUES (
                 COALESCE($6, 'worker-1'), 'leader-1', '[]'::JSONB,
                 $1, $2, 'user-1', 'hi', 'standard',
                 'agent-x', $3, $4, 1,
                 $5, $6, $7,
                 $8, CASE WHEN $9::BIGINT IS NULL THEN NULL
                          ELSE NOW() - ($9::BIGINT * INTERVAL '1 second') END
             ) RETURNING id",
        )
        .bind(channel)
        .bind(user_msg)
        .bind(provider)
        .bind(status)
        .bind(admission_kind)
        .bind(owner)
        .bind(owner_generation)
        .bind(claim_owner)
        .bind(claimed_secs_ago)
        .fetch_one(pool)
        .await
        .expect("seed outbox row")
    }

    async fn outbox_status(pool: &PgPool, id: i64) -> String {
        sqlx::query_scalar::<_, String>("SELECT status FROM intake_outbox WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("read outbox status")
    }

    fn admission_payload(channel: &str, msg: &str, provider: &str) -> InsertPendingPayload {
        InsertPendingPayload {
            target_instance_id: "worker-1".to_string(),
            forwarded_by_instance_id: "leader-1".to_string(),
            provider: provider.to_string(),
            required_labels: json!([]),
            channel_id: channel.to_string(),
            user_msg_id: msg.to_string(),
            request_owner_id: "user-1".to_string(),
            request_owner_name: Some("Tester".to_string()),
            user_text: "hello".to_string(),
            reply_context: None,
            has_reply_boundary: false,
            dm_hint: Some(false),
            turn_kind: "standard".to_string(),
            merge_consecutive: false,
            reply_to_user_message: false,
            defer_watcher_resume: false,
            wait_for_completion: false,
            preserve_on_cancel: false,
            agent_id: "agent-x".to_string(),
        }
    }

    // -- advisory key + idempotency key (deterministic, no DB) --------------

    /// Pinned FNV-1a advisory key. Stability is load-bearing across a rolling
    /// deploy: two binaries must derive the SAME lock for a channel or
    /// acquire/transfer take different locks and lose serialization. Reverting
    /// the hash (or the normalization/domain-tag byte layout) changes these
    /// pinned values → this test fails.
    #[test]
    fn advisory_lock_key_is_stable() {
        assert_eq!(
            identity("claude", "123456789").advisory_key(),
            -2_180_495_178_205_472_121
        );
        // Normalization: raw casing/whitespace must fold to the same key.
        assert_eq!(
            identity("  CLAUDE ", " 123456789 ").advisory_key(),
            identity("claude", "123456789").advisory_key()
        );
        // Distinct channel and distinct provider must not collide.
        assert_ne!(
            identity("claude", "123456789").advisory_key(),
            identity("claude", "123456780").advisory_key()
        );
        assert_ne!(
            identity("claude", "123456789").advisory_key(),
            identity("codex", "123456789").advisory_key()
        );
    }

    /// Idempotency key composition (§3.8). Normalized provider + trimmed
    /// channel + user message + attempt, unit-separator joined. Reverting the
    /// separator, order, or normalization changes the literal → this fails.
    #[test]
    fn idempotency_key_is_composed_and_normalized() {
        assert_eq!(
            idempotency_key("  Claude ", " 42 ", "msg-9", 1),
            "claude\u{1f}42\u{1f}msg-9\u{1f}1"
        );
        // Ambiguous-commit retry of the same tuple reproduces the same key.
        assert_eq!(
            idempotency_key("claude", "42", "msg-9", 1),
            idempotency_key("CLAUDE", "42", "msg-9", 1)
        );
        // A different attempt is a different key.
        assert_ne!(
            idempotency_key("claude", "42", "msg-9", 1),
            idempotency_key("claude", "42", "msg-9", 2)
        );
    }

    // -- acquire ------------------------------------------------------------

    /// Acquire path coverage (§3.3.1): fresh → gen0; reuse (no write); live
    /// foreign → observe; stale foreign → reclaim gen+1; superseded/released →
    /// re-acquire without resetting the generation counter.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn acquire_covers_each_generation_rule() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;
        let all_live = |_: &str| true;
        let none_live = |_: &str| false;

        // Fresh channel → gen0 local.
        let fresh = identity("claude", "fresh");
        {
            let mut tx = pool.begin().await.unwrap();
            let out = acquire_owner_in_tx(&mut tx, &fresh, "node-A", all_live)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            assert_eq!(out, AcquireOutcome::AcquiredLocal { generation: 0 });
        }
        let fresh_owner = active_owner(&pool, "claude", "fresh").await;
        assert_eq!(fresh_owner, Some(("node-A".into(), 0)));

        // Same owner reuse → no new row.
        {
            let mut tx = pool.begin().await.unwrap();
            let out = acquire_owner_in_tx(&mut tx, &fresh, "node-A", all_live)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            assert_eq!(out, AcquireOutcome::AcquiredLocal { generation: 0 });
        }
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM intake_session_owners
              WHERE provider='claude' AND raw_channel_id='fresh'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(total, 1, "reuse must not write a new owner row");

        // Live foreign owner → observe (no write).
        insert_owner(&pool, "claude", "foreign", "node-B", 0, "active").await;
        let foreign = identity("claude", "foreign");
        {
            let mut tx = pool.begin().await.unwrap();
            let out = acquire_owner_in_tx(&mut tx, &foreign, "node-A", all_live)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            assert_eq!(
                out,
                AcquireOutcome::ObservedForeign {
                    owner_instance_id: "node-B".into(),
                    generation: 0,
                }
            );
        }
        let foreign_owner = active_owner(&pool, "claude", "foreign").await;
        assert_eq!(foreign_owner, Some(("node-B".into(), 0)));

        // Stale foreign owner → reclaim to gen1 local.
        insert_owner(&pool, "claude", "stale", "node-B", 0, "active").await;
        let stale = identity("claude", "stale");
        {
            let mut tx = pool.begin().await.unwrap();
            let out = acquire_owner_in_tx(&mut tx, &stale, "node-A", none_live)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            assert_eq!(out, AcquireOutcome::AcquiredLocal { generation: 1 });
        }
        let stale_owner = active_owner(&pool, "claude", "stale").await;
        assert_eq!(stale_owner, Some(("node-A".into(), 1)));
        assert_eq!(count_active(&pool, "claude", "stale").await, 1);

        // Superseded latest → re-acquire without resetting generation.
        insert_owner(&pool, "claude", "closed", "node-B", 7, "superseded").await;
        let closed = identity("claude", "closed");
        {
            let mut tx = pool.begin().await.unwrap();
            let out = acquire_owner_in_tx(&mut tx, &closed, "node-A", all_live)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            assert_eq!(
                out,
                AcquireOutcome::AcquiredLocal { generation: 8 },
                "generation must advance from the watermark, never reset"
            );
        }

        pool.close().await;
        pg.drop().await;
    }

    /// CAS atomicity: two instances race to acquire the SAME fresh channel,
    /// each under `pg_advisory_xact_lock`. Exactly one becomes owner
    /// (`AcquiredLocal{0}`), the other observes it (`ObservedForeign`), and
    /// exactly one active gen0 row exists. Reverting the acquire's SELECT +
    /// `iso_unique_active` reliance (or dropping the advisory lock) lets both
    /// insert gen0 → the second hits `iso_unique_active` 23505 and the join
    /// panics, or two active rows survive → the final count assert fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_acquire_yields_single_owner() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;
        let id = identity("claude", "race");
        let key = id.advisory_key();

        async fn one(pool: &PgPool, id: &OwnerIdentity, key: i64, me: &str) -> AcquireOutcome {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(key)
                .execute(&mut *tx)
                .await
                .unwrap();
            let live = |_: &str| true; // both instances considered live
            let out = acquire_owner_in_tx(&mut tx, id, me, live).await.unwrap();
            tx.commit().await.unwrap();
            out
        }

        let (a, b) = tokio::join!(
            one(&pool, &id, key, "node-A"),
            one(&pool, &id, key, "node-B"),
        );

        let mut acquired = 0;
        let mut observed = 0;
        for outcome in [&a, &b] {
            match outcome {
                AcquireOutcome::AcquiredLocal { generation: 0 } => acquired += 1,
                AcquireOutcome::ObservedForeign { generation: 0, .. } => observed += 1,
                other => panic!("unexpected acquire outcome: {other:?}"),
            }
        }
        assert_eq!(acquired, 1, "exactly one instance acquires ownership");
        assert_eq!(observed, 1, "the loser observes the live winner");
        assert_eq!(count_active(&pool, "claude", "race").await, 1);

        pool.close().await;
        pg.drop().await;
    }

    /// The `iso_unique_active` partial unique index is the durable backstop the
    /// concurrent-acquire path relies on: it forbids two `active` rows for one
    /// identity even at DIFFERENT generations, with NO advisory lock involved.
    /// Direct INSERTs prove it independently of the app-side serialization
    /// (removing the index from migration 0094 makes the second INSERT succeed,
    /// so `expect_err` panics and this test fails).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn iso_unique_active_forbids_two_active_owners() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;

        insert_owner(&pool, "claude", "dup", "node-A", 0, "active").await;

        let result = sqlx::query(
            "INSERT INTO intake_session_owners
                 (provider, raw_channel_id, owner_instance_id, generation, status)
             VALUES ('claude', 'dup', 'node-B', 1, 'active')",
        )
        .execute(&pool)
        .await;
        let error = result.expect_err("second active owner must violate iso_unique_active");
        assert!(
            error.to_string().contains("iso_unique_active"),
            "expected iso_unique_active violation, got: {error}"
        );

        pool.close().await;
        pg.drop().await;
    }

    // -- transfer 3-way -----------------------------------------------------

    /// Transfer success: active `A@0` → supersede + insert `B@1` active.
    /// Removing the supersede or the successor INSERT leaves the channel
    /// without a single fresh active owner → the post-commit asserts fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transfer_succeeds_and_advances_generation() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;
        let id = identity("claude", "xfer");
        insert_owner(&pool, "claude", "xfer", "node-A", 0, "active").await;

        let mut tx = pool.begin().await.unwrap();
        let out = transfer_owner_in_tx(&mut tx, &id, "node-A", 0, "node-B")
            .await
            .unwrap();
        assert_eq!(out, TransferOutcome::Transferred { new_generation: 1 });
        tx.commit().await.unwrap();

        assert_eq!(
            active_owner(&pool, "claude", "xfer").await,
            Some(("node-B".into(), 1))
        );
        assert_eq!(count_active(&pool, "claude", "xfer").await, 1);

        pool.close().await;
        pg.drop().await;
    }

    /// Transfer ChannelClosed: no owner row, and latest = `released`. Reverting
    /// either guard would misroute these into supersede/INSERT.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transfer_channel_closed_when_absent_or_released() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;

        // No history.
        let empty = identity("claude", "empty");
        {
            let mut tx = pool.begin().await.unwrap();
            let out = transfer_owner_in_tx(&mut tx, &empty, "node-A", 0, "node-B")
                .await
                .unwrap();
            tx.rollback().await.unwrap();
            assert_eq!(out, TransferOutcome::ChannelClosed);
        }

        // Latest released.
        insert_owner(&pool, "claude", "rel", "node-A", 3, "released").await;
        let rel = identity("claude", "rel");
        {
            let mut tx = pool.begin().await.unwrap();
            let out = transfer_owner_in_tx(&mut tx, &rel, "node-A", 3, "node-B")
                .await
                .unwrap();
            tx.rollback().await.unwrap();
            assert_eq!(out, TransferOutcome::ChannelClosed);
        }

        pool.close().await;
        pg.drop().await;
    }

    /// Transfer CasConflict: owner mismatch, generation mismatch, and a
    /// non-active latest each fail the CAS and mutate nothing. Reverting the
    /// owner/generation/status guard would let a stale expectation supersede
    /// the live owner → active owner assert fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transfer_cas_conflict_on_mismatch() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;
        insert_owner(&pool, "claude", "cas", "node-A", 5, "active").await;
        let cas = identity("claude", "cas");

        // Wrong expected owner.
        {
            let mut tx = pool.begin().await.unwrap();
            let out = transfer_owner_in_tx(&mut tx, &cas, "node-Z", 5, "node-B")
                .await
                .unwrap();
            tx.rollback().await.unwrap();
            assert_eq!(out, TransferOutcome::CasConflict);
        }
        // Wrong expected generation.
        {
            let mut tx = pool.begin().await.unwrap();
            let out = transfer_owner_in_tx(&mut tx, &cas, "node-A", 4, "node-B")
                .await
                .unwrap();
            tx.rollback().await.unwrap();
            assert_eq!(out, TransferOutcome::CasConflict);
        }
        // Latest non-active (superseded shadow above a lower active is
        // impossible under the index; here the latest itself is superseded).
        insert_owner(&pool, "claude", "cas2", "node-A", 1, "superseded").await;
        let cas2 = identity("claude", "cas2");
        {
            let mut tx = pool.begin().await.unwrap();
            let out = transfer_owner_in_tx(&mut tx, &cas2, "node-A", 1, "node-B")
                .await
                .unwrap();
            tx.rollback().await.unwrap();
            assert_eq!(out, TransferOutcome::CasConflict);
        }

        // The live owner is untouched.
        let cas_owner = active_owner(&pool, "claude", "cas").await;
        assert_eq!(cas_owner, Some(("node-A".into(), 5)));

        pool.close().await;
        pg.drop().await;
    }

    /// Exercise the production successor-INSERT guard against an occupied next
    /// generation. The transfer retains A@5, an out-of-band writer adds history
    /// generation 6, and the exact helper INSERT therefore affects 0 rows.
    async fn assert_transfer_insert_guard(pool: &PgPool, channel: &str) {
        insert_owner(pool, "claude", channel, "node-A", 5, "active").await;
        let (read_tx, read_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let transfer_channel = channel.to_owned();

        let transfer_pool = pool.clone();
        let transfer = tokio::spawn(async move {
            let id = identity("claude", &transfer_channel);
            let mut tx = transfer_pool.begin().await.unwrap();
            let out = transfer_owner_with_test_seams(
                &mut tx,
                &id,
                "node-A",
                5,
                "node-B",
                |latest| {
                    assert_eq!(
                        latest,
                        Some(&OwnerRecordSnapshot {
                            owner_instance_id: "node-A".into(),
                            generation: 5,
                            status: "active".into(),
                        })
                    );
                    async move {
                        read_tx.send(()).expect("test waits for read signal");
                        resume_rx.await.expect("test resumes transfer");
                    }
                },
                || async {},
            )
            .await
            .unwrap();
            match &out {
                TransferOutcome::Transferred { .. } => tx.commit().await.unwrap(),
                _ => tx.rollback().await.unwrap(),
            }
            out
        });

        read_rx
            .await
            .expect("transfer must retain the A@5 snapshot");
        insert_owner(pool, "claude", channel, "node-X", 6, "superseded").await;
        resume_tx.send(()).expect("transfer still waits after read");

        let out = transfer.await.unwrap();
        assert_eq!(
            out,
            TransferOutcome::CasConflict,
            "0-row successor insert must be a CAS conflict"
        );
        assert_eq!(
            active_owner(pool, "claude", channel).await,
            Some(("node-A".into(), 5)),
            "rollback must restore the pre-transfer active owner (no active-less channel)"
        );
        assert_eq!(count_active(pool, "claude", channel).await, 1);
    }

    /// Fix1 — removing only the production INSERT rows-affected guard reports
    /// `Transferred` for the occupied generation-6 attempt and commits A@5 as
    /// superseded, so the helper's outcome and active-owner assertions fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn transfer_insert_guard_sql_maps_conflict_to_cas_conflict() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;
        assert_transfer_insert_guard(&pool, "fix1").await;
        pool.close().await;
        pg.drop().await;
    }

    /// Fix1 — the supersede rows-affected guard under a retained stale snapshot.
    /// The helper reads A@0 and pauses; an external transition supersedes A@0 and
    /// commits without occupying generation 1. The helper's supersede then affects
    /// 0 rows. Removing only the supersede guard lets its successor INSERT succeed
    /// and returns `Transferred`, so this outcome assert fails. The INSERT guard
    /// cannot mask this mutation because generation 1 is deliberately free.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn transfer_supersede_guard_rejects_stale_snapshot_before_insert() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;
        insert_owner(&pool, "claude", "fix1-sup", "node-A", 0, "active").await;
        let (read_tx, read_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();

        let transfer_pool = pool.clone();
        let transfer = tokio::spawn(async move {
            let id = identity("claude", "fix1-sup");
            let mut tx = transfer_pool.begin().await.unwrap();
            let out = transfer_owner_with_test_seams(
                &mut tx,
                &id,
                "node-A",
                0,
                "node-B",
                |latest| {
                    assert_eq!(latest.map(|owner| owner.generation), Some(0));
                    async move {
                        read_tx.send(()).expect("test waits for read signal");
                        resume_rx.await.expect("test resumes transfer");
                    }
                },
                || async {},
            )
            .await
            .unwrap();
            match &out {
                TransferOutcome::Transferred { .. } => tx.commit().await.unwrap(),
                _ => tx.rollback().await.unwrap(),
            }
            out
        });

        read_rx
            .await
            .expect("transfer must retain the A@0 snapshot");
        let id = identity("claude", "fix1-sup");
        let mut external = pool.begin().await.unwrap();
        let superseded = supersede_active_owner(&mut external, &id, "node-A", 0)
            .await
            .unwrap();
        assert_eq!(superseded, 1);
        external.commit().await.unwrap();
        resume_tx.send(()).expect("transfer still waits after read");

        let out = transfer.await.unwrap();
        assert_eq!(
            out,
            TransferOutcome::CasConflict,
            "0-row supersede must stop before the free successor insert"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*)::BIGINT FROM intake_session_owners
                  WHERE provider = 'claude' AND raw_channel_id = 'fix1-sup'
                    AND generation = 1",
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            0,
            "CAS loser must not write generation 1"
        );

        pool.close().await;
        pg.drop().await;
    }

    /// Real-helper atomicity: two transfer calls race for the same channel
    /// WITHOUT the advisory lock (deliberately exercising guards the lock normally
    /// makes redundant). Both calls are stopped after reading the same `A@0`
    /// snapshot. Writer B then supersedes A@0 but pauses before INSERT; writer C
    /// starts its supersede and is proven blocked on B's uncommitted row update.
    /// B commits B@1, after which C's supersede affects 0 rows and returns
    /// `CasConflict`. This fixes the exact interleaving rather than trusting the
    /// Tokio scheduler.
    ///
    /// Mutation proof: removing only the supersede rows-affected guard lets C
    /// reach a test seam that removes committed B@1, after which C writes the now
    /// free generation 1 and reports a second `Transferred`. The focused helper
    /// check below exercises the INSERT guard independently on a second channel;
    /// removing either guard alone fails this test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn transfer_concurrent_race_preserves_single_active() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;
        insert_owner(&pool, "claude", "rc", "node-A", 0, "active").await;
        let both_read = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let (winner_pid_tx, winner_pid_rx) = tokio::sync::oneshot::channel();
        let (loser_pid_tx, loser_pid_rx) = tokio::sync::oneshot::channel();
        let (winner_superseded_tx, winner_superseded_rx) = tokio::sync::oneshot::channel();
        let (allow_winner_insert_tx, allow_winner_insert_rx) = tokio::sync::oneshot::channel();

        let winner_pool = pool.clone();
        let winner_read = both_read.clone();
        let mut winner = tokio::spawn(async move {
            let id = identity("claude", "rc");
            let mut tx = winner_pool.begin().await.unwrap();
            let backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            winner_pid_tx
                .send(backend_pid)
                .expect("test waits for winner backend PID");
            let out = transfer_owner_with_test_seams(
                &mut tx,
                &id,
                "node-A",
                0,
                "node-B",
                |latest| {
                    assert_eq!(
                        latest,
                        Some(&OwnerRecordSnapshot {
                            owner_instance_id: "node-A".into(),
                            generation: 0,
                            status: "active".into(),
                        })
                    );
                    async move {
                        winner_read.wait().await;
                    }
                },
                || async move {
                    winner_superseded_tx
                        .send(())
                        .expect("test waits for winner supersede signal");
                    allow_winner_insert_rx
                        .await
                        .expect("test releases winner insert");
                },
            )
            .await
            .unwrap();
            match &out {
                TransferOutcome::Transferred { .. } => tx.commit().await.unwrap(),
                _ => tx.rollback().await.unwrap(),
            }
            out
        });

        let loser_pool = pool.clone();
        let loser_mutation_pool = pool.clone();
        let loser_read = both_read;
        let mut loser = tokio::spawn(async move {
            let id = identity("claude", "rc");
            let mut tx = loser_pool.begin().await.unwrap();
            let backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            loser_pid_tx
                .send(backend_pid)
                .expect("test waits for loser backend PID");
            let out = transfer_owner_with_test_seams(
                &mut tx,
                &id,
                "node-A",
                0,
                "node-C",
                |latest| {
                    assert_eq!(
                        latest,
                        Some(&OwnerRecordSnapshot {
                            owner_instance_id: "node-A".into(),
                            generation: 0,
                            status: "active".into(),
                        })
                    );
                    async move {
                        loser_read.wait().await;
                        // B is deterministically first to the UPDATE: C cannot
                        // issue its supersede until B reports that it completed.
                        winner_superseded_rx
                            .await
                            .expect("winner must supersede before loser writes");
                    }
                },
                || async move {
                    // Normally unreachable: C's supersede affected 0 rows. If the
                    // guard is removed, free B@1 out-of-band so C's INSERT succeeds
                    // and the final outcome asserts observe two transfers.
                    sqlx::query(
                        "DELETE FROM intake_session_owners
                          WHERE provider = 'claude' AND raw_channel_id = 'rc'
                            AND owner_instance_id = 'node-B' AND generation = 1",
                    )
                    .execute(&loser_mutation_pool)
                    .await
                    .unwrap();
                },
            )
            .await
            .unwrap();
            match &out {
                TransferOutcome::Transferred { .. } => tx.commit().await.unwrap(),
                _ => tx.rollback().await.unwrap(),
            }
            out
        });

        // C has passed the common-read barrier and was released to supersede, but
        // B still owns the row lock. Prove C is waiting specifically on B's
        // transaction ID before allowing B to insert and commit.
        let winner_pid = winner_pid_rx.await.expect("winner backend PID");
        let loser_pid = loser_pid_rx.await.expect("loser backend PID");
        let wait_for_loser_row_lock = async {
            loop {
                let blocked_by_winner: bool =
                    sqlx::query_scalar("SELECT $1::INT = ANY(pg_blocking_pids($2::INT))")
                        .bind(winner_pid)
                        .bind(loser_pid)
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                if blocked_by_winner {
                    break;
                }
                tokio::task::yield_now().await;
            }
        };
        tokio::select! {
            wait = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                wait_for_loser_row_lock,
            ) => wait.expect("loser must block on winner's supersede row lock"),
            completed = &mut loser => panic!("loser completed before winner commit: {completed:?}"),
        }
        assert!(
            !loser.is_finished(),
            "loser supersede must wait on winner row lock"
        );
        allow_winner_insert_tx
            .send(())
            .expect("winner still waits before successor insert");

        let a = winner.await.unwrap();
        let b = loser.await.unwrap();
        let mut transferred = 0;
        let mut conflicts = 0;
        for out in [&a, &b] {
            match out {
                TransferOutcome::Transferred { new_generation: 1 } => transferred += 1,
                TransferOutcome::CasConflict => conflicts += 1,
                other => panic!("unexpected transfer outcome: {other:?}"),
            }
        }
        assert_eq!(transferred, 1, "exactly one transfer wins");
        assert_eq!(conflicts, 1, "the forced loser is a CAS conflict");
        assert_eq!(
            active_owner(&pool, "claude", "rc").await,
            Some(("node-B".into(), 1)),
            "the committed winner must be the sole active generation"
        );
        assert_eq!(count_active(&pool, "claude", "rc").await, 1);

        // Removing only the INSERT guard does not bypass C's earlier supersede
        // guard, so exercise the production INSERT branch independently inside
        // this same mutation test. Either guard mutation now fails this test.
        assert_transfer_insert_guard(&pool, "rc-insert-guard").await;

        pool.close().await;
        pg.drop().await;
    }

    // -- admission SAVEPOINT (§3.3.2) ---------------------------------------

    /// Fix4 — an open-route 23505 rolls back only the SAVEPOINT, leaving `tx`
    /// alive for the in-tx re-evaluation (no 25P02). A different user message on
    /// the same channel defers; the same message dedups. Reverting the SAVEPOINT
    /// wrap makes the second query fail with 25P02 (aborted transaction) and the
    /// `.await.unwrap()` on the follow-up query panics.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admission_open_route_conflict_reevaluates_in_tx() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;
        // Existing open route: msg-A pending on chanO, target node-A.
        let existing = insert_outbox(
            &pool,
            "chanO",
            "msg-A",
            "claude",
            "pending",
            "forwarded",
            Some("node-A"),
            Some(0),
            None,
            None,
        )
        .await;

        // DIFFERENT message on the same channel → DeferredOpenRoute.
        {
            let mut tx = pool.begin().await.unwrap();
            let payload = admission_payload("chanO", "msg-B", "claude");
            let idem = idempotency_key("claude", "chanO", "msg-B", 1);
            let out = insert_admission_savepoint(
                &mut tx,
                &payload,
                AdmissionKind::Forwarded,
                "node-A",
                0,
                "node-A",
                &idem,
                1,
            )
            .await
            .unwrap();
            assert_eq!(
                out,
                AdmissionOutcome::DeferredOpenRoute {
                    existing_outbox_id: existing,
                    existing_target_instance_id: "node-A".into(),
                }
            );
            // tx must still be usable after the savepoint rollback (Fix4).
            let alive: i64 = sqlx::query_scalar("SELECT 1::BIGINT")
                .fetch_one(&mut *tx)
                .await
                .expect("tx alive after ROLLBACK TO SAVEPOINT");
            assert_eq!(alive, 1);
            tx.commit().await.unwrap();
        }

        // SAME message on the same channel → SkippedDuplicate.
        {
            let mut tx = pool.begin().await.unwrap();
            let payload = admission_payload("chanO", "msg-A", "claude");
            let idem = idempotency_key("claude", "chanO", "msg-A", 1);
            let out = insert_admission_savepoint(
                &mut tx,
                &payload,
                AdmissionKind::Forwarded,
                "node-A",
                0,
                "node-A",
                &idem,
                1,
            )
            .await
            .unwrap();
            assert_eq!(
                out,
                AdmissionOutcome::SkippedDuplicate {
                    existing_outbox_id: existing
                }
            );
            tx.commit().await.unwrap();
        }

        pool.close().await;
        pg.drop().await;
    }

    /// Fix4 — an idempotency-key 23505 (ambiguous-commit retry) rolls back the
    /// SAVEPOINT and resolves to the prior row's id. Reverting the SAVEPOINT
    /// wrap aborts the tx and the lookup panics; reverting the IdempotentHit
    /// branch surfaces the raw 23505.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admission_idempotency_conflict_returns_prior_row() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;
        let idem = idempotency_key("claude", "chanK1", "msg-1", 1);
        // Prior row with the same key on a DIFFERENT, terminal (non-open)
        // channel so only the idempotency index collides.
        let prior: i64 = sqlx::query_scalar(
            "INSERT INTO intake_outbox (
                 target_instance_id, forwarded_by_instance_id, required_labels,
                 channel_id, user_msg_id, request_owner_id, user_text, turn_kind,
                 agent_id, provider, status, attempt_no,
                 admission_kind, owner_instance_id, owner_generation, idempotency_key
             ) VALUES (
                 'node-A', 'leader-1', '[]'::JSONB,
                 'chanK1', 'msg-1', 'user-1', 'hi', 'standard',
                 'agent-x', 'claude', 'done', 1,
                 'forwarded', 'node-A', 0, $1
             ) RETURNING id",
        )
        .bind(&idem)
        .fetch_one(&pool)
        .await
        .unwrap();

        let mut tx = pool.begin().await.unwrap();
        // Reuse the same key on a fresh channel → idempotency collision only.
        let payload = admission_payload("chanK2", "msg-2", "claude");
        let out = insert_admission_savepoint(
            &mut tx,
            &payload,
            AdmissionKind::Forwarded,
            "node-A",
            0,
            "node-A",
            &idem,
            1,
        )
        .await
        .unwrap();
        assert_eq!(out, AdmissionOutcome::IdempotentHit { outbox_id: prior });
        // tx still alive.
        let alive: i64 = sqlx::query_scalar("SELECT 1::BIGINT")
            .fetch_one(&mut *tx)
            .await
            .expect("tx alive after idempotency savepoint rollback");
        assert_eq!(alive, 1);
        tx.commit().await.unwrap();

        pool.close().await;
        pg.drop().await;
    }

    /// A clean admission inserts a fresh row with the owner stamp and
    /// forwarded/pending initial state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admission_insert_stamps_owner_and_returns_id() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;

        let mut tx = pool.begin().await.unwrap();
        let payload = admission_payload("chanFresh", "msg-1", "claude");
        let idem = idempotency_key("claude", "chanFresh", "msg-1", 1);
        let out = insert_admission_savepoint(
            &mut tx,
            &payload,
            AdmissionKind::Forwarded,
            "node-A",
            0,
            "node-A",
            &idem,
            1,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let AdmissionOutcome::Inserted { outbox_id } = out else {
            panic!("expected Inserted, got {out:?}");
        };

        #[derive(sqlx::FromRow)]
        struct Stamp {
            status: String,
            admission_kind: String,
            owner_instance_id: Option<String>,
            owner_generation: Option<i64>,
            idempotency_key: Option<String>,
        }
        let stamp: Stamp = sqlx::query_as(
            "SELECT status, admission_kind, owner_instance_id, owner_generation, idempotency_key
               FROM intake_outbox WHERE id = $1",
        )
        .bind(outbox_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stamp.status, "pending");
        assert_eq!(stamp.admission_kind, "forwarded");
        assert_eq!(stamp.owner_instance_id.as_deref(), Some("node-A"));
        assert_eq!(stamp.owner_generation, Some(0));
        assert_eq!(stamp.idempotency_key.as_deref(), Some(idem.as_str()));

        pool.close().await;
        pg.drop().await;
    }

    // -- claim double fence (§3.5.1, Fix2) ----------------------------------

    /// Happy path: a forwarded, fence-valid pending row is claimed and the
    /// claim token is stamped in `claim_owner`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn claim_promotes_fence_valid_pending_row() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;
        insert_owner(&pool, "claude", "chanC", "node-W", 0, "active").await;
        let row_id = insert_outbox(
            &pool,
            "chanC",
            "msg-1",
            "claude",
            "pending",
            "forwarded",
            Some("node-W"),
            Some(0),
            None,
            None,
        )
        .await;

        let claimed = claim_pending_for_target_fenced(&pool, "node-W", "node-W#restart7", "claude")
            .await
            .unwrap();
        let claimed = claimed.expect("fence-valid row must be claimed");
        assert_eq!(claimed.id, row_id);
        assert_eq!(claimed.status, "claimed");
        assert_eq!(claimed.claim_owner.as_deref(), Some("node-W#restart7"));

        pool.close().await;
        pg.drop().await;
    }

    /// A local-admission row is never claimed by the worker claim path (P1-b):
    /// `admission_kind='local'` is excluded by the candidate SELECT.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn claim_ignores_local_admission_rows() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;
        insert_owner(&pool, "claude", "chanL", "node-W", 0, "active").await;
        insert_outbox(
            &pool,
            "chanL",
            "msg-1",
            "claude",
            "pending",
            "local",
            Some("node-W"),
            Some(0),
            None,
            None,
        )
        .await;

        let claimed = claim_pending_for_target_fenced(&pool, "node-W", "tok", "claude")
            .await
            .unwrap();
        assert!(claimed.is_none(), "local rows are not worker-claimable");

        pool.close().await;
        pg.drop().await;
    }

    /// Fix2 — the promote UPDATE's owner fence (the SECOND, defense-in-depth
    /// layer behind the P1-B advisory lock). Drives the two real claim SQL
    /// constants (`CLAIM_CANDIDATE_SQL`, `CLAIM_PROMOTE_SQL`) across a transfer
    /// FULLY COMMITTED between the candidate SELECT and the promote UPDATE: the
    /// candidate SELECT passes (owner still `node-W@0`), ownership moves to
    /// `node-V@1`, so the fenced UPDATE (fresh statement snapshot) sees the new
    /// owner and affects 0 rows; the row stays `pending`. Reverting Fix2 (drop
    /// the EXISTS fence from `CLAIM_PROMOTE_SQL`) makes the UPDATE match on
    /// id + target + owner alone → 1 row → the 0-rows / still-pending asserts
    /// fail. The OVERLAPPING (uncommitted-transfer) race that the plain fence
    /// cannot catch is covered by
    /// `claim_advisory_lock_serializes_with_inflight_transfer` (P1-B).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn claim_promote_fence_blocks_stale_claim_after_transfer() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;
        insert_owner(&pool, "claude", "chanRace", "node-W", 0, "active").await;
        let row_id = insert_outbox(
            &pool,
            "chanRace",
            "msg-1",
            "claude",
            "pending",
            "forwarded",
            Some("node-W"),
            Some(0),
            None,
            None,
        )
        .await;

        // Claim tx: run the real candidate SELECT (locks only the outbox row).
        let mut claim_tx = pool.begin().await.unwrap();
        let candidate: Option<ClaimCandidate> = sqlx::query_as(CLAIM_CANDIDATE_SQL)
            .bind("node-W")
            .bind("claude")
            .fetch_optional(&mut *claim_tx)
            .await
            .unwrap();
        let candidate_id = candidate.map(|c| c.id);
        assert_eq!(
            candidate_id,
            Some(row_id),
            "candidate SELECT passes the fence"
        );

        // Concurrently: transfer node-W@0 → node-V@1 (touches only the owner
        // table, so it does not block on the outbox row lock).
        {
            let id = identity("claude", "chanRace");
            let mut xf = pool.begin().await.unwrap();
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(id.advisory_key())
                .execute(&mut *xf)
                .await
                .unwrap();
            let out = transfer_owner_in_tx(&mut xf, &id, "node-W", 0, "node-V")
                .await
                .unwrap();
            assert_eq!(out, TransferOutcome::Transferred { new_generation: 1 });
            xf.commit().await.unwrap();
        }

        // Promote UPDATE now sees node-V@1 as active → fence fails → 0 rows.
        let promoted: Option<IntakeOutboxRow> = sqlx::query_as(CLAIM_PROMOTE_SQL)
            .bind(row_id)
            .bind("node-W")
            .bind("node-W#restart1")
            .fetch_optional(&mut *claim_tx)
            .await
            .unwrap();
        claim_tx.commit().await.unwrap();

        assert!(
            promoted.is_none(),
            "stale claim must be fenced out after transfer"
        );
        assert_eq!(
            outbox_status(&pool, row_id).await,
            "pending",
            "row stays pending"
        );

        pool.close().await;
        pg.drop().await;
    }

    /// P1-B — the claim's channel advisory lock linearizes with an in-flight
    /// transfer. `transfer_tx` holds the channel advisory lock and has
    /// superseded `node-W@0` but has NOT committed. The real claim runs
    /// concurrently: it finds the candidate (owner still reads `node-W@0`
    /// active under READ COMMITTED — the supersede is uncommitted) and then
    /// blocks acquiring the SAME advisory lock. Only after `transfer_tx` commits
    /// (`node-W@0` → `node-V@1`) does the claim proceed and re-read the fresh
    /// owner, so the fence fails and it returns None; the row stays `pending`.
    ///
    /// Mutation: remove the `pg_advisory_xact_lock` from the claim. The promote
    /// UPDATE then runs against its statement snapshot WHILE the supersede is
    /// still uncommitted — READ COMMITTED sees `node-W@0` STILL active, the
    /// stale claim succeeds (Some), and stale `node-W` could double-execute
    /// alongside the new owner. The `is_none()` assert catches it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn claim_advisory_lock_serializes_with_inflight_transfer() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;
        insert_owner(&pool, "claude", "chanLin", "node-W", 0, "active").await;
        let row_id = insert_outbox(
            &pool,
            "chanLin",
            "msg-1",
            "claude",
            "pending",
            "forwarded",
            Some("node-W"),
            Some(0),
            None,
            None,
        )
        .await;

        let id = identity("claude", "chanLin");

        // transfer_tx: hold the channel advisory lock + supersede node-W@0,
        // uncommitted. Remember its backend PID so pg_locks can prove the claim
        // is queued on this exact lock rather than relying on scheduler timing.
        let mut transfer_tx = pool.begin().await.unwrap();
        let transfer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *transfer_tx)
            .await
            .unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(id.advisory_key())
            .execute(&mut *transfer_tx)
            .await
            .unwrap();
        let superseded = supersede_active_owner(&mut transfer_tx, &id, "node-W", 0)
            .await
            .unwrap();
        assert_eq!(superseded, 1);

        // Spawn the real claim with a seam that signals only after its candidate
        // SELECT has succeeded and locked the outbox row.
        let claim_pool = pool.clone();
        let (candidate_seen_tx, candidate_seen_rx) = tokio::sync::oneshot::channel();
        let mut claim = tokio::spawn(async move {
            claim_pending_after_candidate_fenced(
                &claim_pool,
                "node-W",
                "tok-r1",
                "claude",
                |candidate| {
                    assert_eq!(candidate.id, row_id);
                    async move {
                        candidate_seen_tx
                            .send(())
                            .expect("test waits for candidate signal");
                    }
                },
            )
            .await
        });
        candidate_seen_rx
            .await
            .expect("claim must acquire the seeded candidate");

        // Poll pg_locks until the claim has an ungranted request matching the
        // exact advisory lock held by transfer_tx. If the production advisory
        // lock is removed, the claim instead completes immediately with Some;
        // this select takes the completed-task branch and fails deterministically.
        let wait_for_same_advisory_lock = async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS (
                         SELECT 1
                           FROM pg_locks held
                           JOIN pg_locks waiter
                             ON waiter.locktype = held.locktype
                            AND waiter.database IS NOT DISTINCT FROM held.database
                            AND waiter.classid IS NOT DISTINCT FROM held.classid
                            AND waiter.objid IS NOT DISTINCT FROM held.objid
                            AND waiter.objsubid IS NOT DISTINCT FROM held.objsubid
                            AND waiter.mode = held.mode
                          WHERE held.pid = $1
                            AND held.locktype = 'advisory'
                            AND held.granted
                            AND NOT waiter.granted
                    )",
                )
                .bind(transfer_pid)
                .fetch_one(&pool)
                .await
                .unwrap();
                if waiting {
                    break;
                }
                tokio::task::yield_now().await;
            }
        };
        tokio::select! {
            wait = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                wait_for_same_advisory_lock,
            ) => wait.expect("claim must reach the advisory-lock wait"),
            completed = &mut claim => {
                let completed = completed
                    .expect("claim task")
                    .expect("claim query");
                panic!(
                    "claim completed before waiting on transfer advisory lock: claimed={}",
                    completed.is_some()
                );
            }
        }
        assert!(
            !claim.is_finished(),
            "claim must still be blocked while transfer_tx owns the advisory lock"
        );

        // Complete + commit the transfer → node-W@0 superseded, node-V@1 active.
        sqlx::query(
            "INSERT INTO intake_session_owners
                 (provider, raw_channel_id, owner_instance_id, generation, status)
             VALUES ($1, $2, 'node-V', 1, 'active')",
        )
        .bind(id.provider())
        .bind(id.raw_channel_id())
        .execute(&mut *transfer_tx)
        .await
        .unwrap();
        transfer_tx.commit().await.unwrap();

        let claimed = claim.await.unwrap().unwrap();
        assert!(
            claimed.is_none(),
            "claim must serialize behind the transfer and fence out the stale owner"
        );
        assert_eq!(outbox_status(&pool, row_id).await, "pending");

        pool.close().await;
        pg.drop().await;
    }

    // -- stale sweep alias fence (§3.5.3, Fix3) -----------------------------

    /// Fix3 — the sweep re-arms ONLY fence-valid stale claims. A live `codex@0`
    /// owner shares the raw channel `shared` with a `claude` claimed row stamped
    /// for a NON-active owner (`node-STALE`). The correctly-aliased, normalized
    /// fence must NOT re-arm that orphan (claude has no active `node-STALE@0` on
    /// `shared`), while it DOES re-arm a `claude` row on a separate channel whose
    /// stamp matches its live owner. If the correlated columns were left
    /// unqualified (the tautology bug), the codex active owner on `shared` would
    /// satisfy the EXISTS and wrongly resurrect the orphan → the "orphan stays
    /// claimed" assert fails. (Orphan and valid rows live on distinct channels
    /// because the channel-only open-route unique index forbids two `claimed`
    /// rows per channel.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sweep_fence_does_not_resurrect_cross_provider_orphan() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;

        // Cross-provider active owner on the shared raw channel.
        insert_owner(&pool, "codex", "shared", "node-CDX", 0, "active").await;
        // Orphan: claude claimed row on `shared` stamped for a NON-active owner.
        let orphan = insert_outbox(
            &pool,
            "shared",
            "msg-orphan",
            "claude",
            "claimed",
            "forwarded",
            Some("node-STALE"),
            Some(0),
            Some("node-STALE#r1"),
            Some(3600),
        )
        .await;

        // Fence-valid claim on a separate channel stamped for its live owner.
        insert_owner(&pool, "claude", "valid-chan", "node-CLD", 0, "active").await;
        let valid = insert_outbox(
            &pool,
            "valid-chan",
            "msg-valid",
            "claude",
            "claimed",
            "forwarded",
            Some("node-CLD"),
            Some(0),
            Some("node-CLD#r1"),
            Some(3600),
        )
        .await;

        let rearmed = sweep_stale_pre_accept_claims_fenced(&pool, 1)
            .await
            .unwrap();
        assert_eq!(rearmed, 1, "only the fence-valid stale claim is re-armed");
        assert_eq!(
            outbox_status(&pool, orphan).await,
            "claimed",
            "cross-provider active owner must NOT resurrect a non-active-owner claim"
        );
        assert_eq!(
            outbox_status(&pool, valid).await,
            "pending",
            "fence-valid claim re-armed"
        );

        pool.close().await;
        pg.drop().await;
    }

    // -- adoption (§3.6) ----------------------------------------------------

    /// Adoption seeds gen0 (`adopted_from_session=true`) only when unowned; an
    /// existing owner is left authoritative.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adopt_seeds_generation_zero_only_when_unowned() {
        let pg = TestPostgresDb::create().await;
        let pool = pg.connect_and_migrate().await;

        // Unowned → adopt gen0.
        let adopt = identity("claude", "adopt");
        {
            let mut tx = pool.begin().await.unwrap();
            let out = adopt_owner_from_session_in_tx(&mut tx, &adopt, "node-A")
                .await
                .unwrap();
            tx.commit().await.unwrap();
            assert_eq!(out, AdoptOutcome::Adopted { generation: 0 });
        }
        let adopted: bool = sqlx::query_scalar(
            "SELECT adopted_from_session FROM intake_session_owners
              WHERE provider='claude' AND raw_channel_id='adopt' AND status='active'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(adopted, "adopted owner row is flagged adopted_from_session");

        // Already owned → refuse.
        {
            let mut tx = pool.begin().await.unwrap();
            let out = adopt_owner_from_session_in_tx(&mut tx, &adopt, "node-B")
                .await
                .unwrap();
            tx.commit().await.unwrap();
            assert_eq!(
                out,
                AdoptOutcome::AlreadyOwned {
                    owner_instance_id: "node-A".into(),
                    generation: 0,
                }
            );
        }

        pool.close().await;
        pg.drop().await;
    }
}
