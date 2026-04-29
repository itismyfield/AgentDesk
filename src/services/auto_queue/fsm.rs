fn attempt_restore_dispatch(
    deps: &AutoQueueActivateDeps,
    run_id: &str,
    candidate: &RestoreDispatchCandidate,
) -> Result<RestoreDispatchAttemptResult, String> {
    let entry = &candidate.entry;
    let entry_log_ctx = AutoQueueLogContext::new()
        .run(run_id)
        .entry(&entry.entry_id)
        .card(&entry.card_id)
        .agent(&entry.agent_id)
        .thread_group(entry.thread_group);
    let card_state = load_activate_card_state_prefer_pg(deps, &entry.card_id, &entry.entry_id)
        .map_err(|error| format!("{}: eager restore reload failed: {error}", entry.entry_id))?;
    if card_state.entry_status != crate::db::auto_queue::ENTRY_STATUS_PENDING {
        return Ok(RestoreDispatchAttemptResult::default());
    }

    if card_state.has_active_dispatch() {
        let dispatch_id = card_state.latest_dispatch_id.clone().ok_or_else(|| {
            format!(
                "{}: active dispatch state missing dispatch id during eager restore",
                entry.entry_id
            )
        })?;
        let slot_allocation = allocate_slot_for_group_agent_prefer_pg(
            deps,
            run_id,
            entry.thread_group,
            &entry.agent_id,
        )
        .map_err(|error| {
            format!(
                "{}: eager existing dispatch slot allocation failed: {error}",
                entry.entry_id
            )
        })?;
        let slot_index = slot_allocation
            .as_ref()
            .map(|allocation| allocation.slot_index);
        let mut result = RestoreDispatchAttemptResult::default();
        if let Some(allocation) = slot_allocation {
            if allocation.newly_assigned || allocation.reassigned_from_other_group {
                result.rebound_slot = true;
            }
        } else {
            result.unbound_dispatch = true;
        }
        match update_entry_status_prefer_pg(
            deps,
            &entry.entry_id,
            crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
            "restore_run_attach_existing_dispatch",
            &crate::db::auto_queue::EntryStatusUpdateOptions {
                dispatch_id: Some(dispatch_id),
                slot_index,
            },
        ) {
            Ok(_) => {
                result.dispatched = true;
                return Ok(result);
            }
            Err(error) => {
                return Err(format!(
                    "{}: eager attach existing dispatch failed: {error}",
                    entry.entry_id
                ));
            }
        }
    }

    let slot_allocation =
        allocate_slot_for_group_agent_prefer_pg(deps, run_id, entry.thread_group, &entry.agent_id)
            .map_err(|error| {
                format!(
                    "{}: eager restore slot allocation failed: {error}",
                    entry.entry_id
                )
            })?;
    let slot_index = slot_allocation
        .as_ref()
        .map(|allocation| allocation.slot_index);
    let mut result = RestoreDispatchAttemptResult::default();
    let reset_slot_thread_before_reuse = if let Some(allocation) = slot_allocation {
        let reset = slot_requires_thread_reset_before_reuse_prefer_pg(
            deps,
            &entry.agent_id,
            allocation.slot_index,
            allocation.newly_assigned,
            allocation.reassigned_from_other_group,
        )?;
        if allocation.newly_assigned || allocation.reassigned_from_other_group {
            result.rebound_slot = true;
        }
        reset
    } else {
        return Ok(result);
    };
    match update_entry_status_prefer_pg(
        deps,
        &entry.entry_id,
        crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
        "restore_run_dispatch_reserve",
        &crate::db::auto_queue::EntryStatusUpdateOptions {
            dispatch_id: None,
            slot_index,
        },
    ) {
        Ok(update) if !update.changed => return Ok(result),
        Ok(_) => {}
        Err(error) => {
            return Err(format!(
                "{}: eager restore reservation failed: {error}",
                entry.entry_id
            ));
        }
    }

    let dispatch_context = build_auto_queue_dispatch_context(
        &entry.entry_id,
        entry.thread_group,
        slot_index,
        reset_slot_thread_before_reuse,
        [("restored_run", json!(true)), ("run_id", json!(run_id))],
    );
    let dispatch_result = create_activate_dispatch_prefer_pg(
        deps,
        &entry.card_id,
        &entry.agent_id,
        "implementation",
        &candidate.title,
        &dispatch_context,
    );
    let created_dispatch = dispatch_result.is_ok();

    let dispatch_id = match dispatch_result {
        Ok(dispatch_id) => Some(dispatch_id),
        Err(error) => {
            let error_text = error.to_string();
            crate::auto_queue_log!(
                warn,
                "restore_run_create_dispatch_failed",
                entry_log_ctx.clone().maybe_slot_index(slot_index),
                "[auto-queue] restore_run create_dispatch failed for entry {}: {}",
                entry.entry_id,
                error_text
            );
            let recovered_dispatch =
                load_activate_card_state_prefer_pg(deps, &entry.card_id, &entry.entry_id)
                    .ok()
                    .filter(|state| state.has_active_dispatch())
                    .and_then(|state| state.latest_dispatch_id);
            if recovered_dispatch.is_none() {
                let failure = record_entry_dispatch_failure(
                    deps,
                    run_id,
                    &entry.entry_id,
                    &entry.card_id,
                    &entry.agent_id,
                    entry.thread_group,
                    slot_index,
                    "restore_run_create_dispatch_failed",
                    &error_text,
                    &entry_log_ctx,
                )?;
                crate::auto_queue_log!(
                    warn,
                    "restore_run_create_dispatch_retry_scheduled",
                    entry_log_ctx.clone().maybe_slot_index(slot_index),
                    "[auto-queue] restore_run dispatch failure for entry {} scheduled retry {}/{} -> {}",
                    entry.entry_id,
                    failure.retry_count,
                    failure.retry_limit,
                    failure.to_status
                );
            }
            recovered_dispatch
        }
    };

    let Some(dispatch_id) = dispatch_id else {
        return Ok(result);
    };

    match update_entry_status_prefer_pg(
        deps,
        &entry.entry_id,
        crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
        "restore_run_create_dispatch",
        &crate::db::auto_queue::EntryStatusUpdateOptions {
            dispatch_id: Some(dispatch_id.clone()),
            slot_index,
        },
    ) {
        Ok(_) => {
            result.dispatched = true;
            result.created_dispatch = created_dispatch;
            Ok(result)
        }
        Err(error) => {
            crate::auto_queue_log!(
                warn,
                "restore_run_mark_dispatched_failed",
                entry_log_ctx
                    .clone()
                    .dispatch(&dispatch_id)
                    .maybe_slot_index(slot_index),
                "[auto-queue] failed to mark restored entry {} dispatched after create_dispatch: {}",
                entry.entry_id,
                error
            );
            Ok(result)
        }
    }
}

