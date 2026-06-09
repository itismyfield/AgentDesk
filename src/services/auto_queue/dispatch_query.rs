use super::*;

pub(super) async fn resolve_dispatch_cards_with_pg(
    pool: &sqlx::PgPool,
    repo: Option<&str>,
    issue_numbers: &[i64],
) -> Result<HashMap<i64, ResolvedDispatchCard>, String> {
    if issue_numbers.is_empty() {
        return Ok(HashMap::new());
    }

    let rows = sqlx::query(
        "SELECT id,
                repo_id,
                status,
                assigned_agent_id,
                github_issue_number::BIGINT AS github_issue_number
         FROM kanban_cards
         WHERE ($1::TEXT IS NULL OR repo_id = $1)
           AND github_issue_number::BIGINT = ANY($2::BIGINT[])",
    )
    .bind(repo)
    .bind(issue_numbers.to_vec())
    .fetch_all(pool)
    .await
    .map_err(|err| format!("{err}"))?;

    let mut cards_by_issue = HashMap::new();
    for row in rows {
        let card = ResolvedDispatchCard {
            card_id: row.try_get("id").map_err(|err| format!("{err}"))?,
            repo_id: row.try_get("repo_id").map_err(|err| format!("{err}"))?,
            status: row.try_get("status").map_err(|err| format!("{err}"))?,
            assigned_agent_id: row
                .try_get("assigned_agent_id")
                .map_err(|err| format!("{err}"))?,
            issue_number: row
                .try_get("github_issue_number")
                .map_err(|err| format!("{err}"))?,
        };
        if cards_by_issue
            .insert(card.issue_number, card.clone())
            .is_some()
        {
            return Err(format!(
                "multiple kanban cards matched issue #{}; specify repo to disambiguate",
                card.issue_number
            ));
        }
    }

    for issue_number in issue_numbers {
        if !cards_by_issue.contains_key(issue_number) {
            let suffix = repo
                .map(|repo| format!(" in repo {repo}"))
                .unwrap_or_default();
            return Err(format!(
                "kanban card not found for issue #{issue_number}{suffix}"
            ));
        }
    }

    Ok(cards_by_issue)
}

pub(super) async fn find_matching_active_run_id_pg(
    pool: &sqlx::PgPool,
    repo: Option<&str>,
    agent_id: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    let rows = sqlx::query(
        "SELECT id, status
         FROM auto_queue_runs
         WHERE status IN ('active', 'paused')
           AND ($1::TEXT IS NULL OR repo = $1 OR repo IS NULL OR repo = '')
           AND ($2::TEXT IS NULL OR agent_id = $2 OR agent_id IS NULL OR agent_id = '')
         ORDER BY created_at DESC, id DESC",
    )
    .bind(repo.map(str::trim).filter(|value| !value.is_empty()))
    .bind(agent_id.map(str::trim).filter(|value| !value.is_empty()))
    .fetch_all(pool)
    .await
    .map_err(|err| format!("query live runs: {err}"))?;

    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("id").map_err(|err| format!("{err}"))?,
                row.try_get("status").map_err(|err| format!("{err}"))?,
            ))
        })
        .collect()
}
