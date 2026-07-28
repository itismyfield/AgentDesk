use super::super::*;
use super::support::*;

#[test]
fn automation_recommender_requires_minimum_evidence_count_before_agent_action() {
    let loader = automation_recommender_loader();
    let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let observations = routine_observations("ops/bursty.js:complete", 2, 4);

    let action = loader
        .execute_tick(
            "monitoring/automation-candidate-recommender.js",
            automation_recommender_context(None, observations, vec![], now),
        )
        .unwrap();

    match action {
        crate::services::routines::RoutineAction::Complete {
            result_json,
            checkpoint,
            ..
        } => {
            let result = result_json.expect("complete action should explain why no agent ran");
            assert!(
                result
                    .get("decision_summary")
                    .and_then(Value::as_str)
                    .is_some_and(|summary| summary.contains("최소 5회 미만"))
            );
            assert!(
                result
                    .get("top_evidence_summary")
                    .and_then(Value::as_str)
                    .is_some_and(|summary| summary.contains("score=100"))
            );
            let checkpoint = checkpoint.unwrap();
            let candidate = checkpoint
                .pointer("/candidates/ops~1bursty.js:complete")
                .expect("candidate should be tracked below the evidence floor");
            assert_eq!(candidate.get("score").and_then(Value::as_i64), Some(100));
            assert_eq!(
                candidate.get("evidence_count").and_then(Value::as_i64),
                Some(4)
            );
        }
        other => panic!("unexpected action: {other:?}"),
    }
}

#[test]
fn automation_recommender_expires_stale_candidates_before_escalation() {
    let loader = automation_recommender_loader();
    let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let checkpoint = serde_json::json!({
        "version": 1,
        "cursors": {},
        "candidates": {
            "stale.js:complete": {
                "category": "routine-candidate",
                "state": "observing",
                "score": 100,
                "evidence_count": 20,
                "first_seen_at": "2026-03-01T00:00:00Z",
                "last_seen_at": "2026-03-01T00:00:00Z",
                "examples": [],
                "last_recommended_at": null,
                "last_recommendation_hash": null,
                "cooldown_until": null,
                "automation_ref": null
            }
        },
        "suppressions": {},
        "recommendations": [],
        "last_tick_at": null,
        "stats": {
            "ticks": 0,
            "observations_seen": 0,
            "agent_escalations": 0,
            "recommendations_today": 0,
            "recommendation_day": null
        }
    });

    let action = loader
        .execute_tick(
            "monitoring/automation-candidate-recommender.js",
            automation_recommender_context(Some(checkpoint), vec![], vec![], now),
        )
        .unwrap();

    match action {
        crate::services::routines::RoutineAction::Complete { checkpoint, .. } => {
            assert_eq!(
                checkpoint
                    .unwrap()
                    .pointer("/candidates/stale.js:complete/state")
                    .and_then(Value::as_str),
                Some("expired")
            );
        }
        other => panic!("unexpected action: {other:?}"),
    }
}

#[test]
fn automation_recommender_checkpoint_guard_prunes_lru_candidate_first() {
    let loader = automation_recommender_loader();
    let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let checkpoint = serde_json::json!({
        "version": 1,
        "cursors": {},
        "candidates": {
            "old-high-score.js:complete": {
                "category": "routine-candidate",
                "state": "observing",
                "score": 99,
                "evidence_count": 20,
                "first_seen_at": "2026-04-20T00:00:00Z",
                "last_seen_at": "2026-04-20T00:00:00Z",
                "examples": [{"summary": "x".repeat(70000), "timestamp": "2026-04-20T00:00:00Z"}],
                "last_recommended_at": null,
                "last_recommendation_hash": null,
                "cooldown_until": null,
                "automation_ref": null
            },
            "recent-low-score.js:complete": {
                "category": "routine-candidate",
                "state": "observing",
                "score": 1,
                "evidence_count": 1,
                "first_seen_at": "2026-04-30T06:59:00Z",
                "last_seen_at": "2026-04-30T06:59:00Z",
                "examples": [],
                "last_recommended_at": null,
                "last_recommendation_hash": null,
                "cooldown_until": null,
                "automation_ref": null
            }
        },
        "suppressions": {},
        "recommendations": [],
        "last_tick_at": null,
        "stats": {
            "ticks": 0,
            "observations_seen": 0,
            "agent_escalations": 0,
            "recommendations_today": 3,
            "recommendation_day": "2026-04-30"
        }
    });

    let action = loader
        .execute_tick(
            "monitoring/automation-candidate-recommender.js",
            automation_recommender_context(Some(checkpoint), vec![], vec![], now),
        )
        .unwrap();

    match action {
        crate::services::routines::RoutineAction::Complete { checkpoint, .. } => {
            let candidates = checkpoint
                .unwrap()
                .get("candidates")
                .and_then(Value::as_object)
                .cloned()
                .unwrap();
            assert!(!candidates.contains_key("old-high-score.js:complete"));
            assert!(candidates.contains_key("recent-low-score.js:complete"));
        }
        other => panic!("unexpected action: {other:?}"),
    }
}
