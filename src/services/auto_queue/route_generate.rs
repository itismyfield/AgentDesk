use super::*;

/// POST /api/queue/generate
///
/// Creates a queue run from ready cards, ordered by priority.
///
/// This endpoint is single-call complete. Do NOT chain /redispatch, /retry,
/// or /transition after it for the same card — that creates duplicate
/// dispatches (see #1442 incident). The response surfaces structured skip
/// breakdowns (`skipped_due_to_active_dispatch`, `skipped_due_to_dependency`,
/// `skipped_due_to_filter`) so callers can make follow-up decisions without
/// guessing.
pub async fn generate(
    State(state): State<AppState>,
    Json(body): Json<GenerateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let guild_id = state
        .config
        .onboarding
        .effective_guild_id(&state.config.discord);
    let _ignored_unified_thread = body.unified_thread.is_some();
    let force = body.force.unwrap_or(false);
    let review_mode = match normalize_auto_queue_review_mode(body.review_mode.as_deref()) {
        Ok(mode) => mode,
        Err(err) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": err }))),
    };
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    let requested_entries = match normalize_generate_entries(&body) {
        Ok(entries) => entries,
        Err(err) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": err })));
        }
    };
    let requested_issue_numbers = requested_entries
        .as_ref()
        .map(|entries| {
            entries
                .iter()
                .map(|entry| entry.issue_number)
                .collect::<Vec<_>>()
        })
        .or_else(|| body.issue_numbers.clone().filter(|nums| !nums.is_empty()));
    if body.auto_assign_agent.unwrap_or(false)
        && let (Some(agent_id), Some(issue_numbers)) = (
            body.agent_id
                .as_deref()
                .filter(|value| !value.trim().is_empty()),
            requested_issue_numbers.as_ref(),
        )
    {
        let mut cards =
            match resolve_dispatch_cards_with_pg(pool, body.repo.as_deref(), issue_numbers).await {
                Ok(cards) => cards,
                Err(error) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
            };
        if let Err(error) =
            apply_dispatch_agent_assignments_with_pg(pool, &mut cards, Some(agent_id), true).await
        {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": error })));
        }
    }
    // (index, batch_phase, thread_group)
    let requested_entry_meta: HashMap<i64, (usize, i64, Option<i64>)> = requested_entries
        .as_ref()
        .map(|entries| {
            entries
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    (
                        entry.issue_number,
                        (index, entry.batch_phase, entry.thread_group),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let mut cards: Vec<GenerateCandidate> = {
        let conflicting_live_runs = match find_matching_active_run_id_pg(
            pool,
            body.repo.as_deref(),
            body.agent_id.as_deref(),
        )
        .await
        {
            Ok(runs) => runs,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
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
            if let Err(error) = cancel_selected_runs_with_pg(
                state.health_registry.clone(),
                pool,
                &target_run_ids,
                "auto_queue_force_new_run",
            )
            .await
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        }

        match state
            .auto_queue_service()
            .prepare_generate_cards_with_pg(
                pool,
                &crate::services::auto_queue::PrepareGenerateInput {
                    repo: body.repo.clone(),
                    agent_id: body.agent_id.clone(),
                    issue_numbers: requested_issue_numbers.clone(),
                },
            )
            .await
        {
            Ok(cards) => cards
                .into_iter()
                .map(|card| GenerateCandidate {
                    card_id: card.card_id,
                    agent_id: card.agent_id,
                    priority: card.priority,
                    description: card.description,
                    metadata: card.metadata,
                    github_issue_number: card.github_issue_number,
                })
                .collect(),
            Err(error) => return error.into_json_response(),
        }
    };

    if !requested_entry_meta.is_empty() {
        cards.sort_by_key(|card| {
            card.github_issue_number
                .and_then(|issue_number| requested_entry_meta.get(&issue_number).copied())
                .map(|(index, _, _)| index)
                .unwrap_or(usize::MAX)
        });
    }

    // #1444 idempotency guard: filter out any candidate card that already has
    // a pending/dispatched dispatch. This prevents the #1442 incident pattern
    // where `/redispatch` creates a new dispatch and a follow-up
    // `/queue/generate` would silently create another. The filtered
    // cards get reported under `skipped_due_to_active_dispatch` so callers
    // see WHY their issue didn't make it into the run.
    //
    // Codex iter-1 P2: fail closed on lookup errors. If the active-dispatch
    // probe returns a SQL error, we cannot prove the card is safe to enqueue
    // — return 500 so the caller does not silently get a duplicate dispatch.
    let mut active_dispatch_skips: Vec<serde_json::Value> = Vec::new();
    {
        let mut retained = Vec::with_capacity(cards.len());
        for card in cards.into_iter() {
            match active_dispatch_id_for_card_pg(pool, &card.card_id).await {
                Ok(Some(existing_dispatch_id)) => {
                    if let Some(issue_number) = card.github_issue_number {
                        active_dispatch_skips.push(json!({
                            "issue_number": issue_number,
                            "existing_dispatch_id": existing_dispatch_id,
                        }));
                    }
                    crate::auto_queue_log!(
                        info,
                        "generate_skip_active_dispatch_pg_1444",
                        AutoQueueLogContext::new()
                            .card(card.card_id.as_str())
                            .agent(card.agent_id.as_str())
                            .dispatch(&existing_dispatch_id),
                        "⏭ GENERATE: card {} already has active dispatch {}, skipping",
                        card.card_id,
                        existing_dispatch_id
                    );
                }
                Ok(None) => retained.push(card),
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({
                            "error": format!(
                                "active-dispatch lookup failed for card {}: {error}",
                                card.card_id
                            ),
                        })),
                    );
                }
            }
        }
        cards = retained;
    }

    // #1442: capture skip-reason breakdowns for the requested issue_numbers
    // (or for everything filtered out when no explicit list was given). This
    // lets callers see why a card was excluded without chaining extra calls.
    // Codex P2: scope the lookup to the same repo/agent filters
    // `prepare_generate_cards_with_pg` applied so that an unrelated card on
    // another repo or assigned to another agent isn't surfaced as the skip
    // reason for the requested issue.
    let candidate_issue_numbers: std::collections::HashSet<i64> = cards
        .iter()
        .filter_map(|card| card.github_issue_number)
        .collect();
    let mut skip_breakdown = collect_generate_skip_breakdown(
        pool,
        requested_issue_numbers.as_deref(),
        &candidate_issue_numbers,
        body.repo.as_deref(),
        body.agent_id.as_deref(),
    )
    .await;
    // Merge in the explicit-filter skips from the #1444 guard above. Dedupe
    // on issue_number so we don't double-report (the breakdown helper only
    // surfaces issues NOT in the candidate pool — our filtered cards came
    // from the pool so they wouldn't otherwise appear).
    //
    // Codex iter-3 P3: also strip any matching entries from `filter` so the
    // response can't contradict itself by reporting the same issue under
    // both `skipped_due_to_active_dispatch` AND `skipped_due_to_filter`.
    // This happens when `kanban_cards.latest_dispatch_id` is stale or null
    // even though `task_dispatches` still has a live row — the breakdown
    // helper looks up the dispatch via the (stale) pointer and falls back
    // to a "wrong status" filter reason, while the new card-id-based probe
    // correctly identifies the live dispatch.
    {
        let already: std::collections::HashSet<i64> = skip_breakdown
            .active_dispatch
            .iter()
            .filter_map(|entry| entry.get("issue_number").and_then(|v| v.as_i64()))
            .collect();
        let active_skip_numbers: std::collections::HashSet<i64> = active_dispatch_skips
            .iter()
            .filter_map(|entry| entry.get("issue_number").and_then(|v| v.as_i64()))
            .collect();
        if !active_skip_numbers.is_empty() {
            skip_breakdown.filter.retain(|entry| {
                entry
                    .get("issue_number")
                    .and_then(|v| v.as_i64())
                    .map(|n| !active_skip_numbers.contains(&n))
                    .unwrap_or(true)
            });
        }
        for entry in active_dispatch_skips {
            let issue_number = entry.get("issue_number").and_then(|v| v.as_i64());
            if issue_number.map(|n| already.contains(&n)).unwrap_or(false) {
                continue;
            }
            skip_breakdown.active_dispatch.push(entry);
        }
    }

    if cards.is_empty() {
        let mut counts_map = serde_json::Map::new();
        if let Some(pipeline) = crate::pipeline::try_get() {
            for pipeline_state in &pipeline.states {
                if !pipeline_state.terminal {
                    let c = state
                        .auto_queue_service()
                        .count_cards_by_status_with_pg(
                            pool,
                            body.repo.as_deref(),
                            body.agent_id.as_deref(),
                            &pipeline_state.id,
                        )
                        .await
                        .unwrap_or(0);
                    counts_map.insert(pipeline_state.id.clone(), serde_json::json!(c));
                }
            }
        }
        return (
            StatusCode::OK,
            Json(json!({
                "run": null,
                "entries": [],
                "message": "No dispatchable cards found",
                "hint": "Move cards to a dispatchable state before generating a queue.",
                "counts": counts_map,
                "skipped_due_to_active_dispatch": skip_breakdown.active_dispatch,
                "skipped_due_to_dependency": Vec::<serde_json::Value>::new(),
                "skipped_due_to_filter": skip_breakdown.filter,
            })),
        );
    }

    let issue_to_idx: HashMap<i64, usize> = cards
        .iter()
        .enumerate()
        .filter_map(|(idx, card)| {
            card.github_issue_number
                .map(|issue_number| (issue_number, idx))
        })
        .collect();
    let mut filtered_cards = Vec::with_capacity(cards.len());
    let mut excluded_count = 0usize;
    let mut skipped_due_to_dependency: Vec<serde_json::Value> = Vec::new();
    let mut dependency_status_cache: HashMap<i64, Option<String>> = HashMap::new();
    for card in &cards {
        let dep_parse = extract_dependency_parse_result(card);
        crate::auto_queue_log!(
            info,
            "generate.dependency_parse",
            AutoQueueLogContext::new()
                .card(card.card_id.as_str())
                .agent(card.agent_id.as_str()),
            "issue_number={} parsed_dependencies={:?} signals={:?}",
            card.github_issue_number
                .map(|issue_number| format!("#{issue_number}"))
                .unwrap_or_else(|| "<none>".to_string()),
            dep_parse.numbers,
            dep_parse.signals
        );

        let mut unresolved_external_dependencies = Vec::new();
        for dep_num in &dep_parse.numbers {
            if issue_to_idx.contains_key(dep_num) {
                continue;
            }

            let dep_status = if let Some(status) = dependency_status_cache.get(dep_num) {
                status.clone()
            } else {
                let status = sqlx::query_scalar::<_, String>(
                    "SELECT status
                         FROM kanban_cards
                         WHERE github_issue_number::BIGINT = $1
                         ORDER BY updated_at DESC NULLS LAST, created_at DESC, id DESC
                         LIMIT 1",
                )
                .bind(*dep_num)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();
                dependency_status_cache.insert(*dep_num, status.clone());
                status
            };

            if dep_status.as_deref() != Some("done") {
                unresolved_external_dependencies.push(format!(
                    "#{dep_num}:{}",
                    dep_status.as_deref().unwrap_or("missing")
                ));
            }
        }

        if unresolved_external_dependencies.is_empty() {
            filtered_cards.push(card.clone());
        } else {
            crate::auto_queue_log!(
                info,
                "generate.exclude_unresolved_dependencies",
                AutoQueueLogContext::new()
                    .card(card.card_id.as_str())
                    .agent(card.agent_id.as_str()),
                "issue_number={} unresolved_external_dependencies={:?}",
                card.github_issue_number
                    .map(|issue_number| format!("#{issue_number}"))
                    .unwrap_or_else(|| "<none>".to_string()),
                unresolved_external_dependencies
            );
            excluded_count += 1;
            if let Some(issue_number) = card.github_issue_number {
                skipped_due_to_dependency.push(json!({
                    "issue_number": issue_number,
                    "unresolved_deps": unresolved_external_dependencies,
                }));
            }
        }
    }

    if filtered_cards.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({
                "run": null,
                "entries": [],
                "message": format!("No cards available ({}개 외부 의존성 미충족으로 제외)", excluded_count),
                "skipped_due_to_active_dispatch": skip_breakdown.active_dispatch,
                "skipped_due_to_dependency": skipped_due_to_dependency,
                "skipped_due_to_filter": skip_breakdown.filter,
            })),
        );
    }

    let plan = build_group_plan(&filtered_cards);
    let mut grouped_entries = plan.entries.clone();
    let mut thread_group_count = plan.thread_group_count.max(1);
    let mut recommended_parallel_threads = plan.recommended_parallel_threads.max(1);
    let dependency_edges = plan.dependency_edges;
    let similarity_edges = plan.similarity_edges;
    let path_backed_card_count = plan.path_backed_card_count;
    let mut max_concurrent = body
        .max_concurrent_threads
        .unwrap_or(recommended_parallel_threads)
        .clamp(1, 10)
        .min(thread_group_count.max(1));

    // Apply explicit batch_phase/thread_group overrides from API entries.
    if !requested_entry_meta.is_empty() {
        let mut has_explicit_groups = false;
        for planned in &mut grouped_entries {
            let card = &filtered_cards[planned.card_idx];
            if let Some(issue_number) = card.github_issue_number {
                if let Some(&(_, batch_phase, thread_group)) =
                    requested_entry_meta.get(&issue_number)
                {
                    planned.batch_phase = batch_phase;
                    if let Some(tg) = thread_group {
                        planned.thread_group = tg;
                        has_explicit_groups = true;
                    }
                }
            }
        }
        if has_explicit_groups {
            thread_group_count = grouped_entries
                .iter()
                .map(|e| e.thread_group)
                .collect::<std::collections::HashSet<_>>()
                .len() as i64;
            recommended_parallel_threads = thread_group_count.clamp(1, 4);
            if let Some(requested_max) = body.max_concurrent_threads {
                max_concurrent = requested_max.clamp(1, 10).min(thread_group_count.max(1));
            } else {
                max_concurrent = recommended_parallel_threads;
            }
        }
    }

    let batch_phase_count = grouped_entries
        .iter()
        .map(|entry| entry.batch_phase)
        .max()
        .unwrap_or(0)
        + 1;
    let ai_rationale = if path_backed_card_count == 0 && dependency_edges == 0 {
        format!(
            "스마트 플래너: 의존성/파일 경로 신호가 약해 {}개 독립 그룹, {}개 페이즈로 계획. {}개 카드 큐잉, 추천 병렬 {}개, 적용 {}개",
            thread_group_count,
            batch_phase_count,
            filtered_cards.len(),
            recommended_parallel_threads,
            max_concurrent
        )
    } else if path_backed_card_count == 0 {
        format!(
            "스마트 플래너: 파일 경로 신호 없이 의존성 {}건으로 {}개 그룹, {}개 페이즈 계획. {}개 카드 큐잉, {}개 외부 의존성 미충족 제외, 추천 병렬 {}개, 적용 {}개",
            dependency_edges,
            thread_group_count,
            batch_phase_count,
            filtered_cards.len(),
            excluded_count,
            recommended_parallel_threads,
            max_concurrent
        )
    } else {
        format!(
            "스마트 플래너: 파일 경로 유사도 {}건 + 의존성 {}건으로 {}개 그룹, {}개 페이즈 계획. 파일 경로 추출 카드 {}개, {}개 카드 큐잉, {}개 외부 의존성 미충족 제외, 추천 병렬 {}개, 적용 {}개",
            similarity_edges,
            dependency_edges,
            thread_group_count,
            batch_phase_count,
            path_backed_card_count,
            filtered_cards.len(),
            excluded_count,
            recommended_parallel_threads,
            max_concurrent
        )
    };

    // Create run + entries atomically so partial inserts cannot masquerade as success.
    let run_id = uuid::Uuid::new_v4().to_string();
    let ai_model_str = "smart-planner".to_string();
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("begin auto-queue generate transaction: {error}")})),
            );
        }
    };
    if let Err(error) = sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, review_mode, status, ai_model, ai_rationale, unified_thread, max_concurrent_threads, thread_group_count
         ) VALUES (
            $1, $2, $3, $4, 'generated', $5, $6, FALSE, $7, $8
         )",
    )
    .bind(&run_id)
    .bind(body.repo.as_deref())
    .bind(body.agent_id.as_deref())
    .bind(review_mode)
    .bind(&ai_model_str)
    .bind(&ai_rationale)
    .bind(max_concurrent)
    .bind(thread_group_count)
    .execute(&mut *tx)
    .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("create auto-queue run: {error}")})),
        );
    }

    let mut entry_ids = Vec::new();
    for planned in &grouped_entries {
        let card = &filtered_cards[planned.card_idx];
        let entry_id = uuid::Uuid::new_v4().to_string();
        let agent = if card.agent_id.is_empty() {
            body.agent_id.as_deref().unwrap_or("")
        } else {
            card.agent_id.as_str()
        };
        if let Err(error) = sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, priority_rank, thread_group, reason, batch_phase
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8
             )",
        )
        .bind(&entry_id)
        .bind(&run_id)
        .bind(&card.card_id)
        .bind(agent)
        .bind(planned.priority_rank)
        .bind(planned.thread_group)
        .bind(&planned.reason)
        .bind(planned.batch_phase)
        .execute(&mut *tx)
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("create auto-queue entry: {error}")})),
            );
        }
        entry_ids.push(entry_id);
    }
    if let Err(error) = tx.commit().await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("commit auto-queue generate transaction: {error}")})),
        );
    };

    let mut entries = Vec::with_capacity(entry_ids.len());
    for entry_id in &entry_ids {
        entries.push(
            state
                .auto_queue_service()
                .entry_json_with_pg(pool, entry_id, guild_id)
                .await
                .unwrap_or(serde_json::Value::Null),
        );
    }

    let run = state
        .auto_queue_service()
        .run_json_with_pg(pool, &run_id)
        .await
        .unwrap_or(serde_json::Value::Null);

    (
        StatusCode::OK,
        Json(json!({
            "run": run,
            "entries": entries,
            "skipped_due_to_active_dispatch": skip_breakdown.active_dispatch,
            "skipped_due_to_dependency": skipped_due_to_dependency,
            "skipped_due_to_filter": skip_breakdown.filter,
        })),
    )
}

