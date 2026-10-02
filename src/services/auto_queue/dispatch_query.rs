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

/// Runs that block a new generate in the same scope. A run not started yet
/// counts too, so a repeated generate cannot leave a second unstarted queue.
pub(super) async fn find_matching_active_run_id_pg<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    repo: Option<&str>,
    agent_id: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    let rows = sqlx::query(
        "SELECT id, status
         FROM auto_queue_runs
         WHERE status IN ('generated', 'pending', 'active', 'paused')
           AND ($1::TEXT IS NULL OR repo = $1 OR repo IS NULL OR repo = '')
           AND ($2::TEXT IS NULL OR agent_id = $2 OR agent_id IS NULL OR agent_id = '')
         ORDER BY created_at DESC, id DESC",
    )
    .bind(repo.map(str::trim).filter(|value| !value.is_empty()))
    .bind(agent_id.map(str::trim).filter(|value| !value.is_empty()))
    .fetch_all(executor)
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

/// Generate and campaign handoff create runs under this one lock, held until commit.
/// It is global because a run with no repo or agent conflicts with every scope.
pub(super) async fn lock_run_creation_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), String> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('aq_run_create'))")
        .execute(&mut **tx)
        .await
        .map_err(|err| format!("lock auto-queue run creation: {err}"))?;
    Ok(())
}

#[cfg(test)]
mod generate_conflict_tests {
    use super::*;

    #[tokio::test]
    async fn an_unstarted_run_blocks_a_second_generate_in_its_scope_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-generated', 'r/one', 'agent-a', 'generated'),
                    ('run-done', 'r/one', 'agent-a', 'completed'),
                    ('run-other', 'r/two', 'agent-a', 'generated')",
        )
        .execute(&pool)
        .await
        .expect("seed runs"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        let found = find_matching_active_run_id_pg(&pool, Some("r/one"), Some("agent-a"))
            .await
            .expect("query conflicts"); // agentdesk-audit: allow-unwrap — test assertion
        assert_eq!(
            found,
            vec![("run-generated".to_string(), "generated".to_string())]
        );
    }
}
