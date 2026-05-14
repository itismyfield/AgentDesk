use super::*;

#[derive(Debug, Serialize)]
pub(super) struct AutoQueueHistoryRun {
    pub(super) id: String,
    pub(super) repo: Option<String>,
    pub(super) agent_id: Option<String>,
    pub(super) status: String,
    pub(super) timeout_minutes: i64,
    pub(super) timeout_exceeded: bool,
    pub(super) timeout_overrun_ms: i64,
    pub(super) created_at: i64,
    pub(super) completed_at: Option<i64>,
    pub(super) duration_ms: i64,
    pub(super) entry_count: i64,
    pub(super) done_count: i64,
    pub(super) skipped_count: i64,
    pub(super) pending_count: i64,
    pub(super) dispatched_count: i64,
    pub(super) success_rate: f64,
    pub(super) failure_rate: f64,
}

#[derive(Debug, Serialize)]
pub(super) struct AutoQueueHistorySummary {
    pub(super) total_runs: usize,
    pub(super) completed_runs: usize,
    pub(super) success_rate: f64,
    pub(super) failure_rate: f64,
}

#[derive(Debug, Clone)]
pub(super) struct GroupPlan {
    pub(super) entries: Vec<PlannedEntry>,
    pub(super) thread_group_count: i64,
    pub(super) recommended_parallel_threads: i64,
    pub(super) dependency_edges: usize,
    pub(super) similarity_edges: usize,
    pub(super) path_backed_card_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GroupKind {
    Independent,
    Similarity,
    Dependency,
    Mixed,
}

#[derive(Debug, Clone)]
pub(super) struct RequestedGenerateEntry {
    pub(super) issue_number: i64,
    pub(super) batch_phase: i64,
    pub(super) thread_group: Option<i64>,
    /// Validated phase-gate kind id (#2125). `None` falls back to catalog
    /// `default_kind` at status-response time.
    pub(super) phase_gate_kind: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct ResolvedDispatchCard {
    pub(super) issue_number: i64,
    pub(super) card_id: String,
    pub(super) repo_id: Option<String>,
    pub(super) status: String,
    pub(super) assigned_agent_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct ActivateCardState {
    pub(super) status: String,
    pub(super) title: String,
    pub(super) metadata: Option<String>,
    pub(super) latest_dispatch_id: Option<String>,
    pub(super) latest_dispatch_status: Option<String>,
    pub(super) entry_status: String,
    pub(super) repo_id: Option<String>,
    pub(super) assigned_agent_id: Option<String>,
}

impl ActivateCardState {
    pub(super) fn has_active_dispatch(&self) -> bool {
        self.latest_dispatch_id.is_some()
            && matches!(
                self.latest_dispatch_status.as_deref(),
                Some("pending") | Some("dispatched")
            )
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    pub(super) fn is_terminal(&self, conn: &sqlite_test::Connection) -> bool {
        crate::pipeline::ensure_loaded();
        crate::pipeline::resolve_for_card(
            conn,
            self.repo_id.as_deref(),
            self.assigned_agent_id.as_deref(),
        )
        .is_terminal(&self.status)
    }
}

#[derive(Debug, Clone)]
pub(super) struct RestoreEntryRecord {
    pub(super) entry_id: String,
    pub(super) card_id: String,
    pub(super) agent_id: String,
    pub(super) thread_group: i64,
}

#[derive(Debug, Default)]
pub(super) struct RestoreRunCounts {
    pub(super) restored_pending: usize,
    pub(super) restored_done: usize,
    pub(super) restored_dispatched: usize,
    pub(super) rebound_slots: usize,
    pub(super) created_dispatches: usize,
    pub(super) unbound_dispatches: usize,
}

pub(super) const RUN_STATUS_RESTORING: &str = "restoring";

#[derive(Debug, Clone)]
pub(super) enum RestoreEntryDecision {
    Pending,
    Done,
    ExistingDispatch { dispatch_id: String, title: String },
    NewDispatch { title: String },
}

#[derive(Debug, Clone)]
pub(super) struct RestoreDispatchCandidate {
    pub(super) entry: RestoreEntryRecord,
    pub(super) title: String,
}

#[derive(Debug, Default)]
pub(super) struct RestoreDispatchAttemptResult {
    pub(super) dispatched: bool,
    pub(super) created_dispatch: bool,
    pub(super) rebound_slot: bool,
    pub(super) unbound_dispatch: bool,
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn load_activate_card_state(
    conn: &sqlite_test::Connection,
    card_id: &str,
    entry_id: &str,
) -> sqlite_test::Result<ActivateCardState> {
    let (status, title, metadata, latest_dispatch_id, repo_id, assigned_agent_id): (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn.query_row(
        "SELECT status, title, metadata, latest_dispatch_id, repo_id, assigned_agent_id
         FROM kanban_cards
         WHERE id = ?1",
        [card_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?;
    let latest_dispatch_status = latest_dispatch_id.as_deref().and_then(|dispatch_id| {
        conn.query_row(
            "SELECT status FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok()
    });
    let entry_status = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = ?1",
            [entry_id],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "pending".to_string());

    Ok(ActivateCardState {
        status,
        title,
        metadata,
        latest_dispatch_id,
        latest_dispatch_status,
        entry_status,
        repo_id,
        assigned_agent_id,
    })
}

pub(super) async fn load_activate_card_state_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
    entry_id: &str,
) -> Result<ActivateCardState, String> {
    let row = sqlx::query(
        "SELECT status, title, metadata::TEXT AS metadata, latest_dispatch_id, repo_id, assigned_agent_id
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres card {card_id}: {error}"))?
    .ok_or_else(|| format!("postgres card {card_id} not found"))?;

    let mut latest_dispatch_id: Option<String> = row
        .try_get("latest_dispatch_id")
        .map_err(|error| format!("decode latest_dispatch_id for {card_id}: {error}"))?;
    let mut latest_dispatch_status = if let Some(dispatch_id) = latest_dispatch_id.as_deref() {
        sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
            .bind(dispatch_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| format!("load postgres dispatch status for {dispatch_id}: {error}"))?
    } else {
        None
    };
    if !matches!(
        latest_dispatch_status.as_deref(),
        Some("pending") | Some("dispatched")
    ) {
        if let Some(row) = sqlx::query(
            "SELECT td.id, td.status
             FROM sessions s
             JOIN task_dispatches td ON td.id = s.active_dispatch_id
             WHERE td.kanban_card_id = $1
               AND td.status IN ('pending', 'dispatched')
               AND COALESCE(s.status, '') NOT IN ('disconnected', 'completed', 'failed', 'cancelled')
             ORDER BY s.last_heartbeat DESC NULLS LAST, td.created_at DESC
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load postgres live session dispatch for {card_id}: {error}"))?
        {
            latest_dispatch_id = Some(row.try_get("id").map_err(|error| {
                format!("decode live session dispatch id for {card_id}: {error}")
            })?);
            latest_dispatch_status = Some(row.try_get("status").map_err(|error| {
                format!("decode live session dispatch status for {card_id}: {error}")
            })?);
        }
    }
    let entry_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_entries WHERE id = $1")
            .bind(entry_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| {
                format!("load postgres auto-queue entry status for {entry_id}: {error}")
            })?
            .unwrap_or_else(|| "pending".to_string());

    Ok(ActivateCardState {
        status: row
            .try_get("status")
            .map_err(|error| format!("decode status for {card_id}: {error}"))?,
        title: row
            .try_get("title")
            .map_err(|error| format!("decode title for {card_id}: {error}"))?,
        metadata: row
            .try_get("metadata")
            .map_err(|error| format!("decode metadata for {card_id}: {error}"))?,
        latest_dispatch_id,
        latest_dispatch_status,
        entry_status,
        repo_id: row
            .try_get("repo_id")
            .map_err(|error| format!("decode repo_id for {card_id}: {error}"))?,
        assigned_agent_id: row
            .try_get("assigned_agent_id")
            .map_err(|error| format!("decode assigned_agent_id for {card_id}: {error}"))?,
    })
}

pub(super) async fn resolve_activate_pipeline_pg(
    pool: &sqlx::PgPool,
    repo_id: Option<&str>,
    agent_id: Option<&str>,
) -> Result<crate::pipeline::PipelineConfig, String> {
    crate::pipeline::ensure_loaded();

    let repo_override = if let Some(repo_id) = repo_id {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config::text AS pipeline_config FROM github_repos WHERE id = $1",
        )
        .bind(repo_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load repo pipeline override for {repo_id}: {error}"))?
        .flatten()
        .map(|json| crate::pipeline::parse_override(&json))
        .transpose()
        .map_err(|error| format!("parse repo pipeline override for {repo_id}: {error}"))?
        .flatten()
    } else {
        None
    };

    let agent_override = if let Some(agent_id) = agent_id {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config::text AS pipeline_config FROM agents WHERE id = $1",
        )
        .bind(agent_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load agent pipeline override for {agent_id}: {error}"))?
        .flatten()
        .map(|json| crate::pipeline::parse_override(&json))
        .transpose()
        .map_err(|error| format!("parse agent pipeline override for {agent_id}: {error}"))?
        .flatten()
    } else {
        None
    };

    Ok(crate::pipeline::resolve(
        repo_override.as_ref(),
        agent_override.as_ref(),
    ))
}
