use super::*;

/// (thread_group, priority_rank, batch_phase) per card in order. A requested
/// `thread_group` keeps its lane; other cards get new lanes after the requested ones.
fn assign_lanes(
    issue_numbers: impl Iterator<Item = Option<i64>>,
    requested: &HashMap<i64, (usize, i64, Option<i64>, Option<String>)>,
) -> Result<Vec<(i64, i64, i64)>, String> {
    let mut next_lane = requested
        .values()
        .filter_map(|(_, _, lane, _)| *lane)
        .max()
        .map_or(Some(0), |max| max.checked_add(1));
    let mut lane_lengths: HashMap<i64, i64> = HashMap::new();
    issue_numbers
        .map(|issue_number| {
            let meta = issue_number.and_then(|number| requested.get(&number));
            let lane = match meta.and_then(|(_, _, lane, _)| *lane) {
                Some(lane) => lane,
                None => {
                    let lane = next_lane.ok_or_else(|| {
                        "thread_group is too large to number lanes for the other cards".to_string()
                    })?;
                    next_lane = lane.checked_add(1);
                    lane
                }
            };
            let rank = lane_lengths.entry(lane).or_insert(0);
            *rank += 1;
            Ok((lane, *rank - 1, meta.map_or(0, |(_, phase, _, _)| *phase)))
        })
        .collect()
}

/// Issue numbers a card's metadata names under `depends_on` / `dependencies`.
/// Issue bodies are not read: prerequisites are declared, not guessed.
fn declared_dependencies(metadata: Option<&str>, self_issue: Option<i64>) -> Vec<i64> {
    fn collect(value: &Value, out: &mut std::collections::BTreeSet<i64>) {
        match value {
            Value::Number(number) => out.extend(number.as_i64()),
            Value::String(raw) => out.extend(
                raw.split(|ch: char| ch == ',' || ch.is_whitespace())
                    .filter_map(|token| token.trim_start_matches('#').parse::<i64>().ok()),
            ),
            Value::Array(items) => items.iter().for_each(|item| collect(item, out)),
            _ => {}
        }
    }
    let Some(Value::Object(object)) = metadata.and_then(|raw| serde_json::from_str(raw).ok())
    else {
        return Vec::new();
    };
    let mut numbers = std::collections::BTreeSet::new();
    for (key, value) in &object {
        if key.eq_ignore_ascii_case("depends_on") || key.eq_ignore_ascii_case("dependencies") {
            collect(value, &mut numbers);
        }
    }
    numbers
        .into_iter()
        .filter(|number| *number > 0 && Some(*number) != self_issue)
        .collect()
}

