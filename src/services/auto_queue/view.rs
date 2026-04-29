#[derive(Debug, Serialize)]
struct AutoQueueHistoryRun {
    id: String,
    repo: Option<String>,
    agent_id: Option<String>,
    status: String,
    created_at: i64,
    completed_at: Option<i64>,
    duration_ms: i64,
    entry_count: i64,
    done_count: i64,
    skipped_count: i64,
    pending_count: i64,
    dispatched_count: i64,
    success_rate: f64,
    failure_rate: f64,
}

#[derive(Debug, Serialize)]
struct AutoQueueHistorySummary {
    total_runs: usize,
    completed_runs: usize,
    success_rate: f64,
    failure_rate: f64,
}

#[derive(Debug, Clone)]
struct GroupPlan {
    entries: Vec<PlannedEntry>,
    thread_group_count: i64,
    recommended_parallel_threads: i64,
    dependency_edges: usize,
    similarity_edges: usize,
    path_backed_card_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupKind {
    Independent,
    Similarity,
    Dependency,
    Mixed,
}

#[derive(Debug, Clone, Copy)]
struct RequestedGenerateEntry {
    issue_number: i64,
    batch_phase: i64,
    thread_group: Option<i64>,
}

#[derive(Debug, Clone)]
struct ResolvedDispatchCard {
    issue_number: i64,
    card_id: String,
    repo_id: Option<String>,
    status: String,
    assigned_agent_id: Option<String>,
}

#[derive(Debug, Clone)]
struct ActivateCardState {
    status: String,
    title: String,
    metadata: Option<String>,
    latest_dispatch_id: Option<String>,
    latest_dispatch_status: Option<String>,
    entry_status: String,
    repo_id: Option<String>,
    assigned_agent_id: Option<String>,
}

impl ActivateCardState {
    fn has_active_dispatch(&self) -> bool {
        self.latest_dispatch_id.is_some()
            && matches!(
                self.latest_dispatch_status.as_deref(),
                Some("pending") | Some("dispatched")
            )
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    fn is_terminal(&self, conn: &sqlite_test::Connection) -> bool {
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
struct RestoreEntryRecord {
    entry_id: String,
    card_id: String,
    agent_id: String,
    thread_group: i64,
}

#[derive(Debug, Default)]
struct RestoreRunCounts {
    restored_pending: usize,
    restored_done: usize,
    restored_dispatched: usize,
    rebound_slots: usize,
    created_dispatches: usize,
    unbound_dispatches: usize,
}

const RUN_STATUS_RESTORING: &str = "restoring";

#[derive(Debug, Clone)]
enum RestoreEntryDecision {
    Pending,
    Done,
    ExistingDispatch { dispatch_id: String, title: String },
    NewDispatch { title: String },
}

#[derive(Debug, Clone)]
struct RestoreDispatchCandidate {
    entry: RestoreEntryRecord,
    title: String,
}

#[derive(Debug, Default)]
struct RestoreDispatchAttemptResult {
    dispatched: bool,
    created_dispatch: bool,
    rebound_slot: bool,
    unbound_dispatch: bool,
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn load_activate_card_state(
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

async fn load_activate_card_state_pg(
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

    let latest_dispatch_id: Option<String> = row
        .try_get("latest_dispatch_id")
        .map_err(|error| format!("decode latest_dispatch_id for {card_id}: {error}"))?;
    let latest_dispatch_status = if let Some(dispatch_id) = latest_dispatch_id.as_deref() {
        sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
            .bind(dispatch_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| format!("load postgres dispatch status for {dispatch_id}: {error}"))?
    } else {
        None
    };
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

async fn resolve_activate_pipeline_pg(
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

