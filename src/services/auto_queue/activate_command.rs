use super::*;

/// Early-return HTTP payload shared by `activate_with_deps_pg` and the phase
/// helpers extracted from it (Part of #3038). Helpers signal an early exit by
/// returning `Err(ActivateResponse)`; the orchestrator propagates it verbatim
/// with `?`, preserving the exact `(StatusCode, Json)` and control-flow of the
/// original monolithic function.
type ActivateResponse = (StatusCode, Json<serde_json::Value>);

/// Plan produced by `compute_activate_groups_to_dispatch`: the ordered list of
/// thread groups to attempt dispatching plus the concurrency counters the
/// dispatch loop consults.
struct ActivateGroupsPlan {
    groups_to_dispatch: Vec<i64>,
    active_group_count: i64,
    active_turn_count: i64,
    current_phase: Option<i64>,
}

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
    let run_id = match resolve_activate_target_run_id(pool, &body, active_only).await {
        Ok(run_id) => run_id,
        Err(response) => return response,
    };
    let run_log_ctx = AutoQueueLogContext::new().run(&run_id);
    let _activate_lock_guard = match acquire_activate_run_lock(pool, &run_id).await {
        Ok(guard) => guard,
        Err(response) => return response,
    };
    if let Err(response) = promote_run_and_clear_inactive_slots(pool, &run_id, active_only).await {
        return response;
    }
    let mut cleared_slots: HashSet<(String, i64)> = HashSet::new();
    if let Err(response) = complete_run_if_empty(pool, &run_id, &run_log_ctx).await {
        return response;
    }
    let max_concurrent = match load_activate_capacity_and_prepare_slots(pool, &run_id).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let ActivateGroupsPlan {
        groups_to_dispatch,
        active_group_count,
        active_turn_count,
        current_phase,
    } = match compute_activate_groups_to_dispatch(pool, &run_id, &body).await {
        Ok(plan) => plan,
        Err(response) => return response,
    };
    let mut dispatched = Vec::new();
    // feature: rate-limit-aware-dispatch-gate — additive per-entry defer
    // details surfaced on the activate/dispatch-next response. Each element is
    // `{entry_id, ...ProviderPressureDecision fields}`. Never mutates entry
    // status; deferred entries stay `pending`.
    let mut deferred_entries: Vec<serde_json::Value> = Vec::new();

    // feature: rate-limit-aware-dispatch-gate.
    //
    // (1) P2 — populate the gate snapshots on EVERY serving node. `RateLimitSync`
    // is leader-only, so on a follower the process-local pressure/agent maps are
    // never filled by the sync loop. Refresh them here (throttled to ~120s) from
    // the SHARED DB cache — read-only, no provider credentials — so the gate is
    // not silently a no-op on whichever node serves `dispatch-next`.
    //
    // (2) P1 — honor the PERSISTED runtime toggle/threshold. The enable flag and
    // gate danger threshold set via `PUT /api/settings/runtime-config` land in
    // `kv_meta` and never reach `config_live_reload::current()`, so we read the
    // persisted `runtime-config` here (mirroring `effective_max_entry_retries`)
    // and pass the values into the gate. On a missing/error read the overrides
    // are `None` and the gate falls back to the YAML snapshot then the compiled
    // defaults (enabled, 100%). Both reads happen once per activate, off the
    // per-entry loop.
    {
        let now = chrono::Utc::now().timestamp();
        crate::services::dispatch_gate::refresh_snapshots_if_stale(pool, now).await;
    }
    let (gate_enabled_override, gate_danger_override) = {
        let runtime_config = match load_kv_meta_value_pg(pool, "runtime-config") {
            Ok(raw) => raw.and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok()),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "[auto-queue] failed to load persisted runtime-config for dispatch gate; falling back to YAML/defaults"
                );
                None
            }
        };
        crate::services::dispatch_gate::persisted_runtime_overrides(runtime_config.as_ref())
    };

    let mut new_dispatches_this_activate = 0_i64;
    for group in &groups_to_dispatch {
        // #2034 + #2048 F4: cap on TOTAL active turns. Refresh from DB at
        // every iteration so policy-driven follow-ups (review-decision,
        // rework, create-pr) created between iterations are seen. The
        // per-run advisory lock prevents concurrent activate from
        // interleaving; mid-loop turn increase comes only from these
        // follow-up paths.
        let active_turn_count_now = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM task_dispatches d
             CROSS JOIN LATERAL (SELECT COALESCE(NULLIF(d.context, ''), '{}')::jsonb AS ctx) c
             WHERE d.status IN ('pending', 'dispatched')
               AND COALESCE((c.ctx->>'sidecar_dispatch')::BOOLEAN, FALSE) = FALSE
               AND c.ctx->'phase_gate' IS NULL
               AND EXISTS (
                   SELECT 1
                   FROM auto_queue_entries e
                   WHERE e.run_id = $1
                     AND e.agent_id = d.to_agent_id
               )",
        )
        .bind(&run_id)
        .fetch_one(pool)
        .await
        .unwrap_or(active_turn_count + new_dispatches_this_activate);
        if active_turn_count_now >= max_concurrent {
            break;
        }
        if activate_fallback_capacity_reached(
            active_turn_count,
            active_group_count,
            new_dispatches_this_activate,
            max_concurrent,
        ) {
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
        let Some((entry_id, card_id, agent_id, batch_phase, retry_count)) = entry else {
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
                .expect("active dispatch state requires dispatch id") // agentdesk-audit: allow-unwrap pre-existing invariant relocated unchanged during #3038 phase extraction; has_active_dispatch() guarantees latest_dispatch_id is Some
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
            continue;
        }

        // feature: rate-limit-aware-dispatch-gate (REQ-002). Evaluate provider
        // pressure AFTER the still_pending recheck and BEFORE slot allocation.
        // This is an O(1), lock-cheap, DB-free read of the in-memory pressure
        // snapshot (refreshed off the hot path by rate_limit_sync_loop / lazily
        // on this serving node). On a `defer` decision we create NO slot, NO
        // dispatch row, perform NO status mutation (the entry stays `pending` and
        // resumes on the next activation once pressure clears), and surface an
        // additive defer detail.
        //
        // The enable flag and gate danger threshold are resolved from the
        // PERSISTED runtime-config (kv_meta, written by
        // PUT /api/settings/runtime-config) — read once per activate above — so a
        // dashboard/API rollback toggle (and a persisted danger-threshold change)
        // takes effect at runtime. The persisted value never reaches the YAML
        // live snapshot, so the gate must honor kv_meta here; on a missing/error
        // read the overrides are `None` and the gate falls back to the YAML
        // snapshot then the compiled defaults (enabled, 100%).
        let gate_decision =
            crate::services::dispatch_gate::evaluate_agent_provider_pressure_with_overrides(
                &agent_id,
                chrono::Utc::now().timestamp(),
                gate_enabled_override,
                gate_danger_override,
            );
        if gate_decision.verdict.is_defer() {
            crate::auto_queue_log!(
                info,
                "activate_deferred_due_to_rate_limit_pg",
                entry_log_ctx.clone(),
                "[auto-queue] deferring entry {entry_id} for {agent_id}: provider {:?} at {:?}% (danger {}%), entry stays pending",
                gate_decision.provider,
                gate_decision.utilization_pct,
                gate_decision.danger_pct
            );
            let mut detail =
                serde_json::to_value(&gate_decision).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(map) = detail.as_object_mut() {
                map.insert("entry_id".to_string(), serde_json::json!(entry_id));
            }
            deferred_entries.push(detail);
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

        match crate::db::auto_queue::slot_has_recent_terminal_auto_queue_dispatch_pg(
            pool,
            &agent_id,
            allocation.slot_index,
        )
        .await
        {
            Ok(true) => {
                crate::auto_queue_log!(
                    info,
                    "activate_slot_terminal_cooldown_pg",
                    entry_log_ctx.clone().slot_index(allocation.slot_index),
                    "[auto-queue] delaying entry {entry_id} for {agent_id} slot {}: previous terminal dispatch is still within {}s bridge cooldown",
                    allocation.slot_index,
                    crate::db::auto_queue::SLOT_TERMINAL_DISPATCH_COOLDOWN_SECONDS
                );
                continue;
            }
            Ok(false) => {}
            Err(error) => crate::auto_queue_log!(
                warn,
                "activate_slot_terminal_cooldown_probe_failed_pg",
                entry_log_ctx.clone().slot_index(allocation.slot_index),
                "[auto-queue] failed to inspect terminal dispatch cooldown for {} slot {}: {}",
                agent_id,
                allocation.slot_index,
                error
            ),
        }

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

        let retry_resume_session_id = if retry_count > 0 {
            match crate::db::auto_queue::latest_entry_phase_codex_session_id_pg(
                pool,
                &entry_id,
                "implementation",
            )
            .await
            {
                Ok(value) => value,
                Err(error) => {
                    crate::auto_queue_log!(
                        warn,
                        "activate_retry_resume_lookup_failed_pg",
                        entry_log_ctx.clone().maybe_slot_index(slot_index),
                        "[auto-queue] failed to load previous Codex session for retry entry {}: {}",
                        entry_id,
                        error
                    );
                    None
                }
            }
        } else {
            None
        };
        let mut dispatch_extra_fields = Vec::new();
        if let Some(session_id) = retry_resume_session_id.as_deref() {
            dispatch_extra_fields.push(("reset_provider_state", json!(false)));
            dispatch_extra_fields.push(("force_new_session", json!(false)));
            dispatch_extra_fields.push(("auto_queue_retry", json!(true)));
            dispatch_extra_fields.push(("auto_queue_retry_count", json!(retry_count)));
            dispatch_extra_fields.push(("auto_queue_retry_resume_session_id", json!(session_id)));
            crate::auto_queue_log!(
                info,
                "activate_retry_resume_session_selected_pg",
                entry_log_ctx.clone().maybe_slot_index(slot_index),
                "[auto-queue] retry entry {entry_id} will resume previous Codex thread for same phase"
            );
        }

        let dispatch_context = build_auto_queue_dispatch_context(
            &entry_id,
            *group,
            slot_index,
            reset_slot_thread_before_reuse,
            dispatch_extra_fields,
        );
        let dispatch_id = match create_activate_dispatch_for_entry_pg(
            pool,
            &card_id,
            &agent_id,
            "implementation",
            &initial_state.title,
            &dispatch_context,
            ActivateDispatchEntryAttachment::new(
                &entry_id,
                slot_index,
                "activate_dispatch_created_pg",
            ),
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

                let failure_result = record_entry_dispatch_failure(
                    deps,
                    &run_id,
                    &entry_id,
                    &card_id,
                    &agent_id,
                    *group,
                    slot_index,
                    "activate_dispatch_create_failed_pg",
                    &error.to_string(),
                    &entry_log_ctx,
                );
                crate::auto_queue_log!(
                    warn,
                    "activate_dispatch_create_failed_pg",
                    entry_log_ctx.clone().maybe_slot_index(slot_index),
                    "[auto-queue] create_dispatch PG failed for entry {entry_id} (group {group}): {error}"
                );
                match failure_result {
                    Ok(failure) => crate::auto_queue_log!(
                        warn,
                        "activate_dispatch_create_failure_recorded_pg",
                        entry_log_ctx.clone().maybe_slot_index(slot_index),
                        "[auto-queue] dispatch creation failure recorded for entry {} retry {}/{} -> {}",
                        entry_id,
                        failure.retry_count,
                        failure.retry_limit,
                        failure.to_status
                    ),
                    Err(update_error) => crate::auto_queue_log!(
                        warn,
                        "activate_dispatch_create_failure_record_failed_pg",
                        entry_log_ctx.clone().maybe_slot_index(slot_index),
                        "[auto-queue] failed to record dispatch creation failure for entry {}: {}",
                        entry_id,
                        update_error
                    ),
                }
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

        new_dispatches_this_activate += 1;
        dispatched.push(deps.entry_json_pg(pool, &entry_id).await);
    }

    match finalize_activate_run_and_build_response(
        pool,
        &run_id,
        &run_log_ctx,
        dispatched,
        deferred_entries,
    )
    .await
    {
        Ok(response) | Err(response) => response,
    }
}

/// Resolves the target `run_id` for the activate request: prefers the explicit
/// `body.run_id`, otherwise selects the most recent matching run. An empty
/// match short-circuits with the canonical "No active run" OK response, and DB
/// errors short-circuit with a 500 — both returned as `Err(ActivateResponse)`.
async fn resolve_activate_target_run_id(
    pool: &sqlx::PgPool,
    body: &ActivateBody,
    active_only: bool,
) -> Result<String, ActivateResponse> {
    if let Some(run_id) = body.run_id.clone() {
        return Ok(run_id);
    }
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
        Ok(Some(run_id)) => Ok(run_id),
        Ok(None) => Err((
            StatusCode::OK,
            Json(json!({ "dispatched": [], "count": 0, "message": "No active run" })),
        )),
        Err(error) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("load postgres auto-queue run: {error}")})),
        )),
    }
}

