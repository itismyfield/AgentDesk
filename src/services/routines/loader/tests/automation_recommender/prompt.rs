use super::super::*;
use super::support::*;

#[test]
fn automation_recommender_prompt_includes_quality_sections_and_gated_handoff() {
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
        crate::services::routines::RoutineAction::Agent { prompt, .. } => {
            assert!(prompt.contains("에이전트가 도출한 내용은 반드시 한국어"));
            assert!(prompt.contains("## 성공/실패 한 줄 요약"));
            assert!(prompt.contains("## 선택 판단 근거"));
            assert!(prompt.contains("## 루트 기반 JS 자동화 패턴 탐지 가이드"));
            assert!(prompt.contains("## 이전 작업/체크포인트 수렴 대응"));
            assert!(prompt.contains("대체 탐색 경로"));
            assert!(prompt.contains("반복 제안이 되지 않게"));
            assert!(prompt.contains("## 이미 자동화됨 판단 기준"));
            assert!(prompt.contains("automation_ref 또는 source_ref"));
            assert!(prompt.contains("지속 증거가 없는 accepted"));
            assert!(prompt.contains("## 자료 범위 및 검색 정책"));
            assert!(prompt.contains("외부 웹자료 검색은 기본 동작이 아닙니다"));
            assert!(prompt.contains("PostgreSQL-backed routine observation"));
            assert!(prompt.contains("루트 원인 또는 반복 수동 작업 가설"));
            assert!(prompt.contains("rule-vs-agent 선택 이유"));
            assert!(prompt.contains("오탐/중복 억제 방법"));
            assert!(prompt.contains("다른 탐색/진행 방식"));
            assert!(prompt.contains("## Before / After"));
            assert!(prompt.contains("## 예상 구현 파일"));
            assert!(prompt.contains("## 검증 방법"));
            assert!(prompt.contains("## 게이트된 핸드오프 초안"));
            assert!(prompt.contains("requires_human_approval"));
            assert!(prompt.contains("구현, 파일 수정, 서비스 재시작"));
        }
        other => panic!("unexpected action: {other:?}"),
    }
}

#[test]
fn automation_recommender_prompt_includes_prior_checkpoint_convergence_guidance() {
    let loader = automation_recommender_loader();
    let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let checkpoint = serde_json::json!({
        "version": 1,
        "cursors": {},
        "candidates": {
            "ops/retry.js:complete": {
                "category": "routine-candidate",
                "state": "recommended",
                "score": 70,
                "evidence_count": 4,
                "examples": [],
                "last_recommended_at": "2026-04-30T05:00:00Z",
                "last_recommendation_hash": "old-hash",
                "cooldown_until": null
            }
        },
        "suppressions": {},
        "recommendations": [],
        "last_tick_at": "2026-04-30T06:59:00Z",
        "stats": {
            "ticks": 7,
            "observations_seen": 10,
            "agent_escalations": 1,
            "recommendations_today": 0,
            "recommendation_day": "2026-04-30"
        }
    });
    let observations = vec![routine_observation(
        "ops/retry.js:complete",
        1,
        "2026-04-30T06:59:00Z",
    )];

    let action = loader
        .execute_tick(
            "monitoring/automation-candidate-recommender.js",
            automation_recommender_context(Some(checkpoint), observations, vec![], now),
        )
        .unwrap();

    match action {
        crate::services::routines::RoutineAction::Agent { prompt, .. } => {
            assert!(prompt.contains("이 후보는 이전 추천/체크포인트 이력이 있습니다"));
            assert!(prompt.contains("이전 추천 시각=2026-04-30T05:00:00Z"));
            assert!(prompt.contains("같은 결론에 수렴하더라도"));
            assert!(prompt.contains("대체 탐색 경로"));
        }
        other => panic!("unexpected action: {other:?}"),
    }
}

#[test]
fn automation_recommender_truncates_prompt_by_utf8_bytes_without_node_buffer() {
    let loader = automation_recommender_loader();
    let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let long_summary = "가나다라마바사아자차카타파하".repeat(320);
    let observations = (0..5)
        .map(|idx| {
            serde_json::json!({
                "timestamp": "2026-04-30T06:59:00Z",
                "source": "routine_result",
                "category": "routine-candidate",
                "signature": "ops/long.js:complete",
                "summary": format!("{idx}: {long_summary}"),
                "occurrences": 1,
                "evidence_ref": format!("long:{idx}"),
            })
        })
        .collect::<Vec<_>>();

    let action = loader
        .execute_tick(
            "monitoring/automation-candidate-recommender.js",
            automation_recommender_context(None, observations, vec![], now),
        )
        .unwrap();

    match action {
        crate::services::routines::RoutineAction::Agent { prompt, .. } => {
            assert!(prompt.len() <= 12_288);
            assert!(prompt.contains("## 이전 작업/체크포인트 수렴 대응"));
            assert!(prompt.contains("## 이미 자동화됨 판단 기준"));
            assert!(prompt.contains("## 자료 범위 및 검색 정책"));
            assert!(prompt.contains("## 지시사항"));
            assert!(!prompt.contains('\u{FFFD}'));
        }
        other => panic!("unexpected action: {other:?}"),
    }
}
