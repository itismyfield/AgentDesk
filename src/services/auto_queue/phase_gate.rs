async fn create_activate_dispatch_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<String, String> {
    if dispatch_type != "review-decision"
        && let Some(existing_id) = sqlx::query_scalar::<_, String>(
            "SELECT id
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = $2
               AND status IN ('pending', 'dispatched')
             ORDER BY created_at DESC
             LIMIT 1",
        )
        .bind(card_id)
        .bind(dispatch_type)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("lookup active postgres dispatch for {card_id}: {error}"))?
    {
        return Ok(existing_id);
    }

    let row = sqlx::query(
        "SELECT status,
                review_status,
                latest_dispatch_id,
                repo_id,
                assigned_agent_id,
                github_issue_number::BIGINT AS github_issue_number
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres dispatch card {card_id}: {error}"))?
    .ok_or_else(|| format!("card {card_id} not found"))?;

    let old_status: String = row
        .try_get("status")
        .map_err(|error| format!("decode old status for {card_id}: {error}"))?;
    let review_status: Option<String> = row
        .try_get("review_status")
        .map_err(|error| format!("decode review_status for {card_id}: {error}"))?;
    let latest_dispatch_id: Option<String> = row
        .try_get("latest_dispatch_id")
        .map_err(|error| format!("decode latest_dispatch_id for {card_id}: {error}"))?;
    let repo_id: Option<String> = row
        .try_get("repo_id")
        .map_err(|error| format!("decode repo_id for {card_id}: {error}"))?;
    let assigned_agent_id: Option<String> = row
        .try_get("assigned_agent_id")
        .map_err(|error| format!("decode assigned_agent_id for {card_id}: {error}"))?;
    let github_issue_number: Option<i64> = row
        .try_get("github_issue_number")
        .map_err(|error| format!("decode github_issue_number for {card_id}: {error}"))?;

    let agent_exists =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM agents WHERE id = $1")
            .bind(to_agent_id)
            .fetch_one(pool)
            .await
            .map_err(|error| format!("check postgres dispatch agent {to_agent_id}: {error}"))?
            > 0;
    if !agent_exists {
        return Err(format!(
            "Cannot create {dispatch_type} dispatch: agent '{to_agent_id}' not found (card {card_id})"
        ));
    }

    let channel_value = crate::db::agents::resolve_agent_dispatch_channel_pg(
        pool,
        to_agent_id,
        Some(dispatch_type),
    )
    .await
    .map_err(|error| {
        format!("resolve postgres dispatch channel for {to_agent_id} ({dispatch_type}): {error}")
    })?
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty())
    .ok_or_else(|| {
        format!(
            "Cannot create {dispatch_type} dispatch: agent '{to_agent_id}' has no discord channel (card {card_id})"
        )
    })?;
    if resolve_activate_dispatch_channel_id(&channel_value).is_none() {
        return Err(format!(
            "Cannot create {dispatch_type} dispatch: agent '{to_agent_id}' has invalid discord channel '{channel_value}' (card {card_id})"
        ));
    }

    let effective =
        resolve_activate_pipeline_pg(pool, repo_id.as_deref(), assigned_agent_id.as_deref())
            .await?;
    if effective.is_terminal(&old_status) {
        return Err(format!(
            "Cannot create {dispatch_type} dispatch for terminal card {card_id} (status: {old_status})"
        ));
    }

    let mut context_with_strategy = if context.is_object() {
        context.clone()
    } else {
        json!({})
    };
    if let Some(default_force_new_session) =
        crate::dispatch::dispatch_type_force_new_session_default(Some(dispatch_type))
        && let Some(obj) = context_with_strategy.as_object_mut()
    {
        obj.entry("force_new_session".to_string())
            .or_insert(json!(default_force_new_session));
    }
    if let Some(obj) = context_with_strategy.as_object_mut() {
        if let Some(repo_id) = repo_id.as_deref() {
            obj.entry("repo".to_string())
                .or_insert_with(|| json!(repo_id));
            obj.entry("target_repo".to_string())
                .or_insert_with(|| json!(repo_id));
        }
        if let Some(issue_number) = github_issue_number {
            obj.entry("issue_number".to_string())
                .or_insert_with(|| json!(issue_number));
        }
    }
    if let Ok(Some((worktree_path, worktree_branch, _))) =
        crate::dispatch::resolve_card_worktree(pool, card_id, Some(&context_with_strategy)).await
        && let Some(obj) = context_with_strategy.as_object_mut()
    {
        obj.entry("worktree_path".to_string())
            .or_insert_with(|| json!(worktree_path));
        obj.entry("worktree_branch".to_string())
            .or_insert_with(|| json!(worktree_branch));
    }

    let parent_dispatch_id = context_with_strategy
        .get("parent_dispatch_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let chain_depth = if let Some(parent_dispatch_id) = parent_dispatch_id.as_deref() {
        sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(chain_depth, 0)::BIGINT + 1
             FROM task_dispatches
             WHERE id = $1",
        )
        .bind(parent_dispatch_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            format!("load parent dispatch chain depth for {parent_dispatch_id}: {error}")
        })?
        .unwrap_or(1)
    } else {
        0
    };

    let dispatch_id = uuid::Uuid::new_v4().to_string();
    let kickoff_state = if matches!(
        dispatch_type,
        "review" | "review-decision" | "rework" | "consultation"
    ) {
        None
    } else {
        Some(
            effective
                .kickoff_for(&old_status)
                .unwrap_or_else(|| effective.initial_state().to_string()),
        )
    };
    let decision = crate::engine::transition::decide_transition(
        &crate::engine::transition::TransitionContext {
            card: crate::engine::transition::CardState {
                id: card_id.to_string(),
                status: old_status.clone(),
                review_status,
                latest_dispatch_id,
            },
            pipeline: effective.clone(),
            gates: crate::engine::transition::GateSnapshot::default(),
        },
        &crate::engine::transition::TransitionEvent::DispatchAttached {
            dispatch_id: dispatch_id.clone(),
            dispatch_type: dispatch_type.to_string(),
            kickoff_state,
        },
    );
    if let crate::engine::transition::TransitionOutcome::Blocked(reason) = &decision.outcome {
        return Err(reason.clone());
    }

    let context_str = serde_json::to_string(&context_with_strategy)
        .map_err(|error| format!("encode dispatch context for {card_id}: {error}"))?;
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("open postgres activate dispatch transaction: {error}"))?;

    if dispatch_type != "review-decision"
        && let Some(existing_id) = sqlx::query_scalar::<_, String>(
            "SELECT id
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = $2
               AND status IN ('pending', 'dispatched')
             ORDER BY created_at DESC
             LIMIT 1",
        )
        .bind(card_id)
        .bind(dispatch_type)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| {
            format!("recheck active postgres dispatch for {card_id} during create: {error}")
        })?
    {
        tx.rollback().await.ok();
        return Ok(existing_id);
    }

    sqlx::query(
        "INSERT INTO task_dispatches (
            id,
            kanban_card_id,
            to_agent_id,
            dispatch_type,
            status,
            title,
            context,
            parent_dispatch_id,
            chain_depth,
            created_at,
            updated_at
        ) VALUES (
            $1, $2, $3, $4, 'pending', $5, $6, $7, $8, NOW(), NOW()
        )",
    )
    .bind(&dispatch_id)
    .bind(card_id)
    .bind(to_agent_id)
    .bind(dispatch_type)
    .bind(title)
    .bind(&context_str)
    .bind(parent_dispatch_id.as_deref())
    .bind(chain_depth)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("insert postgres dispatch {dispatch_id} for {card_id}: {error}"))?;

    sqlx::query(
        "INSERT INTO dispatch_events (
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload_json
        ) VALUES (
            $1, $2, $3, NULL, 'pending', 'create_dispatch', NULL
        )",
    )
    .bind(&dispatch_id)
    .bind(card_id)
    .bind(dispatch_type)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("insert postgres dispatch event for {dispatch_id}: {error}"))?;

    sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title)
         VALUES ($1, 'notify', $2, $3, $4)
         ON CONFLICT DO NOTHING",
    )
    .bind(&dispatch_id)
    .bind(to_agent_id)
    .bind(card_id)
    .bind(title)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("insert postgres dispatch outbox for {dispatch_id}: {error}"))?;

    for intent in &decision.intents {
        crate::engine::transition_executor_pg::execute_activate_transition_intent_pg(
            &mut tx, intent,
        )
        .await?;
    }

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres dispatch {dispatch_id}: {error}"))?;

    Ok(dispatch_id)
}

