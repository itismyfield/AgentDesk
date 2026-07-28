use super::super::*;
use super::support::*;

#[test]
fn automation_recommender_inventory_wildcard_suppresses_matching_observations() {
    let loader = automation_recommender_loader();
    let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let observations = (0..6)
        .map(|_| {
            routine_observation(
                "monitoring/working-watchdog.js:complete",
                2,
                "2026-04-30T06:59:00Z",
            )
        })
        .collect::<Vec<_>>();
    let inventory = vec![serde_json::json!({
        "pattern_id": "monitoring/working-watchdog.js:*",
        "status": "implemented",
        "reason": "registered routine",
        "source_ref": "routine:monitoring-working-watchdog",
        "updated_at": "2026-04-30T06:00:00Z"
    })];

    let action = loader
        .execute_tick(
            "monitoring/automation-candidate-recommender.js",
            automation_recommender_context(None, observations, inventory, now),
        )
        .unwrap();

    match action {
        crate::services::routines::RoutineAction::Complete {
            result_json,
            checkpoint,
            last_result,
            ..
        } => {
            assert_eq!(
                last_result.as_deref(),
                Some("성공 요약: 새 자동화 추천 후보 없음 (관찰=6, 후보=0, 오늘 추천=0)")
            );
            let result = result_json.expect("complete action should include summary result");
            assert_eq!(
                result.get("summary").and_then(Value::as_str),
                Some("관찰=6, 후보=0, 오늘 추천=0")
            );
            assert!(
                result
                    .get("outcome_summary")
                    .and_then(Value::as_str)
                    .is_some_and(|summary| summary.starts_with("성공 요약:"))
            );
            assert!(
                result
                    .get("suppression_summary")
                    .and_then(Value::as_str)
                    .is_some_and(|summary| summary.contains("자동화 인벤토리 상태=implemented"))
            );
            assert_eq!(
                result.get("scoring_summary").and_then(Value::as_str),
                Some(
                    "scored=0, deduped=0, suppressed=6, ema_scored=0.000, saturation_ticks=1, fast_fail_ticks=0, reopt_count=0"
                )
            );
            let checkpoint = checkpoint.unwrap();
            assert_eq!(
                checkpoint
                    .get("candidates")
                    .and_then(Value::as_object)
                    .unwrap()
                    .len(),
                0
            );
        }
        other => panic!("unexpected action: {other:?}"),
    }
}

#[test]
fn automation_recommender_requires_durable_ref_before_accepted_inventory_suppresses() {
    let loader = automation_recommender_loader();
    let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let observations = routine_observations("ops/retry.js:complete", 2, 5);
    let inventory = vec![serde_json::json!({
        "pattern_id": "ops/retry.js:complete",
        "status": "accepted",
        "reason": "proposal accepted but not implemented",
        "updated_at": "2026-04-30T06:00:00Z"
    })];

    let action = loader
        .execute_tick(
            "monitoring/automation-candidate-recommender.js",
            automation_recommender_context(None, observations, inventory, now),
        )
        .unwrap();

    match action {
        crate::services::routines::RoutineAction::Agent {
            prompt, checkpoint, ..
        } => {
            assert!(prompt.contains("지속 증거가 없는 accepted"));
            let checkpoint = checkpoint.unwrap();
            assert!(
                checkpoint
                    .pointer("/candidates/ops~1retry.js:complete")
                    .is_some()
            );
        }
        other => panic!("unexpected action: {other:?}"),
    }
}

#[test]
fn automation_recommender_inventory_wildcard_drops_matching_checkpoint_candidates() {
    let loader = automation_recommender_loader();
    let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let checkpoint = serde_json::json!({
        "version": 1,
        "cursors": {},
        "candidates": {
            "monitoring/working-watchdog.js:complete": {
                "category": "routine-candidate",
                "state": "recommended",
                "score": 100,
                "evidence_count": 89,
                "cooldown_until": null
            }
        },
        "suppressions": {},
        "recommendations": [{
            "pattern_id": "monitoring/working-watchdog.js:complete",
            "recommended_at": "2026-04-30T06:59:00Z",
            "hash": "existing",
            "score": 100,
            "evidence_count": 89
        }],
        "last_tick_at": "2026-04-30T06:59:00Z",
        "stats": {
            "ticks": 7,
            "observations_seen": 100,
            "agent_escalations": 1,
            "recommendations_today": 1,
            "recommendation_day": "2026-04-30"
        }
    });
    let inventory = vec![serde_json::json!({
        "pattern_id": "monitoring/working-watchdog.js:*",
        "status": "implemented",
        "reason": "registered routine",
        "source_ref": "routine:monitoring-working-watchdog",
        "updated_at": "2026-04-30T06:00:00Z"
    })];

    let action = loader
        .execute_tick(
            "monitoring/automation-candidate-recommender.js",
            automation_recommender_context(Some(checkpoint), vec![], inventory, now),
        )
        .unwrap();

    match action {
        crate::services::routines::RoutineAction::Complete { checkpoint, .. } => {
            let checkpoint = checkpoint.unwrap();
            assert_eq!(
                checkpoint
                    .get("candidates")
                    .and_then(Value::as_object)
                    .unwrap()
                    .len(),
                0
            );
            assert_eq!(
                checkpoint
                    .get("recommendations")
                    .and_then(Value::as_array)
                    .unwrap()
                    .len(),
                0
            );
        }
        other => panic!("unexpected action: {other:?}"),
    }
}
