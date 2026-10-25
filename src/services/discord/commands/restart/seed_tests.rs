use super::*;
use crate::services::discord::router::HeadlessTurnStartError;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn busy_seed_retries_once_then_reports_busy() {
    let calls = AtomicUsize::new(0);
    let status = retry_restart_seed(|| {
        calls.fetch_add(1, Ordering::SeqCst);
        async { Err(HeadlessTurnStartError::Conflict("busy".into())) }
    })
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(status, RestartSeedStatus::Busy);
    assert!(restart_seed_note(&status).contains("1회 재시도 후에도"));
}

#[tokio::test]
async fn accepted_seed_is_never_submitted_again() {
    let calls = AtomicUsize::new(0);
    let status = retry_restart_seed(|| {
        calls.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    })
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(status, RestartSeedStatus::Started);
    assert!(restart_seed_note(&status).contains("시작됐습니다"));
}

#[tokio::test]
async fn busy_seed_can_start_on_its_only_retry() {
    let mut calls = 0;
    let status = retry_restart_seed(|| {
        calls += 1;
        let result = if calls == 1 {
            Err(HeadlessTurnStartError::Conflict("busy".into()))
        } else {
            Ok(())
        };
        std::future::ready(result)
    })
    .await;
    assert_eq!(calls, 2);
    assert_eq!(status, RestartSeedStatus::Started);
}

#[tokio::test]
async fn retry_failure_is_reported_without_a_third_submission() {
    let mut calls = 0;
    let status = retry_restart_seed(|| {
        calls += 1;
        std::future::ready(Err(if calls == 1 {
            HeadlessTurnStartError::Conflict("busy".into())
        } else {
            HeadlessTurnStartError::Internal("launch refused".into())
        }))
    })
    .await;
    assert_eq!(calls, 2);
    let response = build_restart_response(
        &ProviderKind::Claude,
        &RestartAction::NoActiveSession,
        None,
        &status,
    );
    assert!(
        response.contains("1회 재시도 후 실패: launch refused"),
        "{response}"
    );
    assert!(!response.contains("세션 재시작됨"), "{response}");
}

#[tokio::test]
async fn non_busy_failures_are_not_retried() {
    for error in [
        HeadlessTurnStartError::Internal("internal".into()),
        HeadlessTurnStartError::InvalidTarget("target".into()),
    ] {
        let mut calls = 0;
        let status = retry_restart_seed(|| {
            calls += 1;
            std::future::ready(Err(error.clone()))
        })
        .await;
        assert_eq!(calls, 1);
        assert_eq!(status, RestartSeedStatus::Failed(error.to_string()));
    }
}
