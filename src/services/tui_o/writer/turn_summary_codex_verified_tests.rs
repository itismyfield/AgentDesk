use super::*;
use crate::services::tui_o::shadow::UnitKind;
use crate::services::tui_o::writer::pieces::SummaryInputs;

fn summaries_on(verified: &mut Verified) {
    let channel = verified.writer.channel();
    let ledger = verified.writer.store().ledger();
    let inputs = SummaryInputs {
        turn_marks: ledger.summary_turn_marks(),
        disposed: ledger.disposed_unit_hashes(),
        context_window: None,
    };
    verified.collector.deriver =
        UnitDeriver::new(channel, ShadowProvider::Codex).with_turn_summary(inputs);
}

fn queued_summary(verified: &Verified) -> OwedWork {
    let summaries = verified
        .collector
        .log
        .work
        .iter()
        .filter(|work| {
            matches!(&work.derived, Derived::Piece(piece)
            if piece.unit_key.kind == UnitKind::Tool
                && piece.unit_key.native_key == "answer#turn_end")
        })
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 1, "{:?}", verified.collector.log.work);
    summaries[0].clone()
}

fn grant(span: &CodexEpisodeSpan) -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        span.episode.turn_nonce.clone(),
    )));
    issue_for_tests(span.execution.clone(), span.episode.clone(), &token).unwrap();
    token
}

fn assert_no_prepare_or_post(verified: &mut Verified) {
    assert!(verified.port.posts().is_empty());
    let ledger = verified.writer.store().ledger();
    assert_eq!(ledger.next_serial(), 0, "a held summary cannot prepare");
    assert!(ledger.unresolved().is_none());
    assert_eq!(ledger.violation(), None);
    assert!(!verified.writer.is_stopped());
    let summary = queued_summary(verified);
    assert!(matches!(summary.derived, Derived::Piece(_)));
}

#[tokio::test(start_paused = true)]
async fn native_codex_summary_uses_its_closing_record_origin_and_exact_episode_permit() {
    let mut verified = Verified::new(585_710, true);
    summaries_on(&mut verified);
    let (span, _token) = turn(
        verified.writer.store(),
        &verified.a.clone(),
        1,
        &["answer"],
        true,
    );
    verified.read();
    let summary = queued_summary(&verified);
    assert_eq!(summary.source, verified.a);
    let terminal_end = span.end.unwrap();
    let terminal_start = terminal_end - event("task_complete", "n1").len() as u64;
    assert_eq!(summary.range, terminal_start..terminal_end);
    assert!(
        summary.origin.is_none(),
        "native capture cannot mint an origin"
    );
    let Derived::Piece(piece) = &summary.derived else {
        unreachable!()
    };
    assert_eq!(piece.unit_key.provider, ShadowProvider::Codex);
    assert_eq!(piece.index, 0);
    assert_eq!(piece.payload, "-# ✅ 완료");
    crate::services::tui_o::writer::owed::reevaluate(
        verified.writer.store(),
        &mut verified.collector.log.work,
    )
    .unwrap();
    let origin = queued_summary(&verified).origin.unwrap();
    assert_eq!(origin.span, span);
    assert_eq!(origin.range, summary.range);
    assert!(admission::lookup(&origin).is_ok());
    verified.deliver().await;
    verified.deliver().await;
    assert_eq!(verified.port.posts(), ["answer", "-# ✅ 완료"]);
    assert!(verified.collector.log.work.is_empty());
    let ledger = verified.writer.store().ledger();
    assert_eq!(ledger.next_serial(), 2);
    let posted = ledger.piece(1).unwrap();
    assert_eq!(posted.unit_key, piece.unit_key);
    assert!(matches!(posted.outcome, Some(PieceOutcome::Posted(_))));
    assert!(
        posted.local_retry.is_none(),
        "exact permits retain retry authority"
    );
    assert_eq!(ledger.violation(), None);
}

