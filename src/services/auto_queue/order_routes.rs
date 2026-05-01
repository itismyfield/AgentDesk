use super::*;

// ── Authenticated order submission callback ─────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct OrderBody {
    /// Ordered list of GitHub issue numbers (or card IDs)
    pub order: Vec<serde_json::Value>,
    pub rationale: Option<String>,
    /// Alias for rationale (compatibility)
    pub reasoning: Option<String>,
}

/// POST /api/queue/runs/:id/order
/// Authenticated callback: provides the ordered card list for a pending run.
pub(super) async fn resolve_submit_order_card_with_pg(
    pool: &sqlx::PgPool,
    run_repo: Option<&str>,
    item: &serde_json::Value,
) -> Result<Option<ResolvedDispatchCard>, String> {
    let row = if let Some(issue_number) = item.as_i64() {
        sqlx::query(
            "SELECT id,
                    repo_id,
                    status,
                    assigned_agent_id,
                    github_issue_number::BIGINT AS github_issue_number
             FROM kanban_cards
             WHERE github_issue_number = $1
               AND ($2::TEXT IS NULL OR repo_id = $2)
             ORDER BY id ASC
             LIMIT 1",
        )
        .bind(issue_number)
        .bind(run_repo)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load kanban card for issue #{issue_number}: {error}"))?
    } else if let Some(card_id) = item.as_str() {
        sqlx::query(
            "SELECT id,
                    repo_id,
                    status,
                    assigned_agent_id,
                    github_issue_number::BIGINT AS github_issue_number
             FROM kanban_cards
             WHERE id = $1
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load kanban card {card_id}: {error}"))?
    } else {
        None
    };

    let Some(row) = row else {
        return Ok(None);
    };

    Ok(Some(ResolvedDispatchCard {
        issue_number: row
            .try_get("github_issue_number")
            .map_err(|error| format!("decode github_issue_number: {error}"))?,
        card_id: row
            .try_get("id")
            .map_err(|error| format!("decode card id: {error}"))?,
        repo_id: row
            .try_get("repo_id")
            .map_err(|error| format!("decode repo_id: {error}"))?,
        status: row
            .try_get("status")
            .map_err(|error| format!("decode status: {error}"))?,
        assigned_agent_id: row
            .try_get("assigned_agent_id")
            .map_err(|error| format!("decode assigned_agent_id: {error}"))?,
    }))
}

pub(super) async fn submit_order_with_pg(
    state: &AppState,
    run_id: &str,
    headers: &HeaderMap,
    body: &OrderBody,
    pool: &sqlx::PgPool,
) -> (StatusCode, Json<serde_json::Value>) {
    let caller_agent_id =
        crate::server::routes::kanban::resolve_requesting_agent_id_with_pg(pool, headers).await;
    let run_row = match sqlx::query(
        "SELECT status, repo
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("load auto-queue run '{run_id}': {error}")})),
            );
        }
    };
    let Some(run_row) = run_row else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "run not found or not pending"})),
        );
    };
    let run_status: String = match run_row.try_get("status") {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("decode auto-queue run status: {error}")})),
            );
        }
    };
    if run_status != "pending" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "run not found or not pending"})),
        );
    }
    let run_repo: Option<String> = match run_row.try_get("repo") {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("decode auto-queue run repo: {error}")})),
            );
        }
    };
    let run_log_ctx = AutoQueueLogContext::new().run(run_id);

    let mut created = 0;
    for (rank, item) in body.order.iter().enumerate() {
        let card = match resolve_submit_order_card_with_pg(pool, run_repo.as_deref(), item).await {
            Ok(Some(card)) => card,
            Ok(None) => continue,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        };

        let dispatchable_check = crate::pipeline::try_get()
            .map(|pipeline| {
                pipeline
                    .dispatchable_states()
                    .iter()
                    .any(|state| *state == card.status)
            })
            .unwrap_or(card.status == "ready");
        if !dispatchable_check {
            crate::auto_queue_log!(
                info,
                "submit_order_card_not_dispatchable",
                run_log_ctx.clone().card(&card.card_id),
                "[auto-queue] Skipping card {} (status={}, not dispatchable)",
                card.card_id,
                card.status
            );
            continue;
        }

        let entry_id = uuid::Uuid::new_v4().to_string();
        if sqlx::query(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, priority_rank)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&entry_id)
        .bind(run_id)
        .bind(&card.card_id)
        .bind(card.assigned_agent_id.as_deref().unwrap_or(""))
        .bind(rank as i64)
        .execute(pool)
        .await
        .is_ok()
        {
            created += 1;
        }
    }

    let rationale = body
        .rationale
        .clone()
        .or(body.reasoning.clone())
        .unwrap_or_else(|| {
            caller_agent_id
                .as_deref()
                .map(|agent_id| format!("{agent_id} order submitted"))
                .unwrap_or_else(|| "API order submitted".to_string())
        });
    if created > 0 {
        if let Err(error) = sqlx::query(
            "UPDATE auto_queue_runs
             SET status = 'active',
                 ai_rationale = $1
             WHERE id = $2",
        )
        .bind(&rationale)
        .bind(run_id)
        .execute(pool)
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("activate auto-queue run '{run_id}': {error}")})),
            );
        }
    } else {
        crate::auto_queue_log!(
            warn,
            "submit_order_no_ready_cards",
            run_log_ctx.clone(),
            "[auto-queue] submit_order: no ready cards enqueued, run {run_id} stays pending"
        );
        if let Err(error) = sqlx::query(
            "UPDATE auto_queue_runs
             SET status = 'completed',
                 ai_rationale = $1
             WHERE id = $2",
        )
        .bind(format!("{rationale} (no ready cards — auto-completed)"))
        .bind(run_id)
        .execute(pool)
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("complete auto-queue run '{run_id}': {error}")})),
            );
        }
    }

    let _ = state;

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "created": created,
            "run_id": run_id,
            "message": "Queue active. Call POST /api/queue/dispatch-next to start dispatching.",
        })),
    )
}

