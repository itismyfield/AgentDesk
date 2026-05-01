use super::*;

pub(crate) async fn activate_with_deps_pg(
    deps: &AutoQueueActivateDeps,
    body: ActivateBody,
) -> (StatusCode, Json<serde_json::Value>) {
    let _ignored_unified_thread = body.unified_thread.is_some();
    let Some(pool) = deps.pg_pool.as_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool is not configured"})),
        );
    };
    let active_only = body.active_only.unwrap_or(false);
    let run_id = if let Some(run_id) = body.run_id.clone() {
        run_id
    } else {
        let repo = body
            .repo
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        let agent_id = body
            .agent_id
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        let status_clause = if active_only {
            "status = 'active'"
        } else {
            "status IN ('active', 'generated', 'pending')"
        };
        let query = format!(
            "SELECT id
             FROM auto_queue_runs
             WHERE ($1::TEXT IS NULL OR repo = $1 OR repo IS NULL OR repo = '')
               AND ($2::TEXT IS NULL OR agent_id = $2 OR agent_id IS NULL OR agent_id = '')
               AND {status_clause}
             ORDER BY created_at DESC
             LIMIT 1"
        );
        match sqlx::query_scalar::<_, String>(&query)
            .bind(repo)
            .bind(agent_id)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(run_id)) => run_id,
            Ok(None) => {
                return (
                    StatusCode::OK,
                    Json(json!({ "dispatched": [], "count": 0, "message": "No active run" })),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("load postgres auto-queue run: {error}")})),
                );
            }
        }
    };
    let run_log_ctx = AutoQueueLogContext::new().run(&run_id);
    if !active_only
        && let Err(error) = sqlx::query(
            "UPDATE auto_queue_runs
             SET status = 'active'
             WHERE id = $1
               AND status IN ('generated', 'pending')",
        )
        .bind(&run_id)
        .execute(pool)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("promote postgres auto-queue run {run_id}: {error}")})),
        );
    }
    if let Err(error) = crate::db::auto_queue::clear_inactive_slot_assignments_pg(pool).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                json!({"error": format!("clear inactive postgres auto-queue slots for {run_id}: {error}")}),
            ),
        );
    }
    let mut cleared_slots: HashSet<(String, i64)> = HashSet::new();
    let entry_count = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1",
    )
    .bind(&run_id)
    .fetch_one(pool)
    .await
    {
        Ok(count) => count,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("count postgres auto-queue entries for {run_id}: {error}")}),
                ),
            );
        }
    };
    if entry_count == 0 {
        if let Err(error) = sqlx::query(
            "UPDATE auto_queue_runs
             SET status = 'completed',
                 completed_at = NOW()
             WHERE id = $1",
        )
        .bind(&run_id)
        .execute(pool)
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("complete stale postgres auto-queue run {run_id}: {error}")}),
                ),
            );
        }
        crate::auto_queue_log!(
            info,
            "activate_stale_empty_run_completed_pg",
            run_log_ctx.clone(),
            "[auto-queue] Completed stale empty PG run {run_id} — no entries, skipping fallback populate (#85)"
        );
        return (
            StatusCode::OK,
            Json(
                json!({ "dispatched": [], "count": 0, "message": "Stale empty run completed — no entries to dispatch" }),
            ),
        );
    }
    let (max_concurrent, _thread_group_count) = match sqlx::query(
        "SELECT COALESCE(max_concurrent_threads, 1)::BIGINT AS max_concurrent_threads,
                COALESCE(thread_group_count, 1)::BIGINT AS thread_group_count
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(&run_id)
    .fetch_one(pool)
    .await
    {
        Ok(row) => {
            let max_concurrent = match row.try_get::<i64, _>("max_concurrent_threads") {
                Ok(value) => value,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode postgres auto-queue max_concurrent_threads for {run_id}: {error}")}),
                        ),
                    );
                }
            };
            let thread_group_count = match row.try_get::<i64, _>("thread_group_count") {
                Ok(value) => value,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode postgres auto-queue thread_group_count for {run_id}: {error}")}),
                        ),
                    );
                }
            };
            (max_concurrent, thread_group_count)
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres auto-queue run capacity for {run_id}: {error}")}),
                ),
            );
        }
    };
    let run_agents_rows = match sqlx::query(
        "SELECT DISTINCT agent_id
         FROM auto_queue_entries
         WHERE run_id = $1",
    )
    .bind(&run_id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres auto-queue run agents for {run_id}: {error}")}),
                ),
            );
        }
    };
    for row in run_agents_rows {
        let agent_id: String = match row.try_get("agent_id") {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("decode postgres auto-queue run agent for {run_id}: {error}")}),
                    ),
                );
            }
        };
        if let Err(error) =
            crate::db::auto_queue::ensure_agent_slot_pool_rows_pg(pool, &agent_id, max_concurrent)
                .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("prepare postgres slot pool rows for run {run_id} agent {agent_id}: {error}")}),
                ),
            );
        }
    }
    let current_phase = match crate::db::auto_queue::current_batch_phase_pg(pool, &run_id).await {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres auto-queue current phase for {run_id}: {error}")}),
                ),
            );
        }
    };
    let active_groups_rows = match sqlx::query(
        "SELECT DISTINCT COALESCE(thread_group, 0)::BIGINT AS thread_group
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status = 'dispatched'
         ORDER BY thread_group ASC",
    )
    .bind(&run_id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres active groups for {run_id}: {error}")}),
                ),
            );
        }
    };
    let active_groups: Vec<i64> = {
        let mut groups = Vec::with_capacity(active_groups_rows.len());
        for row in active_groups_rows {
            match row.try_get::<i64, _>("thread_group") {
                Ok(value) => groups.push(value),
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode postgres active group for {run_id}: {error}")}),
                        ),
                    );
                }
            }
        }
        groups
    };
    let active_group_count = active_groups.len() as i64;
    let pending_group_rows = match sqlx::query(
        "SELECT DISTINCT COALESCE(thread_group, 0)::BIGINT AS thread_group,
                         COALESCE(batch_phase, 0)::BIGINT AS batch_phase
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status = 'pending'
         ORDER BY thread_group ASC, batch_phase ASC",
    )
    .bind(&run_id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres pending groups for {run_id}: {error}")}),
                ),
            );
        }
    };
    let pending_groups: Vec<i64> = {
        let active_set: HashSet<i64> = active_groups.iter().copied().collect();
        let mut groups = Vec::new();
        let mut seen = HashSet::new();
        for row in pending_group_rows {
            let thread_group = match row.try_get::<i64, _>("thread_group") {
                Ok(value) => value,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode postgres pending group for {run_id}: {error}")}),
                        ),
                    );
                }
            };
            let batch_phase = match row.try_get::<i64, _>("batch_phase") {
                Ok(value) => value,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode postgres pending batch_phase for {run_id}: {error}")}),
                        ),
                    );
                }
            };
            if !active_set.contains(&thread_group)
                && crate::db::auto_queue::batch_phase_is_eligible(batch_phase, current_phase)
                && seen.insert(thread_group)
            {
                groups.push(thread_group);
            }
        }
        groups
    };
    let mut dispatched = Vec::new();
    let mut groups_to_dispatch = Vec::new();
    if let Some(group) = body.thread_group {
        let has_pending = match crate::db::auto_queue::group_has_pending_entries_pg(
            pool,
            &run_id,
            group,
            current_phase,
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres pending group eligibility for {run_id}:{group}: {error}")}),
                    ),
                );
            }
        };
        let has_dispatched = match group_has_dispatched_entries_pg(pool, &run_id, group).await {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres dispatched group state for {run_id}:{group}: {error}")}),
                    ),
                );
            }
        };
        if has_pending && !has_dispatched {
            groups_to_dispatch.push(group);
        }
    }
    match crate::db::auto_queue::assigned_groups_with_pending_entries_pg(
        pool,
        &run_id,
        current_phase,
    )
    .await
    {
        Ok(groups) => {
            for group in groups {
                if !groups_to_dispatch.contains(&group) {
                    groups_to_dispatch.push(group);
                }
            }
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres assigned groups for {run_id}: {error}")}),
                ),
            );
        }
    }

    for &group in &active_groups {
        let has_pending = match crate::db::auto_queue::group_has_pending_entries_pg(
            pool,
            &run_id,
            group,
            current_phase,
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres continuation eligibility for {run_id}:{group}: {error}")}),
                    ),
                );
            }
        };
        let has_dispatched = match group_has_dispatched_entries_pg(pool, &run_id, group).await {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres dispatched continuation state for {run_id}:{group}: {error}")}),
                    ),
                );
            }
        };
        if has_pending && !has_dispatched && !groups_to_dispatch.contains(&group) {
            groups_to_dispatch.push(group);
        }
    }

    for group in pending_groups {
        if !groups_to_dispatch.contains(&group) {
            groups_to_dispatch.push(group);
        }
    }

    let mut dispatched_groups_this_activate = 0_i64;
    for group in &groups_to_dispatch {
        if (active_group_count + dispatched_groups_this_activate) >= max_concurrent {
            break;
        }

        let entry = match crate::db::auto_queue::first_pending_entry_for_group_pg(
            pool,
            &run_id,
            *group,
            current_phase,
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres pending entry for {run_id}:{group}: {error}")}),
                    ),
                );
            }
        };
        let Some((entry_id, card_id, agent_id, batch_phase)) = entry else {
            continue;
        };
        let entry_log_ctx = AutoQueueLogContext::new()
            .run(&run_id)
            .entry(&entry_id)
            .card(&card_id)
            .agent(&agent_id)
            .thread_group(*group)
            .batch_phase(batch_phase);

        let initial_state = match load_activate_card_state_pg(pool, &card_id, &entry_id).await {
            Ok(state) => state,
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "activate_load_card_failed_pg",
                    entry_log_ctx.clone(),
                    "[auto-queue] failed to load PG card {} before activate for entry {}: {error}",
                    card_id,
                    entry_id
                );
                continue;
            }
        };

        // #953: do not collapse same-agent dispatch capacity to a single
        // active card. Slot allocation below is the actual concurrency guard.
        // Same-channel turn races remain blocked by the mailbox/channel lock.

        let effective = match resolve_activate_pipeline_pg(
            pool,
            initial_state.repo_id.as_deref(),
            initial_state.assigned_agent_id.as_deref(),
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "activate_pipeline_resolve_failed_pg",
                    entry_log_ctx.clone(),
                    "[auto-queue] failed to resolve PG pipeline for card {} during activate: {}",
                    card_id,
                    error
                );
                continue;
            }
        };

        if initial_state.entry_status != "pending" {
            if initial_state.entry_status == "dispatched" {
                dispatched_groups_this_activate += 1;
            }
            continue;
        }

        if effective.is_terminal(&initial_state.status) || initial_state.status == "done" {
            if let Err(error) = crate::db::auto_queue::update_entry_status_on_pg(
                pool,
                &entry_id,
                crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
                "activate_done_skip_pg",
                &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
            )
            .await
            {
                crate::auto_queue_log!(
                    warn,
                    "activate_done_skip_failed_pg",
                    entry_log_ctx.clone(),
                    "[auto-queue] failed to skip terminal PG card entry {} during activate: {}",
                    entry_id,
                    error
                );
            }
            continue;
        }

        if initial_state.has_active_dispatch() {
            let dispatch_id = initial_state
                .latest_dispatch_id
                .as_ref()
                .expect("active dispatch state requires dispatch id")
                .clone();
            // #1444 idempotency log: emit a clearly-tagged DISPATCH-NEXT skip
            // marker so the operator can correlate "no new dispatch was
            // created" with the active-dispatch reuse path. We continue to
            // attach the entry to the existing dispatch (the existing
            // semantics) rather than truly skip — leaving the entry pending
            // would cause subsequent dispatch-next calls to retry the same
            // card forever. The attach is itself idempotent: a card with an
            // active dispatch never gets a NEW dispatch_id from this path.
            crate::auto_queue_log!(
                info,
                "dispatch_next_skip_active_dispatch_pg_1444",
                entry_log_ctx.clone().dispatch(&dispatch_id),
                "⏭ DISPATCH-NEXT: card {} already has active dispatch {}, skipping",
                card_id,
                dispatch_id
            );
            if let Err(error) = crate::db::auto_queue::update_entry_status_on_pg(
                pool,
                &entry_id,
                crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
                "activate_attach_existing_dispatch_pg",
                &crate::db::auto_queue::EntryStatusUpdateOptions {
                    dispatch_id: Some(dispatch_id.clone()),
                    slot_index: None,
                },
            )
            .await
            {
                crate::auto_queue_log!(
                    warn,
                    "activate_attach_existing_dispatch_failed_pg",
                    entry_log_ctx.clone().dispatch(&dispatch_id),
                    "[auto-queue] failed to attach existing PG dispatch {dispatch_id} to entry {entry_id}: {error}"
                );
            }
            dispatched_groups_this_activate += 1;
            continue;
        }

        let still_pending = match sqlx::query_scalar::<_, bool>(
            "SELECT status = 'pending'
             FROM auto_queue_entries
             WHERE id = $1",
        )
        .bind(&entry_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some(value)) => value,
            Ok(None) => false,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("recheck postgres auto-queue entry status for {entry_id}: {error}")}),
                    ),
                );
            }
        };
        if !still_pending {
            crate::auto_queue_log!(
                warn,
                "activate_concurrent_race_detected_pg",
                entry_log_ctx.clone(),
                "[auto-queue] entry {entry_id} is no longer pending before slot allocation; concurrent activate likely claimed it"
            );
            dispatched_groups_this_activate += 1;
            continue;
        }

        let slot_allocation = match crate::db::auto_queue::allocate_slot_for_group_agent_pg(
            pool, &run_id, *group, &agent_id,
        )
        .await
        {
            Ok(allocation) => allocation,
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "activate_slot_allocation_failed_pg",
                    entry_log_ctx.clone(),
                    "[auto-queue] failed to allocate PG slot for entry {} run {} agent {} group {}: {}",
                    entry_id,
                    run_id,
                    agent_id,
                    group,
                    error
                );
                continue;
            }
        };
        let slot_index = slot_allocation
            .as_ref()
            .map(|allocation| allocation.slot_index);
        let Some(allocation) = slot_allocation else {
            crate::auto_queue_log!(
                warn,
                "activate_slot_pool_exhausted_pg",
                entry_log_ctx.clone(),
                "[auto-queue] Skipping group {group} for {agent_id}: no free PG slot in pool (possible concurrent slot claim)"
            );
            continue;
        };

        let reset_slot_thread_before_reuse = match slot_requires_thread_reset_before_reuse_pg(
            pool,
            &agent_id,
            allocation.slot_index,
            allocation.newly_assigned,
            allocation.reassigned_from_other_group,
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "activate_slot_reset_probe_failed_pg",
                    entry_log_ctx.clone().slot_index(allocation.slot_index),
                    "[auto-queue] failed to inspect PG slot reuse state for {} slot {}: {}",
                    agent_id,
                    allocation.slot_index,
                    error
                );
                false
            }
        };
        let clear_slot_session_before_dispatch = reset_slot_thread_before_reuse;
        let slot_key = (agent_id.clone(), allocation.slot_index);
        if clear_slot_session_before_dispatch && !cleared_slots.contains(&slot_key) {
            match crate::services::auto_queue::runtime::clear_slot_threads_for_slot_pg(
                deps.health_registry.clone(),
                pool,
                &agent_id,
                allocation.slot_index,
            )
            .await
            {
                Ok(cleared) => {
                    if cleared > 0 {
                        crate::auto_queue_log!(
                            info,
                            "activate_slot_cleared_before_dispatch_pg",
                            entry_log_ctx.clone().slot_index(allocation.slot_index),
                            "[auto-queue] cleared {cleared} PG slot thread session(s) before dispatching {agent_id} slot {} group {group}",
                            allocation.slot_index
                        );
                    }
                }
                Err(error) => crate::auto_queue_log!(
                    warn,
                    "activate_slot_clear_failed_pg",
                    entry_log_ctx.clone().slot_index(allocation.slot_index),
                    "[auto-queue] failed to clear PG slot thread session(s) for {} slot {}: {}",
                    agent_id,
                    allocation.slot_index,
                    error
                ),
            }
            cleared_slots.insert(slot_key);
        }

        match crate::db::auto_queue::update_entry_status_on_pg(
            pool,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
            "activate_dispatch_reserve_pg",
            &crate::db::auto_queue::EntryStatusUpdateOptions {
                dispatch_id: None,
                slot_index,
            },
        )
        .await
        {
            Ok(result) if !result.changed => {
                crate::auto_queue_log!(
                    info,
                    "activate_dispatch_reserve_already_claimed_pg",
                    entry_log_ctx.clone().maybe_slot_index(slot_index),
                    "[auto-queue] entry {entry_id} was already reserved by another activate worker; skipping duplicate PG dispatch creation"
                );
                continue;
            }
            Ok(_) => {}
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "activate_dispatch_reserve_failed_pg",
                    entry_log_ctx.clone().maybe_slot_index(slot_index),
                    "[auto-queue] failed to reserve PG entry {} before create_dispatch: {}",
                    entry_id,
                    error
                );
                continue;
            }
        }

        let dispatch_context = build_auto_queue_dispatch_context(
            &entry_id,
            *group,
            slot_index,
            reset_slot_thread_before_reuse,
            std::iter::empty(),
        );
        let dispatch_id = match create_activate_dispatch_pg(
            pool,
            &card_id,
            &agent_id,
            "implementation",
            &initial_state.title,
            &dispatch_context,
        )
        .await
        {
            Ok(dispatch_id) => dispatch_id,
            Err(error) => {
                let recovered_state = load_activate_card_state_pg(pool, &card_id, &entry_id)
                    .await
                    .ok();
                if let Some(dispatch_id) = recovered_state
                    .as_ref()
                    .filter(|state| state.has_active_dispatch())
                    .and_then(|state| state.latest_dispatch_id.clone())
                {
                    if let Err(update_error) = crate::db::auto_queue::update_entry_status_on_pg(
                        pool,
                        &entry_id,
                        crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
                        "activate_dispatch_error_recover_pg",
                        &crate::db::auto_queue::EntryStatusUpdateOptions {
                            dispatch_id: Some(dispatch_id),
                            slot_index,
                        },
                    )
                    .await
                    {
                        crate::auto_queue_log!(
                            warn,
                            "activate_create_dispatch_recover_failed_pg",
                            entry_log_ctx.clone().maybe_slot_index(slot_index),
                            "[auto-queue] failed to recover PG entry {entry_id} after create_dispatch error: {update_error}"
                        );
                    } else {
                        continue;
                    }
                }

                if recovered_state.as_ref().is_some_and(|state| {
                    state.latest_dispatch_id.is_some() || state.status != initial_state.status
                }) {
                    crate::auto_queue_log!(
                        warn,
                        "activate_create_dispatch_error_kept_reservation_pg",
                        entry_log_ctx
                            .clone()
                            .maybe_slot_index(slot_index)
                            .maybe_dispatch(
                                recovered_state
                                    .as_ref()
                                    .and_then(|state| state.latest_dispatch_id.as_deref())
                            ),
                        "[auto-queue] create_dispatch PG errored for entry {entry_id} after card progressed; keeping reservation"
                    );
                    continue;
                }

                if let Err(update_error) = crate::db::auto_queue::update_entry_status_on_pg(
                    pool,
                    &entry_id,
                    crate::db::auto_queue::ENTRY_STATUS_PENDING,
                    "activate_dispatch_reserve_revert_pg",
                    &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                )
                .await
                {
                    crate::auto_queue_log!(
                        warn,
                        "activate_dispatch_reserve_revert_failed_pg",
                        entry_log_ctx.clone().maybe_slot_index(slot_index),
                        "[auto-queue] failed to revert PG reservation for entry {} after create_dispatch error: {}",
                        entry_id,
                        update_error
                    );
                } else if let Some(assigned_slot) = slot_index
                    && let Err(release_error) =
                        crate::db::auto_queue::release_slot_for_group_agent_pg(
                            pool,
                            &run_id,
                            *group,
                            &agent_id,
                            assigned_slot,
                        )
                        .await
                {
                    crate::auto_queue_log!(
                        warn,
                        "activate_dispatch_revert_slot_release_failed_pg",
                        entry_log_ctx.clone().slot_index(assigned_slot),
                        "[auto-queue] failed to release PG slot {} for entry {} after create_dispatch error: {}",
                        assigned_slot,
                        entry_id,
                        release_error
                    );
                }
                crate::auto_queue_log!(
                    error,
                    "activate_dispatch_create_failed_pg",
                    entry_log_ctx.clone().maybe_slot_index(slot_index),
                    "[auto-queue] create_dispatch PG failed for entry {entry_id} (group {group}), leaving as pending for retry: {error}"
                );
                continue;
            }
        };

        if let Err(error) = crate::db::auto_queue::update_entry_status_on_pg(
            pool,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
            "activate_dispatch_created_pg",
            &crate::db::auto_queue::EntryStatusUpdateOptions {
                dispatch_id: Some(dispatch_id.clone()),
                slot_index,
            },
        )
        .await
        {
            crate::auto_queue_log!(
                warn,
                "activate_dispatch_mark_failed_pg",
                entry_log_ctx
                    .clone()
                    .dispatch(&dispatch_id)
                    .maybe_slot_index(slot_index),
                "[auto-queue] failed to mark PG entry {} dispatched after create_dispatch: {}",
                entry_id,
                error
            );
        }

        dispatched_groups_this_activate += 1;
        dispatched.push(deps.entry_json_pg(pool, &entry_id).await);
    }

    let remaining = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(&run_id)
    .fetch_one(pool)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("count postgres remaining entries for {run_id}: {error}")}),
                ),
            );
        }
    };
    if remaining == 0 {
        if let Err(error) = crate::db::auto_queue::release_run_slots_pg(pool, &run_id).await {
            crate::auto_queue_log!(
                warn,
                "activate_release_run_slots_failed_pg",
                run_log_ctx.clone(),
                "[auto-queue] failed to release PG slots for drained run {}: {}",
                run_id,
                error
            );
        }
        let still_dispatched = match sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_entries
             WHERE run_id = $1
               AND status = 'dispatched'",
        )
        .bind(&run_id)
        .fetch_one(pool)
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("count postgres dispatched entries for {run_id}: {error}")}),
                    ),
                );
            }
        };
        if still_dispatched == 0
            && let Err(error) = sqlx::query(
                "UPDATE auto_queue_runs
                 SET status = 'completed',
                     completed_at = NOW()
                 WHERE id = $1
                   AND status IN ('active', 'paused', 'generated', 'pending')",
            )
            .bind(&run_id)
            .execute(pool)
            .await
        {
            crate::auto_queue_log!(
                warn,
                "activate_finalize_run_failed_pg",
                run_log_ctx.clone(),
                "[auto-queue] failed to finalize PG run {} after dispatch drain: {}",
                run_id,
                error
            );
        }
    }

    let active_group_count = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(DISTINCT COALESCE(thread_group, 0))::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status = 'dispatched'",
    )
    .bind(&run_id)
    .fetch_one(pool)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("count postgres active groups for {run_id}: {error}")}),
                ),
            );
        }
    };
    let pending_group_count = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(DISTINCT COALESCE(thread_group, 0))::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status = 'pending'",
    )
    .bind(&run_id)
    .fetch_one(pool)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("count postgres pending groups for {run_id}: {error}")}),
                ),
            );
        }
    };

    (
        StatusCode::OK,
        Json(json!({
            "dispatched": dispatched,
            "count": dispatched.len(),
            "active_groups": active_group_count,
            "pending_groups": pending_group_count,
        })),
    )
}