async fn load_restore_entries_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<Vec<RestoreEntryRecord>, String> {
    let rows = sqlx::query(
        "SELECT id, kanban_card_id, agent_id, COALESCE(thread_group, 0)::BIGINT AS thread_group
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status = 'skipped'
         ORDER BY priority_rank ASC, created_at ASC, id ASC",
    )
    .bind(run_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("load postgres restore entries for {run_id}: {error}"))?;

    rows.into_iter()
        .map(|row| {
            Ok(RestoreEntryRecord {
                entry_id: row
                    .try_get("id")
                    .map_err(|error| format!("decode restore entry id for {run_id}: {error}"))?,
                card_id: row.try_get("kanban_card_id").map_err(|error| {
                    format!("decode restore entry card_id for {run_id}: {error}")
                })?,
                agent_id: row.try_get("agent_id").map_err(|error| {
                    format!("decode restore entry agent_id for {run_id}: {error}")
                })?,
                thread_group: row.try_get("thread_group").map_err(|error| {
                    format!("decode restore entry thread_group for {run_id}: {error}")
                })?,
            })
        })
        .collect()
}

async fn decide_restore_transition_pg(
    pool: &sqlx::PgPool,
    entry: &RestoreEntryRecord,
) -> Result<RestoreEntryDecision, String> {
    let card_state = load_activate_card_state_pg(pool, &entry.card_id, &entry.entry_id).await?;
    let dispatch_history =
        crate::db::auto_queue::list_entry_dispatch_history_pg(pool, &entry.entry_id)
            .await
            .map_err(|error| {
                format!(
                    "load postgres dispatch history for restore entry {}: {error}",
                    entry.entry_id
                )
            })?;

    if dispatch_history.is_empty() {
        return Ok(RestoreEntryDecision::Pending);
    }

    let pipeline = resolve_activate_pipeline_pg(
        pool,
        card_state.repo_id.as_deref(),
        card_state.assigned_agent_id.as_deref(),
    )
    .await?;
    if pipeline.is_terminal(&card_state.status) {
        return Ok(RestoreEntryDecision::Done);
    }

    if card_state.has_active_dispatch() {
        if let Some(dispatch_id) = card_state.latest_dispatch_id {
            return Ok(RestoreEntryDecision::ExistingDispatch {
                dispatch_id,
                title: card_state.title,
            });
        }
    }

    Ok(RestoreEntryDecision::NewDispatch {
        title: card_state.title,
    })
}