/// #2048 F4: serialize per-run activate so concurrent
/// `POST /api/queue/activate` and `policy::activateRun` invocations cannot
/// both observe the same `active_turn_count` snapshot and each create
/// `(max_concurrent_threads - N)` dispatches, exceeding the cap. Lock is
/// session-scoped, pinned to a dedicated connection so it survives the
/// multi-step pool-based loop. A drop guard releases the lock on every
/// early-return path.
async fn acquire_activate_run_lock(
    pool: &sqlx::PgPool,
    run_id: &str,
) -> Result<ActivateLockReleaseGuard, ActivateResponse> {
    let activate_lock_conn_result = pool.acquire().await;
    let mut activate_lock_conn = match activate_lock_conn_result {
        Ok(conn) => conn,
        Err(error) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("acquire activate lock connection for {run_id}: {error}")}),
                ),
            ));
        }
    };
    if let Err(error) = sqlx::query("SELECT pg_advisory_lock(hashtext($1), hashtext($2))")
        .bind("aq_activate")
        .bind(run_id)
        .execute(&mut *activate_lock_conn)
        .await
    {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("acquire activate advisory lock for {run_id}: {error}")})),
        ));
    }
    Ok(ActivateLockReleaseGuard::new(
        activate_lock_conn,
        run_id.to_string(),
    ))
}

