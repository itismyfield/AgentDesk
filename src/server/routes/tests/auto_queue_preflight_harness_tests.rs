use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use serde_json::{Value, json};
use sqlx::{Column, Row};
use std::{
    collections::BTreeSet,
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
            dispatch_type: Some("implementation".to_string()),
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
    validate_fixture_lane(fixture)?;
    match fixture.scenario_kind.as_str() {
        "basic_roundtrip" => run_basic_roundtrip(fixture, report).await,
        "phase_gate_paths" => run_phase_gate_paths(fixture, report).await,
        "review_paths" => run_review_paths(fixture, report).await,
        "multislot_recovery" => run_multislot_recovery(fixture, report).await,
        "pipeline_compatibility" => run_pipeline_compatibility(fixture, report).await,
        other => Err(format!(
            "unknown auto-queue preflight scenario_kind={other}"
        )),
    }
}

async fn run_basic_roundtrip(
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
        Some(json!({
            "run_id": run_id.as_str(),
            "repo": fixture.repo.as_str(),
            "agent_id": fixture.agent_id.as_str()
        })),
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

async fn run_phase_gate_paths(
    fixture: &PreflightFixture,
    report: &mut PreflightReport,
) -> Result<(), String> {
    if fixture.entries.len() < 2 {
        return Err("phase_gate_paths fixture must contain at least two entries".to_string());
    }
    if fixture.review_mode != "disabled" {
        return Err("phase_gate_paths fixture must use review_mode=disabled".to_string());
    }

    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(8).await;
    seed_fixture(&pool, fixture).await?;
    let app = build_preflight_app(pool.clone(), fixture)?;
    let (run_id, entries) = generate_fixture_run(&app, &pool, fixture, report).await?;

    dispatch_next_for_run(&app, &pool, fixture, report, &run_id).await?;
    complete_live_dispatches_for_run(&app, &pool, fixture, report, &run_id).await?;

    let blocked_card_id = entries
        .first()
        .map(|entry| entry.card_id.as_str())
        .ok_or_else(|| "phase_gate_paths missing first generated entry".to_string())?;
    let blocking_gate_dispatch = insert_synthetic_dispatch(
        &pool,
        fixture,
        blocked_card_id,
        "phase-gate",
        "completed",
        phase_gate_context(fixture, &run_id, 0, Some(1), false),
        Some(json!({
            "verdict": "phase_gate_failed",
            "summary": "sandbox blocked phase gate"
        })),
    )
    .await?;
    crate::db::auto_queue::save_phase_gate_state_on_pg(
        &pool,
        &run_id,
        0,
        &crate::db::auto_queue::PhaseGateStateWrite {
            status: "failed".to_string(),
            verdict: Some("phase_gate_failed".to_string()),
            dispatch_ids: vec![blocking_gate_dispatch.clone()],
            pass_verdict: "phase_gate_passed".to_string(),
            next_phase: Some(1),
            final_phase: false,
            anchor_card_id: Some(blocked_card_id.to_string()),
            failure_reason: Some("sandbox blocked phase gate".to_string()),
            created_at: None,
        },
    )
    .await?;
    record_observation(
        report,
        "phase_gate_blocked",
        json!({
            "run_id": run_id,
            "dispatch_id": blocking_gate_dispatch,
            "phase": 0,
            "reason": "sandbox blocked phase gate"
        }),
    );

    let blocked_dispatch = dispatch_next_for_run(&app, &pool, fixture, report, &run_id).await?;
    if blocked_dispatch
        .get("message")
        .and_then(Value::as_str)
        .is_none_or(|message| !message.contains("phase gate"))
    {
        report.raw_failure_reasons.push(format!(
            "phase gate did not visibly block dispatch-next: {blocked_dispatch}"
        ));
    }

    request_json(
        &app,
        report,
        &fixture.auth_token,
        Method::PATCH,
        format!("/api/dispatches/{blocking_gate_dispatch}"),
        Some(json!({
            "result": {
                "verdict": "phase_gate_passed",
                "summary": "sandbox repaired phase gate"
            }
        })),
    )
    .await?;
    request_json(
        &app,
        report,
        &fixture.auth_token,
        Method::POST,
        format!("/api/queue/runs/{run_id}/phase-gates/repair"),
        Some(json!({ "phase": 0, "dispatch_id": blocking_gate_dispatch.as_str() })),
    )
    .await?;
    record_observation(
        report,
        "phase_gate_repaired",
        json!({ "run_id": run_id.as_str(), "phase": 0 }),
    );
    sqlx::query("UPDATE kanban_cards SET status = 'done' WHERE id = $1")
        .bind(blocked_card_id)
        .execute(&pool)
        .await
        .map_err(|error| format!("mark repaired phase 0 card terminal: {error}"))?;
    record_observation(
        report,
        "phase_gate_phase_terminal",
        json!({ "run_id": run_id.as_str(), "phase": 0, "card_id": blocked_card_id }),
    );
    let next_thread_group = entries
        .last()
        .and_then(|entry| {
            fixture
                .entries
                .iter()
                .find(|fixture_entry| Some(fixture_entry.issue_number) == entry.issue_number)
                .and_then(|fixture_entry| fixture_entry.thread_group)
        })
        .unwrap_or(1);
    crate::db::auto_queue::rebind_slot_for_group_agent_pg(
        &pool,
        &run_id,
        next_thread_group,
        &fixture.agent_id,
        1,
    )
    .await?;
    record_observation(
        report,
        "phase_gate_next_slot_rebound",
        json!({
            "run_id": run_id.as_str(),
            "phase": 1,
            "thread_group": next_thread_group,
            "slot_index": 1,
        }),
    );

    dispatch_next_for_run(&app, &pool, fixture, report, &run_id).await?;
    let phase_one_live = live_dispatch_ids_for_run(&pool, &run_id).await?;
    if phase_one_live.is_empty() {
        report.raw_failure_reasons.push(format!(
            "phase gate repair did not allow phase 1 dispatch for run {run_id}"
        ));
    }

    let final_card_id = entries
        .last()
        .map(|entry| entry.card_id.as_str())
        .ok_or_else(|| "phase_gate_paths missing final generated entry".to_string())?;
    let final_gate_dispatch = insert_synthetic_dispatch(
        &pool,
        fixture,
        final_card_id,
        "phase-gate",
        "pending",
        phase_gate_context(fixture, &run_id, 1, None, true),
        None,
    )
    .await?;
    crate::db::auto_queue::save_phase_gate_state_on_pg(
        &pool,
        &run_id,
        1,
        &crate::db::auto_queue::PhaseGateStateWrite {
            status: "pending".to_string(),
            verdict: None,
            dispatch_ids: vec![final_gate_dispatch.clone()],
            pass_verdict: "phase_gate_passed".to_string(),
            next_phase: None,
            final_phase: true,
            anchor_card_id: Some(final_card_id.to_string()),
            failure_reason: Some("sandbox final phase gate pending".to_string()),
            created_at: None,
        },
    )
    .await?;
    complete_live_dispatches_for_run(&app, &pool, fixture, report, &run_id).await?;
    request_json(
        &app,
        report,
        &fixture.auth_token,
        Method::PATCH,
        format!("/api/dispatches/{final_gate_dispatch}"),
        Some(json!({
            "status": "completed",
            "allowed_from": ["pending", "dispatched"],
            "result": {
                "verdict": "phase_gate_passed",
                "summary": "sandbox final phase gate passed"
            }
        })),
    )
    .await?;
    append_unique(&mut report.dispatch_ids, final_gate_dispatch);
    record_observation(
        report,
        "phase_gate_final_completed",
        json!({ "run_id": run_id.as_str(), "phase": 1, "expected_run_status": "completed" }),
    );

    finish_report_snapshot(&app, &pool, fixture, report, &run_id).await?;
    db.drop().await;
    Ok(())
}

async fn run_review_paths(
    fixture: &PreflightFixture,
    report: &mut PreflightReport,
) -> Result<(), String> {
    if fixture.agent_mode != "controlled" {
        return Err("review_paths fixture must declare agent_mode=controlled".to_string());
    }

    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(8).await;
    seed_fixture(&pool, fixture).await?;
    let app = build_preflight_app(pool.clone(), fixture)?;
    let (run_id, entries) = generate_fixture_run(&app, &pool, fixture, report).await?;
    dispatch_next_for_run(&app, &pool, fixture, report, &run_id).await?;
    complete_live_dispatches_for_run(&app, &pool, fixture, report, &run_id).await?;

    let card_id = entries
        .first()
        .map(|entry| entry.card_id.as_str())
        .ok_or_else(|| "review_paths fixture generated no entry".to_string())?;
    prepare_review_fixture_target(&pool, card_id, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        .await?;
    let card_state_before_review = load_card_lifecycle(&pool, card_id).await?;
    if card_state_before_review.0 != "review" {
        enter_review_state_with_transition_intents(&pool, card_id, &card_state_before_review.0)
            .await?;
        record_observation(
            report,
            "review_state_prepared",
            json!({
                "card_id": card_id,
                "from_status": card_state_before_review.0,
                "review_status_before": card_state_before_review.1,
            }),
        );
    }

    let review_dispatch_id = create_review_dispatch_with_production_core(&pool, fixture, card_id)
        .await
        .map_err(|error| format!("create production review dispatch: {error}"))?;
    stamp_review_dispatch_target(
        &pool,
        &review_dispatch_id,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .await?;
    let second_review_dispatch_id =
        create_review_dispatch_with_production_core(&pool, fixture, card_id)
            .await
            .map_err(|error| format!("reuse production review dispatch: {error}"))?;
    if review_dispatch_id != second_review_dispatch_id {
        report.raw_failure_reasons.push(format!(
            "review dispatch creation was not idempotent: {review_dispatch_id} vs {second_review_dispatch_id}"
        ));
    }

    request_json(
        &app,
        report,
        &fixture.auth_token,
        Method::POST,
        "/api/reviews/verdict".to_string(),
        Some(json!({
            "dispatch_id": review_dispatch_id,
            "overall": "improve",
            "items": [
                { "category": "fixture", "summary": "sandbox review requests a controlled fix" }
            ],
            "notes": "sandbox review verdict generated by preflight harness",
            "provider": "claude"
        })),
    )
    .await?;

    crate::services::dispatches::outbox_route::handle_completed_dispatch_followups_with_pg(
        None,
        Some(&pool),
        &review_dispatch_id,
    )
    .await
    .map_err(|error| {
        format!("process review verdict followup for {review_dispatch_id}: {error}")
    })?;

    let review_decision_id = latest_dispatch_id_for_card(
        &pool,
        card_id,
        "review-decision",
        &["pending", "dispatched"],
    )
    .await?
    .ok_or_else(|| {
        format!("review verdict did not create a production review-decision dispatch for {card_id}")
    })?;
    assert_review_decision_context_source(&pool, &review_decision_id, &review_dispatch_id).await?;

    let decision_response = request_json(
        &app,
        report,
        &fixture.auth_token,
        Method::POST,
        "/api/reviews/decision".to_string(),
        Some(json!({
            "card_id": card_id,
            "decision": "accept",
            "dispatch_id": review_decision_id,
            "commit_sha": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "comment": "sandbox accept after review-decision committed fixes"
        })),
    )
    .await?;
    let skip_rework = decision_response
        .get("skip_rework")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let direct_review_created = decision_response
        .get("direct_review_created")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let review_auto_approved = decision_response
        .get("review_auto_approved")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let rework_dispatch_created = decision_response
        .get("rework_dispatch_created")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !skip_rework || rework_dispatch_created || !(direct_review_created || review_auto_approved) {
        report.raw_failure_reasons.push(format!(
            "review-decision accept did not exercise direct-review skip-rework path: {decision_response}"
        ));
    }

    let live_rework_count =
        count_dispatches_for_card(&pool, card_id, "rework", Some(&["pending", "dispatched"]))
            .await?;
    if live_rework_count != 0 {
        report.raw_failure_reasons.push(format!(
            "skip-rework accept still left live rework dispatches for {card_id}: {live_rework_count}"
        ));
    }
    let live_review_ids =
        dispatch_ids_for_card(&pool, card_id, "review", Some(&["pending", "dispatched"])).await?;
    if direct_review_created
        && !live_review_ids
            .iter()
            .any(|dispatch_id| dispatch_id != &review_dispatch_id)
    {
        report.raw_failure_reasons.push(format!(
            "direct-review accept reported success but no fresh live review dispatch was persisted: {live_review_ids:?}"
        ));
    }

    sqlx::query("UPDATE kanban_cards SET status = 'done' WHERE id = $1")
        .bind(card_id)
        .execute(&pool)
        .await
        .map_err(|error| format!("mark review_paths card terminal: {error}"))?;
    record_observation(
        report,
        "review_paths_controlled",
        json!({
            "run_id": run_id,
            "card_id": card_id,
            "review_dispatch_id": review_dispatch_id,
            "review_decision_id": review_decision_id,
            "production_review_dedupe": review_dispatch_id == second_review_dispatch_id,
            "skip_rework": skip_rework,
            "direct_review_created": direct_review_created,
            "review_auto_approved": review_auto_approved,
            "rework_dispatch_created": rework_dispatch_created,
            "live_review_dispatches_after_accept": live_review_ids,
            "live_rework_dispatches_after_accept": live_rework_count
        }),
    );
    append_card_dispatches(&pool, report, card_id).await?;
    append_unique(&mut report.dispatch_ids, review_dispatch_id);
    append_unique(&mut report.dispatch_ids, review_decision_id);
    strip_fixture_execution_target_context(&pool, fixture).await?;
    cleanup_fixture_dispatch_outbox(&pool, fixture).await?;
    force_fixture_run_terminal(&pool, &run_id).await?;
    finish_report_snapshot(&app, &pool, fixture, report, &run_id).await?;
    db.drop().await;
    Ok(())
}

async fn run_multislot_recovery(
    fixture: &PreflightFixture,
    report: &mut PreflightReport,
) -> Result<(), String> {
    if fixture.entries.len() < 3 || fixture.max_concurrent_threads < 2 {
        return Err(
            "multislot_recovery fixture must contain at least three entries and max_concurrent_threads >= 2"
                .to_string(),
        );
    }

    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(8).await;
    seed_fixture(&pool, fixture).await?;
    let app = build_preflight_app(pool.clone(), fixture)?;
    let (run_id, entries) = generate_fixture_run(&app, &pool, fixture, report).await?;

    let reversed: Vec<String> = entries
        .iter()
        .rev()
        .map(|entry| entry.entry_id.clone())
        .collect();
    request_json(
        &app,
        report,
        &fixture.auth_token,
        Method::PATCH,
        "/api/queue/reorder".to_string(),
        Some(json!({ "ordered_ids": reversed.clone(), "agent_id": fixture.agent_id.as_str() })),
    )
    .await?;
    let persisted_order = load_pending_entry_order(&pool, &run_id, Some(&fixture.agent_id)).await?;
    if persisted_order != reversed {
        report.raw_failure_reasons.push(format!(
            "queue reorder did not persist requested order: requested={reversed:?} persisted={persisted_order:?}"
        ));
    }
    record_observation(
        report,
        "reorder_applied",
        json!({
            "run_id": run_id.as_str(),
            "requested_order": reversed,
            "persisted_order": persisted_order,
        }),
    );

    dispatch_next_for_run(&app, &pool, fixture, report, &run_id).await?;
    let inflight = load_entry_snapshots(&pool, &report.entry_ids).await?;
    let occupied_slots: BTreeSet<i64> = inflight
        .iter()
        .filter(|entry| entry.status == "dispatched")
        .filter_map(|entry| entry.slot_index)
        .collect();
    if occupied_slots.len() < 2 {
        report.raw_failure_reasons.push(format!(
            "multislot dispatch did not allocate multiple same-agent slots: {inflight:?}"
        ));
    }
    record_observation(
        report,
        "multislot_dispatched",
        json!({
            "run_id": run_id,
            "agent_id": fixture.agent_id,
            "slot_indexes": occupied_slots.iter().copied().collect::<Vec<_>>()
        }),
    );

    let recoverable_before_cancel = pending_or_dispatched_entry_count(&pool, &run_id).await?;
    let cancel_response = request_json(
        &app,
        report,
        &fixture.auth_token,
        Method::POST,
        format!("/api/queue/cancel?run_id={run_id}"),
        None,
    )
    .await?;
    assert_cancel_persisted(
        &pool,
        report,
        &run_id,
        &cancel_response,
        recoverable_before_cancel,
    )
    .await?;
    let restore_response = request_json(
        &app,
        report,
        &fixture.auth_token,
        Method::POST,
        format!("/api/queue/runs/{run_id}/restore"),
        None,
    )
    .await?;
    let restored_live_entries = assert_restore_persisted(
        &pool,
        report,
        &run_id,
        &restore_response,
        recoverable_before_cancel,
    )
    .await?;
    record_observation(
        report,
        "cancel_restore_recovered",
        json!({
            "run_id": run_id.as_str(),
            "recoverable_before_cancel": recoverable_before_cancel,
            "restored_live_entries": restored_live_entries,
            "cancel_response": cancel_response,
            "restore_response": restore_response,
        }),
    );

    if let Some(terminal_cleanup_entry) = entries.last() {
        sqlx::query("UPDATE kanban_cards SET status = 'done' WHERE id = $1")
            .bind(&terminal_cleanup_entry.card_id)
            .execute(&pool)
            .await
            .map_err(|error| format!("seed terminal-card cleanup fallback: {error}"))?;
        record_observation(
            report,
            "terminal_card_cleanup_seeded",
            json!({
                "entry_id": terminal_cleanup_entry.entry_id,
                "card_id": terminal_cleanup_entry.card_id,
            }),
        );
    }

    drain_run_to_terminal(&app, &pool, fixture, report, &run_id, restored_live_entries).await?;
    finish_report_snapshot(&app, &pool, fixture, report, &run_id).await?;
    db.drop().await;
    Ok(())
}

async fn run_pipeline_compatibility(
    fixture: &PreflightFixture,
    report: &mut PreflightReport,
) -> Result<(), String> {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(8).await;
    seed_fixture(&pool, fixture).await?;
    let app = build_preflight_app(pool.clone(), fixture)?;
    let (run_id, _) = generate_fixture_run(&app, &pool, fixture, report).await?;

    crate::pipeline::ensure_loaded();
    let pipeline =
        crate::pipeline::resolve_for_card_pg(&pool, Some(&fixture.repo), Some(&fixture.agent_id))
            .await;
    for expected in &fixture.required_transitions {
        let supported = pipeline
            .find_transition(&expected.from, &expected.to)
            .is_some();
        let label = expected.label.as_deref().unwrap_or("transition");
        record_observation(
            report,
            "pipeline_transition_check",
            json!({
                "repo": fixture.repo,
                "agent_id": fixture.agent_id,
                "label": label,
                "from": expected.from,
                "to": expected.to,
                "expected_supported": expected.supported,
                "actual_supported": supported,
            }),
        );
        match (expected.supported, supported) {
            (true, false) => report.raw_failure_reasons.push(format!(
                "pipeline compatibility missing required transition {label}: {} -> {} for repo {} agent {}",
                expected.from, expected.to, fixture.repo, fixture.agent_id
            )),
            (false, true) => report.raw_failure_reasons.push(format!(
                "pipeline compatibility expected unsupported transition but found {label}: {} -> {} for repo {} agent {}",
                expected.from, expected.to, fixture.repo, fixture.agent_id
            )),
            (false, false) => report.preflight_failure_reasons.push(format!(
                "unsupported direct transition {label}: {} -> {} for repo {} agent {}",
                expected.from, expected.to, fixture.repo, fixture.agent_id
            )),
            (true, true) => {}
        }
    }
    for expected in &fixture.expected_preflight_failures {
        if !report
            .preflight_failure_reasons
            .iter()
            .any(|reason| reason.contains(expected))
        {
            report.raw_failure_reasons.push(format!(
                "expected preflight failure containing {expected:?}, got {:?}",
                report.preflight_failure_reasons
            ));
        }
    }
    if fixture.expected_preflight_failures.is_empty()
        && !report.preflight_failure_reasons.is_empty()
    {
        report.raw_failure_reasons.push(format!(
            "unexpected pipeline preflight failures: {:?}",
            report.preflight_failure_reasons
        ));
    }

    finish_report_snapshot(&app, &pool, fixture, report, &run_id).await?;
    db.drop().await;
    Ok(())
}

fn validate_fixture_lane(fixture: &PreflightFixture) -> Result<(), String> {
    match fixture.agent_mode.as_str() {
        "none" | "controlled" => Ok(()),
        "real_live" if live_preflight_allowed() => Ok(()),
        "real_live" => Err(
            "agent_mode=real_live requires AGENTDESK_AUTO_QUEUE_PREFLIGHT_ALLOW_LIVE=1".to_string(),
        ),
        other => Err(format!(
            "unknown auto-queue preflight agent_mode={other}; expected none, controlled, or real_live"
        )),
    }
}

fn live_preflight_allowed() -> bool {
    env::var("AGENTDESK_AUTO_QUEUE_PREFLIGHT_ALLOW_LIVE")
        .ok()
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

async fn generate_fixture_run(
    app: &Router,
    pool: &sqlx::PgPool,
    fixture: &PreflightFixture,
    report: &mut PreflightReport,
) -> Result<(String, Vec<GeneratedEntryRow>), String> {
    let generate = request_json(
        app,
        report,
        &fixture.auth_token,
        Method::POST,
        "/api/queue/generate".to_string(),
        Some(generate_body_for_fixture(fixture)),
    )
    .await?;
    let run_id = required_string(&generate, &["run", "id"])?;
    report.run_id = Some(run_id.clone());

    let generated_entries = load_generated_entries(pool, &run_id).await?;
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
    record_observation(
        report,
        "generated_run",
        json!({
            "run_id": run_id,
            "entry_ids": report.entry_ids,
            "issue_numbers": generated_entries
                .iter()
                .filter_map(|entry| entry.issue_number)
                .collect::<Vec<_>>(),
        }),
    );

    Ok((run_id, generated_entries))
}

fn generate_body_for_fixture(fixture: &PreflightFixture) -> Value {
    json!({
        "repo": fixture.repo.as_str(),
        "agent_id": fixture.agent_id.as_str(),
        "review_mode": fixture.review_mode.as_str(),
        "max_concurrent_threads": fixture.max_concurrent_threads,
        "force": true,
        "entries": fixture.entries.iter().map(|entry| {
            json!({
                "issue_number": entry.issue_number,
                "batch_phase": entry.batch_phase.unwrap_or(0),
                "thread_group": entry.thread_group.unwrap_or(0),
                "phase_gate_kind": entry.phase_gate_kind.as_str(),
            })
        }).collect::<Vec<_>>(),
    })
}

async fn dispatch_next_for_run(
    app: &Router,
    pool: &sqlx::PgPool,
    fixture: &PreflightFixture,
    report: &mut PreflightReport,
    run_id: &str,
) -> Result<Value, String> {
    let dispatch_next = request_json(
        app,
        report,
        &fixture.auth_token,
        Method::POST,
        "/api/queue/dispatch-next".to_string(),
        Some(json!({
            "run_id": run_id,
            "repo": fixture.repo.as_str(),
            "agent_id": fixture.agent_id.as_str()
        })),
    )
    .await?;
    if dispatch_next.get("error").is_some() {
        report.raw_failure_reasons.push(format!(
            "/api/queue/dispatch-next returned error body: {dispatch_next}"
        ));
    }

    let count = dispatch_next
        .get("count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let dispatched_count = dispatch_next
        .get("dispatched")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let pending_group_count = dispatch_next
        .get("pending_groups")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let visibly_blocked = dispatch_next
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("phase gate"));
    if count <= 0 && dispatched_count == 0 && pending_group_count > 0 && !visibly_blocked {
        report.raw_failure_reasons.push(format!(
            "/api/queue/dispatch-next did not activate or visibly block work: {dispatch_next}"
        ));
    }

    append_entry_bound_dispatches(pool, report).await?;
    record_observation(
        report,
        "dispatch_next",
        json!({
            "run_id": run_id,
            "count": count,
            "dispatched_count": dispatched_count,
            "message": dispatch_next.get("message").cloned().unwrap_or(Value::Null),
        }),
    );
    Ok(dispatch_next)
}

async fn complete_live_dispatches_for_run(
    app: &Router,
    pool: &sqlx::PgPool,
    fixture: &PreflightFixture,
    report: &mut PreflightReport,
    run_id: &str,
) -> Result<(), String> {
    let dispatch_ids = live_dispatch_ids_for_run(pool, run_id).await?;
    for dispatch_id in dispatch_ids {
        request_json(
            app,
            report,
            &fixture.auth_token,
            Method::PATCH,
            format!("/api/dispatches/{dispatch_id}"),
            Some(json!({
                "status": "completed",
                "allowed_from": ["pending", "dispatched"],
                "result": {
                    "summary": "sandbox auto-queue advanced preflight dispatch completed",
                    "assistant_message": "sandbox auto-queue advanced preflight dispatch completed",
                    "agent_response_present": true,
                    "work_outcome": "sandbox_preflight_pass",
                    "completion_source": "auto_queue_preflight_fixture",
                    "fixture_id": fixture.fixture_id.as_str(),
                    "scenario_kind": fixture.scenario_kind.as_str(),
                    "production_mutation_allowed": false
                }
            })),
        )
        .await?;
        append_unique(&mut report.dispatch_ids, dispatch_id);
    }
    append_entry_bound_dispatches(pool, report).await
}

async fn append_entry_bound_dispatches(
    pool: &sqlx::PgPool,
    report: &mut PreflightReport,
) -> Result<(), String> {
    let entries = load_entry_snapshots(pool, &report.entry_ids).await?;
    for dispatch_id in dispatch_ids_from_entries(&entries) {
        append_unique(&mut report.dispatch_ids, dispatch_id);
    }
    Ok(())
}

async fn finish_report_snapshot(
    app: &Router,
    pool: &sqlx::PgPool,
    fixture: &PreflightFixture,
    report: &mut PreflightReport,
    run_id: &str,
) -> Result<(), String> {
    let status_final = request_json(
        app,
        report,
        &fixture.auth_token,
        Method::GET,
        queue_path("/api/queue/status", fixture, Some(20)),
        None,
    )
    .await?;
    report.status_final = Some(status_final);

    let history_final = request_json(
        app,
        report,
        &fixture.auth_token,
        Method::GET,
        queue_path("/api/queue/history", fixture, Some(8)),
        None,
    )
    .await?;
    report.history_final = Some(history_final.clone());

    let snapshot = load_snapshot(
        pool,
        Some(run_id),
        &report.entry_ids,
        &report.dispatch_ids,
        report,
    )
    .await?;
    apply_snapshot_to_report(report, &snapshot);
    report
        .raw_failure_reasons
        .extend(validate_preflight_snapshot(&snapshot));
    report
        .raw_failure_reasons
        .extend(validate_history_contains_run(&history_final, run_id));
    Ok(())
}

fn phase_gate_context(
    fixture: &PreflightFixture,
    run_id: &str,
    phase: i64,
    next_phase: Option<i64>,
    final_phase: bool,
) -> Value {
    json!({
        "sandbox_preflight": true,
        "fixture_mode": true,
        "fixture_id": fixture.fixture_id.as_str(),
        "scenario_kind": fixture.scenario_kind.as_str(),
        "agent_mode": fixture.agent_mode.as_str(),
        "production_mutation_allowed": false,
        "phase_gate": {
            "run_id": run_id,
            "batch_phase": phase,
            "phase": phase,
            "next_phase": next_phase,
            "final_phase": final_phase,
            "pass_verdict": "phase_gate_passed",
            "checks": ["merge_verified", "issue_closed", "build_passed"]
        }
    })
}

async fn insert_synthetic_dispatch(
    pool: &sqlx::PgPool,
    fixture: &PreflightFixture,
    card_id: &str,
    dispatch_type: &str,
    status: &str,
    context: Value,
    result: Option<Value>,
) -> Result<String, String> {
    let dispatch_id = format!(
        "preflight-{}-{}",
        sanitize_identifier(dispatch_type),
        uuid::Uuid::new_v4()
    );
    let title = format!(
        "Sandbox preflight {} for {}",
        dispatch_type, fixture.fixture_id
    );
    let result_string = result.map(|value| value.to_string());
    sqlx::query(
        "INSERT INTO task_dispatches (
             id, kanban_card_id, to_agent_id, dispatch_type, status, title,
             context, result, created_at, updated_at, completed_at
         )
         VALUES (
             $1, $2, $3, $4, $5, $6, $7, $8, NOW(), NOW(),
             CASE WHEN $5 IN ('completed', 'failed', 'cancelled') THEN NOW() ELSE NULL END
         )",
    )
    .bind(&dispatch_id)
    .bind(card_id)
    .bind(&fixture.agent_id)
    .bind(dispatch_type)
    .bind(status)
    .bind(title)
    .bind(context.to_string())
    .bind(result_string)
    .execute(pool)
    .await
    .map_err(|error| format!("insert synthetic {dispatch_type} dispatch for {card_id}: {error}"))?;
    Ok(dispatch_id)
}

async fn prepare_review_fixture_target(
    pool: &sqlx::PgPool,
    card_id: &str,
    reviewed_commit: &str,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE agents a
         SET discord_channel_id = COALESCE(discord_channel_id, '1470000000000038000'),
             discord_channel_cdx = COALESCE(discord_channel_cdx, '1470000000000038001'),
             discord_channel_alt = COALESCE(discord_channel_alt, '1470000000000038001'),
             discord_channel_cc = COALESCE(discord_channel_cc, '1470000000000038002')
         FROM kanban_cards c
         WHERE c.id = $1
           AND a.id = c.assigned_agent_id",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("seed fixture review agent channels for {card_id}: {error}"))?;

    sqlx::query(
        "UPDATE kanban_cards
         SET github_issue_number = NULL,
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("clear fixture issue number for review target {card_id}: {error}"))?;

    let row = sqlx::query(
        "SELECT id, result
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type IN ('implementation', 'rework')
           AND status = 'completed'
         ORDER BY COALESCE(completed_at, updated_at) DESC, updated_at DESC, id DESC
         LIMIT 1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load completed work dispatch for review target {card_id}: {error}"))?
    .ok_or_else(|| format!("no completed work dispatch found for review target {card_id}"))?;
    let dispatch_id: String = row
        .try_get("id")
        .map_err(|error| format!("decode completed work dispatch id for {card_id}: {error}"))?;
    let result_raw: Option<String> = row
        .try_get("result")
        .map_err(|error| format!("decode completed work dispatch result for {card_id}: {error}"))?;
    let mut result = result_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    result.insert("completed_commit".to_string(), json!(reviewed_commit));
    result.insert("sandbox_review_target".to_string(), json!(true));
    result.insert("production_mutation_allowed".to_string(), json!(false));
    sqlx::query(
        "UPDATE task_dispatches
         SET result = $2,
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(&dispatch_id)
    .bind(Value::Object(result).to_string())
    .execute(pool)
    .await
    .map_err(|error| format!("stamp completed work dispatch {dispatch_id}: {error}"))?;
    Ok(())
}

async fn enter_review_state_with_transition_intents(
    pool: &sqlx::PgPool,
    card_id: &str,
    from_status: &str,
) -> Result<(), String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin review-state transition for {card_id}: {error}"))?;
    let intents = [
        crate::engine::transition::TransitionIntent::UpdateStatus {
            card_id: card_id.to_string(),
            from: from_status.to_string(),
            to: "review".to_string(),
        },
        crate::engine::transition::TransitionIntent::SetReviewStatus {
            card_id: card_id.to_string(),
            review_status: Some("reviewing".to_string()),
        },
        crate::engine::transition::TransitionIntent::SyncReviewState {
            card_id: card_id.to_string(),
            state: "reviewing".to_string(),
        },
    ];
    for intent in &intents {
        crate::engine::transition_executor_pg::execute_pg_transition_intent(&mut tx, intent)
            .await?;
    }
    tx.commit()
        .await
        .map_err(|error| format!("commit review-state transition for {card_id}: {error}"))?;
    Ok(())
}

async fn create_review_dispatch_with_production_core(
    pool: &sqlx::PgPool,
    fixture: &PreflightFixture,
    card_id: &str,
) -> Result<String, String> {
    let context = json!({
        "sandbox_preflight": true,
        "fixture_mode": true,
        "fixture_id": fixture.fixture_id.as_str(),
        "scenario_kind": fixture.scenario_kind.as_str(),
        "agent_mode": fixture.agent_mode.as_str(),
        "production_mutation_allowed": false,
        "review_mode": fixture.review_mode.as_str(),
        "review_mode_hint": "controlled_review_creation_path"
    });
    let (dispatch_id, _old_status, _reused) = crate::dispatch::create_dispatch_core_with_options(
        pool,
        card_id,
        &fixture.agent_id,
        "review",
        &format!("Sandbox preflight review for {}", fixture.fixture_id),
        &context,
        crate::dispatch::DispatchCreateOptions {
            skip_outbox: true,
            sidecar_dispatch: false,
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(dispatch_id)
}

async fn stamp_review_dispatch_target(
    pool: &sqlx::PgPool,
    dispatch_id: &str,
    reviewed_commit: &str,
) -> Result<(), String> {
    let context_raw = sqlx::query_scalar::<_, Option<String>>(
        "SELECT context
         FROM task_dispatches
         WHERE id = $1
           AND dispatch_type = 'review'",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load review dispatch context {dispatch_id}: {error}"))?
    .flatten()
    .ok_or_else(|| format!("review dispatch {dispatch_id} missing context"))?;
    let mut context = serde_json::from_str::<Value>(&context_raw)
        .unwrap_or_else(|_| json!({}))
        .as_object()
        .cloned()
        .unwrap_or_default();
    context.insert("reviewed_commit".to_string(), json!(reviewed_commit));
    context.insert("sandbox_review_target".to_string(), json!(true));
    context.insert("production_mutation_allowed".to_string(), json!(false));
    sqlx::query(
        "UPDATE task_dispatches
         SET context = $2,
             updated_at = NOW()
         WHERE id = $1
           AND dispatch_type = 'review'",
    )
    .bind(dispatch_id)
    .bind(Value::Object(context).to_string())
    .execute(pool)
    .await
    .map_err(|error| format!("stamp review dispatch target {dispatch_id}: {error}"))?;
    Ok(())
}

async fn assert_review_decision_context_source(
    pool: &sqlx::PgPool,
    review_decision_id: &str,
    source_review_dispatch_id: &str,
) -> Result<(), String> {
    let context_raw = sqlx::query_scalar::<_, Option<String>>(
        "SELECT context
         FROM task_dispatches
         WHERE id = $1
           AND dispatch_type = 'review-decision'",
    )
    .bind(review_decision_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load review-decision context {review_decision_id}: {error}"))?
    .flatten()
    .ok_or_else(|| format!("review-decision dispatch {review_decision_id} missing context"))?;
    let context = serde_json::from_str::<Value>(&context_raw)
        .map_err(|error| format!("parse review-decision context {review_decision_id}: {error}"))?;
    let actual_source = context
        .get("source_review_dispatch_id")
        .and_then(Value::as_str);
    if actual_source != Some(source_review_dispatch_id) {
        return Err(format!(
            "review-decision {review_decision_id} did not bind to source review {source_review_dispatch_id}: context={context}"
        ));
    }
    if context
        .get("reviewed_commit")
        .and_then(Value::as_str)
        .is_none()
    {
        return Err(format!(
            "review-decision {review_decision_id} did not inherit reviewed_commit from source review: context={context}"
        ));
    }
    Ok(())
}

async fn dispatch_ids_for_card(
    pool: &sqlx::PgPool,
    card_id: &str,
    dispatch_type: &str,
    statuses: Option<&[&str]>,
) -> Result<Vec<String>, String> {
    if let Some(statuses) = statuses {
        let statuses: Vec<String> = statuses
            .iter()
            .map(|status| (*status).to_string())
            .collect();
        return sqlx::query_scalar::<_, String>(
            "SELECT id
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = $2
               AND status = ANY($3::TEXT[])
             ORDER BY created_at ASC, id ASC",
        )
        .bind(card_id)
        .bind(dispatch_type)
        .bind(statuses)
        .fetch_all(pool)
        .await
        .map_err(|error| format!("load {dispatch_type} dispatch ids for {card_id}: {error}"));
    }

    sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = $2
         ORDER BY created_at ASC, id ASC",
    )
    .bind(card_id)
    .bind(dispatch_type)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load {dispatch_type} dispatch ids for {card_id}: {error}"))
}

async fn latest_dispatch_id_for_card(
    pool: &sqlx::PgPool,
    card_id: &str,
    dispatch_type: &str,
    statuses: &[&str],
) -> Result<Option<String>, String> {
    let statuses: Vec<String> = statuses
        .iter()
        .map(|status| (*status).to_string())
        .collect();
    sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = $2
           AND status = ANY($3::TEXT[])
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
    )
    .bind(card_id)
    .bind(dispatch_type)
    .bind(statuses)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load latest {dispatch_type} dispatch for {card_id}: {error}"))
}

async fn append_card_dispatches(
    pool: &sqlx::PgPool,
    report: &mut PreflightReport,
    card_id: &str,
) -> Result<(), String> {
    let dispatch_ids = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM task_dispatches
         WHERE kanban_card_id = $1
         ORDER BY created_at ASC, id ASC",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load dispatch ids for card {card_id}: {error}"))?;
    for dispatch_id in dispatch_ids {
        append_unique(&mut report.dispatch_ids, dispatch_id);
    }
    Ok(())
}

async fn cleanup_fixture_dispatch_outbox(
    pool: &sqlx::PgPool,
    fixture: &PreflightFixture,
) -> Result<(), String> {
    sqlx::query(
        "DELETE FROM dispatch_outbox
         WHERE dispatch_id IN (
             SELECT td.id
             FROM task_dispatches td
             JOIN kanban_cards c ON c.id = td.kanban_card_id
             WHERE c.metadata->>'fixture_id' = $1
         )",
    )
    .bind(&fixture.fixture_id)
    .execute(pool)
    .await
    .map_err(|error| {
        format!(
            "cleanup fixture dispatch_outbox rows for {}: {error}",
            fixture.fixture_id
        )
    })?;
    Ok(())
}

async fn strip_fixture_execution_target_context(
    pool: &sqlx::PgPool,
    fixture: &PreflightFixture,
) -> Result<(), String> {
    let rows = sqlx::query(
        "SELECT td.id, td.context
         FROM task_dispatches td
         JOIN kanban_cards c ON c.id = td.kanban_card_id
         WHERE c.metadata->>'fixture_id' = $1
           AND td.context IS NOT NULL
           AND BTRIM(td.context) <> ''
         ORDER BY td.created_at ASC, td.id ASC",
    )
    .bind(&fixture.fixture_id)
    .fetch_all(pool)
    .await
    .map_err(|error| {
        format!(
            "load fixture dispatch contexts for {}: {error}",
            fixture.fixture_id
        )
    })?;

    for row in rows {
        let dispatch_id: String = row
            .try_get("id")
            .map_err(|error| format!("decode fixture dispatch id: {error}"))?;
        let context_raw: String = row
            .try_get("context")
            .map_err(|error| format!("decode fixture dispatch context {dispatch_id}: {error}"))?;
        let mut context = serde_json::from_str::<Value>(&context_raw)
            .map_err(|error| format!("parse fixture dispatch context {dispatch_id}: {error}"))?;
        let before = context.to_string();
        strip_execution_target_fields(&mut context);
        let after = context.to_string();
        if after == before {
            continue;
        }
        sqlx::query(
            "UPDATE task_dispatches
             SET context = $2,
                 updated_at = NOW()
             WHERE id = $1",
        )
        .bind(&dispatch_id)
        .bind(after)
        .execute(pool)
        .await
        .map_err(|error| format!("strip fixture dispatch context {dispatch_id}: {error}"))?;
    }
    Ok(())
}

fn strip_execution_target_fields(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let keys_to_remove: Vec<String> = map
                .keys()
                .filter(|key| {
                    let lower = key.to_ascii_lowercase();
                    lower.contains("worktree") || lower.contains("branch")
                })
                .cloned()
                .collect();
            for key in keys_to_remove {
                map.remove(&key);
            }
            for child in map.values_mut() {
                strip_execution_target_fields(child);
            }
        }
        Value::Array(items) => {
            for child in items {
                strip_execution_target_fields(child);
            }
        }
        Value::String(text) => {
            let lower = text.to_ascii_lowercase();
            if lower.contains("worktree") || lower.contains("branch") {
                *text = "sandbox_stripped_execution_target".to_string();
            }
        }
        _ => {}
    }
}

