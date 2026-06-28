use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use serde_json::{Value, json};
use sqlx::{Column, Row};
use std::{
    env, fs,
    path::{Path, PathBuf},
};
use tower::ServiceExt;

#[path = "preflight_harness/types.rs"]
mod types;
#[path = "preflight_harness/validation.rs"]
mod validation;

use self::types::{
    DispatchSnapshot, EndpointObservation, EntrySnapshot, PreflightFixture, PreflightReport,
    PreflightSnapshot, SafetyProof, SlotId,
};
use self::validation::{
    apply_snapshot_to_report, validate_history_contains_run, validate_preflight_snapshot,
};

#[tokio::test]
#[ignore = "requires a local PostgreSQL test server; run scripts/e2e/auto-queue-preflight.sh"]
async fn auto_queue_preflight_fixture_sandbox_roundtrip() -> Result<(), String> {
    let fixture_path = fixture_path_from_env();
    let report_path = report_path_from_env();
    let fixture = load_fixture(&fixture_path)
        .map_err(|error| format!("load fixture {}: {error}", fixture_path.display()))?;
    let mut report = PreflightReport::new(&fixture);

    if let Err(error) = run_preflight(&fixture, &mut report).await {
        report.raw_failure_reasons.push(error);
    }

    write_report(&report_path, &report)
        .map_err(|error| format!("write preflight report {}: {error}", report_path.display()))?;

    if !report.raw_failure_reasons.is_empty() {
        return Err(format!(
            "auto-queue preflight failed; report: {}; failures: {:?}",
            report_path.display(),
            report.raw_failure_reasons
        ));
    }

    Ok(())
}

#[test]
fn auto_queue_preflight_detects_split_brain_completion() {
    let failures = validate_preflight_snapshot(&PreflightSnapshot {
        run_id: Some("run-split-brain".to_string()),
        run_status: Some("active".to_string()),
        entries: vec![EntrySnapshot {
            id: "entry-split-brain".to_string(),
            status: "dispatched".to_string(),
            dispatch_id: Some("dispatch-split-brain".to_string()),
            slot_index: Some(0),
        }],
        dispatches: vec![DispatchSnapshot {
            id: "dispatch-split-brain".to_string(),
            status: "completed".to_string(),
        }],
        reserved_slots: Vec::new(),
        phase_gates: Vec::new(),
        diagnostics: Vec::new(),
        safety: SafetyProof::default(),
    });

    assert!(
        failures
            .iter()
            .any(|failure| failure.contains("split-brain")),
        "expected split-brain failure, got {failures:?}"
    );
}

