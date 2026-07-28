use super::super::*;
use super::support::*;

#[test]
fn automation_recommender_uses_weight_for_error_assessment_and_persists_fields() {
    let loader = automation_recommender_loader();
    let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let observations = routine_observations("ops/retry.js:complete", 2, 5);

    let action = loader
        .execute_tick(
            "monitoring/automation-candidate-recommender.js",
            automation_recommender_context(None, observations, vec![], now),
        )
        .unwrap();

    match action {
        crate::services::routines::RoutineAction::Agent {
            prompt, checkpoint, ..
        } => {
            assert!(prompt.contains("반복 실패 루틴에 대한 자동 재시도 또는 알림"));
            assert!(prompt.contains("실패 요약:"));
            let checkpoint = checkpoint.unwrap();
            let candidate = checkpoint
                .pointer("/candidates/ops~1retry.js:complete")
                .expect("candidate should be persisted");
            assert_eq!(
                candidate
                    .get("suggested_automation")
                    .and_then(Value::as_str),
                Some("반복 실패 루틴에 대한 자동 재시도 또는 알림")
            );
            assert!(
                candidate
                    .get("outcome_summary")
                    .and_then(Value::as_str)
                    .is_some_and(|summary| summary.starts_with("실패 요약:"))
            );
            assert!(
                candidate
                    .get("decision_summary")
                    .and_then(Value::as_str)
                    .is_some_and(|summary| summary.starts_with("선택 이유:"))
            );
            assert!(
                candidate
                    .get("top_evidence_summary")
                    .and_then(Value::as_str)
                    .is_some_and(|summary| summary.contains("repeated evidence"))
            );
            assert_eq!(
                candidate
                    .get("score_delta_last_tick")
                    .and_then(Value::as_f64),
                Some(150.0)
            );
            assert_eq!(
                candidate
                    .get("recommended_execution")
                    .and_then(Value::as_str),
                Some("agent")
            );
            assert!(candidate.get("before_after").is_some());
            assert!(candidate.get("expected_files").is_some());
            assert!(candidate.get("expected_side_effects").is_some());
            assert!(candidate.get("verification_method").is_some());
            assert_eq!(
                candidate
                    .pointer("/gated_handoff/status")
                    .and_then(Value::as_str),
                Some("requires_human_approval")
            );
            assert!(
                checkpoint
                    .pointer("/recommendations/0/outcome_summary")
                    .and_then(Value::as_str)
                    .is_some_and(|summary| summary.starts_with("실패 요약:"))
            );
            assert!(
                checkpoint
                    .pointer("/recommendations/0/decision_summary")
                    .and_then(Value::as_str)
                    .is_some_and(|summary| summary.starts_with("선택 이유:"))
            );
        }
        other => panic!("unexpected action: {other:?}"),
    }
}
#[test]
fn automation_recommender_expands_api_friction_category() {
    let loader = automation_recommender_loader();
    let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let observations = vec![categorized_observation(
        "api-friction:/api/docs/kanban",
        "api-friction",
        "api_friction",
        5,
        "2026-04-30T06:59:00Z",
    )];

    let action = loader
        .execute_tick(
            "monitoring/automation-candidate-recommender.js",
            automation_recommender_context(None, observations, vec![], now),
        )
        .unwrap();

    match action {
        crate::services::routines::RoutineAction::Agent {
            prompt, checkpoint, ..
        } => {
            assert!(prompt.contains("카테고리: api-friction"));
            assert!(prompt.contains("API 마찰 모니터"));
            assert!(prompt.contains("src/services/api_friction.rs"));
            let candidate = checkpoint
                .unwrap()
                .pointer("/candidates/api-friction:~1api~1docs~1kanban/category")
                .and_then(Value::as_str)
                .unwrap()
                .to_string();
            assert_eq!(candidate, "api-friction");
        }
        other => panic!("unexpected action: {other:?}"),
    }
}

#[test]
fn automation_recommender_expands_release_and_outbox_categories() {
    let loader = automation_recommender_loader();
    let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    let release_action = loader
        .execute_tick(
            "monitoring/automation-candidate-recommender.js",
            automation_recommender_context(
                None,
                vec![categorized_observation(
                    "release-freshness:worker-inventory",
                    "release-freshness",
                    "precomputed_digest",
                    5,
                    "2026-04-30T06:59:00Z",
                )],
                vec![],
                now,
            ),
        )
        .unwrap();
    match release_action {
        crate::services::routines::RoutineAction::Agent { prompt, .. } => {
            assert!(prompt.contains("카테고리: release-freshness"));
            assert!(prompt.contains("릴리스 신선도 모니터"));
            let inventory_path = ["docs", "generated", "worker-inventory.md"].join("/");
            assert!(prompt.contains(&inventory_path));
        }
        other => panic!("unexpected action: {other:?}"),
    }

    let outbox_action = loader
        .execute_tick(
            "monitoring/automation-candidate-recommender.js",
            automation_recommender_context(
                None,
                vec![categorized_observation(
                    "outbox-delivery:notify:routine_run_failed",
                    "outbox-delivery",
                    "message_outbox",
                    5,
                    "2026-04-30T06:59:00Z",
                )],
                vec![],
                now,
            ),
        )
        .unwrap();
    match outbox_action {
        crate::services::routines::RoutineAction::Agent { prompt, .. } => {
            assert!(prompt.contains("카테고리: outbox-delivery"));
            assert!(prompt.contains("메시지 아웃박스 전달 모니터"));
            assert!(prompt.contains("src/services/message_outbox.rs"));
        }
        other => panic!("unexpected action: {other:?}"),
    }
}

#[test]
fn automation_recommender_accepts_memento_digest_occurrence_counts() {
    let loader = automation_recommender_loader();
    let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let observations = vec![categorized_observation(
        "memento-hygiene:api-friction-memory",
        "memento-hygiene",
        "memento_digest",
        5,
        "2026-04-30T06:59:00Z",
    )];

    let action = loader
        .execute_tick(
            "monitoring/automation-candidate-recommender.js",
            automation_recommender_context(None, observations, vec![], now),
        )
        .unwrap();

    match action {
        crate::services::routines::RoutineAction::Agent {
            prompt, checkpoint, ..
        } => {
            assert!(prompt.contains("카테고리: memento-hygiene"));
            assert!(prompt.contains("Memento 위생 다이제스트 모니터"));
            assert!(prompt.contains("src/services/memory"));
            assert_eq!(
                checkpoint
                    .unwrap()
                    .pointer("/candidates/memento-hygiene:api-friction-memory/evidence_count")
                    .and_then(Value::as_i64),
                Some(5)
            );
        }
        other => panic!("unexpected action: {other:?}"),
    }
}
