use super::*;

/// Stable semantic identity for the terminal auto-queue entry failure card.
/// The rendered cause is intentionally excluded from dedupe identity because
/// it may gain detail between retries while still describing the same stage.
pub(super) const FAILED_ENTRY_ALERT_REASON_CODE: &str = "auto_queue.entry_dispatch_failed";
pub(super) const FAILED_ENTRY_ALERT_DEDUPE_TTL_SECS: i64 = 30 * 60;

pub(super) fn failed_entry_alert_session_key(entry_id: &str, transition_id: i64) -> String {
    format!("auto_queue.entry:{entry_id}:failure-transition:{transition_id}")
}

fn failed_entry_alert(
    target: &str,
    run_id: &str,
    entry_id: &str,
    card_id: &str,
    agent_id: &str,
    thread_group: i64,
    cause: &str,
    result: &crate::db::auto_queue::EntryDispatchFailureResult,
) -> Result<crate::db::auto_queue::EntryDispatchFailureAlert, String> {
    let failure_transition_id = result.failure_transition_id.ok_or_else(|| {
        format!("{entry_id}: terminal dispatch failure is missing its durable transition identity")
    })?;
    let session_key = failed_entry_alert_session_key(entry_id, failure_transition_id);
    let short_run_id = &run_id[..8.min(run_id.len())];
    let short_entry_id = &entry_id[..8.min(entry_id.len())];
    let content = format!(
        "자동큐 entry 실패: run {short_run_id} / entry {short_entry_id} / card {card_id} / agent {agent_id} / G{thread_group} / retry {}/{} / {}",
        result.retry_count,
        result.retry_limit,
        compact_failure_summary(cause)
    );
    Ok(crate::db::auto_queue::EntryDispatchFailureAlert {
        target: target.to_string(),
        content,
        bot: "notify".to_string(),
        source: "auto-queue".to_string(),
        reason_code: Some(FAILED_ENTRY_ALERT_REASON_CODE.to_string()),
        session_key: Some(session_key),
        dedupe_ttl_secs: FAILED_ENTRY_ALERT_DEDUPE_TTL_SECS,
    })
}

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

pub(super) fn normalize_human_alert_target(channel: String) -> Option<String> {
    let channel = channel.trim();
    if channel.is_empty() {
        return None;
    }
    Some(if channel.starts_with("channel:") {
        channel.to_string()
    } else {
        format!("channel:{channel}")
    })
}

pub(super) fn human_alert_target(deps: &AutoQueueActivateDeps) -> Option<String> {
    let pool = deps.pg_pool.as_ref()?;
    let from_pg = match load_kv_meta_value_pg(pool, "kanban_human_alert_channel_id") {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                %error,
                "[auto-queue] failed to load postgres human alert channel override"
            );
            None
        }
    };
    from_pg
        .or_else(|| deps.config.kanban.human_alert_channel_id.clone())
        .and_then(normalize_human_alert_target)
}

