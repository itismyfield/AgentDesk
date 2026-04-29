async fn resolve_dispatch_cards_with_pg(
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

async fn apply_dispatch_agent_assignments_with_pg(
    pool: &sqlx::PgPool,
    cards_by_issue: &mut HashMap<i64, ResolvedDispatchCard>,
    agent_id: Option<&str>,
    auto_assign_agent: bool,
) -> Result<(), String> {
    let target_agent = agent_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    for issue_number in cards_by_issue.keys().copied().collect::<Vec<_>>() {
        let Some(card) = cards_by_issue.get_mut(&issue_number) else {
            continue;
        };
        let current_agent = card
            .assigned_agent_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        match (target_agent.as_deref(), current_agent.as_deref()) {
            (Some(target), Some(current)) if current != target => {
                let repo_hint = card
                    .repo_id
                    .as_deref()
                    .map(|repo| format!(" in repo {repo}"))
                    .unwrap_or_default();
                return Err(format!(
                    "issue #{issue_number}{repo_hint} is assigned to {current}, not {target}"
                ));
            }
            (Some(target), None) if auto_assign_agent => {
                let updated = sqlx::query(
                    "UPDATE kanban_cards
                     SET assigned_agent_id = $1,
                         updated_at = NOW()
                     WHERE id = $2
                       AND (assigned_agent_id IS NULL OR BTRIM(assigned_agent_id) = '')",
                )
                .bind(target)
                .bind(&card.card_id)
                .execute(pool)
                .await
                .map_err(|err| format!("{err}"))?;

                if updated.rows_affected() == 0 {
                    let actual = sqlx::query_scalar::<_, Option<String>>(
                        "SELECT assigned_agent_id
                         FROM kanban_cards
                         WHERE id = $1",
                    )
                    .bind(&card.card_id)
                    .fetch_optional(pool)
                    .await
                    .map_err(|err| format!("{err}"))?
                    .flatten()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty());

                    match actual.as_deref() {
                        Some(actual) if actual == target => {}
                        Some(actual) => {
                            let repo_hint = card
                                .repo_id
                                .as_deref()
                                .map(|repo| format!(" in repo {repo}"))
                                .unwrap_or_default();
                            return Err(format!(
                                "issue #{issue_number}{repo_hint} is assigned to {actual}, not {target}"
                            ));
                        }
                        None => {
                            return Err(format!(
                                "issue #{issue_number} has no assigned agent; provide auto_assign_agent=true or assign it first"
                            ));
                        }
                    }
                }

                card.assigned_agent_id = Some(target.to_string());
            }
            (Some(_), None) => {
                return Err(format!(
                    "issue #{issue_number} has no assigned agent; provide auto_assign_agent=true or assign it first"
                ));
            }
            (None, None) => {
                return Err(format!(
                    "issue #{issue_number} has no assigned agent; provide agent_id or assign it first"
                ));
            }
            _ => {}
        }
    }

    Ok(())
}

async fn validate_dispatchable_cards_with_pg(
    pool: &sqlx::PgPool,
    cards_by_issue: &HashMap<i64, ResolvedDispatchCard>,
) -> Result<(), String> {
    crate::pipeline::ensure_loaded();

    for card in cards_by_issue.values() {
        if card.status == "backlog" {
            continue;
        }

        let effective = crate::pipeline::resolve_for_card_pg(
            pool,
            card.repo_id.as_deref(),
            card.assigned_agent_id.as_deref(),
        )
        .await;
        let enqueueable_states = enqueueable_states_for(&effective);
        if enqueueable_states.iter().any(|state| state == &card.status) {
            continue;
        }

        return Err(format!(
            "issue #{} is in status '{}' and cannot be auto-queued; allowed states are backlog or {}",
            card.issue_number,
            card.status,
            enqueueable_states.join(", ")
        ));
    }

    Ok(())
}

async fn find_matching_active_run_id_pg(
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

#[derive(Debug)]
struct AddedRunEntry {
    entry_id: String,
    thread_group: i64,
    priority_rank: i64,
}