async fn run_preflight(
    fixture: &PreflightFixture,
    report: &mut PreflightReport,
) -> Result<(), String> {
    if fixture.entries.is_empty() {
        return Err("fixture must contain at least one entry".to_string());
    }
    if fixture.review_mode != "disabled" {
        return Err("sandbox preflight fixture must use review_mode=disabled".to_string());
    }

    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(8).await;
    seed_fixture(&pool, fixture).await?;
    let app = build_preflight_app(pool.clone(), fixture)?;

    let generate_body = json!({
        "repo": fixture.repo,
        "agent_id": fixture.agent_id,
        "review_mode": fixture.review_mode,
        "max_concurrent_threads": fixture.max_concurrent_threads,
        "force": true,
        "entries": fixture.entries.iter().map(|entry| {
            json!({
                "issue_number": entry.issue_number,
                "batch_phase": entry.batch_phase.unwrap_or(0),
                "thread_group": entry.thread_group.unwrap_or(0),
                "phase_gate_kind": entry.phase_gate_kind,
            })
        }).collect::<Vec<_>>(),
    });
    let generate = request_json(
        &app,
        report,
        &fixture.auth_token,
        Method::POST,
        "/api/queue/generate".to_string(),
        Some(generate_body),
    )
    .await?;
    let run_id = required_string(&generate, &["run", "id"])?;
    report.run_id = Some(run_id.clone());

    let generated_entries = load_generated_entries(&pool, &run_id).await?;
    if generated_entries.is_empty() {
        return Err(format!(
            "/api/queue/generate created no entries for fixture {}",
            fixture.fixture_id
        ));
    }
    report.entry_ids = generated_entries
        .iter()
        .map(|entry| entry.entry_id.clone())
        .collect();

    let dispatch_next = request_json(
        &app,
        report,
        &fixture.auth_token,
        Method::POST,
        "/api/queue/dispatch-next".to_string(),
        Some(json!({ "run_id": run_id, "repo": fixture.repo, "agent_id": fixture.agent_id })),
    )
    .await?;
    if dispatch_next.get("error").is_some() {
        report.raw_failure_reasons.push(format!(
            "/api/queue/dispatch-next returned error body: {dispatch_next}"
        ));
    }
    validate_dispatch_next_created_work(&dispatch_next, report);
    let inflight_entries = load_entry_snapshots(&pool, &report.entry_ids).await?;
    let dispatch_ids = dispatch_ids_from_entries(&inflight_entries);
    if dispatch_ids.is_empty() {
        report.raw_failure_reasons.push(format!(
            "/api/queue/dispatch-next created no entry-bound dispatches: {dispatch_next}"
        ));
    }
    if !inflight_entries
        .iter()
        .any(|entry| entry.slot_index.is_some() && entry.dispatch_id.is_some())
    {
        report.raw_failure_reasons.push(format!(
            "/api/queue/dispatch-next did not record a slot-bound entry: {inflight_entries:?}"
        ));
    }
    report.dispatch_ids = dispatch_ids.clone();

    let status_path = queue_path("/api/queue/status", fixture, Some(20));
    let status_inflight = request_json(
        &app,
        report,
        &fixture.auth_token,
        Method::GET,
        status_path,
        None,
    )
    .await?;
    report.status_inflight = Some(status_inflight.clone());

    for dispatch_id in &dispatch_ids {
        request_json(
            &app,
            report,
            &fixture.auth_token,
            Method::PATCH,
            format!("/api/dispatches/{dispatch_id}"),
            Some(json!({
                "status": "completed",
                "allowed_from": ["pending", "dispatched"],
                "result": {
                    "summary": "sandbox auto-queue preflight fixture completed",
                    "assistant_message": "sandbox auto-queue preflight fixture completed",
                    "agent_response_present": true,
                    "work_outcome": "sandbox_preflight_pass",
                    "completion_source": "auto_queue_preflight_fixture",
                    "fixture_id": fixture.fixture_id,
                    "production_mutation_allowed": false
                }
            })),
        )
        .await?;
    }

    let status_final = request_json(
        &app,
        report,
        &fixture.auth_token,
        Method::GET,
        queue_path("/api/queue/status", fixture, Some(20)),
        None,
    )
    .await?;
    report.status_final = Some(status_final.clone());

    let history_final = request_json(
        &app,
        report,
        &fixture.auth_token,
        Method::GET,
        queue_path("/api/queue/history", fixture, Some(8)),
        None,
    )
    .await?;
    report.history_final = Some(history_final.clone());

    let snapshot = load_snapshot(
        &pool,
        Some(&run_id),
        &report.entry_ids,
        &dispatch_ids,
        report,
    )
    .await?;
    apply_snapshot_to_report(report, &snapshot);
    report
        .raw_failure_reasons
        .extend(validate_preflight_snapshot(&snapshot));
    report
        .raw_failure_reasons
        .extend(validate_history_contains_run(&history_final, &run_id));

    db.drop().await;
    Ok(())
}

fn build_preflight_app(pool: sqlx::PgPool, fixture: &PreflightFixture) -> Result<Router, String> {
    let mut config = crate::config::Config::default();
    config.server.host = "127.0.0.1".to_string();
    config.server.auth_token = Some(fixture.auth_token.clone());
    config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    let engine = crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone()))
        .map_err(|error| format!("create policy engine: {error}"))?;
    let broadcast_tx = crate::server::ws::new_broadcast();
    let batch_buffer = crate::server::ws::spawn_batch_flusher(broadcast_tx.clone());
    let api = crate::server::routes::api_router_with_pg(
        engine,
        config,
        broadcast_tx,
        batch_buffer,
        None,
        Some(pool),
    );
    Ok(Router::new().nest("/api", api))
}

