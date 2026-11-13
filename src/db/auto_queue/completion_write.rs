//! Run-completion notice SQL on the completing transaction: the facts read after the
//! completing UPDATE and slot release, and the tx borrow that keeps the notice on that tx.

use sqlx::Row as SqlxRow;

use super::runs::RunCompletionOutcome;

/// Result of the DB half of one completion attempt.
pub(crate) enum RunCompletionPrepared<'borrow, 'conn> {
    /// Nothing was written; the outcome is final.
    Unchanged(RunCompletionOutcome),
    /// The run row is `completed` and its slots are released on the borrowed tx.
    Completed(PreparedCompletion<'borrow, 'conn>),
}

/// A run the borrowed tx just completed. Only the completion writer builds it, and the
/// notice can only be staged through this borrow; the tx owner still commits or rolls back.
pub(crate) struct PreparedCompletion<'borrow, 'conn> {
    tx: &'borrow mut sqlx::Transaction<'conn, sqlx::Postgres>,
    run_id: String,
    has_entries: bool,
}

impl<'borrow, 'conn> PreparedCompletion<'borrow, 'conn> {
    pub(super) fn new(
        tx: &'borrow mut sqlx::Transaction<'conn, sqlx::Postgres>,
        run_id: &str,
        has_entries: bool,
    ) -> Self {
        Self {
            tx,
            run_id: run_id.to_string(),
            has_entries,
        }
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }

    /// A run that never held an entry has nothing to announce.
    pub(crate) fn has_entries(&self) -> bool {
        self.has_entries
    }

    pub(crate) async fn notice_facts(
        &mut self,
    ) -> Result<Option<RunCompletionNoticeFacts>, String> {
        load_run_completion_notice_facts_on_pg_tx(self.tx, &self.run_id).await
    }

    /// The completing tx itself, for the notice INSERTs.
    pub(crate) fn transaction(&mut self) -> &mut sqlx::Transaction<'conn, sqlx::Postgres> {
        self.tx
    }
}

/// Recipients and label inputs of one run-completion notice.
pub(crate) struct RunCompletionNoticeFacts {
    pub(crate) repo: Option<String>,
    pub(crate) targets: Vec<String>,
    pub(crate) entry_count: i64,
}

/// `None` when the run has no notice channel; the entry count is then never read.
pub(crate) async fn load_run_completion_notice_facts_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<Option<RunCompletionNoticeFacts>, String> {
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
        return Ok(None);
    }

    let entry_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| format!("count auto-queue entries for run {run_id}: {error}"))?;
    Ok(Some(RunCompletionNoticeFacts {
        repo,
        targets,
        entry_count,
    }))
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