pub async fn submit_order(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<OrderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(response) =
        crate::server::routes::kanban::require_explicit_bearer_token(&headers, "submit_order")
    {
        return response;
    }
    let Some(pg_pool) = state.pg_pool.clone() else {
        return pg_unavailable_response();
    };
    submit_order_with_pg(&state, &run_id, &headers, &body, &pg_pool).await
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{
        GenerateCandidate, QueueEntryOrder, build_group_plan, extract_dependency_numbers,
        extract_dependency_parse_result, reorder_entry_ids,
        slot_requires_thread_reset_before_reuse,
    };
    use sqlite_test::Connection;
    use std::collections::HashMap;

    fn entry(id: &str, status: &str, agent_id: &str) -> QueueEntryOrder {
        QueueEntryOrder {
            id: id.to_string(),
            status: status.to_string(),
            agent_id: agent_id.to_string(),
        }
    }

    fn candidate(
        issue_number: i64,
        priority: &str,
        description: Option<&str>,
        metadata: Option<&str>,
    ) -> GenerateCandidate {
        GenerateCandidate {
            card_id: format!("card-{issue_number}"),
            agent_id: "agent-a".to_string(),
            priority: priority.to_string(),
            description: description.map(str::to_string),
            metadata: metadata.map(str::to_string),
            github_issue_number: Some(issue_number),
        }
    }

    fn slot_reset_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE auto_queue_slots (
                agent_id TEXT NOT NULL,
                slot_index INTEGER NOT NULL,
                thread_id_map TEXT,
                PRIMARY KEY (agent_id, slot_index)
            );
            CREATE TABLE task_dispatches (
                id TEXT PRIMARY KEY,
                to_agent_id TEXT,
                thread_id TEXT,
                context TEXT
            );",
        )
        .expect("schema");
        conn
    }

    #[test]
    fn slot_thread_reset_is_not_implicit_for_auto_queue_assignment() {
        let conn = slot_reset_conn();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-a', 0, '{\"123\":\"thread-1\"}')",
            [],
        )
        .expect("seed slot binding");

        assert!(
            !slot_requires_thread_reset_before_reuse(&conn, "agent-a", 0, false, false),
            "same-run slot rebind must keep the existing thread binding"
        );
        assert!(
            !slot_requires_thread_reset_before_reuse(&conn, "agent-a", 0, true, false),
            "cross-run reclaim must keep the existing slot binding unless explicitly reset"
        );
        assert!(
            !slot_requires_thread_reset_before_reuse(&conn, "agent-a", 0, false, true),
            "different-group same-run reuse must keep the existing slot binding"
        );
    }

    #[test]
    fn extract_dependency_numbers_ignores_context_issue_references_in_description() {
        let card = candidate(
            497,
            "medium",
            Some("## 컨텍스트\n관련: #494\n이미 해결한 #493을 참고"),
            None,
        );

        assert_eq!(extract_dependency_numbers(&card), Vec::<i64>::new());
    }

    #[test]
    fn extract_dependency_numbers_parses_explicit_sections_and_json_metadata() {
        let card = candidate(
            497,
            "medium",
            Some("## 선행 작업\n- #494\n- #495\n## 컨텍스트\n관련: #493"),
            Some(r##"{"depends_on":[496,"#497","#498"]}"##),
        );

        let parsed = extract_dependency_parse_result(&card);
        assert_eq!(parsed.numbers, vec![494, 495, 496, 498]);
        assert!(
            parsed
                .signals
                .iter()
                .any(|signal| signal.contains("description:section:## 선행 작업")),
            "section-based dependency extraction should be recorded in signals"
        );
        assert!(
            parsed
                .signals
                .iter()
                .any(|signal| signal == "metadata:json:depends_on"),
            "json-based dependency extraction should be recorded in signals"
        );
    }

    #[test]
    fn extract_dependency_numbers_keeps_section_open_for_issue_ref_lines() {
        let card = candidate(
            497,
            "medium",
            Some("## 선행 작업\n#494\n- #495\n## 컨텍스트\n#493"),
            None,
        );

        let parsed = extract_dependency_parse_result(&card);
        assert_eq!(parsed.numbers, vec![494, 495]);
        assert!(
            parsed
                .signals
                .iter()
                .any(|signal| signal.contains("description:section:## 선행 작업")),
            "issue-ref lines inside dependency sections must remain section-scoped"
        );
    }

    #[test]
    fn extract_dependency_numbers_allows_bare_dependency_lists_in_metadata() {
        let card = candidate(202, "medium", None, Some("#201 #203"));

        assert_eq!(extract_dependency_numbers(&card), vec![201, 203]);
    }

    #[test]
    fn reorder_entry_ids_reorders_only_pending_entries_in_scope() {
        let entries = vec![
            entry("done-a", "done", "agent-a"),
            entry("a-1", "pending", "agent-a"),
            entry("b-1", "pending", "agent-b"),
            entry("a-2", "pending", "agent-a"),
            entry("done-b", "done", "agent-b"),
        ];

        let reordered = reorder_entry_ids(
            &entries,
            &["a-2".to_string(), "a-1".to_string()],
            Some("agent-a"),
        )
        .expect("agent reorder should succeed");

        assert_eq!(
            reordered,
            vec![
                "done-a".to_string(),
                "a-2".to_string(),
                "b-1".to_string(),
                "a-1".to_string(),
                "done-b".to_string(),
            ]
        );
    }

    #[test]
    fn reorder_entry_ids_filters_non_pending_ids_from_legacy_payloads() {
        let entries = vec![
            entry("done-a", "done", "agent-a"),
            entry("p-1", "pending", "agent-a"),
            entry("p-2", "pending", "agent-a"),
            entry("done-b", "done", "agent-a"),
        ];

        let reordered = reorder_entry_ids(
            &entries,
            &[
                "done-a".to_string(),
                "p-2".to_string(),
                "p-1".to_string(),
                "done-b".to_string(),
            ],
            None,
        )
        .expect("legacy payload should still reorder pending entries");

        assert_eq!(
            reordered,
            vec![
                "done-a".to_string(),
                "p-2".to_string(),
                "p-1".to_string(),
                "done-b".to_string(),
            ]
        );
    }

    #[test]
    fn build_group_plan_spreads_similarity_only_cards_across_groups() {
        let plan = build_group_plan(&[
            candidate(
                523,
                "high",
                Some("touches src/services/discord/tmux.rs"),
                None,
            ),
            candidate(
                545,
                "medium",
                Some("touches src/services/discord/tmux.rs"),
                None,
            ),
        ]);

        let entry_by_issue: HashMap<i64, (i64, i64)> = plan
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.card_idx as i64,
                    (entry.thread_group, entry.batch_phase),
                )
            })
            .collect();

        assert_eq!(plan.thread_group_count, 2);
        assert_eq!(plan.similarity_edges, 1);
        assert_eq!(entry_by_issue.get(&0).unwrap().0, 0);
        assert_eq!(entry_by_issue.get(&1).unwrap().0, 1);
        assert_eq!(entry_by_issue.get(&0).unwrap().1, 0);
        assert_eq!(entry_by_issue.get(&1).unwrap().1, 1);
    }

    #[test]
    fn build_group_plan_reuses_phases_for_non_conflicting_similarity_chain() {
        let plan = build_group_plan(&[
            candidate(101, "high", Some("touches src/a.rs"), None),
            candidate(102, "medium", Some("touches src/a.rs and src/b.rs"), None),
            candidate(103, "low", Some("touches src/b.rs"), None),
        ]);

        let phases_by_idx: HashMap<usize, i64> = plan
            .entries
            .iter()
            .map(|entry| (entry.card_idx, entry.batch_phase))
            .collect();

        assert_eq!(plan.thread_group_count, 3);
        assert_eq!(phases_by_idx.get(&0).copied(), Some(0));
        assert_eq!(phases_by_idx.get(&1).copied(), Some(1));
        assert_eq!(phases_by_idx.get(&2).copied(), Some(0));
    }

    #[test]
    fn build_group_plan_keeps_dependency_chain_in_one_group() {
        let plan = build_group_plan(&[
            candidate(201, "high", Some("base work"), None),
            candidate(202, "medium", Some("depends on #201"), None),
        ]);

        let entries_by_idx: HashMap<usize, (i64, i64)> = plan
            .entries
            .iter()
            .map(|entry| (entry.card_idx, (entry.thread_group, entry.batch_phase)))
            .collect();

        assert_eq!(plan.thread_group_count, 1);
        assert_eq!(entries_by_idx.get(&0).copied(), Some((0, 0)));
        assert_eq!(entries_by_idx.get(&1).copied(), Some((0, 1)));
    }

    // ── #1065 param standardization tests ───────────────────────────────

    #[test]
    fn param_standardization_reorder_body_accepts_snake_case() {
        let payload = r#"{"ordered_ids":["a","b"],"agent_id":"agent-x"}"#;
        let body: super::ReorderBody =
            serde_json::from_str(payload).expect("snake_case canonical payload must parse");
        assert_eq!(body.ordered_ids, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(body.agent_id.as_deref(), Some("agent-x"));
    }

    #[test]
    fn param_standardization_reorder_body_rejects_legacy_camel_case_alias() {
        let payload = r#"{"orderedIds":["a","b"],"agentId":"agent-x"}"#;
        assert!(
            serde_json::from_str::<super::ReorderBody>(payload).is_err(),
            "legacy camelCase payload must not parse after #904 path cleanup"
        );
    }

    #[test]
    fn path_prefix_canonical_queue_handlers_compile() {
        let _ = super::generate;
        let _ = super::reorder;
    }
}