async fn seed_fixture(pool: &sqlx::PgPool, fixture: &PreflightFixture) -> Result<(), String> {
    let pipeline_config = json!({
        "fixture_mode": true,
        "pipeline_id": fixture.pipeline_id,
        "group": fixture.group,
        "production_mutation_allowed": false
    });
    sqlx::query(
        "INSERT INTO github_repos (id, display_name, sync_enabled, default_agent_id, pipeline_config)
         VALUES ($1, $2, FALSE, $3, $4::jsonb)
         ON CONFLICT (id) DO UPDATE
         SET display_name = EXCLUDED.display_name,
             sync_enabled = FALSE,
             default_agent_id = EXCLUDED.default_agent_id,
             pipeline_config = EXCLUDED.pipeline_config",
    )
    .bind(&fixture.repo)
    .bind(format!("fixture {}", fixture.repo))
    .bind(&fixture.agent_id)
    .bind(pipeline_config.to_string())
    .execute(pool)
    .await
    .map_err(|error| format!("seed fixture repo: {error}"))?;

    sqlx::query(
        "INSERT INTO agents (
             id, name, provider, discord_channel_id, discord_channel_cdx,
             discord_channel_cc, discord_channel_alt, status, pipeline_config
         )
         VALUES ($1, $2, 'codex', NULL, NULL, NULL, NULL, 'idle', $3::jsonb)
         ON CONFLICT (id) DO UPDATE
         SET name = EXCLUDED.name,
             provider = EXCLUDED.provider,
             discord_channel_id = EXCLUDED.discord_channel_id,
             discord_channel_cdx = EXCLUDED.discord_channel_cdx,
             discord_channel_cc = EXCLUDED.discord_channel_cc,
             discord_channel_alt = EXCLUDED.discord_channel_alt,
             status = EXCLUDED.status,
             pipeline_config = EXCLUDED.pipeline_config",
    )
    .bind(&fixture.agent_id)
    .bind(
        fixture
            .agent_name
            .as_deref()
            .unwrap_or("Sandbox Auto Queue Agent"),
    )
    .bind(pipeline_config.to_string())
    .execute(pool)
    .await
    .map_err(|error| format!("seed fixture agent: {error}"))?;

    for entry in &fixture.entries {
        let card_id = fixture_card_id(fixture, entry.issue_number);
        let metadata = json!({
            "fixture_mode": true,
            "sandbox_preflight": true,
            "fixture_id": fixture.fixture_id,
            "group": fixture.group,
            "pipeline_id": fixture.pipeline_id,
            "production_mutation_allowed": false
        });
        sqlx::query(
            "INSERT INTO kanban_cards (
                 id, repo_id, title, status, priority, assigned_agent_id,
                 github_issue_url, github_issue_number, metadata, description
             )
             VALUES ($1, $2, $3, 'ready', $4, $5, $6, $7, $8::jsonb, $9)
             ON CONFLICT (id) DO UPDATE
             SET repo_id = EXCLUDED.repo_id,
                 title = EXCLUDED.title,
                 status = 'ready',
                 priority = EXCLUDED.priority,
                 assigned_agent_id = EXCLUDED.assigned_agent_id,
                 github_issue_url = EXCLUDED.github_issue_url,
                 github_issue_number = EXCLUDED.github_issue_number,
                 latest_dispatch_id = NULL,
                 metadata = EXCLUDED.metadata,
                 description = EXCLUDED.description",
        )
        .bind(card_id)
        .bind(&fixture.repo)
        .bind(&entry.title)
        .bind(&entry.priority)
        .bind(&fixture.agent_id)
        .bind(format!(
            "https://github.com/{}/issues/{}",
            fixture.repo, entry.issue_number
        ))
        .bind(entry.issue_number as i32)
        .bind(metadata.to_string())
        .bind(entry.description.as_deref())
        .execute(pool)
        .await
        .map_err(|error| format!("seed fixture card #{}: {error}", entry.issue_number))?;
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct GeneratedEntryRow {
    entry_id: String,
}

async fn load_generated_entries(
    pool: &sqlx::PgPool,
    run_id: &str,
) -> Result<Vec<GeneratedEntryRow>, String> {
    let rows = sqlx::query(
        "SELECT id
         FROM auto_queue_entries
         WHERE run_id = $1
         ORDER BY priority_rank ASC, id ASC",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load generated entries for run {run_id}: {error}"))?;

    rows.into_iter()
        .map(|row| {
            Ok(GeneratedEntryRow {
                entry_id: row
                    .try_get("id")
                    .map_err(|error| format!("decode generated entry id: {error}"))?,
            })
        })
        .collect()
}

