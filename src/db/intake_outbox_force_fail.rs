//! Operator force-fail and retry-as-new transition for intake outbox rows.

use super::intake_outbox::IntakeOutboxRow;
use super::intake_outbox_status::{IntakeOutboxStatus, OperatorRetryClass};
use sqlx::PgPool;

/// Reasons `force_fail_and_retry_as_new` may refuse to operate.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ForceFailError {
    #[error("intake_outbox row id={0} does not exist")]
    NotFound(i64),
    #[error(
        "intake_outbox row id={id} is in status='{status}'; force-fail is only allowed from \
         accepted/spawned/failed_post_accept/unknown (running transition 12 from any other state could \
         double-emit a Discord turn)"
    )]
    DisallowedStatus { id: i64, status: String },
    #[error(
        "intake_outbox row id={id} has no recorded provider; a retry would insert pending work \
         that no worker can claim (claim is scoped on intake_outbox.provider since #4349). This \
         row predates the provider column and its forwarding bot is unknowable — set \
         intake_outbox.provider explicitly before retrying, or leave it terminal"
    )]
    UnknownProvider { id: i64 },
    #[error("postgres error: {0}")]
    Db(#[from] sqlx::Error),
}

pub(crate) fn force_fail_provider_ready(provider: &str) -> bool {
    !provider.trim().is_empty()
}

/// Atomically force-terminates an eligible row and inserts its next attempt.
///
/// VALIDATES:
/// - Locks the source row and rejects missing, disallowed-status, and
///   empty-provider rows before either write.
/// - Moves `accepted`/`spawned` to `failed_post_accept`, while preserving the
///   status and existing error on already-terminal `failed_post_accept` and
///   official `unknown` rows.
/// - Terminalizes before inserting the child, preserving the one-open-route
///   unique invariant; both writes are atomic and conflicts roll both back.
/// - Copies payload and provider, allocates the family maximum plus one, and
///   links the child to the source.
///
/// SQLx decodes the locked row into the strong status enum before any write.
/// Unknown database spellings therefore return a column-decode error without
/// an UPDATE or child INSERT. Direct SQL can still bypass typed writers, and
/// there is no attempt ceiling or typestate proof for every transition.
/// Pinned PG coverage remains in `intake_outbox::postgres_tests` to keep IDs.
pub(crate) async fn force_fail_and_retry_as_new(
    pool: &PgPool,
    stuck_id: i64,
    operator_reason: &str,
) -> Result<i64, ForceFailError> {
    let mut tx = pool.begin().await?;

    let row: Option<IntakeOutboxRow> =
        sqlx::query_as("SELECT * FROM intake_outbox WHERE id = $1 FOR UPDATE")
            .bind(stuck_id)
            .fetch_optional(&mut *tx)
            .await?;
    let row = row.ok_or(ForceFailError::NotFound(stuck_id))?;

    let force_terminate = match row.status.operator_retry() {
        OperatorRetryClass::ForceTerminate => true,
        OperatorRetryClass::AlreadyTerminal => false,
        OperatorRetryClass::Refuse => {
            return Err(ForceFailError::DisallowedStatus {
                id: stuck_id,
                status: row.status.to_string(),
            });
        }
    };
    if !force_fail_provider_ready(&row.provider) {
        return Err(ForceFailError::UnknownProvider { id: stuck_id });
    }

    if force_terminate {
        sqlx::query(
            "UPDATE intake_outbox
             SET status = $3,
                 completed_at = COALESCE(completed_at, NOW()),
                 last_error = $2
             WHERE id = $1",
        )
        .bind(stuck_id)
        .bind(format!(
            "operator force-fail (was: {}); reason: {}",
            row.status, operator_reason
        ))
        .bind(IntakeOutboxStatus::FailedPostAccept)
        .execute(&mut *tx)
        .await?;
    }

    let next_attempt: Option<i32> = sqlx::query_scalar(
        "SELECT MAX(attempt_no) FROM intake_outbox
         WHERE channel_id = $1 AND user_msg_id = $2",
    )
    .bind(&row.channel_id)
    .bind(&row.user_msg_id)
    .fetch_one(&mut *tx)
    .await?;
    let next_attempt = next_attempt.unwrap_or(0) + 1;

    let new_id: i64 = sqlx::query_scalar(
        r#"
        INSERT INTO intake_outbox (
            target_instance_id, forwarded_by_instance_id, required_labels,
            channel_id, user_msg_id, request_owner_id, request_owner_name,
            user_text, reply_context, has_reply_boundary, dm_hint, turn_kind,
            merge_consecutive, reply_to_user_message, defer_watcher_resume,
            wait_for_completion, preserve_on_cancel, agent_id, provider,
            status, attempt_no, parent_outbox_id
        ) VALUES (
            $1, $2, $3,
            $4, $5, $6, $7,
            $8, $9, $10, $11, $12,
            $13, $14, $15,
            $16, $17, $18, $19,
            $20, $21, $22
        )
        RETURNING id
        "#,
    )
    .bind(&row.target_instance_id)
    .bind(&row.forwarded_by_instance_id)
    .bind(&row.required_labels)
    .bind(&row.channel_id)
    .bind(&row.user_msg_id)
    .bind(&row.request_owner_id)
    .bind(row.request_owner_name.as_deref())
    .bind(&row.user_text)
    .bind(row.reply_context.as_deref())
    .bind(row.has_reply_boundary)
    .bind(row.dm_hint)
    .bind(&row.turn_kind)
    .bind(row.merge_consecutive)
    .bind(row.reply_to_user_message)
    .bind(row.defer_watcher_resume)
    .bind(row.wait_for_completion)
    .bind(row.preserve_on_cancel)
    .bind(&row.agent_id)
    .bind(&row.provider)
    .bind(IntakeOutboxStatus::Pending)
    .bind(next_attempt)
    .bind(stuck_id)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(new_id)
}
