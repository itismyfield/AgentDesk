use super::*;

pub(super) fn effective_max_entry_retries(deps: &AutoQueueActivateDeps) -> i64 {
    let from_pg = deps.pg_pool.as_ref().and_then(|pool| {
        match load_kv_meta_value_pg(pool, "runtime-config") {
            Ok(raw) => raw
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .and_then(|value| value.get("maxEntryRetries").and_then(Value::as_u64)),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "[auto-queue] failed to load postgres runtime-config for maxEntryRetries"
                );
                None
            }
        }
    });
    let fallback = crate::services::settings::runtime_config_defaults(deps.config.as_ref())
        .get("maxEntryRetries")
        .and_then(Value::as_u64)
        .unwrap_or(3);
    clamp_retry_limit(from_pg.unwrap_or(fallback))
}

/// Record one dispatch-creation failure as an entry retry or terminal
/// transition. #5993: no operator card rides along; callers WARN-log the cause
/// and the resulting `retry N/M -> status`, and the transition row is the
/// durable record.
pub(super) fn record_entry_dispatch_failure(
    deps: &AutoQueueActivateDeps,
    entry_id: &str,
    trigger_source: &str,
) -> Result<crate::db::auto_queue::EntryDispatchFailureResult, String> {
    let Some(pool) = deps.pg_pool.as_ref() else {
        return Err(format!(
            "{entry_id}: postgres backend is required to record dispatch failure"
        ));
    };
    let retry_limit = effective_max_entry_retries(deps);
    let entry_id_text = entry_id.to_string();
    let trigger_source_text = trigger_source.to_string();
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            crate::db::auto_queue::record_entry_dispatch_failure_on_pg(
                &bridge_pool,
                &entry_id_text,
                retry_limit,
                &trigger_source_text,
            )
            .await
        },
        |error| error,
    )
    .map_err(|error| format!("{entry_id}: dispatch failure state update failed: {error}"))?;

    Ok(result)
}

pub(super) fn normalize_generate_entries(
    body: &GenerateBody,
) -> Result<Option<Vec<RequestedGenerateEntry>>, String> {
    if body
        .entries
        .as_ref()
        .is_some_and(|entries| !entries.is_empty())
        && body
            .issue_numbers
            .as_ref()
            .is_some_and(|issue_numbers| !issue_numbers.is_empty())
    {
        return Err("use either issue_numbers or entries, not both".to_string());
    }

    let Some(entries) = body.entries.as_ref().filter(|entries| !entries.is_empty()) else {
        return Ok(None);
    };

    let mut normalized = Vec::with_capacity(entries.len());
    let mut seen = HashSet::new();
    for entry in entries {
        let batch_phase = entry.batch_phase.unwrap_or(0);
        if batch_phase < 0 {
            return Err("batch_phase must be >= 0".to_string());
        }
        if !seen.insert(entry.issue_number) {
            return Err(format!(
                "duplicate issue_number in entries payload: {}",
                entry.issue_number
            ));
        }
        let phase_gate_kind = match entry
            .phase_gate_kind
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(kind) if !super::phase_gate_catalog::is_valid_phase_gate_kind(kind) => {
                return Err(format!(
                    "unknown phase_gate_kind '{kind}' (see GET /api/queue/phase-gates/catalog)"
                ));
            }
            Some(kind) => {
                if let Some(reason) = crate::phase_gate::kind_unavailable_reason(kind) {
                    return Err(reason.to_string());
                }
                Some(kind.to_string())
            }
            None => None,
        };
        normalized.push(RequestedGenerateEntry {
            issue_number: entry.issue_number,
            batch_phase,
            thread_group: entry.thread_group,
            phase_gate_kind,
        });
    }

    Ok(Some(normalized))
}

pub(super) fn normalize_auto_queue_review_mode(
    review_mode: Option<&str>,
) -> Result<&'static str, String> {
    match review_mode.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some(AUTO_QUEUE_REVIEW_MODE_ENABLED) => Ok(AUTO_QUEUE_REVIEW_MODE_ENABLED),
        Some(AUTO_QUEUE_REVIEW_MODE_DISABLED) => Ok(AUTO_QUEUE_REVIEW_MODE_DISABLED),
        Some(other) => Err(format!(
            "review_mode must be '{AUTO_QUEUE_REVIEW_MODE_ENABLED}' or '{AUTO_QUEUE_REVIEW_MODE_DISABLED}', got '{other}'"
        )),
    }
}