fn validate_dispatch_next_created_work(dispatch_next: &Value, report: &mut PreflightReport) {
    let count = dispatch_next
        .get("count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let dispatched_count = dispatch_next
        .get("dispatched")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if count <= 0 || dispatched_count == 0 {
        report.raw_failure_reasons.push(format!(
            "/api/queue/dispatch-next did not activate work: count={count}, dispatched_count={dispatched_count}, body={dispatch_next}"
        ));
    }
}

fn dispatch_ids_from_entries(entries: &[EntrySnapshot]) -> Vec<String> {
    let mut dispatch_ids = Vec::new();
    for entry in entries {
        if let Some(dispatch_id) = entry.dispatch_id.as_ref()
            && !dispatch_ids.iter().any(|seen| seen == dispatch_id)
        {
            dispatch_ids.push(dispatch_id.clone());
        }
    }
    dispatch_ids
}

async fn request_json(
    app: &Router,
    report: &mut PreflightReport,
    auth_token: &str,
    method: Method,
    path: String,
    body: Option<Value>,
) -> Result<Value, String> {
    let mut builder = Request::builder()
        .method(method.clone())
        .uri(path.clone())
        .header(header::AUTHORIZATION, format!("Bearer {auth_token}"));
    let request_body = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let request = builder
        .body(request_body)
        .map_err(|error| format!("build request {method} {path}: {error}"))?;
    let response = app
        .clone()
        .oneshot(request)
        .await
        .map_err(|error| format!("send request {method} {path}: {error}"))?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|error| format!("read response {method} {path}: {error}"))?;
    let body_json = serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|_| {
        json!({
            "raw": String::from_utf8_lossy(&bytes).to_string()
        })
    });
    report.endpoint_observations.push(EndpointObservation {
        method: method.as_str().to_string(),
        path: path.clone(),
        status: status.as_u16(),
        ok: status.is_success(),
        body: body_json.clone(),
    });
    if !status.is_success() {
        report
            .raw_failure_reasons
            .push(format!("{method} {path} returned {status}: {body_json}"));
    }
    if status == StatusCode::UNAUTHORIZED {
        return Err(format!("{method} {path} unauthorized"));
    }
    Ok(body_json)
}

async fn load_snapshot(
    pool: &sqlx::PgPool,
    run_id: Option<&str>,
    entry_ids: &[String],
    dispatch_ids: &[String],
    report: &PreflightReport,
) -> Result<PreflightSnapshot, String> {
    let run_status = if let Some(run_id) = run_id {
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind(run_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| format!("load run status {run_id}: {error}"))?
    } else {
        None
    };

    let entries = load_entry_snapshots(pool, entry_ids).await?;
    let dispatches = load_dispatch_snapshots(pool, dispatch_ids).await?;
    let reserved_slots = load_reserved_slots(pool, run_id).await?;
    let phase_gates = load_phase_gates(pool, run_id).await?;
    let safety = load_safety_proof(pool).await?;
    let diagnostics = [
        report.status_inflight.as_ref(),
        report.status_final.as_ref(),
    ]
    .into_iter()
    .filter_map(|status| status.and_then(|value| value.get("diagnostics")).cloned())
    .collect();

    Ok(PreflightSnapshot {
        run_id: run_id.map(str::to_string),
        run_status,
        entries,
        dispatches,
        reserved_slots,
        phase_gates,
        diagnostics,
        safety,
    })
}

