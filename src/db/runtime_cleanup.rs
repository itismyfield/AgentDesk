#![allow(dead_code)] // Dormant Task #25 foundation; production consumers land in later slices.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

const CLEAR_FOR_RESUME_PLAN: [(&str, i16); 6] = [
    ("block_runtime_admission", 1),
    ("clear_queued_input", 2),
    ("expire_runtime_lease", 3),
    ("cancel_active_runtime", 4),
    ("clear_persisted_session", 5),
    ("release_runtime_slot", 6),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RuntimeCleanupTarget {
    pub(crate) session_id: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeCleanupAction {
    ClearForResume,
}

impl RuntimeCleanupAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClearForResume => "clear_for_resume",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub(crate) enum RuntimeCleanupState {
    Pending,
    Fenced,
    Applying,
    Completed,
    Aborted,
}

impl RuntimeCleanupState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Fenced => "fenced",
            Self::Applying => "applying",
            Self::Completed => "completed",
            Self::Aborted => "aborted",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CreateRuntimeCleanupOperation<'a> {
    pub(crate) operation_id: Uuid,
    pub(crate) request_key: &'a str,
    pub(crate) target: RuntimeCleanupTarget,
    pub(crate) action: RuntimeCleanupAction,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BeginRuntimeCleanupAttempt<'a> {
    pub(crate) operation_id: Uuid,
    pub(crate) expected_fence: i64,
    pub(crate) claim_owner: &'a str,
    pub(crate) attempt_token: Uuid,
    pub(crate) lease_expires_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CompleteRuntimeCleanupAttempt {
    pub(crate) operation_id: Uuid,
    pub(crate) expected_fence: i64,
    pub(crate) attempt_token: Uuid,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct RuntimeCleanupOperation {
    pub(crate) operation_id: Uuid,
    pub(crate) request_key: String,
    pub(crate) target_session_id: i64,
    pub(crate) requested_action: String,
    pub(crate) state: RuntimeCleanupState,
    pub(crate) fence: Option<i64>,
    pub(crate) claim_owner: Option<String>,
    pub(crate) attempt_token: Option<Uuid>,
    pub(crate) attempt_no: i64,
    pub(crate) lease_expires_at: Option<DateTime<Utc>>,
    pub(crate) commit_decided_at: Option<DateTime<Utc>>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
    pub(crate) completed_at: Option<DateTime<Utc>>,
    pub(crate) aborted_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CreateRuntimeCleanupResult {
    Created { operation_id: Uuid },
    Replayed { operation_id: Uuid },
    RequestConflict { operation_id: Uuid },
    TargetBusy { operation_id: Uuid },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FenceRuntimeCleanupResult {
    Advanced { fence: i64 },
    Replayed { fence: i64 },
    Stale { state: RuntimeCleanupState },
    NotFound,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BeginRuntimeCleanupResult {
    Acquired { attempt_no: i64 },
    Replayed { attempt_no: i64 },
    LeaseHeld { owner: String, attempt_no: i64 },
    LostOwnership { attempt_no: i64 },
    Stale { state: RuntimeCleanupState },
    NotFound,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CompleteRuntimeCleanupResult {
    Completed,
    Replayed,
    LostOwnership,
    Stale { state: RuntimeCleanupState },
    NotFound,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AbortRuntimeCleanupResult {
    Aborted,
    Replayed,
    RollForwardRequired,
    Stale { state: RuntimeCleanupState },
    NotFound,
}

#[derive(sqlx::FromRow)]
struct OperationIdentityRow {
    operation_id: Uuid,
    target_session_id: i64,
    requested_action: String,
}

#[derive(sqlx::FromRow)]
struct OperationStateRow {
    state: RuntimeCleanupState,
    fence: Option<i64>,
    claim_owner: Option<String>,
    attempt_token: Option<Uuid>,
    attempt_no: i64,
    lease_expires_at: Option<DateTime<Utc>>,
}

pub(crate) async fn resolve_runtime_cleanup_target(
    pool: &PgPool,
    locator: &str,
) -> Result<Option<RuntimeCleanupTarget>, sqlx::Error> {
    let session_id = sqlx::query_scalar::<_, i64>(
        "SELECT s.id
         FROM sessions s
         WHERE s.session_key = $1
         UNION
         SELECT a.session_id
         FROM session_key_aliases a
         WHERE a.session_key = $1",
    )
    .bind(locator)
    .fetch_optional(pool)
    .await?;
    Ok(session_id.map(|session_id| RuntimeCleanupTarget { session_id }))
}

pub(crate) async fn create_runtime_cleanup_operation(
    pool: &PgPool,
    command: &CreateRuntimeCleanupOperation<'_>,
) -> Result<CreateRuntimeCleanupResult, sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock_request_key(&mut tx, command.request_key).await?;
    lock_target(&mut tx, command.target.session_id).await?;

    if let Some(existing) = sqlx::query_as::<_, OperationIdentityRow>(
        "SELECT operation_id, target_session_id, requested_action
         FROM runtime_cleanup_operations WHERE request_key = $1
         UNION ALL
         SELECT operation_id, target_session_id, requested_action
         FROM runtime_cleanup_operation_archive WHERE request_key = $1",
    )
    .bind(command.request_key)
    .fetch_optional(&mut *tx)
    .await?
    {
        tx.commit().await?;
        return if existing.target_session_id == command.target.session_id
            && existing.requested_action == command.action.as_str()
        {
            Ok(CreateRuntimeCleanupResult::Replayed {
                operation_id: existing.operation_id,
            })
        } else {
            Ok(CreateRuntimeCleanupResult::RequestConflict {
                operation_id: existing.operation_id,
            })
        };
    }

    if let Some(operation_id) = sqlx::query_scalar::<_, Uuid>(
        "SELECT operation_id FROM runtime_cleanup_operations
         WHERE target_session_id = $1 AND state IN ('pending', 'fenced', 'applying')",
    )
    .bind(command.target.session_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        tx.commit().await?;
        return Ok(CreateRuntimeCleanupResult::TargetBusy { operation_id });
    }

    sqlx::query(
        "INSERT INTO runtime_cleanup_operations
         (operation_id, request_key, target_session_id, requested_action)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(command.operation_id)
    .bind(command.request_key)
    .bind(command.target.session_id)
    .bind(command.action.as_str())
    .execute(&mut *tx)
    .await?;

    for (intent_kind, ordinal) in CLEAR_FOR_RESUME_PLAN {
        sqlx::query(
            "INSERT INTO runtime_cleanup_intents (operation_id, intent_kind, ordinal)
             VALUES ($1, $2, $3)",
        )
        .bind(command.operation_id)
        .bind(intent_kind)
        .bind(ordinal)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(CreateRuntimeCleanupResult::Created {
        operation_id: command.operation_id,
    })
}

pub(crate) async fn fence_runtime_cleanup_operation(
    pool: &PgPool,
    operation_id: Uuid,
) -> Result<FenceRuntimeCleanupResult, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let Some(current) = load_state_for_update(&mut tx, operation_id).await? else {
        tx.commit().await?;
        return Ok(FenceRuntimeCleanupResult::NotFound);
    };
    if current.state == RuntimeCleanupState::Fenced {
        let fence = current
            .fence
            .ok_or_else(|| sqlx::Error::Protocol("fenced cleanup operation has no fence".into()))?;
        tx.commit().await?;
        return Ok(FenceRuntimeCleanupResult::Replayed { fence });
    }
    if current.state != RuntimeCleanupState::Pending {
        tx.commit().await?;
        return Ok(FenceRuntimeCleanupResult::Stale {
            state: current.state,
        });
    }

    let fence = allocate_fence(&mut tx, operation_id).await?;
    sqlx::query(
        "UPDATE runtime_cleanup_operations
         SET state = 'fenced', fence = $2, updated_at = NOW()
         WHERE operation_id = $1",
    )
    .bind(operation_id)
    .bind(fence)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(FenceRuntimeCleanupResult::Advanced { fence })
}

pub(crate) async fn begin_runtime_cleanup_attempt(
    pool: &PgPool,
    command: BeginRuntimeCleanupAttempt<'_>,
) -> Result<BeginRuntimeCleanupResult, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let database_now = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT NOW()")
        .fetch_one(&mut *tx)
        .await?;
    let Some(current) = load_state_for_update(&mut tx, command.operation_id).await? else {
        tx.commit().await?;
        return Ok(BeginRuntimeCleanupResult::NotFound);
    };
    if current.fence != Some(command.expected_fence) {
        tx.commit().await?;
        return Ok(BeginRuntimeCleanupResult::Stale {
            state: current.state,
        });
    }
    if current.state == RuntimeCleanupState::Applying {
        if current.attempt_token == Some(command.attempt_token) {
            if current.claim_owner.as_deref() != Some(command.claim_owner)
                || current.lease_expires_at != Some(command.lease_expires_at)
            {
                tx.commit().await?;
                return Ok(BeginRuntimeCleanupResult::LostOwnership {
                    attempt_no: current.attempt_no,
                });
            }
            tx.commit().await?;
            return Ok(BeginRuntimeCleanupResult::Replayed {
                attempt_no: current.attempt_no,
            });
        }
        if current
            .lease_expires_at
            .is_some_and(|expires| expires > database_now)
        {
            let owner = current.claim_owner.ok_or_else(|| {
                sqlx::Error::Protocol("applying cleanup operation has no claim owner".into())
            })?;
            tx.commit().await?;
            return Ok(BeginRuntimeCleanupResult::LeaseHeld {
                owner,
                attempt_no: current.attempt_no,
            });
        }
    } else if current.state != RuntimeCleanupState::Fenced {
        tx.commit().await?;
        return Ok(BeginRuntimeCleanupResult::Stale {
            state: current.state,
        });
    }

    let attempt_no = current.attempt_no + 1;
    sqlx::query(
        "UPDATE runtime_cleanup_operations
         SET state = 'applying', claim_owner = $2, attempt_token = $3,
             attempt_no = $4, lease_expires_at = $5, attempt_started_at = $6,
             commit_decided_at = COALESCE(commit_decided_at, NOW()), updated_at = NOW()
         WHERE operation_id = $1",
    )
    .bind(command.operation_id)
    .bind(command.claim_owner)
    .bind(command.attempt_token)
    .bind(attempt_no)
    .bind(command.lease_expires_at)
    .bind(database_now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(BeginRuntimeCleanupResult::Acquired { attempt_no })
}

pub(crate) async fn complete_runtime_cleanup_attempt(
    pool: &PgPool,
    command: CompleteRuntimeCleanupAttempt,
) -> Result<CompleteRuntimeCleanupResult, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let Some(current) = load_state_for_update(&mut tx, command.operation_id).await? else {
        tx.commit().await?;
        return Ok(CompleteRuntimeCleanupResult::NotFound);
    };
    if current.fence != Some(command.expected_fence) {
        tx.commit().await?;
        return Ok(CompleteRuntimeCleanupResult::Stale {
            state: current.state,
        });
    }
    if current.state == RuntimeCleanupState::Completed {
        tx.commit().await?;
        return if current.attempt_token == Some(command.attempt_token) {
            Ok(CompleteRuntimeCleanupResult::Replayed)
        } else {
            Ok(CompleteRuntimeCleanupResult::LostOwnership)
        };
    }
    if current.state != RuntimeCleanupState::Applying {
        tx.commit().await?;
        return Ok(CompleteRuntimeCleanupResult::Stale {
            state: current.state,
        });
    }
    if current.attempt_token != Some(command.attempt_token) {
        tx.commit().await?;
        return Ok(CompleteRuntimeCleanupResult::LostOwnership);
    }

    sqlx::query(
        "UPDATE runtime_cleanup_operations
         SET state = 'completed', completed_at = NOW(), updated_at = NOW()
         WHERE operation_id = $1",
    )
    .bind(command.operation_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(CompleteRuntimeCleanupResult::Completed)
}

pub(crate) async fn abort_runtime_cleanup_operation(
    pool: &PgPool,
    operation_id: Uuid,
) -> Result<AbortRuntimeCleanupResult, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let Some(current) = load_state_for_update(&mut tx, operation_id).await? else {
        tx.commit().await?;
        return Ok(AbortRuntimeCleanupResult::NotFound);
    };
    if current.state == RuntimeCleanupState::Aborted {
        tx.commit().await?;
        return Ok(AbortRuntimeCleanupResult::Replayed);
    }
    if current.state == RuntimeCleanupState::Applying
        || current.state == RuntimeCleanupState::Completed
    {
        tx.commit().await?;
        return Ok(AbortRuntimeCleanupResult::RollForwardRequired);
    }
    if current.state != RuntimeCleanupState::Pending && current.state != RuntimeCleanupState::Fenced
    {
        tx.commit().await?;
        return Ok(AbortRuntimeCleanupResult::Stale {
            state: current.state,
        });
    }

    sqlx::query(
        "UPDATE runtime_cleanup_operations
         SET state = 'aborted', aborted_from_state = $2,
             aborted_at = NOW(), updated_at = NOW()
         WHERE operation_id = $1",
    )
    .bind(operation_id)
    .bind(current.state.as_str())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(AbortRuntimeCleanupResult::Aborted)
}

pub(crate) async fn retire_terminal_runtime_cleanup_operation(
    pool: &PgPool,
    operation_id: Uuid,
    terminal_before: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT agentdesk_retire_terminal_runtime_cleanup_operation($1, $2)")
        .bind(operation_id)
        .bind(terminal_before)
        .fetch_one(pool)
        .await
}

pub(crate) async fn load_runtime_cleanup_operation(
    pool: &PgPool,
    operation_id: Uuid,
) -> Result<Option<RuntimeCleanupOperation>, sqlx::Error> {
    sqlx::query_as(
        "SELECT operation_id, request_key, target_session_id, requested_action,
                state, fence, claim_owner, attempt_token, attempt_no,
                lease_expires_at, commit_decided_at, created_at, updated_at,
                completed_at, aborted_at
         FROM runtime_cleanup_operations WHERE operation_id = $1",
    )
    .bind(operation_id)
    .fetch_optional(pool)
    .await
}

async fn load_state_for_update(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: Uuid,
) -> Result<Option<OperationStateRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT state, fence, claim_owner, attempt_token, attempt_no, lease_expires_at
         FROM runtime_cleanup_operations WHERE operation_id = $1 FOR UPDATE",
    )
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await
}

async fn lock_request_key(
    tx: &mut Transaction<'_, Postgres>,
    request_key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('request:' || $1, 4916))")
        .bind(request_key)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn lock_target(
    tx: &mut Transaction<'_, Postgres>,
    session_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(4916, hashint8($1))")
        .bind(session_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn allocate_fence(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: Uuid,
) -> Result<i64, sqlx::Error> {
    let target_session_id = sqlx::query_scalar::<_, i64>(
        "SELECT target_session_id FROM runtime_cleanup_operations WHERE operation_id = $1",
    )
    .bind(operation_id)
    .fetch_one(&mut **tx)
    .await?;

    sqlx::query_scalar(
        "INSERT INTO runtime_cleanup_fences (target_session_id, last_fence, operation_id)
         VALUES ($1, 1, $2)
         ON CONFLICT (target_session_id) DO UPDATE
         SET last_fence = runtime_cleanup_fences.last_fence + 1,
             operation_id = EXCLUDED.operation_id,
             updated_at = NOW()
         RETURNING last_fence",
    )
    .bind(target_session_id)
    .bind(operation_id)
    .fetch_one(&mut **tx)
    .await
}

#[cfg(test)]
mod postgres_tests;