async fn count_dispatches_for_card(
    pool: &sqlx::PgPool,
    card_id: &str,
    dispatch_type: &str,
    statuses: Option<&[&str]>,
) -> Result<i64, String> {
    if let Some(statuses) = statuses {
        let statuses: Vec<String> = statuses
            .iter()
            .map(|status| (*status).to_string())
            .collect();
        return sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = $2
               AND status = ANY($3::TEXT[])",
        )
        .bind(card_id)
        .bind(dispatch_type)
        .bind(statuses)
        .fetch_one(pool)
        .await
        .map_err(|error| format!("count {dispatch_type} dispatches for {card_id}: {error}"));
    }

    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = $2",
    )
    .bind(card_id)
    .bind(dispatch_type)
    .fetch_one(pool)
    .await
    .map_err(|error| format!("count {dispatch_type} dispatches for {card_id}: {error}"))
}

async fn live_dispatch_ids_for_run(
    pool: &sqlx::PgPool,
    run_id: &str,
) -> Result<Vec<String>, String> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT td.id
         FROM auto_queue_entries e
         JOIN task_dispatches td ON td.id = e.dispatch_id
         WHERE e.run_id = $1
           AND td.status IN ('pending', 'dispatched')
         ORDER BY td.id ASC",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load live dispatches for run {run_id}: {error}"))?;
    Ok(rows)
}