async fn load_entry_snapshots(
    pool: &sqlx::PgPool,
    entry_ids: &[String],
) -> Result<Vec<EntrySnapshot>, String> {
    let mut entries = Vec::with_capacity(entry_ids.len());
    for entry_id in entry_ids {
        let row = sqlx::query(
            "SELECT id, status, dispatch_id, slot_index::BIGINT AS slot_index
             FROM auto_queue_entries
             WHERE id = $1",
        )
        .bind(entry_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load entry snapshot {entry_id}: {error}"))?
        .ok_or_else(|| format!("entry {entry_id} missing from auto_queue_entries"))?;
        entries.push(EntrySnapshot {
            id: row
                .try_get("id")
                .map_err(|error| format!("decode entry id {entry_id}: {error}"))?,
            status: row
                .try_get("status")
                .map_err(|error| format!("decode entry status {entry_id}: {error}"))?,
            dispatch_id: row
                .try_get("dispatch_id")
                .map_err(|error| format!("decode entry dispatch id {entry_id}: {error}"))?,
            slot_index: row
                .try_get("slot_index")
                .map_err(|error| format!("decode entry slot index {entry_id}: {error}"))?,
        });
    }
    Ok(entries)
}

async fn load_dispatch_snapshots(
    pool: &sqlx::PgPool,
    dispatch_ids: &[String],
) -> Result<Vec<DispatchSnapshot>, String> {
    let mut dispatches = Vec::with_capacity(dispatch_ids.len());
    for dispatch_id in dispatch_ids {
        let row = sqlx::query("SELECT id, status FROM task_dispatches WHERE id = $1")
            .bind(dispatch_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| format!("load dispatch snapshot {dispatch_id}: {error}"))?
            .ok_or_else(|| format!("dispatch {dispatch_id} missing from task_dispatches"))?;
        dispatches.push(DispatchSnapshot {
            id: row
                .try_get("id")
                .map_err(|error| format!("decode dispatch id {dispatch_id}: {error}"))?,
            status: row
                .try_get("status")
                .map_err(|error| format!("decode dispatch status {dispatch_id}: {error}"))?,
        });
    }
    Ok(dispatches)
}

async fn load_reserved_slots(
    pool: &sqlx::PgPool,
    run_id: Option<&str>,
) -> Result<Vec<SlotId>, String> {
    let Some(run_id) = run_id else {
        return Ok(Vec::new());
    };
    let rows = sqlx::query(
        "SELECT agent_id, slot_index::BIGINT AS slot_index
         FROM auto_queue_slots
         WHERE assigned_run_id = $1
         ORDER BY agent_id, slot_index",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load reserved slots for run {run_id}: {error}"))?;

    rows.into_iter()
        .map(|row| {
            Ok(SlotId {
                agent_id: row
                    .try_get("agent_id")
                    .map_err(|error| format!("decode slot agent id: {error}"))?,
                slot_index: row
                    .try_get("slot_index")
                    .map_err(|error| format!("decode slot index: {error}"))?,
            })
        })
        .collect()
}

async fn load_phase_gates(pool: &sqlx::PgPool, run_id: Option<&str>) -> Result<Vec<Value>, String> {
    let Some(run_id) = run_id else {
        return Ok(Vec::new());
    };
    let rows = sqlx::query(
        "SELECT id::BIGINT AS id,
                phase::BIGINT AS phase,
                status,
                verdict,
                dispatch_id,
                failure_reason
         FROM auto_queue_phase_gates
         WHERE run_id = $1
         ORDER BY phase ASC, id ASC",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load phase gates for run {run_id}: {error}"))?;

    rows.into_iter()
        .map(|row| {
            let id: i64 = row
                .try_get("id")
                .map_err(|error| format!("decode phase gate id: {error}"))?;
            let phase: i64 = row
                .try_get("phase")
                .map_err(|error| format!("decode phase gate phase: {error}"))?;
            let status: String = row
                .try_get("status")
                .map_err(|error| format!("decode phase gate status: {error}"))?;
            let verdict: Option<String> = row
                .try_get("verdict")
                .map_err(|error| format!("decode phase gate verdict: {error}"))?;
            let dispatch_id: Option<String> = row
                .try_get("dispatch_id")
                .map_err(|error| format!("decode phase gate dispatch id: {error}"))?;
            let failure_reason: Option<String> = row
                .try_get("failure_reason")
                .map_err(|error| format!("decode phase gate failure reason: {error}"))?;
            Ok(json!({
                "id": id,
                "phase": phase,
                "status": status,
                "verdict": verdict,
                "dispatch_id": dispatch_id,
                "failure_reason": failure_reason
            }))
        })
        .collect()
}

