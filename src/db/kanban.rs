use rusqlite::{Connection, Row, types::ToSql};

#[derive(Debug, Clone, Default)]
pub struct ListCardsFilter {
    pub status: Option<String>,
    pub repo_id: Option<String>,
    pub assigned_agent_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct KanbanCardRecord {
    pub id: String,
    pub repo_id: Option<String>,
    pub title: String,
    pub status: String,
    pub priority: String,
    pub assigned_agent_id: Option<String>,
    pub github_issue_url: Option<String>,
    pub github_issue_number: Option<i64>,
    pub latest_dispatch_id: Option<String>,
    pub review_round: i64,
    pub metadata_raw: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub description: Option<String>,
    pub blocked_reason: Option<String>,
    pub review_notes: Option<String>,
    pub review_status: Option<String>,
    pub started_at: Option<String>,
    pub requested_at: Option<String>,
    pub completed_at: Option<String>,
    pub pipeline_stage_id: Option<String>,
    pub owner_agent_id: Option<String>,
    pub requester_agent_id: Option<String>,
    pub parent_card_id: Option<String>,
    pub sort_order: i64,
    pub depth: i64,
    pub latest_dispatch_status: Option<String>,
    pub latest_dispatch_title: Option<String>,
    pub latest_dispatch_type: Option<String>,
    pub latest_dispatch_result_summary: Option<String>,
    pub latest_dispatch_chain_depth: Option<i64>,
}

const CARD_SELECT_SQL: &str = "SELECT kc.id, kc.repo_id, kc.title, kc.status, kc.priority, kc.assigned_agent_id, \
    kc.github_issue_url, kc.github_issue_number, kc.latest_dispatch_id, kc.review_round, kc.metadata, \
    kc.created_at, kc.updated_at, \
    td.status AS d_status, td.dispatch_type AS d_type, td.title AS d_title, td.chain_depth AS d_depth, \
    td.result AS d_result, td.context AS d_context, \
    kc.description, kc.blocked_reason, kc.review_notes, kc.review_status, \
    kc.started_at, kc.requested_at, kc.completed_at, kc.pipeline_stage_id, \
    kc.owner_agent_id, kc.requester_agent_id, kc.parent_card_id, kc.sort_order, kc.depth \
    FROM kanban_cards kc LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id";

pub fn list_registered_repo_ids(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT id FROM github_repos")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect()
}

pub fn list_cards(
    conn: &Connection,
    filter: &ListCardsFilter,
    registered_repo_ids: &[String],
) -> rusqlite::Result<Vec<KanbanCardRecord>> {
    let mut sql = format!("{CARD_SELECT_SQL} WHERE 1=1");
    let mut params: Vec<Box<dyn ToSql>> = Vec::new();

    if let Some(status) = filter.status.as_ref() {
        params.push(Box::new(status.clone()));
        sql.push_str(&format!(" AND kc.status = ?{}", params.len()));
    }

    if let Some(repo_id) = filter.repo_id.as_ref() {
        params.push(Box::new(repo_id.clone()));
        sql.push_str(&format!(" AND kc.repo_id = ?{}", params.len()));
    } else if !registered_repo_ids.is_empty() {
        let start = params.len() + 1;
        let placeholders = registered_repo_ids
            .iter()
            .enumerate()
            .map(|(idx, _)| format!("?{}", start + idx))
            .collect::<Vec<_>>()
            .join(",");
        for repo_id in registered_repo_ids {
            params.push(Box::new(repo_id.clone()));
        }
        sql.push_str(&format!(" AND kc.repo_id IN ({placeholders})"));
    }

    if let Some(agent_id) = filter.assigned_agent_id.as_ref() {
        params.push(Box::new(agent_id.clone()));
        sql.push_str(&format!(" AND kc.assigned_agent_id = ?{}", params.len()));
    }

    sql.push_str(" ORDER BY kc.created_at DESC");

    let param_refs: Vec<&dyn ToSql> = params.iter().map(|value| value.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(param_refs.as_slice(), kanban_card_row_to_record)?;
    rows.collect()
}

fn kanban_card_row_to_record(row: &Row<'_>) -> rusqlite::Result<KanbanCardRecord> {
    let latest_dispatch_status = row.get::<_, Option<String>>(13).unwrap_or(None);
    let latest_dispatch_type = row.get::<_, Option<String>>(14).unwrap_or(None);
    let latest_dispatch_result_raw = row.get::<_, Option<String>>(17).unwrap_or(None);
    let latest_dispatch_context_raw = row.get::<_, Option<String>>(18).unwrap_or(None);

    Ok(KanbanCardRecord {
        id: row.get::<_, String>(0)?,
        repo_id: row.get::<_, Option<String>>(1)?,
        title: row.get::<_, String>(2)?,
        status: row.get::<_, String>(3)?,
        priority: row
            .get::<_, Option<String>>(4)?
            .unwrap_or_else(|| "medium".to_string()),
        assigned_agent_id: row.get::<_, Option<String>>(5)?,
        github_issue_url: row.get::<_, Option<String>>(6)?,
        github_issue_number: row.get::<_, Option<i64>>(7)?,
        latest_dispatch_id: row.get::<_, Option<String>>(8)?,
        review_round: row.get::<_, i64>(9).unwrap_or(0),
        metadata_raw: row.get::<_, Option<String>>(10).unwrap_or(None),
        created_at: string_or_integer_timestamp(row, 11),
        updated_at: string_or_integer_timestamp(row, 12),
        latest_dispatch_status: latest_dispatch_status.clone(),
        latest_dispatch_type: latest_dispatch_type.clone(),
        latest_dispatch_title: row.get::<_, Option<String>>(15).unwrap_or(None),
        latest_dispatch_chain_depth: row.get::<_, Option<i64>>(16).unwrap_or(None),
        latest_dispatch_result_summary: crate::dispatch::summarize_dispatch_from_text(
            latest_dispatch_type.as_deref(),
            latest_dispatch_status.as_deref(),
            latest_dispatch_result_raw.as_deref(),
            latest_dispatch_context_raw.as_deref(),
        ),
        description: row.get::<_, Option<String>>(19).unwrap_or(None),
        blocked_reason: row.get::<_, Option<String>>(20).unwrap_or(None),
        review_notes: row.get::<_, Option<String>>(21).unwrap_or(None),
        review_status: row.get::<_, Option<String>>(22).unwrap_or(None),
        started_at: row.get::<_, Option<String>>(23).unwrap_or(None),
        requested_at: row.get::<_, Option<String>>(24).unwrap_or(None),
        completed_at: row.get::<_, Option<String>>(25).unwrap_or(None),
        pipeline_stage_id: row.get::<_, Option<String>>(26).unwrap_or(None),
        owner_agent_id: row.get::<_, Option<String>>(27).unwrap_or(None),
        requester_agent_id: row.get::<_, Option<String>>(28).unwrap_or(None),
        parent_card_id: row.get::<_, Option<String>>(29).unwrap_or(None),
        sort_order: row.get::<_, i64>(30).unwrap_or(0),
        depth: row.get::<_, i64>(31).unwrap_or(0),
    })
}

fn string_or_integer_timestamp(row: &Row<'_>, index: usize) -> Option<String> {
    row.get::<_, Option<String>>(index)
        .ok()
        .flatten()
        .or_else(|| {
            row.get::<_, Option<i64>>(index)
                .ok()
                .flatten()
                .map(|value| value.to_string())
        })
}
