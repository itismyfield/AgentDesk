use super::*;

#[test]
fn held_and_partial_herdr_clear_reports_keep_input_discard_and_execution_separate() {
    for execution in [
        ExecutionCleanup::Held("provider remains alive".into()),
        ExecutionCleanup::Pending("hold fsync failed".into()),
    ] {
        let report = ClearReport {
            execution: Some(execution),
            transition_busy: true,
            discarded: Some([MessageId::new(7_186_401)].into()),
            ..Default::default()
        };
        let reply = report.reply();
        assert!(
            reply.starts_with("대기 메시지 1건을 비웠어요.\n"),
            "{reply}"
        );
        assert!(!reply.contains(SESSION_CLEARED_RESPONSE), "{reply}");
        assert!(
            !reply.contains("초기화했어요"),
            "a held or pending execution must not claim it was reset: {reply}"
        );
        assert!(reply.contains("이전 Herdr 실행"), "{reply}");
    }
    let partial = ClearReport {
        execution: Some(ExecutionCleanup::Retired),
        discarded: None,
        failed: vec!["legacy_pending_discard"],
        ..Default::default()
    };
    let reply = partial.reply();
    assert!(reply.starts_with(COUNT_UNKNOWN), "{reply}");
    assert!(
        !reply.contains("0건") && !reply.contains(SESSION_CLEARED_RESPONSE),
        "{reply}"
    );
    assert!(
        !reply.contains("재시도"),
        "an unattempted discard must not claim a retry: {reply}"
    );
    assert!(
        reply.contains("이전 Herdr 실행을 정리했어요.")
            && reply.contains("일부 정리를 완료하지 못했어요."),
        "{reply}"
    );
    let legacy = ClearReport {
        transition_busy: true,
        failed: vec!["legacy_pending_discard"],
        ..Default::default()
    };
    assert_eq!(
        legacy.warning(),
        Some(format!("⚠️ {RETRY_FAILED} {TRANSITION_BUSY}"))
    );
    let complete = ClearReport {
        execution: Some(ExecutionCleanup::Retired),
        discarded: Some(BTreeSet::new()),
        ..Default::default()
    };
    assert_eq!(
        complete.reply(),
        format!("{SESSION_CLEARED_RESPONSE}\n대기 메시지 0건을 비웠어요.")
    );
}