pub(super) fn compact_failure_summary(message: &str) -> String {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let truncated: String = chars.by_ref().take(180).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

pub(super) fn record_entry_dispatch_failure(
    deps: &AutoQueueActivateDeps,
    run_id: &str,
    entry_id: &str,
    card_id: &str,
    agent_id: &str,
    thread_group: i64,
    slot_index: Option<i64>,
    trigger_source: &str,
    cause: &str,
    log_ctx: &AutoQueueLogContext<'_>,
) -> Result<crate::db::auto_queue::EntryDispatchFailureResult, String> {
    let Some(pool) = deps.pg_pool.as_ref() else {
        return Err(format!(
            "{entry_id}: postgres backend is required to record dispatch failure"
        ));
    };
    let retry_limit = effective_max_entry_retries(deps);
    let entry_id_text = entry_id.to_string();
    let trigger_source_text = trigger_source.to_string();
    let alert_target = human_alert_target(deps);
    let alert_run_id = run_id.to_string();
    let alert_entry_id = entry_id.to_string();
    let alert_card_id = card_id.to_string();
    let alert_agent_id = agent_id.to_string();
    let alert_cause = cause.to_string();
    let result = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            if let Some(target) = alert_target {
                crate::db::auto_queue::record_entry_dispatch_failure_with_alert_on_pg(
                    &bridge_pool,
                    &entry_id_text,
                    retry_limit,
                    &trigger_source_text,
                    move |failure| {
                        failed_entry_alert(
                            &target,
                            &alert_run_id,
                            &alert_entry_id,
                            &alert_card_id,
                            &alert_agent_id,
                            thread_group,
                            &alert_cause,
                            failure,
                        )
                    },
                )
                .await
            } else {
                crate::db::auto_queue::record_entry_dispatch_failure_on_pg(
                    &bridge_pool,
                    &entry_id_text,
                    retry_limit,
                    &trigger_source_text,
                )
                .await
            }
        },
        |error| error,
    )
    .map_err(|error| format!("{entry_id}: dispatch failure state update failed: {error}"))?;

    if result.changed {
        if let Some(assigned_slot) = slot_index {
            let run_id_text = run_id.to_string();
            let agent_id_text = agent_id.to_string();
            let entry_id_text = entry_id.to_string();
            let release_result = crate::utils::async_bridge::block_on_pg_result(
                pool,
                move |bridge_pool| async move {
                    crate::db::auto_queue::release_slot_for_group_agent_pg(
                        &bridge_pool,
                        &run_id_text,
                        thread_group,
                        &agent_id_text,
                        assigned_slot,
                    )
                    .await
                    .map_err(|error| {
                        format!(
                            "release postgres slot {} for failed entry {}: {}",
                            assigned_slot, entry_id_text, error
                        )
                    })
                },
                |error| error,
            );
            if let Err(error) = release_result {
                crate::auto_queue_log!(
                    warn,
                    "entry_dispatch_failure_release_slot_failed",
                    log_ctx.clone().slot_index(assigned_slot),
                    "[auto-queue] failed to release slot {} for entry {} after dispatch failure: {}",
                    assigned_slot,
                    entry_id,
                    error
                );
            }
        }
    }

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
            Some(kind) => Some(kind.to_string()),
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
mod failed_entry_alert_tests {
    use super::*;