async fn apply_restore_state_changes_pg(
    pool: &sqlx::PgPool,
    run_id: &str,
    run_status: Option<&str>,
) -> Result<(RestoreRunCounts, Vec<RestoreDispatchCandidate>), String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("open postgres restore transaction failed: {error}"))?;
    if run_status == Some("cancelled") {
        let restored_run = sqlx::query(
            "UPDATE auto_queue_runs
             SET status = $2,
                 completed_at = NULL
             WHERE id = $1
               AND status = 'cancelled'",
        )
        .bind(run_id)
        .bind(RUN_STATUS_RESTORING)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            format!("failed to start postgres restore for cancelled run '{run_id}': {error}")
        })?
        .rows_affected();
        if restored_run == 0 {
            return Err(format!(
                "failed to start postgres restore for cancelled run '{run_id}'"
            ));
        }
    }

    let entries = load_restore_entries_pg(&mut tx, run_id).await?;
    let mut counts = RestoreRunCounts::default();
    let mut dispatch_candidates = Vec::new();

    for entry in entries {
        match decide_restore_transition_pg(pool, &entry).await {
            Ok(RestoreEntryDecision::Pending) => {
                match crate::db::auto_queue::update_entry_status_on_pg_tx(
                    &mut tx,
                    &entry.entry_id,
                    crate::db::auto_queue::ENTRY_STATUS_PENDING,
                    "restore_run_pending",
                    &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                )
                .await
                {
                    Ok(result) if result.changed => counts.restored_pending += 1,
                    Ok(_) => {}
                    Err(error) => {
                        return Err(format!(
                            "{}: restore to pending failed: {error}",
                            entry.entry_id
                        ));
                    }
                }
            }
            Ok(RestoreEntryDecision::Done) => {
                match crate::db::auto_queue::update_entry_status_on_pg_tx(
                    &mut tx,
                    &entry.entry_id,
                    crate::db::auto_queue::ENTRY_STATUS_DONE,
                    "restore_run_done",
                    &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                )
                .await
                {
                    Ok(result) if result.changed => counts.restored_done += 1,
                    Ok(_) => {}
                    Err(error) => {
                        return Err(format!(
                            "{}: restore to done failed: {error}",
                            entry.entry_id
                        ));
                    }
                }
            }
            Ok(RestoreEntryDecision::ExistingDispatch { title, .. })
            | Ok(RestoreEntryDecision::NewDispatch { title }) => {
                match crate::db::auto_queue::update_entry_status_on_pg_tx(
                    &mut tx,
                    &entry.entry_id,
                    crate::db::auto_queue::ENTRY_STATUS_PENDING,
                    "restore_run_pending_new_dispatch",
                    &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                )
                .await
                {
                    Ok(result) if result.changed => counts.restored_pending += 1,
                    Ok(_) => {}
                    Err(error) => {
                        return Err(format!(
                            "{}: restore pending for redispatch failed: {error}",
                            entry.entry_id
                        ));
                    }
                }
                dispatch_candidates.push(RestoreDispatchCandidate { entry, title });
            }
            Err(error) => {
                return Err(format!(
                    "{}: decide restore transition failed: {error}",
                    entry.entry_id
                ));
            }
        }
    }

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres restore state failed: {error}"))?;
    Ok((counts, dispatch_candidates))
}