async fn load_pending_entry_order(
    pool: &sqlx::PgPool,
    run_id: &str,
    agent_id: Option<&str>,
) -> Result<Vec<String>, String> {
    if let Some(agent_id) = agent_id {
        return sqlx::query_scalar::<_, String>(
            "SELECT id
             FROM auto_queue_entries
             WHERE run_id = $1
               AND agent_id = $2
               AND status = 'pending'
             ORDER BY priority_rank ASC, created_at ASC, id ASC",
        )
        .bind(run_id)
        .bind(agent_id)
        .fetch_all(pool)
        .await
        .map_err(|error| format!("load pending entry order for run {run_id}: {error}"));
    }

    sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status = 'pending'
         ORDER BY priority_rank ASC, created_at ASC, id ASC",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load pending entry order for run {run_id}: {error}"))
}

async fn load_card_lifecycle(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> Result<(String, Option<String>), String> {
    let row = sqlx::query("SELECT status, review_status FROM kanban_cards WHERE id = $1")
        .bind(card_id)
        .fetch_one(pool)
        .await
        .map_err(|error| format!("load card lifecycle {card_id}: {error}"))?;
    Ok((
        row.try_get("status")
            .map_err(|error| format!("decode card status {card_id}: {error}"))?,
        row.try_get("review_status")
            .map_err(|error| format!("decode card review_status {card_id}: {error}"))?,
    ))
}

async fn load_run_status(pool: &sqlx::PgPool, run_id: &str) -> Result<Option<String>, String> {
    sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
        .bind(run_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load run status {run_id}: {error}"))
}

async fn entry_count_for_run_by_status(
    pool: &sqlx::PgPool,
    run_id: &str,
    statuses: &[&str],
) -> Result<i64, String> {
    let statuses: Vec<String> = statuses
        .iter()
        .map(|status| (*status).to_string())
        .collect();
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status = ANY($2::TEXT[])",
    )
    .bind(run_id)
    .bind(statuses)
    .fetch_one(pool)
    .await
    .map_err(|error| format!("count entries by status for run {run_id}: {error}"))
}

async fn assert_cancel_persisted(
    pool: &sqlx::PgPool,
    report: &mut PreflightReport,
    run_id: &str,
    cancel_response: &Value,
    expected_recoverable_entries: i64,
) -> Result<(), String> {
    if json_i64(cancel_response, "cancelled_runs") < 1 {
        report.raw_failure_reasons.push(format!(
            "cancel did not report a cancelled run for {run_id}: {cancel_response}"
        ));
    }
    if json_i64(cancel_response, "cancelled_entries") < expected_recoverable_entries {
        report.raw_failure_reasons.push(format!(
            "cancel reported too few cancelled entries for {run_id}: expected_at_least={expected_recoverable_entries} body={cancel_response}"
        ));
    }
    if json_i64(cancel_response, "cancelled_dispatches") < 1 {
        report.raw_failure_reasons.push(format!(
            "cancel did not cancel any live dispatches for {run_id}: {cancel_response}"
        ));
    }
    if load_run_status(pool, run_id).await?.as_deref() != Some("cancelled") {
        report.raw_failure_reasons.push(format!(
            "cancel did not persist run status=cancelled for {run_id}: {:?}",
            load_run_status(pool, run_id).await?
        ));
    }
    let live_after_cancel = pending_or_dispatched_entry_count(pool, run_id).await?;
    if live_after_cancel != 0 {
        report.raw_failure_reasons.push(format!(
            "cancel left recoverable entries live for {run_id}: pending_or_dispatched={live_after_cancel}"
        ));
    }
    let skipped_after_cancel = entry_count_for_run_by_status(pool, run_id, &["skipped"]).await?;
    if skipped_after_cancel < expected_recoverable_entries {
        report.raw_failure_reasons.push(format!(
            "cancel did not persist skipped entries for {run_id}: expected_at_least={expected_recoverable_entries} skipped={skipped_after_cancel}"
        ));
    }
    Ok(())
}

async fn assert_restore_persisted(
    pool: &sqlx::PgPool,
    report: &mut PreflightReport,
    run_id: &str,
    restore_response: &Value,
    expected_recoverable_entries: i64,
) -> Result<i64, String> {
    if restore_response.get("ok").and_then(Value::as_bool) != Some(true) {
        report.raw_failure_reasons.push(format!(
            "restore did not return ok=true for {run_id}: {restore_response}"
        ));
    }
    if load_run_status(pool, run_id).await?.as_deref() != Some("active") {
        report.raw_failure_reasons.push(format!(
            "restore did not persist run status=active for {run_id}: {:?}",
            load_run_status(pool, run_id).await?
        ));
    }
    let restored_total = json_i64(restore_response, "restored_pending")
        + json_i64(restore_response, "restored_dispatched")
        + json_i64(restore_response, "restored_done");
    if restored_total < expected_recoverable_entries {
        report.raw_failure_reasons.push(format!(
            "restore reported too few restored entries for {run_id}: expected_at_least={expected_recoverable_entries} body={restore_response}"
        ));
    }
    let live_after_restore = pending_or_dispatched_entry_count(pool, run_id).await?;
    if expected_recoverable_entries > 0 && live_after_restore == 0 {
        report.raw_failure_reasons.push(format!(
            "restore persisted no live entries for {run_id}; drain would be a no-op: {restore_response}"
        ));
    }
    Ok(live_after_restore)
}

fn json_i64(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

async fn drain_run_to_terminal(
    app: &Router,
    pool: &sqlx::PgPool,
    fixture: &PreflightFixture,
    report: &mut PreflightReport,
    run_id: &str,
    expected_live_entries: i64,
) -> Result<(), String> {
    let initial_live_entries = pending_or_dispatched_entry_count(pool, run_id).await?;
    if expected_live_entries > 0 && initial_live_entries == 0 {
        report.raw_failure_reasons.push(format!(
            "run {run_id} drain skipped because restore left zero live entries; expected_live_entries={expected_live_entries}"
        ));
        return Ok(());
    }

    for _ in 0..12 {
        complete_live_dispatches_for_run(app, pool, fixture, report, run_id).await?;
        if pending_or_dispatched_entry_count(pool, run_id).await? == 0 {
            return Ok(());
        }
        let before = pending_or_dispatched_entry_count(pool, run_id).await?;
        dispatch_next_for_run(app, pool, fixture, report, run_id).await?;
        complete_live_dispatches_for_run(app, pool, fixture, report, run_id).await?;
        let after = pending_or_dispatched_entry_count(pool, run_id).await?;
        if after == 0 {
            return Ok(());
        }
        if after >= before {
            break;
        }
    }

    report.raw_failure_reasons.push(format!(
        "run {run_id} did not drain to terminal states; initial_live_entries={initial_live_entries}; pending_or_dispatched_entries={}",
        pending_or_dispatched_entry_count(pool, run_id).await?
    ));
    Ok(())
}

async fn pending_or_dispatched_entry_count(
    pool: &sqlx::PgPool,
    run_id: &str,
) -> Result<i64, String> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await
    .map_err(|error| format!("count pending/dispatched entries for run {run_id}: {error}"))
}

async fn force_fixture_run_terminal(pool: &sqlx::PgPool, run_id: &str) -> Result<(), String> {
    sqlx::query(
        "UPDATE auto_queue_entries
         SET status = 'done',
             completed_at = COALESCE(completed_at, NOW())
         WHERE run_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(run_id)
    .execute(pool)
    .await
    .map_err(|error| format!("force fixture entries terminal for run {run_id}: {error}"))?;

    sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'completed',
             completed_at = COALESCE(completed_at, NOW())
         WHERE id = $1",
    )
    .bind(run_id)
    .execute(pool)
    .await
    .map_err(|error| format!("force fixture run terminal for {run_id}: {error}"))?;

    crate::db::auto_queue::release_run_slots_pg(pool, run_id)
        .await
        .map_err(|error| format!("release fixture slots for run {run_id}: {error}"))?;
    Ok(())
}

fn record_observation(report: &mut PreflightReport, kind: &str, payload: Value) {
    let mut object = serde_json::Map::new();
    object.insert("kind".to_string(), Value::String(kind.to_string()));
    match payload {
        Value::Object(payload) => {
            object.extend(payload);
        }
        other => {
            object.insert("value".to_string(), other);
        }
    }
    report.scenario_observations.push(Value::Object(object));
}

fn append_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|seen| seen == &value) {
        values.push(value);
    }
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
    let pipeline_config = fixture_pipeline_config(fixture)?;
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