/// Structured skip-reason breakdown for `/api/queue/generate` (#1442).
#[derive(Debug, Default)]
pub(crate) struct GenerateSkipBreakdown {
    pub active_dispatch: Vec<serde_json::Value>,
    pub filter: Vec<serde_json::Value>,
}

/// Classify why each requested issue_number didn't make it into the candidate
/// pool. When `requested_issue_numbers` is None we skip this work — the
/// breakdown is most useful when callers explicitly asked for specific
/// issues and need to know why something was dropped.
///
/// `repo_filter` and `agent_filter` mirror the filters that
/// `prepare_generate_cards_with_pg` applied so we don't surface an unrelated
/// card on another repo / assigned to another agent as the skip reason for
/// the requested issue (codex P2 follow-up to #1442).
pub(crate) async fn collect_generate_skip_breakdown(
    pool: &sqlx::PgPool,
    requested_issue_numbers: Option<&[i64]>,
    candidate_issue_numbers: &std::collections::HashSet<i64>,
    repo_filter: Option<&str>,
    agent_filter: Option<&str>,
) -> GenerateSkipBreakdown {
    let mut breakdown = GenerateSkipBreakdown::default();
    let Some(requested) = requested_issue_numbers else {
        return breakdown;
    };
    if requested.is_empty() {
        return breakdown;
    }
    let repo = repo_filter.filter(|value| !value.is_empty());
    let agent = agent_filter.filter(|value| !value.is_empty());
    for issue_number in requested {
        if candidate_issue_numbers.contains(issue_number) {
            continue;
        }
        // Look up the most recent matching card (within the same repo /
        // agent filter scope) to determine the actual skip reason
        // (active dispatch, wrong status, missing card).
        match sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT id, status, latest_dispatch_id
             FROM kanban_cards
             WHERE github_issue_number::BIGINT = $1
               AND ($2::TEXT IS NULL OR repo_id = $2)
               AND ($3::TEXT IS NULL OR assigned_agent_id = $3)
             ORDER BY updated_at DESC NULLS LAST, created_at DESC, id DESC
             LIMIT 1",
        )
        .bind(*issue_number)
        .bind(repo)
        .bind(agent)
        .fetch_optional(pool)
        .await
        {
            Ok(Some((_card_id, status, latest_dispatch_id))) => {
                // Check if the card has an active dispatch (status pending or
                // dispatched). This is the #1442 case — caller might assume
                // generate skipped silently and re-call /redispatch.
                let has_active_dispatch = match latest_dispatch_id.as_deref() {
                    Some(dispatch_id) => sqlx::query_scalar::<_, Option<String>>(
                        "SELECT status
                         FROM task_dispatches
                         WHERE id = $1 AND status IN ('pending', 'dispatched')",
                    )
                    .bind(dispatch_id)
                    .fetch_optional(pool)
                    .await
                    .ok()
                    .flatten()
                    .flatten()
                    .map(|_| dispatch_id.to_string()),
                    None => None,
                };
                if let Some(existing_dispatch_id) = has_active_dispatch {
                    breakdown.active_dispatch.push(json!({
                        "issue_number": issue_number,
                        "existing_dispatch_id": existing_dispatch_id,
                    }));
                } else {
                    breakdown.filter.push(json!({
                        "issue_number": issue_number,
                        "reason": format!("card status '{status}' is not enqueueable"),
                    }));
                }
            }
            Ok(None) => {
                breakdown.filter.push(json!({
                    "issue_number": issue_number,
                    "reason": "no kanban card found for this issue number",
                }));
            }
            Err(error) => {
                breakdown.filter.push(json!({
                    "issue_number": issue_number,
                    "reason": format!("lookup failed: {error}"),
                }));
            }
        }
    }
    breakdown
}

/// #1444 idempotency helper: returns the dispatch_id when the card already
/// has a pending/dispatched dispatch on `task_dispatches`, otherwise None.
/// Used by `/api/queue/generate` to silently skip cards that would
/// otherwise queue up a duplicate dispatch on top of an in-flight one.
///
/// Returns a `Result` so callers can fail closed on lookup errors (codex
/// iter-1 P2): swallowing a SQL failure here would let a card with a live
/// dispatch slip into a generated run and reintroduce the #1442 incident.
pub(crate) async fn active_dispatch_id_for_card_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched')
         ORDER BY created_at DESC
         LIMIT 1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
}