async fn finalize_restore_run_pg(pool: &sqlx::PgPool, run_id: &str) -> Result<(), String> {
    let finalized = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'active',
             completed_at = NULL
         WHERE id = $1
           AND status = $2",
    )
    .bind(run_id)
    .bind(RUN_STATUS_RESTORING)
    .execute(pool)
    .await
    .map_err(|error| format!("failed to finalize postgres restore for run '{run_id}': {error}"))?
    .rows_affected();
    if finalized > 0 {
        return Ok(());
    }

    let current_status = sqlx::query_scalar::<_, String>(
        "SELECT status
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("reload postgres restore status for run '{run_id}': {error}"))?;
    match current_status.as_deref() {
        Some("active") => Ok(()),
        Some(status) => Err(format!(
            "failed to finalize postgres restore for run '{run_id}' (status={status})"
        )),
        None => Err(format!(
            "failed to finalize postgres restore for missing run '{run_id}'"
        )),
    }
}

#[derive(Clone)]
pub(crate) struct AutoQueueActivateDeps {
    pg_pool: Option<sqlx::PgPool>,
    engine: crate::engine::PolicyEngine,
    config: Arc<crate::config::Config>,
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    guild_id: Option<String>,
}

impl AutoQueueActivateDeps {
    fn from_state(state: &AppState) -> Self {
        Self {
            pg_pool: state.pg_pool.clone(),
            engine: state.engine.clone(),
            config: state.config.clone(),
            health_registry: state.health_registry.clone(),
            guild_id: state.config.discord.guild_id.clone(),
        }
    }

    pub(crate) fn for_bridge(_db: crate::db::Db, engine: crate::engine::PolicyEngine) -> Self {
        Self {
            pg_pool: engine.pg_pool().cloned(),
            engine,
            config: Arc::new(crate::config::Config::default()),
            health_registry: None,
            guild_id: None,
        }
    }

    fn auto_queue_service(&self) -> crate::services::auto_queue::AutoQueueService {
        crate::services::auto_queue::AutoQueueService::new(self.engine.clone())
    }

    fn entry_json(&self, entry_id: &str) -> serde_json::Value {
        self.auto_queue_service()
            .entry_json(entry_id, self.guild_id.as_deref())
            .unwrap_or(serde_json::Value::Null)
    }

    async fn entry_json_pg(&self, pool: &sqlx::PgPool, entry_id: &str) -> serde_json::Value {
        self.auto_queue_service()
            .entry_json_with_pg(pool, entry_id, self.guild_id.as_deref())
            .await
            .unwrap_or(serde_json::Value::Null)
    }

    fn entry_json_prefer_pg(&self, entry_id: &str) -> serde_json::Value {
        if let Some(pool) = self.pg_pool.as_ref() {
            let entry_id = entry_id.to_string();
            let guild_id = self.guild_id.clone();
            let engine = self.engine.clone();
            return crate::utils::async_bridge::block_on_pg_result(
                pool,
                move |bridge_pool| async move {
                    Ok::<serde_json::Value, String>(
                        crate::services::auto_queue::AutoQueueService::new(engine)
                            .entry_json_with_pg(&bridge_pool, &entry_id, guild_id.as_deref())
                            .await
                            .unwrap_or(serde_json::Value::Null),
                    )
                },
                |error| error,
            )
            .unwrap_or(serde_json::Value::Null);
        }
        serde_json::Value::Null
    }
}

fn load_activate_card_state_prefer_pg(
    deps: &AutoQueueActivateDeps,
    card_id: &str,
    entry_id: &str,
) -> Result<ActivateCardState, String> {
    if let Some(pool) = deps.pg_pool.as_ref() {
        let card_id = card_id.to_string();
        let entry_id = entry_id.to_string();
        return crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |bridge_pool| async move {
                load_activate_card_state_pg(&bridge_pool, &card_id, &entry_id).await
            },
            |error| error,
        );
    }

    let _ = (card_id, entry_id);
    Err("postgres backend required for auto-queue activation".to_string())
}