/// Promotes a generated/pending run to `active` (skipped when `active_only`)
/// and clears inactive slot assignments. Either DB failure short-circuits the
/// request with a 500 returned as `Err(ActivateResponse)`.
async fn promote_run_and_clear_inactive_slots(
    pool: &sqlx::PgPool,
    run_id: &str,
    active_only: bool,
) -> Result<(), ActivateResponse> {
    if !active_only
        && let Err(error) = sqlx::query(
            "UPDATE auto_queue_runs
             SET status = 'active'
             WHERE id = $1
               AND status IN ('generated', 'pending')",
        )
        .bind(run_id)
        .execute(pool)
        .await
    {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("promote postgres auto-queue run {run_id}: {error}")})),
        ));
    }
    if let Err(error) = crate::db::auto_queue::clear_inactive_slot_assignments_pg(pool).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                json!({"error": format!("clear inactive postgres auto-queue slots for {run_id}: {error}")}),
            ),
        ));
    }
    Ok(())
}

/// Counts the run's entries; when zero, best-effort completes the stale run
/// and short-circuits the activate request. Both the "stale empty run
/// completed" OK response and any DB failure are returned as
/// `Err(ActivateResponse)`; `Ok(())` means the run has entries to dispatch.
async fn complete_run_if_empty(
    pool: &sqlx::PgPool,
    run_id: &str,
    run_log_ctx: &AutoQueueLogContext<'_>,
) -> Result<(), ActivateResponse> {
    let entry_count = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await
    {
        Ok(count) => count,
        Err(error) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("count postgres auto-queue entries for {run_id}: {error}")}),
                ),
            ));
        }
    };
    if entry_count == 0 {
        if let Err(error) = sqlx::query(
            // #2048 F18: only auto-complete runs that are still active /
            // promotable. A caller passing a cancelled/completed run_id
            // should not flip its status — only the explicit cancel/complete
            // paths may finalize. The activate path is best-effort.
            "UPDATE auto_queue_runs
             SET status = 'completed',
                 completed_at = NOW()
             WHERE id = $1
               AND status IN ('active', 'paused', 'generated', 'pending')",
        )
        .bind(run_id)
        .execute(pool)
        .await
        {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("complete stale postgres auto-queue run {run_id}: {error}")}),
                ),
            ));
        }
        crate::auto_queue_log!(
            info,
            "activate_stale_empty_run_completed_pg",
            run_log_ctx.clone(),
            "[auto-queue] Completed stale empty PG run {run_id} — no entries, skipping fallback populate (#85)"
        );
        return Err((
            StatusCode::OK,
            Json(
                json!({ "dispatched": [], "count": 0, "message": "Stale empty run completed — no entries to dispatch" }),
            ),
        ));
    }
    Ok(())
}

