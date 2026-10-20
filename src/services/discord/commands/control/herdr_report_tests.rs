use super::*;

#[test]
fn held_and_partial_herdr_clear_reports_keep_input_discard_and_execution_separate() {
    for execution in [
        ExecutionCleanup::Held("provider remains alive".into()),
        ExecutionCleanup::Pending("hold fsync failed".into()),
    ] {
        let report = ClearReport {
            execution: Some(execution),
            discarded: Some([MessageId::new(7_186_401)].into()),
            ..Default::default()
        };
        let reply = report.reply();
        assert!(
            reply.starts_with("대기 메시지 1건을 비웠어요.\n"),
            "{reply}"
        );
        assert!(!reply.contains(SESSION_CLEARED_RESPONSE), "{reply}");
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
        reply.contains("이전 Herdr 실행을 정리했어요.") && reply.contains(RETRY_FAILED),
        "{reply}"
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