    fn must_ok<T, E: std::fmt::Debug>(result: Result<T, E>, context: &str) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("{context}: {error:?}"), // agentdesk-audit: allow-unwrap — test-only helper in #[cfg(test)] module
        }
    }

    fn must_some<T>(value: Option<T>, context: &str) -> T {
        match value {
            Some(value) => value,
            None => panic!("{context}"), // agentdesk-audit: allow-unwrap — test-only helper in #[cfg(test)] module
        }
    }

    #[test]
    fn failed_entry_alert_reason_code_is_stable() {
        assert_eq!(
            FAILED_ENTRY_ALERT_REASON_CODE,
            "auto_queue.entry_dispatch_failed"
        );
    }

    #[test]
    fn failed_entry_alert_identity_is_scoped_per_durable_failure_transition() {
        assert_eq!(
            failed_entry_alert_session_key("entry-1", 3),
            failed_entry_alert_session_key("entry-1", 3)
        );
        assert_ne!(
            failed_entry_alert_session_key("entry-1", 3),
            failed_entry_alert_session_key("entry-1", 4)
        );
        assert_ne!(
            failed_entry_alert_session_key("entry-1", 3),
            failed_entry_alert_session_key("entry-2", 3)
        );
    }

    #[test]
    fn failed_entry_alert_ttl_is_at_least_thirty_minutes() {
        assert!(FAILED_ENTRY_ALERT_DEDUPE_TTL_SECS >= 30 * 60);
    }

    #[test]
    fn failed_entry_alert_dedupe_ignores_rendered_cause() {
        let session_key = failed_entry_alert_session_key("entry-1", 3);
        let first = crate::services::message_outbox::dedupe_key_for_message_for_test(
            "channel:123",
            "timeout",
            Some(FAILED_ENTRY_ALERT_REASON_CODE),
            Some(&session_key),
        );
        let second = crate::services::message_outbox::dedupe_key_for_message_for_test(
            "channel:123",
            "timeout after 45 seconds with extra detail",
            Some(FAILED_ENTRY_ALERT_REASON_CODE),
            Some(&session_key),
        );

        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn failed_entry_transition_and_alert_share_commit_with_explicit_ttl_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        must_ok(
            sqlx::query(
                "INSERT INTO auto_queue_runs (id, status) VALUES ('run-alert-identity', 'active')",
            )
            .execute(&pool)
            .await,
            "seed alert-identity run",
        );
        must_ok(
            sqlx::query(
                "INSERT INTO auto_queue_entries (id, run_id, agent_id, status, retry_count)
             VALUES ('entry-identity', 'run-alert-identity', 'agent-1', 'dispatched', 0)",
            )
            .execute(&pool)
            .await,
            "seed alert-identity entry",
        );

        let first_failure = must_ok(
            crate::db::auto_queue::record_entry_dispatch_failure_with_alert_on_pg(
                &pool,
                "entry-identity",
                1,
                "test_first_failure",
                |failure| {
                    failed_entry_alert(
                        "channel:123",
                        "run-alert-identity",
                        "entry-identity",
                        "card-1",
                        "agent-1",
                        0,
                        "first cause",
                        failure,
                    )
                },
            )
            .await,
            "record first failure",
        );
        must_ok(
            crate::db::auto_queue::update_entry_status_on_pg(
                &pool,
                "entry-identity",
                crate::db::auto_queue::ENTRY_STATUS_PENDING,
                "manual_update",
                &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
            )
            .await,
            "reset failed entry to pending",
        );
        must_ok(
            crate::db::auto_queue::update_entry_status_on_pg(
                &pool,
                "entry-identity",
                crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
                "test_redispatch",
                &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
            )
            .await,
            "redispatch reset entry",
        );
        let second_failure = must_ok(
            crate::db::auto_queue::record_entry_dispatch_failure_with_alert_on_pg(
                &pool,
                "entry-identity",
                1,
                "test_second_failure",
                |failure| {
                    failed_entry_alert(
                        "channel:123",
                        "run-alert-identity",
                        "entry-identity",
                        "card-1",
                        "agent-1",
                        0,
                        "new incident at the same retry count",
                        failure,
                    )
                },
            )
            .await,
            "record second failure",
        );
        assert_eq!(first_failure.retry_count, second_failure.retry_count);
        let first_transition = must_some(
            first_failure.failure_transition_id,
            "first failure transition id missing",
        );
        let second_transition = must_some(
            second_failure.failure_transition_id,
            "second failure transition id missing",
        );
        assert_ne!(first_transition, second_transition);

        let rows_result = sqlx::query_as::<_, (String, String, bool)>(
            "SELECT reason_code, session_key,
                    dedupe_expires_at >= created_at + INTERVAL '30 minutes'
               FROM message_outbox
              ORDER BY id",
        )
        .fetch_all(&pool)
        .await;
        let rows = match rows_result {
            Ok(rows) => rows,
            Err(error) => {
                assert!(false, "load failed-entry alert rows: {error}");
                Vec::new()
            }
        };
        assert_eq!(
            rows,
            vec![
                (
                    FAILED_ENTRY_ALERT_REASON_CODE.to_string(),
                    failed_entry_alert_session_key("entry-identity", first_transition),
                    true,
                ),
                (
                    FAILED_ENTRY_ALERT_REASON_CODE.to_string(),
                    failed_entry_alert_session_key("entry-identity", second_transition),
                    true,
                ),
            ]
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