/// Loads the run's `max_concurrent_threads` capacity and ensures slot-pool
/// rows exist for every agent referenced by the run's entries. Returns the
/// capacity on success; any DB/decode failure short-circuits with a 500
/// returned as `Err(ActivateResponse)`.
async fn load_activate_capacity_and_prepare_slots(
    pool: &sqlx::PgPool,
    run_id: &str,
) -> Result<i64, ActivateResponse> {
    let (max_concurrent, _thread_group_count) = match sqlx::query(
        "SELECT COALESCE(max_concurrent_threads, 1)::BIGINT AS max_concurrent_threads,
                COALESCE(thread_group_count, 1)::BIGINT AS thread_group_count
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await
    {
        Ok(row) => {
            let max_concurrent = match row.try_get::<i64, _>("max_concurrent_threads") {
                Ok(value) => value,
                Err(error) => {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode postgres auto-queue max_concurrent_threads for {run_id}: {error}")}),
                        ),
                    ));
                }
            };
            let thread_group_count = match row.try_get::<i64, _>("thread_group_count") {
                Ok(value) => value,
                Err(error) => {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode postgres auto-queue thread_group_count for {run_id}: {error}")}),
                        ),
                    ));
                }
            };
            (max_concurrent, thread_group_count)
        }
        Err(error) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres auto-queue run capacity for {run_id}: {error}")}),
                ),
            ));
        }
    };
    let run_agents_rows = match sqlx::query(
        "SELECT DISTINCT agent_id
         FROM auto_queue_entries
         WHERE run_id = $1",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres auto-queue run agents for {run_id}: {error}")}),
                ),
            ));
        }
    };
    for row in run_agents_rows {
        let agent_id: String = match row.try_get("agent_id") {
            Ok(value) => value,
            Err(error) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("decode postgres auto-queue run agent for {run_id}: {error}")}),
                    ),
                ));
            }
        };
        if let Err(error) =
            crate::db::auto_queue::ensure_agent_slot_pool_rows_pg(pool, &agent_id, max_concurrent)
                .await
        {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("prepare postgres slot pool rows for run {run_id} agent {agent_id}: {error}")}),
                ),
            ));
        }
    }
    Ok(max_concurrent)
}