#[tokio::test(start_paused = true)]
async fn a_codex_summary_without_its_episode_permit_stays_owed_until_that_permit_exists() {
    let mut verified = Verified::new(585_711, true);
    summaries_on(&mut verified);
    let (span, token) = turn(
        verified.writer.store(),
        &verified.a.clone(),
        1,
        &["answer"],
        false,
    );
    assert!(token.is_none());
    verified.read();
    for _ in 0..2 {
        verified.deliver().await;
        assert_no_prepare_or_post(&mut verified);
        let origin = queued_summary(&verified).origin.unwrap();
        assert_eq!(origin.span, span);
        assert_eq!(
            admission::lookup(&origin).err(),
            Some("episode_permit_unavailable")
        );
    }
    let _token = grant(&span);
    verified.deliver().await;
    assert_eq!(verified.port.posts(), ["answer", "-# ✅ 완료"]);
    assert!(verified.collector.log.work.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_revoked_exact_permit_keeps_the_codex_summary_owed_after_its_body_posted() {
    let channel = 585_712;
    let mut verified = Verified::new(channel, true);
    summaries_on(&mut verified);
    let (span, _token) = turn(
        verified.writer.store(),
        &verified.a.clone(),
        1,
        &["answer"],
        true,
    );
    verified.read();
    let summary = queued_summary(&verified);
    assert_eq!(verified.collector.log.work.pop().unwrap(), summary);
    verified.deliver().await;
    assert_eq!(verified.port.posts(), ["answer"]);
    assert!(verified.collector.log.work.is_empty());
    verified.collector.log.work.push(summary);
    admission::revoke_channel(channel, "turn_summary_test_revoked");
    for _ in 0..2 {
        verified.deliver().await;
        assert_eq!(
            verified.port.posts(),
            ["answer"],
            "the summary must check its own permit"
        );
        let ledger = verified.writer.store().ledger();
        assert_eq!(ledger.next_serial(), 1, "revoked summary must not prepare");
        assert!(ledger.unresolved().is_none() && ledger.violation().is_none());
        assert!(!verified.writer.is_stopped());
        assert_eq!(queued_summary(&verified).origin.unwrap().span, span);
    }
}

#[tokio::test(start_paused = true)]
async fn a_codex_summary_cannot_borrow_another_episodes_permit() {
    let mut verified = Verified::new(585_713, true);
    summaries_on(&mut verified);
    let (span, token) = turn(
        verified.writer.store(),
        &verified.a.clone(),
        1,
        &["answer"],
        false,
    );
    assert!(token.is_none());
    let mut foreign = span.clone();
    foreign.episode.user_message_id = 2;
    foreign.episode.turn_nonce = "turn-2".into();
    foreign.episode.native_turn_id = "n2".into();
    let _token = grant(&foreign);
    verified.read();
    crate::services::tui_o::writer::owed::reevaluate(
        verified.writer.store(),
        &mut verified.collector.log.work,
    )
    .unwrap();
    let summary = queued_summary(&verified);
    let origin = summary.origin.as_ref().unwrap();
    assert_eq!(origin.span, span);
    assert_eq!(
        admission::lookup(origin).err(),
        Some("episode_permit_unavailable")
    );
    let mut foreign_origin = origin.clone();
    foreign_origin.span = foreign;
    let foreign_record = admission::lookup(&foreign_origin).unwrap();
    assert_eq!(
        verified
            .writer
            .deliver_admitted(&summary.derived, Some((&foreign_record, origin)))
            .await,
        Step::Withheld
    );
    assert_no_prepare_or_post(&mut verified);
    for _ in 0..2 {
        verified.deliver().await;
        assert_no_prepare_or_post(&mut verified);
        assert_eq!(queued_summary(&verified).origin.unwrap().span, span);
    }
}

#[tokio::test(start_paused = true)]
async fn a_codex_summary_with_a_permit_but_no_closed_origin_cannot_bypass_the_hold() {
    let mut verified = Verified::new(585_714, true);
    summaries_on(&mut verified);
    let start = len(&verified.a);
    append(
        &verified.a.path,
        &[
            event("task_started", "n1"),
            body("answer", "answer"),
            event("task_complete", "n1"),
        ],
    );
    let span = episode_span(585_714, &verified.a, 1, (start, len(&verified.a)));
    let _token = grant(&span);
    verified.read();
    assert!(queued_summary(&verified).origin.is_none());
    for _ in 0..2 {
        verified.deliver().await;
        assert_no_prepare_or_post(&mut verified);
        assert!(queued_summary(&verified).origin.is_none());
    }
    verified.writer.store().persist_codex_span(span).unwrap();
    verified.deliver().await;
    assert_eq!(verified.port.posts(), ["answer", "-# ✅ 완료"]);
    assert!(verified.collector.log.work.is_empty());
}