fn update_entry_status_prefer_pg(
    deps: &AutoQueueActivateDeps,
    entry_id: &str,
    new_status: &str,
    trigger_source: &str,
    options: &crate::db::auto_queue::EntryStatusUpdateOptions,
) -> Result<crate::db::auto_queue::EntryStatusUpdateResult, String> {
    let Some(pool) = deps.pg_pool.as_ref() else {
        return Err(format!(
            "{entry_id}: postgres backend is required to update auto-queue entry"
        ));
    };
    let entry_id_owned = entry_id.to_string();
    let new_status = new_status.to_string();
    let trigger_source = trigger_source.to_string();
    let options = options.clone();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            crate::db::auto_queue::update_entry_status_on_pg(
                &bridge_pool,
                &entry_id_owned,
                &new_status,
                &trigger_source,
                &options,
            )
            .await
        },
        |error| error,
    )
}

fn allocate_slot_for_group_agent_prefer_pg(
    deps: &AutoQueueActivateDeps,
    run_id: &str,
    thread_group: i64,
    agent_id: &str,
) -> Result<Option<crate::db::auto_queue::SlotAllocation>, String> {
    let Some(pool) = deps.pg_pool.as_ref() else {
        return Err(format!(
            "postgres backend required for auto-queue slot allocation ({run_id}:{thread_group})"
        ));
    };
    let run_id = run_id.to_string();
    let agent_id = agent_id.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            crate::db::auto_queue::allocate_slot_for_group_agent_pg(
                &bridge_pool,
                &run_id,
                thread_group,
                &agent_id,
            )
            .await
        },
        |error| error,
    )
}

fn slot_requires_thread_reset_before_reuse_prefer_pg(
    deps: &AutoQueueActivateDeps,
    agent_id: &str,
    slot_index: i64,
    newly_assigned: bool,
    reassigned_from_other_group: bool,
) -> Result<bool, String> {
    if let Some(pool) = deps.pg_pool.as_ref() {
        let agent_id = agent_id.to_string();
        return crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |bridge_pool| async move {
                slot_requires_thread_reset_before_reuse_pg(
                    &bridge_pool,
                    &agent_id,
                    slot_index,
                    newly_assigned,
                    reassigned_from_other_group,
                )
                .await
            },
            |error| error,
        );
    }

    let _ = (
        agent_id,
        slot_index,
        newly_assigned,
        reassigned_from_other_group,
    );
    Err("postgres backend required for auto-queue slot reset".to_string())
}

async fn select_consultation_counterpart_pg(
    pool: &sqlx::PgPool,
    agent_id: &str,
) -> Result<String, String> {
    let provider = sqlx::query_scalar::<_, String>(
        "SELECT COALESCE(provider, 'claude')
         FROM agents
         WHERE id = $1",
    )
    .bind(agent_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres provider for agent {agent_id}: {error}"))?
    .map(|raw| ProviderKind::from_str_or_unsupported(&raw))
    .unwrap_or_else(|| ProviderKind::default_channel_provider().unwrap_or(ProviderKind::Claude));

    let rows = sqlx::query(
        "SELECT id, COALESCE(provider, 'claude') AS provider
         FROM agents
         WHERE id != $1
         ORDER BY id ASC",
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load postgres consultation counterparts for {agent_id}: {error}"))?;

    let mut available_agents = Vec::with_capacity(rows.len());
    for row in rows {
        let candidate_id: String = row
            .try_get("id")
            .map_err(|error| format!("decode postgres counterpart id for {agent_id}: {error}"))?;
        let provider_raw: String = row.try_get("provider").map_err(|error| {
            format!("decode postgres counterpart provider for {candidate_id}: {error}")
        })?;
        available_agents.push((
            candidate_id,
            ProviderKind::from_str_or_unsupported(&provider_raw),
        ));
    }

    Ok(provider
        .select_counterpart_from(
            available_agents
                .iter()
                .map(|(_, candidate_provider)| candidate_provider.clone()),
        )
        .and_then(|counterpart| {
            available_agents
                .iter()
                .find_map(|(candidate_id, candidate_provider)| {
                    (*candidate_provider == counterpart).then_some(candidate_id.clone())
                })
        })
        .unwrap_or_else(|| agent_id.to_string()))
}

fn select_consultation_counterpart_prefer_pg(
    deps: &AutoQueueActivateDeps,
    agent_id: &str,
) -> Result<String, String> {
    let Some(pool) = deps.pg_pool.as_ref() else {
        return Err(format!(
            "postgres backend is required to select consultation counterpart for {agent_id}"
        ));
    };
    let agent_id_owned = agent_id.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            select_consultation_counterpart_pg(&bridge_pool, &agent_id_owned).await
        },
        |error| error,
    )
}