/// Computes the ordered set of thread groups to attempt dispatching for this
/// activate call, alongside the concurrency counters (`active_turn_count`,
/// `active_group_count`) and `current_phase` the dispatch loop consults. The
/// group ordering and dedup semantics mirror the original inline logic exactly:
/// explicit `body.thread_group` first, then assigned-with-pending groups, then
/// active-group continuations, then phase-eligible pending groups. Any DB or
/// decode failure short-circuits with a 500 returned as `Err(ActivateResponse)`.
async fn compute_activate_groups_to_dispatch(
    pool: &sqlx::PgPool,
    run_id: &str,
    body: &ActivateBody,
) -> Result<ActivateGroupsPlan, ActivateResponse> {
    let current_phase = match crate::db::auto_queue::current_batch_phase_pg(pool, run_id).await {
        Ok(value) => value,
        Err(error) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres auto-queue current phase for {run_id}: {error}")}),
                ),
            ));
        }
    };
    let active_groups_rows = match sqlx::query(
        "SELECT DISTINCT COALESCE(thread_group, 0)::BIGINT AS thread_group
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status = 'dispatched'
         ORDER BY thread_group ASC",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres active groups for {run_id}: {error}")}),
                ),
            ));
        }
    };
    let active_groups: Vec<i64> = {
        let mut groups = Vec::with_capacity(active_groups_rows.len());
        for row in active_groups_rows {
            match row.try_get::<i64, _>("thread_group") {
                Ok(value) => groups.push(value),
                Err(error) => {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode postgres active group for {run_id}: {error}")}),
                        ),
                    ));
                }
            }
        }
        groups
    };
    let active_group_count = active_groups.len() as i64;
    // #2034: max_concurrent_threads now caps the total active turn count
    // across implementation + review + review-decision + rework + create-pr
    // dispatches (not just impl thread groups). Count any non-phase-gate
    // task_dispatch row in 'pending' or 'dispatched' status that belongs to
    // the run's agent(s). Phase-gate sidecar dispatches stay excluded so
    // gate evaluation does not occupy a concurrency slot.
    let active_turn_count = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM task_dispatches d
         CROSS JOIN LATERAL (SELECT COALESCE(NULLIF(d.context, ''), '{}')::jsonb AS ctx) c
         WHERE d.status IN ('pending', 'dispatched')
           AND COALESCE((c.ctx->>'sidecar_dispatch')::BOOLEAN, FALSE) = FALSE
           AND c.ctx->'phase_gate' IS NULL
           AND EXISTS (
               SELECT 1
               FROM auto_queue_entries e
               WHERE e.run_id = $1
                 AND e.agent_id = d.to_agent_id
           )",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres active turn count for {run_id}: {error}")}),
                ),
            ));
        }
    };
    let pending_group_rows = match sqlx::query(
        "SELECT DISTINCT COALESCE(thread_group, 0)::BIGINT AS thread_group,
                         COALESCE(batch_phase, 0)::BIGINT AS batch_phase
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status = 'pending'
         ORDER BY thread_group ASC, batch_phase ASC",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres pending groups for {run_id}: {error}")}),
                ),
            ));
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
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode postgres pending group for {run_id}: {error}")}),
                        ),
                    ));
                }
            };
            let batch_phase = match row.try_get::<i64, _>("batch_phase") {
                Ok(value) => value,
                Err(error) => {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode postgres pending batch_phase for {run_id}: {error}")}),
                        ),
                    ));
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
    let mut groups_to_dispatch = Vec::new();
    if let Some(group) = body.thread_group {
        let has_pending = match crate::db::auto_queue::group_has_pending_entries_pg(
            pool,
            run_id,
            group,
            current_phase,
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres pending group eligibility for {run_id}:{group}: {error}")}),
                    ),
                ));
            }
        };
        let has_dispatched = match group_has_dispatched_entries_pg(pool, run_id, group).await {
            Ok(value) => value,
            Err(error) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres dispatched group state for {run_id}:{group}: {error}")}),
                    ),
                ));
            }
        };
        if has_pending && !has_dispatched {
            groups_to_dispatch.push(group);
        }
    }
    match crate::db::auto_queue::assigned_groups_with_pending_entries_pg(
        pool,
        run_id,
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
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres assigned groups for {run_id}: {error}")}),
                ),
            ));
        }
    }

    for &group in &active_groups {
        let has_pending = match crate::db::auto_queue::group_has_pending_entries_pg(
            pool,
            run_id,
            group,
            current_phase,
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres continuation eligibility for {run_id}:{group}: {error}")}),
                    ),
                ));
            }
        };
        let has_dispatched = match group_has_dispatched_entries_pg(pool, run_id, group).await {
            Ok(value) => value,
            Err(error) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres dispatched continuation state for {run_id}:{group}: {error}")}),
                    ),
                ));
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

    Ok(ActivateGroupsPlan {
        groups_to_dispatch,
        active_group_count,
        active_turn_count,
        current_phase,
    })
}