async fn load_safety_proof(pool: &sqlx::PgPool) -> Result<SafetyProof, String> {
    let message_outbox_rows = load_limited_json_rows(
        pool,
        "SELECT id::BIGINT AS id, target, bot, source, status, reason_code, content
         FROM message_outbox
         ORDER BY id ASC
         LIMIT 10",
    )
    .await?;
    let dispatch_outbox_notify_rows = load_limited_json_rows(
        pool,
        "SELECT id::BIGINT AS id, dispatch_id, action, status, agent_id, card_id, title
         FROM dispatch_outbox
         WHERE action IN ('notify', 'followup')
         ORDER BY id ASC
         LIMIT 10",
    )
    .await?;

    Ok(SafetyProof {
        production_card_count: scalar_i64(
            pool,
            "SELECT COUNT(*)::BIGINT
             FROM kanban_cards
             WHERE COALESCE((metadata->>'sandbox_preflight')::BOOLEAN, FALSE) = FALSE",
        )
        .await?,
        github_pr_tracking_count: scalar_i64(pool, "SELECT COUNT(*)::BIGINT FROM pr_tracking")
            .await?,
        live_session_count: scalar_i64(
            pool,
            "SELECT COUNT(*)::BIGINT
             FROM sessions
             WHERE COALESCE(status, '') NOT IN ('disconnected', 'aborted', 'completed', 'failed', 'cancelled')",
        )
        .await?,
        dispatch_delivery_sent_count: scalar_i64(
            pool,
            "SELECT COUNT(*)::BIGINT
             FROM dispatch_delivery_events
             WHERE status = 'sent'",
        )
        .await?,
        message_outbox_count: scalar_i64(pool, "SELECT COUNT(*)::BIGINT FROM message_outbox")
            .await?,
        message_outbox_rows,
        dispatch_outbox_notify_count: scalar_i64(
            pool,
            "SELECT COUNT(*)::BIGINT
             FROM dispatch_outbox
             WHERE action IN ('notify', 'followup')",
        )
        .await?,
        dispatch_outbox_notify_rows,
        worktree_or_branch_context_count: scalar_i64(
            pool,
            "SELECT COUNT(*)::BIGINT
             FROM task_dispatches
             WHERE context LIKE '%worktree%'
                OR context LIKE '%branch%'",
        )
        .await?,
    })
}

async fn load_limited_json_rows(pool: &sqlx::PgPool, sql: &str) -> Result<Vec<Value>, String> {
    let rows = sqlx::query(sql)
        .fetch_all(pool)
        .await
        .map_err(|error| format!("load safety detail rows `{sql}`: {error}"))?;
    rows.into_iter()
        .map(|row| {
            let mut map = serde_json::Map::new();
            for column in row.columns() {
                let name = column.name();
                let value = decode_row_value(&row, name)?;
                map.insert(name.to_string(), value);
            }
            Ok(Value::Object(map))
        })
        .collect()
}

fn decode_row_value(row: &sqlx::postgres::PgRow, name: &str) -> Result<Value, String> {
    if let Ok(value) = row.try_get::<Option<String>, _>(name) {
        return Ok(value.map(Value::String).unwrap_or(Value::Null));
    }
    if let Ok(value) = row.try_get::<Option<i64>, _>(name) {
        return Ok(value.map(|value| json!(value)).unwrap_or(Value::Null));
    }
    Ok(Value::Null)
}

async fn scalar_i64(pool: &sqlx::PgPool, sql: &str) -> Result<i64, String> {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .map_err(|error| format!("run scalar safety query `{sql}`: {error}"))
}

fn required_string(value: &Value, path: &[&str]) -> Result<String, String> {
    let mut current = value;
    for key in path {
        current = current
            .get(*key)
            .ok_or_else(|| format!("missing JSON path {} in {value}", path.join(".")))?;
    }
    current
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "JSON path {} is not a non-empty string in {value}",
                path.join(".")
            )
        })
}

fn queue_path(base: &str, fixture: &PreflightFixture, limit: Option<usize>) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("repo", &fixture.repo);
    serializer.append_pair("agent_id", &fixture.agent_id);
    if let Some(limit) = limit {
        serializer.append_pair("limit", &limit.to_string());
    }
    format!("{base}?{}", serializer.finish())
}

fn fixture_card_id(fixture: &PreflightFixture, issue_number: i64) -> String {
    format!(
        "preflight-card-{}-{issue_number}",
        sanitize_identifier(&fixture.fixture_id)
    )
}

fn sanitize_identifier(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn load_fixture(path: &Path) -> Result<PreflightFixture, String> {
    let raw = fs::read_to_string(path).map_err(|error| error.to_string())?;
    serde_json::from_str(&raw).map_err(|error| error.to_string())
}

fn fixture_path_from_env() -> PathBuf {
    env::var("AGENTDESK_AUTO_QUEUE_PREFLIGHT_FIXTURE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/auto-queue-preflight/basic.json")
        })
}

fn report_path_from_env() -> PathBuf {
    env::var("AGENTDESK_AUTO_QUEUE_PREFLIGHT_REPORT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join("agentdesk-auto-queue-preflight.json"))
}

fn write_report(path: &Path, report: &PreflightReport) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let raw = serde_json::to_vec_pretty(report).map_err(|error| error.to_string())?;
    fs::write(path, raw).map_err(|error| error.to_string())
}
