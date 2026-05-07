use sqlx::{PgPool, Row as SqlxRow};

use super::entries::{ENTRY_STATUS_DONE, ENTRY_STATUS_USER_CANCELLED};
use super::slots::release_run_slots_on_pg_tx;

async fn queue_run_completion_notify_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<(), String> {
    let row = sqlx::query("SELECT repo, agent_id FROM auto_queue_runs WHERE id = $1")
        .bind(run_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| format!("load completion notify targets for run {run_id}: {error}"))?;
    let repo: Option<String> = row
        .try_get("repo")
        .map_err(|error| format!("decode completion notify repo for run {run_id}: {error}"))?;
    let agent_id: Option<String> = row
        .try_get("agent_id")
        .map_err(|error| format!("decode completion notify agent_id for run {run_id}: {error}"))?;
    let targets = completion_notify_targets_on_pg(tx, run_id, agent_id.as_deref()).await?;
    if targets.is_empty() {
        return Ok(());
    }

    let entry_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| format!("count auto-queue entries for run {run_id}: {error}"))?;
    let repo_label = repo
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("(global)");
    let short_run_id = &run_id[..8.min(run_id.len())];
    let content = format!("자동큐 완료: {repo_label} / run {short_run_id} / {entry_count}개");

    for channel_id in targets {
        sqlx::query(
            "INSERT INTO message_outbox (target, content, bot, source)
             VALUES ($1, $2, 'notify', 'system')",
        )
        .bind(format!("channel:{channel_id}"))
        .bind(&content)
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            format!(
                "queue auto-queue completion notify for run {run_id} channel {channel_id}: {error}"
            )
        })?;
    }

    Ok(())
}

async fn completion_notify_targets_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    run_agent_id: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut targets = Vec::new();

    if let Some(agent_id) = run_agent_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let channel_id = sqlx::query("SELECT discord_channel_id FROM agents WHERE id = $1")
            .bind(agent_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                format!("load completion notify agent channel for run {run_id}: {error}")
            })?
            .map(|row| {
                row.try_get::<Option<String>, _>("discord_channel_id")
                    .map_err(|error| {
                        format!("decode completion notify agent channel for run {run_id}: {error}")
                    })
            })
            .transpose()?
            .flatten();
        if let Some(channel_id) = channel_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            targets.push(channel_id);
        }
    }

    if targets.is_empty() {
        let rows = sqlx::query(
            "SELECT DISTINCT a.discord_channel_id
             FROM auto_queue_entries e
             JOIN agents a ON a.id = e.agent_id
             WHERE e.run_id = $1
               AND a.discord_channel_id IS NOT NULL
               AND TRIM(a.discord_channel_id) != ''",
        )
        .bind(run_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| {
            format!("load completion notify fallback channels for run {run_id}: {error}")
        })?;
        for row in rows {
            let channel_id: String = row.try_get("discord_channel_id").map_err(|error| {
                format!("decode completion notify fallback channel for run {run_id}: {error}")
            })?;
            targets.push(channel_id);
        }
    }

    targets.sort();
    targets.dedup();
    Ok(targets)
}

pub(super) async fn maybe_finalize_run_after_terminal_entry_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    new_status: &str,
) -> Result<bool, String> {
    if new_status == ENTRY_STATUS_DONE {
        return Ok(false);
    }
    // #815 P1: never finalize on `user_cancelled` — it must leave the run in a
    // resumable state so the operator can flip the entry back to `pending`.
    if new_status == ENTRY_STATUS_USER_CANCELLED {
        return Ok(false);
    }

    maybe_finalize_run_if_ready_pg(tx, run_id).await
}

pub(crate) async fn maybe_finalize_run_if_ready_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<bool, String> {
    if super::phase_gates::run_has_blocking_phase_gate_on_pg_tx(tx, run_id).await? {
        return Ok(false);
    }

    let remaining = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("count remaining auto-queue entries for run {run_id}: {error}"))?;
    if remaining > 0 {
        return Ok(false);
    }

    release_run_slots_on_pg_tx(tx, run_id)
        .await
        .map_err(|error| format!("release auto-queue slots for run {run_id}: {error}"))?;

    let updated = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'completed',
             completed_at = NOW()
         WHERE id = $1
           AND status IN ('active', 'paused', 'generated', 'pending')",
    )
    .bind(run_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("complete auto-queue run {run_id}: {error}"))?
    .rows_affected();
    if updated == 0 {
        return Ok(false);
    }

    queue_run_completion_notify_on_pg(tx, run_id).await?;
    Ok(true)
}

pub(super) async fn auto_queue_run_review_disabled_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<bool, String> {
    let review_mode = sqlx::query_scalar::<_, Option<String>>(
        "SELECT review_mode FROM auto_queue_runs WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("load auto-queue review mode for run {run_id}: {error}"))?
    .flatten();

    Ok(review_mode.as_deref().unwrap_or("enabled") == "disabled")
}

pub async fn pause_run_on_pg(pool: &PgPool, run_id: &str) -> Result<bool, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres pause auto-queue run {run_id}: {error}"))?;
    let updated = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'paused',
             completed_at = NULL
         WHERE id = $1
           AND status = 'active'",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("pause postgres auto-queue run {run_id}: {error}"))?
    .rows_affected();
    if updated > 0 {
        release_run_slots_on_pg_tx(&mut tx, run_id)
            .await
            .map_err(|error| {
                format!("release postgres auto-queue slots for paused run {run_id}: {error}")
            })?;
    }
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres pause auto-queue run {run_id}: {error}"))?;
    Ok(updated > 0)
}

pub async fn resume_run_on_pg(pool: &PgPool, run_id: &str) -> Result<bool, String> {
    let updated = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'active',
             completed_at = NULL
         WHERE id = $1
           AND status = 'paused'",
    )
    .bind(run_id)
    .execute(pool)
    .await
    .map_err(|error| format!("resume postgres auto-queue run {run_id}: {error}"))?
    .rows_affected();
    Ok(updated > 0)
}

pub async fn complete_run_on_pg(pool: &PgPool, run_id: &str) -> Result<bool, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres complete auto-queue run {run_id}: {error}"))?;
    let updated = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'completed',
             completed_at = NOW()
         WHERE id = $1
           AND status IN ('active', 'paused', 'generated', 'pending')",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("complete postgres auto-queue run {run_id}: {error}"))?
    .rows_affected();
    if updated == 0 {
        tx.rollback().await.map_err(|error| {
            format!("rollback stale postgres complete auto-queue run {run_id}: {error}")
        })?;
        return Ok(false);
    }

    queue_run_completion_notify_on_pg(&mut tx, run_id).await?;
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres complete auto-queue run {run_id}: {error}"))?;
    Ok(true)
}