/// Final activate phase: drains the run if no entries remain (releasing slots
/// and best-effort completing the run), recomputes the active/pending group
/// counts and post-activate turn count, and builds the success payload. Count
/// failures short-circuit with a 500 returned as `Err(ActivateResponse)`; the
/// success payload is returned as `Ok(ActivateResponse)`.
async fn finalize_activate_run_and_build_response(
    pool: &sqlx::PgPool,
    run_id: &str,
    run_log_ctx: &AutoQueueLogContext<'_>,
    dispatched: Vec<serde_json::Value>,
    deferred_entries: Vec<serde_json::Value>,
) -> Result<ActivateResponse, ActivateResponse> {
    let remaining = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("count postgres remaining entries for {run_id}: {error}")}),
                ),
            ));
        }
    };
    if remaining == 0 {
        if let Err(error) = crate::db::auto_queue::release_run_slots_pg(pool, run_id).await {
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
        .bind(run_id)
        .fetch_one(pool)
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("count postgres dispatched entries for {run_id}: {error}")}),
                    ),
                ));
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
            .bind(run_id)
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
    .bind(run_id)
    .fetch_one(pool)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("count postgres active groups for {run_id}: {error}")}),
                ),
            ));
        }
    };
    let pending_group_count = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(DISTINCT COALESCE(thread_group, 0))::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status = 'pending'",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("count postgres pending groups for {run_id}: {error}")}),
                ),
            ));
        }
    };

    // #2034: surface active_turn_count (impl + review + rework + create-pr,
    // excluding phase-gate sidecar dispatches) so the dashboard and ops
    // tooling can see when max_concurrent_threads is the limiting factor.
    let active_turn_count_after = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM task_dispatches d
         CROSS JOIN LATERAL (SELECT COALESCE(NULLIF(d.context, ''), '{}')::jsonb AS ctx) c
         WHERE d.status IN ('pending', 'dispatched')
           AND COALESCE((c.ctx->>'sidecar_dispatch')::BOOLEAN, FALSE) = FALSE
           AND c.ctx->'phase_gate' IS NULL
           AND EXISTS (
               SELECT 1
               FROM auto_queue_entries e
               WHERE e.run_id = $1
                 AND e.agent_id = d.to_agent_id
           )",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    // feature: rate-limit-aware-dispatch-gate (REQ-004). `deferred_entries` and
    // `deferred_count` are ADDITIVE fields: existing keys
    // (dispatched/count/active_groups/active_turn_count/pending_groups) are
    // unchanged and never removed/retyped. The fields are only attached when at
    // least one entry was gated, so the response shape is byte-identical to the
    // pre-feature contract on the common no-deferral path.
    let mut body = json!({
        "dispatched": dispatched,
        "count": dispatched.len(),
        "active_groups": active_group_count,
        "active_turn_count": active_turn_count_after,
        "pending_groups": pending_group_count,
    });
    if !deferred_entries.is_empty() {
        if let Some(map) = body.as_object_mut() {
            map.insert("deferred_count".to_string(), json!(deferred_entries.len()));
            map.insert("deferred_entries".to_string(), json!(deferred_entries));
        }
    }

    Ok((StatusCode::OK, Json(body)))
}

