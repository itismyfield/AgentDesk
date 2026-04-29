/// POST /api/auto-queue/dispatch
/// Declaratively generate and optionally activate an auto-queue run.
pub async fn dispatch(
    State(state): State<AppState>,
    Json(body): Json<DispatchBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body
        .deploy_phases
        .as_ref()
        .is_some_and(|phases| !phases.is_empty())
        && !deploy_phase_api_enabled(&state)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "deploy_phases requires server.auth_token to be configured"
            })),
        );
    }

    let force = body.force.unwrap_or(false);
    let review_mode = match normalize_auto_queue_review_mode(body.review_mode.as_deref()) {
        Ok(mode) => mode,
        Err(err) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": err }))),
    };
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    let requested_entries = match normalize_dispatch_entries(&body) {
        Ok(entries) => entries,
        Err(err) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": err })));
        }
    };
    let issue_numbers: Vec<i64> = requested_entries
        .iter()
        .map(|entry| entry.issue_number)
        .collect();
    let auto_assign_agent = body.auto_assign_agent.unwrap_or(body.agent_id.is_some());

    let cards_by_issue =
        {
            let mut cards =
                match resolve_dispatch_cards_with_pg(pool, body.repo.as_deref(), &issue_numbers)
                    .await
                {
                    Ok(cards) => cards,
                    Err(err) => {
                        return (StatusCode::BAD_REQUEST, Json(json!({ "error": err })));
                    }
                };

            if let Err(err) = apply_dispatch_agent_assignments_with_pg(
                pool,
                &mut cards,
                body.agent_id.as_deref(),
                auto_assign_agent,
            )
            .await
            {
                return (StatusCode::BAD_REQUEST, Json(json!({ "error": err })));
            }

            if let Err(err) = validate_dispatchable_cards_with_pg(pool, &cards).await {
                return (StatusCode::BAD_REQUEST, Json(json!({ "error": err })));
            }

            let conflicting_live_runs = match find_matching_active_run_id_pg(
                pool,
                body.repo.as_deref(),
                body.agent_id.as_deref(),
            )
            .await
            {
                Ok(runs) => runs,
                Err(err) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": err})),
                    );
                }
            };
            if let Some((run_id, status)) = conflicting_live_runs.first() {
                if !force {
                    return existing_live_run_conflict_response(run_id, status);
                }
                let target_run_ids: Vec<String> = conflicting_live_runs
                    .iter()
                    .map(|(run_id, _)| run_id.clone())
                    .collect();
                if let Err(err) = cancel_selected_runs_with_pg(
                    state.health_registry.clone(),
                    pool,
                    &target_run_ids,
                    "auto_queue_force_new_run",
                )
                .await
                {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": err})),
                    );
                }
            }

            cards
        };

    let distinct_groups = requested_entries
        .iter()
        .filter_map(|entry| entry.thread_group)
        .collect::<HashSet<_>>()
        .len()
        .max(1) as i64;
    let generate_body = GenerateBody {
        repo: body.repo.clone(),
        agent_id: body.agent_id.clone(),
        issue_numbers: None,
        entries: Some(requested_entries.clone()),
        review_mode: Some(review_mode.to_string()),
        mode: None,
        unified_thread: body.unified_thread,
        parallel: None,
        max_concurrent_threads: Some(
            body.max_concurrent_threads
                .unwrap_or(distinct_groups)
                .clamp(1, 10),
        ),
        force: Some(false),
        max_concurrent_per_agent: None,
    };

    let (generate_status, generated_body) =
        generate(State(state.clone()), Json(generate_body)).await;
    if generate_status != StatusCode::OK {
        return (generate_status, generated_body);
    }

    let run_id = match generated_body
        .0
        .get("run")
        .and_then(|run| run.get("id"))
        .and_then(Value::as_str)
    {
        Some(run_id) => run_id.to_string(),
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "dispatch generation did not produce a run"})),
            );
        }
    };

    if let Some(ref deploy_phases) = body.deploy_phases {
        if !deploy_phases.is_empty()
            && let Ok(json_str) = serde_json::to_string(deploy_phases)
        {
            let _ = sqlx::query("UPDATE auto_queue_runs SET deploy_phases = $1 WHERE id = $2")
                .bind(&json_str)
                .bind(&run_id)
                .execute(pool)
                .await;
        }
    }

    let mut rank_per_group = HashMap::<i64, i64>::new();
    for entry in &requested_entries {
        let thread_group = entry.thread_group.unwrap_or(0);
        let priority_rank = rank_per_group.entry(thread_group).or_insert(0);
        let Some(card) = cards_by_issue.get(&entry.issue_number) else {
            continue;
        };
        if let Err(err) = sqlx::query(
            "UPDATE auto_queue_entries
             SET thread_group = $1,
                 priority_rank = $2
             WHERE run_id = $3
               AND kanban_card_id = $4",
        )
        .bind(thread_group)
        .bind(*priority_rank)
        .bind(&run_id)
        .bind(&card.card_id)
        .execute(pool)
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{err}")})),
            );
        }
        *priority_rank += 1;
    }

    let activate_now = body.activate.unwrap_or(true);
    let activation = if activate_now {
        let (activate_status, activate_body) = activate(
            State(state.clone()),
            Json(ActivateBody {
                run_id: Some(run_id.clone()),
                repo: body.repo.clone(),
                agent_id: body.agent_id.clone(),
                thread_group: None,
                unified_thread: body.unified_thread,
                active_only: Some(false),
            }),
        )
        .await;
        if activate_status != StatusCode::OK {
            return (activate_status, activate_body);
        }
        Some(activate_body.0)
    } else {
        None
    };

    let mut snapshot = if let Some(pool) = state.pg_pool_ref() {
        state
            .auto_queue_service()
            .status_json_for_run_with_pg(
                pool,
                &run_id,
                crate::services::auto_queue::StatusInput {
                    repo: body.repo.clone(),
                    agent_id: body.agent_id.clone(),
                    guild_id: None,
                },
            )
            .await
            .unwrap_or_else(|_| {
                json!({
                    "run": null,
                    "entries": [],
                    "agents": {},
                    "thread_groups": {},
                })
            })
    } else {
        state
            .auto_queue_service()
            .status_json_for_run(
                &run_id,
                crate::services::auto_queue::StatusInput {
                    repo: body.repo.clone(),
                    agent_id: body.agent_id.clone(),
                    guild_id: None,
                },
            )
            .unwrap_or_else(|_| {
                json!({
                    "run": null,
                    "entries": [],
                    "agents": {},
                    "thread_groups": {},
                })
            })
    };
    if let Some(obj) = snapshot.as_object_mut() {
        obj.insert("activated".to_string(), json!(activate_now));
        obj.insert(
            "requested".to_string(),
            json!({
                "groups": body.groups.len(),
                "issues": issue_numbers,
                "auto_assign_agent": auto_assign_agent,
            }),
        );
        if let Some(activation) = activation {
            obj.insert("dispatch".to_string(), activation);
        }
    }

    (StatusCode::OK, Json(snapshot))
}

