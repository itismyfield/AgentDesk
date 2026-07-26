#![allow(dead_code)] // Dormant Task #25 foundation; production consumers land in later slices.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeCleanupTargetKind {
    DiscordSession,
}

impl RuntimeCleanupTargetKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::DiscordSession => "discord_session",
        }
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeCleanupIntentKind {
    BlockRuntimeAdmission,
    ClearQueuedInput,
    ExpireRuntimeLease,
    CancelActiveRuntime,
    ClearPersistedSession,
    ReleaseRuntimeSlot,
}

impl RuntimeCleanupIntentKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::BlockRuntimeAdmission => "block_runtime_admission",
            Self::ClearQueuedInput => "clear_queued_input",
            Self::ExpireRuntimeLease => "expire_runtime_lease",
            Self::CancelActiveRuntime => "cancel_active_runtime",
            Self::ClearPersistedSession => "clear_persisted_session",
            Self::ReleaseRuntimeSlot => "release_runtime_slot",
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

    fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Pending, Self::Fenced)
                | (Self::Pending, Self::Aborted)
                | (Self::Fenced, Self::Applying)
                | (Self::Fenced, Self::Aborted)
                | (Self::Applying, Self::Completed)
                | (Self::Applying, Self::Aborted)
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimeCleanupIntent {
    pub(crate) kind: RuntimeCleanupIntentKind,
    pub(crate) ordinal: i16,
}

#[derive(Clone, Debug)]
pub(crate) struct CreateRuntimeCleanupOperation<'a> {
    pub(crate) operation_id: Uuid,
    pub(crate) request_key: &'a str,
    pub(crate) target_kind: RuntimeCleanupTargetKind,
    pub(crate) target_key: &'a str,
    pub(crate) action: RuntimeCleanupAction,
    pub(crate) intents: &'a [RuntimeCleanupIntent],
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct RuntimeCleanupOperation {
    pub(crate) operation_id: Uuid,
    pub(crate) request_key: String,
    pub(crate) target_kind: String,
    pub(crate) target_key: String,
    pub(crate) requested_action: String,
    pub(crate) state: RuntimeCleanupState,
    pub(crate) fence: Option<i64>,
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
pub(crate) enum TransitionRuntimeCleanupResult {
    Advanced {
        operation_id: Uuid,
        state: RuntimeCleanupState,
        fence: Option<i64>,
    },
    Replayed {
        operation_id: Uuid,
        state: RuntimeCleanupState,
        fence: Option<i64>,
    },
    Stale {
        operation_id: Uuid,
        current_state: RuntimeCleanupState,
        current_fence: Option<i64>,
    },
    Illegal {
        operation_id: Uuid,
        current_state: RuntimeCleanupState,
    },
    NotFound,
}

#[derive(sqlx::FromRow)]
struct OperationIdentityRow {
    operation_id: Uuid,
    target_kind: String,
    target_key: String,
    requested_action: String,
}

#[derive(sqlx::FromRow)]
struct OperationStateRow {
    operation_id: Uuid,
    state: RuntimeCleanupState,
    fence: Option<i64>,
}

pub(crate) async fn create_runtime_cleanup_operation(
    pool: &PgPool,
    command: &CreateRuntimeCleanupOperation<'_>,
) -> Result<CreateRuntimeCleanupResult, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let result = create_runtime_cleanup_operation_in_tx(&mut tx, command).await?;
    tx.commit().await?;
    Ok(result)
}

async fn create_runtime_cleanup_operation_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    command: &CreateRuntimeCleanupOperation<'_>,
) -> Result<CreateRuntimeCleanupResult, sqlx::Error> {
    lock_target(tx, command.target_kind, command.target_key).await?;

    if let Some(existing) = sqlx::query_as::<_, OperationIdentityRow>(
        "SELECT operation_id, target_kind, target_key, requested_action
         FROM runtime_cleanup_operations WHERE request_key = $1 FOR UPDATE",
    )
    .bind(command.request_key)
    .fetch_optional(&mut **tx)
    .await?
    {
        return if existing.target_kind == command.target_kind.as_str()
            && existing.target_key == command.target_key
            && existing.requested_action == command.action.as_str()
            && intents_match(tx, existing.operation_id, command.intents).await?
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
         WHERE target_kind = $1 AND target_key = $2
           AND state IN ('pending', 'fenced', 'applying')",
    )
    .bind(command.target_kind.as_str())
    .bind(command.target_key)
    .fetch_optional(&mut **tx)
    .await?
    {
        return Ok(CreateRuntimeCleanupResult::TargetBusy { operation_id });
    }

    let operation_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO runtime_cleanup_operations (
            operation_id, request_key, target_kind, target_key, requested_action
        ) VALUES ($1, $2, $3, $4, $5)
        RETURNING operation_id
        "#,
    )
    .bind(command.operation_id)
    .bind(command.request_key)
    .bind(command.target_kind.as_str())
    .bind(command.target_key)
    .bind(command.action.as_str())
    .fetch_one(&mut **tx)
    .await?;

    for intent in command.intents {
        sqlx::query(
            "INSERT INTO runtime_cleanup_intents (operation_id, intent_kind, ordinal)
             VALUES ($1, $2, $3)",
        )
        .bind(operation_id)
        .bind(intent.kind.as_str())
        .bind(intent.ordinal)
        .execute(&mut **tx)
        .await?;
    }
    Ok(CreateRuntimeCleanupResult::Created { operation_id })
}

