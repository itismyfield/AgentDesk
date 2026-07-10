use std::collections::HashMap;

use sqlx::{PgPool, Postgres, Transaction};

use crate::services::discord::outbound::source_registry::{
    SendCallerClass, validate_send_source_for,
};
use crate::services::message_outbox_recovery_support::{
    FailedOutboxInspection, OutboxRow, RedriveOutcome, load_rows, outcome, semantic_key,
    semantic_siblings, snippet,
};

const RECOVERY_DEDUPE_WINDOW_SECS: i64 = 60 * 60;

#[derive(Debug, thiserror::Error)]
pub(crate) enum RecoveryError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("message_outbox row {id} source `{label}` is not registered for LoopbackInternal")]
    SourceNotAllowed { id: i64, label: String },
}

pub(crate) async fn inspect_failed_rows(
    pool: &PgPool,
    ids: &[i64],
) -> Result<Vec<FailedOutboxInspection>, RecoveryError> {
    let rows = load_rows(pool, ids, false).await?;
    let mut output = Vec::with_capacity(rows.len());
    for row in rows {
        let siblings = semantic_siblings(pool, &row).await?;
        output.push(FailedOutboxInspection {
            id: row.id,
            status: row.status,
            target: row.target,
            bot: row.bot,
            source: row.source,
            reason_code: row.reason_code,
            session_key: row.session_key,
            retry_count: row.retry_count,
            error_snippet: row.error.as_deref().map(|value| snippet(value, 500)),
            dedupe_key: row.dedupe_key,
            content_snippet: snippet(&row.content, 240),
            content_hash: blake3::hash(row.content.as_bytes()).to_hex().to_string(),
            created_at: row.created_at,
            sent_at: row.sent_at,
            claimed_at: row.claimed_at,
            next_attempt_at: row.next_attempt_at,
            semantic_siblings: siblings,
        });
    }
    Ok(output)
}

pub(crate) async fn redrive_failed_rows(
    pool: &PgPool,
    ids: &[i64],
    idempotency_key: &str,
    reason: &str,
    dry_run: bool,
) -> Result<Vec<RedriveOutcome>, RecoveryError> {
    let mut tx = pool.begin().await?;
    let rows = load_rows(&mut *tx, ids, !dry_run).await?;
    for row in &rows {
        validate_send_source_for(&row.source, SendCallerClass::LoopbackInternal).map_err(|_| {
            RecoveryError::SourceNotAllowed {
                id: row.id,
                label: row.source.clone(),
            }
        })?;
    }
    let by_id: HashMap<i64, &OutboxRow> = rows.iter().map(|row| (row.id, row)).collect();
    let mut canonical: HashMap<String, i64> = HashMap::new();
    for row in rows.iter().filter(|row| row.status == "failed") {
        canonical
            .entry(semantic_key(row))
            .and_modify(|id| *id = (*id).min(row.id))
            .or_insert(row.id);
    }
    let mut outcomes = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(previous) = existing_audit(&mut tx, *id, idempotency_key).await? {
            outcomes.push(outcome(*id, "idempotent_replay", None, Some(previous)));
            continue;
        }
        if !dry_run && !claim_audit(&mut tx, *id, idempotency_key, reason).await? {
            let previous = existing_audit(&mut tx, *id, idempotency_key)
                .await?
                .unwrap_or_else(|| "claimed".to_string());
            outcomes.push(outcome(*id, "idempotent_replay", None, Some(previous)));
            continue;
        }
        let (name, canonical_id) = match by_id.get(id).copied() {
            None => ("not_found", None),
            Some(row) if row.status == "sent" => ("already_delivered", None),
            Some(row) if matches!(row.status.as_str(), "pending" | "processing") => {
                ("already_in_flight", None)
            }
            Some(row) if row.status != "failed" => ("not_failed", None),
            Some(row) if canonical.get(&semantic_key(row)).copied() != Some(*id) => (
                "duplicate_failed_identity",
                canonical.get(&semantic_key(row)).copied(),
            ),
            Some(row) => {
                let siblings = semantic_siblings(&mut *tx, row).await?;
                if siblings.iter().any(|item| item.status == "sent") {
                    ("already_delivered", None)
                } else if siblings
                    .iter()
                    .any(|item| matches!(item.status.as_str(), "pending" | "processing"))
                {
                    ("already_in_flight", None)
                } else if dry_run {
                    ("would_redrive", None)
                } else if reset_failed_row(&mut tx, *id).await? {
                    ("redriven", None)
                } else {
                    ("state_changed", None)
                }
            }
        };
        if !dry_run {
            finish_audit(&mut tx, *id, idempotency_key, name).await?;
        }
        outcomes.push(outcome(*id, name, canonical_id, None));
    }
    if dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(outcomes)
}

async fn existing_audit(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
    key: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT outcome FROM message_outbox_redrive_audit WHERE message_outbox_id=$1 AND idempotency_key=$2").bind(id).bind(key).fetch_optional(&mut **tx).await
}

async fn claim_audit(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
    key: &str,
    reason: &str,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, i64>("INSERT INTO message_outbox_redrive_audit(message_outbox_id,idempotency_key,reason) VALUES($1,$2,$3) ON CONFLICT(message_outbox_id,idempotency_key) DO NOTHING RETURNING id").bind(id).bind(key).bind(reason).fetch_optional(&mut **tx).await?.is_some())
}

async fn finish_audit(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
    key: &str,
    name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE message_outbox_redrive_audit SET outcome=$3,completed_at=NOW() WHERE message_outbox_id=$1 AND idempotency_key=$2").bind(id).bind(key).bind(name).execute(&mut **tx).await?;
    Ok(())
}

async fn reset_failed_row(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("UPDATE message_outbox SET status='pending',retry_count=0,next_attempt_at=NOW(),error=NULL,claimed_at=NULL,claim_owner=NULL,sent_at=NULL,dedupe_expires_at=CASE WHEN dedupe_key IS NULL THEN NULL ELSE GREATEST(COALESCE(dedupe_expires_at,NOW()),NOW()+($2::BIGINT*INTERVAL '1 second')) END WHERE id=$1 AND status='failed'").bind(id).bind(RECOVERY_DEDUPE_WINDOW_SECS).execute(&mut **tx).await?.rows_affected()==1)
}