#[cfg(test)]
mod phase_gate_generate_validation_tests {
    use super::*;

    fn body(kind: Option<&str>) -> GenerateBody {
        GenerateBody {
            repo: None,
            agent_id: None,
            auto_assign_agent: None,
            issue_numbers: None,
            entries: Some(vec![GenerateEntryBody {
                issue_number: 4898,
                batch_phase: Some(0),
                thread_group: Some(1),
                phase_gate_kind: kind.map(str::to_string),
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

    #[test]
    fn deploy_gate_generation_is_statically_unavailable() {
        assert_eq!(
            normalize_generate_entries(&body(Some("deploy-gate"))).unwrap_err(),
            crate::phase_gate::DEPLOY_GATE_UNAVAILABLE_REASON
        );
    }

    #[test]
    fn pr_confirm_and_legacy_default_remain_generation_compatible() {
        let explicit = normalize_generate_entries(&body(Some("pr-confirm")))
            .expect("valid") // agentdesk-audit: allow-unwrap — test assertion for available built-in kind
            .expect("entries"); // agentdesk-audit: allow-unwrap — fixture always supplies entries
        assert_eq!(explicit[0].phase_gate_kind.as_deref(), Some("pr-confirm"));
        let legacy = normalize_generate_entries(&body(None))
            .expect("valid") // agentdesk-audit: allow-unwrap — test assertion for legacy omitted kind
            .expect("entries"); // agentdesk-audit: allow-unwrap — fixture always supplies entries
        assert!(legacy[0].phase_gate_kind.is_none());
    }
}

#[cfg(test)]
mod record_entry_dispatch_failure_tests {
    use super::*;

    fn must_ok<T, E: std::fmt::Debug>(result: Result<T, E>, context: &str) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("{context}: {error:?}"), // agentdesk-audit: allow-unwrap — test-only helper in #[cfg(test)] module
        }
    }

    #[tokio::test]
    async fn production_failure_wrapper_preserves_restoring_slot_binding_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        must_ok(
            sqlx::query(
                "INSERT INTO auto_queue_runs (id, status, max_concurrent_threads)
                 VALUES ('run-restore-wrapper', 'restoring', 1)",
            )
            .execute(&pool)
            .await,
            "seed restoring run",
        );
        must_ok(
            sqlx::query(
                "INSERT INTO auto_queue_entries (
                     id, run_id, agent_id, status, retry_count, slot_index, thread_group
                 ) VALUES (
                     'entry-restore-wrapper', 'run-restore-wrapper', 'agent-1',
                     'dispatched', 0, 0, 0
                 )",
            )
            .execute(&pool)
            .await,
            "seed restoring dispatched entry",
        );
        must_ok(
            sqlx::query(
                "INSERT INTO auto_queue_slots (
                     agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
                 ) VALUES ('agent-1', 0, 'run-restore-wrapper', 0, '{}'::jsonb)",
            )
            .execute(&pool)
            .await,
            "seed restoring slot",
        );
        let config = crate::config::Config::default();
        let engine = must_ok(
            crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone())),
            "create test policy engine",
        );
        let deps = AutoQueueActivateDeps {
            pg_pool: Some(pool.clone()),
            engine,
            config: Arc::new(config),
            health_registry: None,
            guild_id: None,
        };

        let result = must_ok(
            record_entry_dispatch_failure(
                &deps,
                "entry-restore-wrapper",
                "restore_run_create_dispatch_failed",
            ),
            "production failure wrapper",
        );
        assert_eq!(
            result.to_status,
            crate::db::auto_queue::ENTRY_STATUS_PENDING
        );
        let state = must_ok(
            sqlx::query_as::<_, (String, String, Option<String>)>(
                "SELECT e.status, r.status, s.assigned_run_id
                 FROM auto_queue_entries e
                 JOIN auto_queue_runs r ON r.id = e.run_id
                 JOIN auto_queue_slots s ON s.agent_id = e.agent_id AND s.slot_index = 0
                 WHERE e.id = 'entry-restore-wrapper'",
            )
            .fetch_one(&pool)
            .await,
            "load production-wrapper state",
        );
        assert_eq!(
            state,
            (
                "pending".to_string(),
                "restoring".to_string(),
                Some("run-restore-wrapper".to_string()),
            )
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