pub(crate) async fn transition_runtime_cleanup_operation(
    pool: &PgPool,
    operation_id: Uuid,
    expected_state: RuntimeCleanupState,
    expected_fence: Option<i64>,
    next_state: RuntimeCleanupState,
) -> Result<TransitionRuntimeCleanupResult, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let Some(current) = sqlx::query_as::<_, OperationStateRow>(
        "SELECT operation_id, state, fence
         FROM runtime_cleanup_operations WHERE operation_id = $1 FOR UPDATE",
    )
    .bind(operation_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.commit().await?;
        return Ok(TransitionRuntimeCleanupResult::NotFound);
    };

    let exact_replay = current.state == next_state
        && (current.fence == expected_fence
            || (expected_state == RuntimeCleanupState::Pending
                && next_state == RuntimeCleanupState::Fenced
                && expected_fence.is_none()
                && current.fence.is_some()));
    if exact_replay {
        tx.commit().await?;
        return Ok(TransitionRuntimeCleanupResult::Replayed {
            operation_id,
            state: current.state,
            fence: current.fence,
        });
    }

    if current.state != expected_state || current.fence != expected_fence {
        tx.commit().await?;
        return Ok(TransitionRuntimeCleanupResult::Stale {
            operation_id,
            current_state: current.state,
            current_fence: current.fence,
        });
    }

    if !current.state.can_transition_to(next_state) {
        tx.commit().await?;
        return Ok(TransitionRuntimeCleanupResult::Illegal {
            operation_id,
            current_state: current.state,
        });
    }

    let next_fence = if next_state == RuntimeCleanupState::Fenced {
        Some(allocate_fence(&mut tx, operation_id).await?)
    } else {
        current.fence
    };

    sqlx::query(
        "UPDATE runtime_cleanup_operations
         SET state = $2,
             fence = $3,
             updated_at = NOW(),
             completed_at = CASE WHEN $2 = 'completed' THEN NOW() ELSE completed_at END,
             aborted_at = CASE WHEN $2 = 'aborted' THEN NOW() ELSE aborted_at END
         WHERE operation_id = $1",
    )
    .bind(operation_id)
    .bind(next_state.as_str())
    .bind(next_fence)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(TransitionRuntimeCleanupResult::Advanced {
        operation_id,
        state: next_state,
        fence: next_fence,
    })
}

pub(crate) async fn load_runtime_cleanup_operation(
    pool: &PgPool,
    operation_id: Uuid,
) -> Result<Option<RuntimeCleanupOperation>, sqlx::Error> {
    sqlx::query_as(
        "SELECT operation_id, request_key, target_kind, target_key,
                requested_action, state, fence, created_at, updated_at,
                completed_at, aborted_at
         FROM runtime_cleanup_operations WHERE operation_id = $1",
    )
    .bind(operation_id)
    .fetch_optional(pool)
    .await
}

async fn intents_match(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: Uuid,
    intents: &[RuntimeCleanupIntent],
) -> Result<bool, sqlx::Error> {
    let stored = sqlx::query_as::<_, (String, i16)>(
        "SELECT intent_kind, ordinal FROM runtime_cleanup_intents
         WHERE operation_id = $1 ORDER BY ordinal",
    )
    .bind(operation_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(stored.len() == intents.len()
        && stored.iter().zip(intents).all(|((kind, ordinal), intent)| {
            kind == intent.kind.as_str() && *ordinal == intent.ordinal
        }))
}

async fn lock_target(
    tx: &mut Transaction<'_, Postgres>,
    target_kind: RuntimeCleanupTargetKind,
    target_key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1 || ':' || $2, 4916))")
        .bind(target_kind.as_str())
        .bind(target_key)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn allocate_fence(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: Uuid,
) -> Result<i64, sqlx::Error> {
    let operation = sqlx::query_as::<_, OperationIdentityRow>(
        "SELECT operation_id, target_kind, target_key, requested_action
         FROM runtime_cleanup_operations WHERE operation_id = $1",
    )
    .bind(operation_id)
    .fetch_one(&mut **tx)
    .await?;

    sqlx::query_scalar(
        r#"
        INSERT INTO runtime_cleanup_fences (
            target_kind, target_key, last_fence, operation_id
        ) VALUES ($1, $2, 1, $3)
        ON CONFLICT (target_kind, target_key) DO UPDATE
        SET last_fence = runtime_cleanup_fences.last_fence + 1,
            operation_id = EXCLUDED.operation_id,
            updated_at = NOW()
        RETURNING last_fence
        "#,
    )
    .bind(operation.target_kind)
    .bind(operation.target_key)
    .bind(operation_id)
    .fetch_one(&mut **tx)
    .await
}

#[cfg(test)]
mod postgres_tests;