fn record_consultation_dispatch_prefer_pg(
    deps: &AutoQueueActivateDeps,
    entry_id: &str,
    card_id: &str,
    dispatch_id: &str,
    trigger_source: &str,
    base_metadata_json: &str,
) -> Result<crate::db::auto_queue::ConsultationDispatchRecordResult, String> {
    let Some(pool) = deps.pg_pool.as_ref() else {
        return Err(format!(
            "{entry_id}: postgres backend is required to record consultation dispatch"
        ));
    };
    let entry_id_owned = entry_id.to_string();
    let card_id = card_id.to_string();
    let dispatch_id = dispatch_id.to_string();
    let trigger_source = trigger_source.to_string();
    let base_metadata_json = base_metadata_json.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            crate::db::auto_queue::record_consultation_dispatch_on_pg(
                &bridge_pool,
                &entry_id_owned,
                &card_id,
                &dispatch_id,
                &trigger_source,
                &base_metadata_json,
            )
            .await
        },
        |error| error,
    )
}

fn create_activate_dispatch_prefer_pg(
    deps: &AutoQueueActivateDeps,
    card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<String, String> {
    if let Some(pool) = deps.pg_pool.as_ref() {
        let card_id = card_id.to_string();
        let to_agent_id = to_agent_id.to_string();
        let dispatch_type = dispatch_type.to_string();
        let title = title.to_string();
        let context = context.clone();
        return crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |bridge_pool| async move {
                create_activate_dispatch_pg(
                    &bridge_pool,
                    &card_id,
                    &to_agent_id,
                    &dispatch_type,
                    &title,
                    &context,
                )
                .await
            },
            |error| error,
        );
    }

    let _ = (deps, card_id, to_agent_id, dispatch_type, title, context);
    Err("postgres backend required for auto-queue dispatch creation".to_string())
}

pub(crate) async fn activate_with_bridge_pg(
    _db: Option<crate::db::Db>,
    engine: crate::engine::PolicyEngine,
    body: ActivateBody,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pg_pool) = engine.pg_pool().cloned() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool is not configured"})),
        );
    };
    let deps = AutoQueueActivateDeps {
        pg_pool: Some(pg_pool),
        engine,
        config: Arc::new(crate::config::Config::default()),
        health_registry: None,
        guild_id: None,
    };
    activate_with_deps_pg(&deps, body).await
}

enum ActivatePreflightOutcome {
    Continue,
    Dispatched(serde_json::Value),
    Skipped,
    Deferred,
}

fn run_activate_blocking<T, F>(operation: F) -> T
where
    F: FnOnce() -> T,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(operation)
    } else {
        operation()
    }
}

fn clamp_retry_limit(value: u64) -> i64 {
    value.max(1).min(i64::MAX as u64) as i64
}

fn load_kv_meta_value_pg(pool: &sqlx::PgPool, key: &str) -> Result<Option<String>, String> {
    let key_text = key.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            sqlx::query_scalar::<_, String>(
                "SELECT value
                 FROM kv_meta
                 WHERE key = $1
                   AND (expires_at IS NULL OR expires_at > NOW())
                 LIMIT 1",
            )
            .bind(&key_text)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|error| format!("load postgres kv_meta {key_text}: {error}"))
        },
        |error| error,
    )
}