fn activate_fallback_capacity_reached(
    active_turn_count: i64,
    active_group_count: i64,
    new_dispatches_this_activate: i64,
    max_concurrent: i64,
) -> bool {
    (active_turn_count + new_dispatches_this_activate) >= max_concurrent
        // Legacy guard preserved as a no-op fallback so that impl-only runs
        // (where active_turn_count == active_group_count) keep the same
        // termination behaviour even if turn_count is briefly under-counted
        // due to dispatch row write lag.
        || (active_group_count + new_dispatches_this_activate) >= max_concurrent
}

/// #2048 F4: RAII guard that releases the per-run `aq_activate` session
/// advisory lock acquired at the start of `activate_with_deps_pg`. We can't
/// hold the lock across the (many) early `return` sites without a closure
/// rewrite, so a Drop impl keeps the unlock-on-exit invariant intact even
/// for panic paths. Unlock runs on a detached tokio task; if the runtime is
/// unavailable, Postgres releases session locks when the session ends.
struct ActivateLockReleaseGuard {
    conn: Option<sqlx::pool::PoolConnection<sqlx::Postgres>>,
    run_id: String,
}

#[cfg(test)]
mod tests {
    use super::activate_fallback_capacity_reached;

    #[test]
    fn fallback_capacity_counts_only_new_dispatches_this_activate() {
        let active_turn_count = 2;
        let active_group_count = 2;
        let max_concurrent = 3;

        assert!(
            !activate_fallback_capacity_reached(
                active_turn_count,
                active_group_count,
                0,
                max_concurrent,
            ),
            "visiting already-active or reattached groups must not consume the remaining slot"
        );
        assert!(
            activate_fallback_capacity_reached(
                active_turn_count,
                active_group_count,
                1,
                max_concurrent,
            ),
            "one newly-created dispatch should consume the last available slot"
        );
    }
}

impl ActivateLockReleaseGuard {
    fn new(conn: sqlx::pool::PoolConnection<sqlx::Postgres>, run_id: String) -> Self {
        Self {
            conn: Some(conn),
            run_id,
        }
    }
}

impl Drop for ActivateLockReleaseGuard {
    fn drop(&mut self) {
        let Some(mut conn) = self.conn.take() else {
            return;
        };
        let run_id = std::mem::take(&mut self.run_id);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(error) =
                    sqlx::query("SELECT pg_advisory_unlock(hashtext($1), hashtext($2))")
                        .bind("aq_activate")
                        .bind(&run_id)
                        .execute(&mut *conn)
                        .await
                {
                    tracing::warn!(
                        run_id,
                        error = %error,
                        "[auto-queue] failed to release activate advisory lock"
                    );
                }
            });
        }
    }
}