/// POST /api/queue/generate
///
/// Creates a queue run from ready cards. Auto-queue does not plan: cards keep
/// the order given (`entries`, else priority then age), a requested
/// `thread_group` keeps its lane, and every other card gets a lane of its own.
/// A card whose declared prerequisites are not done is held back.
///
/// This endpoint is single-call complete. Do NOT chain /redispatch, /retry,
/// or /transition after it for the same card — that creates duplicate
/// dispatches (see #1442 incident). The response surfaces structured skip
/// breakdowns (`skipped_due_to_active_dispatch`, `skipped_due_to_dependency`,
/// `skipped_due_to_filter`) so
/// callers can make follow-up decisions without guessing.
pub async fn generate(
    State(state): State<AppState>,
    Json(body): Json<GenerateBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let guild_id = state
        .config
        .onboarding
        .effective_guild_id(&state.config.discord);
    let _ignored_unified_thread = body.unified_thread.is_some();
    let force = body.force.unwrap_or(false);
    let review_mode = match normalize_auto_queue_review_mode(body.review_mode.as_deref()) {
        Ok(mode) => mode,
        Err(err) => return Err(AppError::bad_request(err).with_code(ErrorCode::AutoQueue)),
    };
    // Validate the request body BEFORE the PG availability check so callers
    // get a 400 with the actual error (e.g. unknown phase_gate_kind) instead
    // of a 503 that hides the underlying input mistake (#2125).
    let requested_entries = match normalize_generate_entries(&body) {
        Ok(entries) => entries,
        Err(err) => {
            return Err(AppError::bad_request(err).with_code(ErrorCode::AutoQueue));
        }
    };
    let Some(pool) = state.pg_pool_ref() else {
        return Err(auto_queue_tuple_error(pg_unavailable_response()));
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
                Err(error) => {
                    return Err(AppError::bad_request(error).with_code(ErrorCode::AutoQueue));
                }
            };
        if let Err(error) =
            apply_dispatch_agent_assignments_with_pg(pool, &mut cards, Some(agent_id), true).await
        {
            return Err(AppError::bad_request(error).with_code(ErrorCode::AutoQueue));
        }
    }
    // (index, batch_phase, thread_group, phase_gate_kind)
    let requested_entry_meta: HashMap<i64, (usize, i64, Option<i64>, Option<String>)> =
        requested_entries
            .as_ref()
            .map(|entries| {
                entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        (
                            entry.issue_number,
                            (
                                index,
                                entry.batch_phase,
                                entry.thread_group,
                                entry.phase_gate_kind.clone(),
                            ),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
    let mut cards = {
        let conflicting_live_runs = match find_matching_active_run_id_pg(
            pool,
            body.repo.as_deref(),
            body.agent_id.as_deref(),
        )
        .await
        {
            Ok(runs) => runs,
            Err(error) => {
                return Err(AppError::internal(error).with_code(ErrorCode::AutoQueue));
            }
        };
        if let Some((run_id, status)) = conflicting_live_runs.first() {
            if !force {
                return Ok(existing_live_run_conflict_response(run_id, status));
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
                return Err(AppError::internal(error).with_code(ErrorCode::AutoQueue));
            }
        }

        state
            .auto_queue_service()
            .prepare_generate_cards_with_pg(
                pool,
                &crate::services::auto_queue::PrepareGenerateInput {
                    repo: body.repo.clone(),
                    agent_id: body.agent_id.clone(),
                    issue_numbers: requested_issue_numbers.clone(),
                },
            )
            .await?
    };

    if !requested_entry_meta.is_empty() {
        cards.sort_by_key(|card| {
            card.github_issue_number
                .and_then(|issue_number| {
                    requested_entry_meta
                        .get(&issue_number)
                        .map(|(index, _, _, _)| *index)
                })
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
                    return Err(AppError::internal(format!(
                        "active-dispatch lookup failed for card {}: {error}",
                        card.card_id
                    ))
                    .with_code(ErrorCode::AutoQueue));
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

    // A card waits until the issues its metadata names are done in its own repo;
    // a card without a repo matches none, so it waits.
    let mut dependency_skips: Vec<serde_json::Value> = Vec::new();
    {
        let mut retained = Vec::with_capacity(cards.len());
        for card in cards.into_iter() {
            let dependencies =
                declared_dependencies(card.metadata.as_deref(), card.github_issue_number);
            let statuses: HashMap<i64, Option<String>> = if dependencies.is_empty() {
                HashMap::new()
            } else {
                sqlx::query_as::<_, (i64, Option<String>)>(
                    "SELECT dep.issue, (
                         SELECT prerequisite.status FROM kanban_cards prerequisite
                          WHERE prerequisite.repo_id = card.repo_id
                            AND prerequisite.github_issue_number::BIGINT = dep.issue
                          ORDER BY prerequisite.updated_at DESC NULLS LAST,
                                   prerequisite.created_at DESC, prerequisite.id DESC
                          LIMIT 1)
                     FROM kanban_cards card CROSS JOIN UNNEST($2::BIGINT[]) AS dep(issue)
                     WHERE card.id = $1",
                )
                .bind(&card.card_id)
                .bind(&dependencies)
                .fetch_all(pool)
                .await
                .map_err(|error| {
                    AppError::internal(format!(
                        "dependency lookup failed for card {}: {error}",
                        card.card_id
                    ))
                    .with_code(ErrorCode::AutoQueue)
                })?
                .into_iter()
                .collect()
            };
            let unresolved: Vec<String> = dependencies
                .iter()
                .filter_map(|dependency| {
                    let status = statuses.get(dependency).cloned().flatten();
                    (status.as_deref() != Some("done")).then(|| {
                        format!("#{dependency}:{}", status.as_deref().unwrap_or("missing"))
                    })
                })
                .collect();
            if unresolved.is_empty() {
                retained.push(card);
            } else if let Some(issue_number) = card.github_issue_number {
                dependency_skips.push(json!({
                    "issue_number": issue_number,
                    "unresolved_deps": unresolved,
                }));
            }
        }
        cards = retained;
    }

    if cards.is_empty() {
        let statuses: Vec<String> = crate::pipeline::try_get()
            .map(|pipeline| {
                pipeline
                    .states
                    .iter()
                    .filter(|pipeline_state| !pipeline_state.terminal)
                    .map(|pipeline_state| pipeline_state.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let counts = match state
            .auto_queue_service()
            .count_cards_by_statuses_with_pg(
                pool,
                body.repo.as_deref(),
                body.agent_id.as_deref(),
                &statuses,
            )
            .await
        {
            Ok(counts) => counts,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    status_count = statuses.len(),
                    repo = ?body.repo,
                    agent_id = ?body.agent_id,
                    "failed to load empty-generate status counts"
                );
                HashMap::new()
            }
        };
        let counts_map = empty_generate_status_counts(&statuses, &counts);
        return Ok((
            StatusCode::OK,
            Json(json!({
                "run": null,
                "entries": [],
                "message": "No dispatchable cards found",
                "hint": "Move cards to a dispatchable state before generating a queue.",
                "counts": counts_map,
                "skipped_due_to_active_dispatch": skip_breakdown.active_dispatch,
                "skipped_due_to_dependency": dependency_skips,
                "skipped_due_to_filter": skip_breakdown.filter,
            })),
        ));
    }

    let planned = assign_lanes(
        cards.iter().map(|card| card.github_issue_number),
        &requested_entry_meta,
    )
    .map_err(|error| AppError::bad_request(error).with_code(ErrorCode::AutoQueue))?;
    let thread_group_count = planned
        .iter()
        .map(|(lane, _, _)| *lane)
        .collect::<HashSet<_>>()
        .len() as i64;
    let max_concurrent = body
        .max_concurrent_threads
        .unwrap_or_else(|| thread_group_count.clamp(1, 4))
        .clamp(1, 10)
        .min(thread_group_count.max(1));
    let ai_rationale = format!(
        "{}개 카드, 레인 {}개, 동시 {}개",
        cards.len(),
        thread_group_count,
        max_concurrent
    );

    // Create run + entries atomically so partial inserts cannot masquerade as success.
    let run_id = uuid::Uuid::new_v4().to_string();
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            return Err(AppError::internal(format!(
                "begin auto-queue generate transaction: {error}"
            ))
            .with_code(ErrorCode::AutoQueue));
        }
    };
    // A generate that passed the check above at the same time may have committed a run since.
    if let Err(error) = lock_run_creation_on_pg_tx(&mut tx).await {
        return Err(AppError::internal(error).with_code(ErrorCode::AutoQueue));
    }
    match find_matching_active_run_id_pg(&mut *tx, body.repo.as_deref(), body.agent_id.as_deref())
        .await
    {
        Ok(runs) => {
            if let Some((run_id, status)) = runs.first() {
                return Ok(existing_live_run_conflict_response(run_id, status));
            }
        }
        Err(error) => return Err(AppError::internal(error).with_code(ErrorCode::AutoQueue)),
    }
    if let Err(error) = sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, review_mode, status, ai_model, ai_rationale, unified_thread, max_concurrent_threads, thread_group_count
         ) VALUES (
            $1, $2, $3, $4, 'generated', 'generate', $5, FALSE, $6, $7
         )",
    )
    .bind(&run_id)
    .bind(body.repo.as_deref())
    .bind(body.agent_id.as_deref())
    .bind(review_mode)
    .bind(&ai_rationale)
    .bind(max_concurrent)
    .bind(thread_group_count)
    .execute(&mut *tx)
    .await
    {
        return Err(AppError::internal(format!("create auto-queue run: {error}")).with_code(ErrorCode::AutoQueue));
    }

    let mut entry_ids = Vec::new();
    for (card, (thread_group, priority_rank, batch_phase)) in cards.iter().zip(planned) {
        let entry_id = uuid::Uuid::new_v4().to_string();
        let agent = if card.agent_id.is_empty() {
            body.agent_id.as_deref().unwrap_or("")
        } else {
            card.agent_id.as_str()
        };
        let phase_gate_kind = card
            .github_issue_number
            .and_then(|issue_number| requested_entry_meta.get(&issue_number))
            .and_then(|(_, _, _, kind)| kind.clone());
        if let Err(error) = sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, priority_rank, thread_group, batch_phase, phase_gate_kind
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8
             )",
        )
        .bind(&entry_id)
        .bind(&run_id)
        .bind(&card.card_id)
        .bind(agent)
        .bind(priority_rank)
        .bind(thread_group)
        .bind(batch_phase)
        .bind(phase_gate_kind.as_deref())
        .execute(&mut *tx)
        .await
        {
            return Err(AppError::internal(format!("create auto-queue entry: {error}")).with_code(ErrorCode::AutoQueue));
        }
        entry_ids.push(entry_id);
    }
    if let Err(error) = tx.commit().await {
        return Err(
            AppError::internal(format!("commit auto-queue generate transaction: {error}"))
                .with_code(ErrorCode::AutoQueue),
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

    Ok((
        StatusCode::OK,
        Json(json!({
            "run": run,
            "entries": entries,
            "skipped_due_to_active_dispatch": skip_breakdown.active_dispatch,
            "skipped_due_to_dependency": dependency_skips,
            "skipped_due_to_filter": skip_breakdown.filter,
        })),
    ))
}

fn empty_generate_status_counts(
    statuses: &[String],
    counts: &HashMap<String, i64>,
) -> serde_json::Map<String, serde_json::Value> {
    statuses
        .iter()
        .map(|status| {
            (
                status.clone(),
                serde_json::json!(counts.get(status).copied().unwrap_or(0)),
            )
        })
        .collect()
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

#[cfg(test)]
mod lane_assignment_tests {
    use super::*;

    #[test]
    fn requested_lanes_are_kept_and_other_cards_get_their_own() {
        let requested = HashMap::from([
            (1, (0, 0, Some(2), None)),
            (2, (1, 1, Some(2), None)),
            (3, (2, 1, None, None)),
        ]);
        let lanes = assign_lanes([Some(1), Some(2), Some(3), None].into_iter(), &requested);
        assert_eq!(lanes, Ok(vec![(2, 0, 0), (2, 1, 1), (3, 0, 1), (4, 0, 0)]));
    }

    #[test]
    fn the_largest_lane_is_kept_and_only_extra_lanes_overflow() {
        let requested = HashMap::from([(1, (0, 0, Some(i64::MAX), None))]);
        assert_eq!(
            assign_lanes([Some(1)].into_iter(), &requested),
            Ok(vec![(i64::MAX, 0, 0)])
        );
        assert!(assign_lanes([Some(1), Some(2)].into_iter(), &requested).is_err());
    }

    #[test]
    fn dependencies_come_from_metadata_only() {
        let metadata = r##"{"depends_on":[7, "#42", "8, #9"],"Dependencies":7,"labels":"#5"}"##;
        assert_eq!(
            declared_dependencies(Some(metadata), Some(9)),
            vec![7, 8, 42]
        );
        assert!(declared_dependencies(Some("depends on #7"), None).is_empty());
        assert!(declared_dependencies(None, None).is_empty());
    }
}

#[cfg(test)]
mod empty_generate_status_count_tests {
    use super::empty_generate_status_counts;
    use std::collections::HashMap;

    #[test]
    fn requested_states_keep_zero_counts_and_ignore_unrequested_rows() {
        let statuses = vec!["ready".to_string(), "review".to_string()];
        let counts = HashMap::from([("ready".to_string(), 3), ("done".to_string(), 9)]);

        let result = empty_generate_status_counts(&statuses, &counts);

        assert_eq!(
            serde_json::Value::Object(result),
            serde_json::json!({ "ready": 3, "review": 0 })
        );
    }
}

#[cfg(test)]
mod deploy_gate_request_rejection_tests {
    use super::*;

    fn body(kind: &str) -> GenerateBody {
        GenerateBody {
            repo: Some("itismyfield/AgentDesk".to_string()),
            agent_id: Some("project-agentdesk".to_string()),
            auto_assign_agent: None,
            issue_numbers: None,
            entries: Some(vec![GenerateEntryBody {
                issue_number: 4898,
                batch_phase: Some(0),
                thread_group: Some(1),
                phase_gate_kind: Some(kind.to_string()),
            }]),
            review_mode: None,
            mode: None,
            unified_thread: None,
            parallel: None,
            max_concurrent_threads: None,
            force: None,
            max_concurrent_per_agent: None,
        }
    }

    pub(super) fn state_with_postgres(pg_pool: Option<sqlx::PgPool>) -> AppState {
        let config = crate::config::Config::default();
        let broadcast_tx = crate::eventbus::new_broadcast();
        let batch_buffer = crate::eventbus::spawn_batch_flusher(broadcast_tx.clone());
        AppState {
            pg_pool,
            engine: crate::engine::PolicyEngine::new(&config)
                .expect("construct policy engine for validation test"), // agentdesk-audit: allow-unwrap — test fixture construction
            config: Arc::new(config),
            broadcast_tx,
            batch_buffer,
            health_registry: None,
            cluster_instance_id: None,
        }
    }

    #[tokio::test]
    async fn unavailable_deploy_gate_is_a_typed_client_error_before_database_access() {
        let error = generate(State(state_with_postgres(None)), Json(body("deploy-gate")))
            .await
            .expect_err("unavailable deploy-gate must be rejected");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error.code(), ErrorCode::AutoQueue);
        assert_eq!(
            error.message(),
            crate::phase_gate::DEPLOY_GATE_UNAVAILABLE_REASON
        );
    }

    #[cfg(test)]
    mod postgres_tests {
        use super::*;

        #[tokio::test]
        async fn unavailable_deploy_gate_creates_no_database_rows() {
            let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = pg_db.connect_and_migrate().await;
            let runs_before =
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM auto_queue_runs")
                    .fetch_one(&pool)
                    .await
                    .expect("count auto-queue runs before request"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
            let entries_before =
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM auto_queue_entries")
                    .fetch_one(&pool)
                    .await
                    .expect("count auto-queue entries before request"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

            let error = generate(
                State(state_with_postgres(Some(pool.clone()))),
                Json(body("deploy-gate")),
            )
            .await
            .expect_err("unavailable deploy-gate must be rejected");
            assert_eq!(error.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM auto_queue_runs")
                    .fetch_one(&pool)
                    .await
                    .expect("count auto-queue runs after request"), // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
                runs_before
            );
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM auto_queue_entries")
                    .fetch_one(&pool)
                    .await
                    .expect("count auto-queue entries after request"), // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
                entries_before
            );

            pool.close().await;
            pg_db.drop().await;
        }
    }
}

#[cfg(test)]
mod dependency_hold_tests {
    use super::*;

    async fn generate_json(pool: &sqlx::PgPool, body: serde_json::Value) -> serde_json::Value {
        let body: GenerateBody = serde_json::from_value(body).expect("generate body"); // agentdesk-audit: allow-unwrap — static test fixture
        let state =
            super::deploy_gate_request_rejection_tests::state_with_postgres(Some(pool.clone()));
        let (_, Json(response)) = generate(State(state), Json(body))
            .await
            .expect("generate succeeds"); // agentdesk-audit: allow-unwrap — test assertion
        response
    }

    async fn queued_cards(pool: &sqlx::PgPool, response: &serde_json::Value) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT kanban_card_id FROM auto_queue_entries WHERE run_id = $1 ORDER BY kanban_card_id",
        )
        .bind(response["run"]["id"].as_str())
        .fetch_all(pool)
        .await
        .expect("list entries") // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    }

    #[tokio::test]
    async fn a_card_waits_until_its_declared_prerequisite_is_done_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query(
            "INSERT INTO agents (id, name, provider, status) VALUES ('agent-dep', 'Dep', 'codex', 'idle')",
        )
        .execute(&pool)
        .await
        .expect("seed agent"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        // other/repo#100 is done and newer, but #101 depends on dep/repo#100.
        sqlx::query(
            "INSERT INTO kanban_cards (id, repo_id, title, status, assigned_agent_id, github_issue_number, metadata, updated_at)
             VALUES ('card-100', 'dep/repo', 'Prerequisite', 'in_progress', 'agent-dep', 100, NULL, NOW() - INTERVAL '1 hour'),
                    ('card-b100', 'other/repo', 'Same number', 'done', 'agent-dep', 100, NULL, NOW()),
                    ('card-101', 'dep/repo', 'Dependent', 'ready', 'agent-dep', 101, '{\"depends_on\":[100]}'::jsonb, NOW()),
                    ('card-102', 'dep/repo', 'Free', 'ready', 'agent-dep', 102, NULL, NOW()),
                    ('card-103', 'dep/repo', 'After free', 'ready', 'agent-dep', 103, '{\"depends_on\":[102]}'::jsonb, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed cards"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        // No repo, issues named directly, prerequisite #102 in the same request.
        let response = generate_json(
            &pool,
            json!({ "agent_id": "agent-dep", "issue_numbers": [101, 102, 103] }),
        )
        .await;
        let mut skipped = response["skipped_due_to_dependency"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        skipped.sort_by_key(|entry| entry["issue_number"].as_i64());
        assert_eq!(
            skipped,
            vec![
                json!({ "issue_number": 101, "unresolved_deps": ["#100:in_progress"] }),
                json!({ "issue_number": 103, "unresolved_deps": ["#102:ready"] }),
            ]
        );
        assert_eq!(
            queued_cards(&pool, &response).await,
            vec!["card-102".to_string()]
        );

        sqlx::query("UPDATE kanban_cards SET status = 'done' WHERE id = 'card-100'")
            .execute(&pool)
            .await
            .expect("finish prerequisite"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        let response = generate_json(
            &pool,
            json!({ "agent_id": "agent-dep", "issue_numbers": [101], "force": true }),
        )
        .await;
        assert_eq!(response["skipped_due_to_dependency"], json!([]));
        assert_eq!(
            queued_cards(&pool, &response).await,
            vec!["card-101".to_string()]
        );
        pool.close().await;
        pg_db.drop().await;
    }
}

#[cfg(test)]
mod concurrent_generate_tests {
    use super::*;

    fn spawn_generate(pool: &sqlx::PgPool) -> tokio::task::JoinHandle<u16> {
        let state =
            super::deploy_gate_request_rejection_tests::state_with_postgres(Some(pool.clone()));
        tokio::spawn(async move {
            let body: GenerateBody =
                serde_json::from_value(json!({ "repo": "par/repo", "agent_id": "agent-par" }))
                    .expect("generate body"); // agentdesk-audit: allow-unwrap — static test fixture
            let (status, _) = generate(State(state), Json(body))
                .await
                .expect("generate answers"); // agentdesk-audit: allow-unwrap — test assertion
            status.as_u16()
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_generates_at_once_create_one_run_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(8).await;
        sqlx::query(
            "INSERT INTO agents (id, name, provider, status) VALUES ('agent-par', 'Par', 'codex', 'idle')",
        )
        .execute(&pool)
        .await
        .expect("seed agent"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query(
            "INSERT INTO kanban_cards (id, repo_id, title, status, assigned_agent_id, github_issue_number)
             VALUES ('card-201', 'par/repo', 'One', 'ready', 'agent-par', 201),
                    ('card-202', 'par/repo', 'Two', 'ready', 'agent-par', 202)",
        )
        .execute(&pool)
        .await
        .expect("seed cards"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        // Both requests pass the first conflict check while run creation is held.
        let mut holder = pool.begin().await.expect("begin lock holder"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('aq_run_create'))")
            .execute(&mut *holder)
            .await
            .expect("hold run creation"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *holder)
            .await
            .expect("holder pid"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        let first = spawn_generate(&pool);
        let second = spawn_generate(&pool);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))",
        )
        .bind(holder_pid)
        .fetch_one(&pool)
        .await
        .expect("inspect blockers") // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
            < 2
        {
            assert!(
                std::time::Instant::now() < deadline,
                "both generates wait on the run-creation lock"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        holder.rollback().await.expect("release run creation"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        let mut statuses = vec![
            first.await.expect("first generate"), // agentdesk-audit: allow-unwrap — test assertion
            second.await.expect("second generate"), // agentdesk-audit: allow-unwrap — test assertion
        ];
        statuses.sort_unstable();
        assert_eq!(statuses, vec![200, 409]);
        let runs: i64 = sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM auto_queue_runs")
            .fetch_one(&pool)
            .await
            .expect("count runs"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        assert_eq!(runs, 1);
        pool.close().await;
        pg_db.drop().await;
    }
}