fn fixture_pipeline_config(fixture: &PreflightFixture) -> Result<Value, String> {
    let mut object = match fixture.pipeline_config.clone() {
        Some(Value::Object(object)) => object,
        Some(other) => {
            return Err(format!(
                "fixture pipeline_config must be a JSON object when present, got {other}"
            ));
        }
        None => serde_json::Map::new(),
    };
    object.insert("fixture_mode".to_string(), json!(true));
    object.insert(
        "pipeline_id".to_string(),
        json!(fixture.pipeline_id.as_str()),
    );
    object.insert("group".to_string(), json!(fixture.group.as_str()));
    object.insert("production_mutation_allowed".to_string(), json!(false));
    Ok(Value::Object(object))
}

#[derive(Debug, Clone)]
struct GeneratedEntryRow {
    entry_id: String,
    card_id: String,
    issue_number: Option<i64>,
}

async fn load_generated_entries(
    pool: &sqlx::PgPool,
    run_id: &str,
) -> Result<Vec<GeneratedEntryRow>, String> {
    let rows = sqlx::query(
        "SELECT e.id,
                e.kanban_card_id,
                c.github_issue_number::BIGINT AS github_issue_number
         FROM auto_queue_entries e
         LEFT JOIN kanban_cards c ON c.id = e.kanban_card_id
         WHERE e.run_id = $1
         ORDER BY COALESCE(e.batch_phase, 0) ASC,
                  COALESCE(e.thread_group, 0) ASC,
                  e.priority_rank ASC,
                  c.github_issue_number ASC,
                  e.id ASC",
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
                card_id: row
                    .try_get("kanban_card_id")
                    .map_err(|error| format!("decode generated entry card id: {error}"))?,
                issue_number: row
                    .try_get("github_issue_number")
                    .map_err(|error| format!("decode generated entry issue number: {error}"))?,
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
        let row =
            sqlx::query("SELECT id, status, dispatch_type FROM task_dispatches WHERE id = $1")
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
            dispatch_type: row
                .try_get("dispatch_type")
                .map_err(|error| format!("decode dispatch type {dispatch_id}: {error}"))?,
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
