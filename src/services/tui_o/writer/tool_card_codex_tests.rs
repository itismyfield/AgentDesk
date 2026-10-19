use super::*;
use crate::services::tui_o::shadow::UnitKind;
use crate::services::tui_o::writer::pieces::ToolCardInputs;

fn cards_on(verified: &mut Verified) {
    let channel = verified.writer.store().init().channel;
    let ledger = verified.writer.store().ledger();
    verified.collector.deriver =
        UnitDeriver::new(channel, ShadowProvider::Codex).with_tool_cards(ToolCardInputs {
            known: ledger.non_card_tool_keys(),
            recorded_cards: ledger.card_tool_keys(),
            switch: Arc::new(|| true),
        });
}

fn append_tool_turn(verified: &mut Verified, closes_on_body: bool) -> CodexEpisodeSpan {
    let start = len(&verified.a);
    let call = serde_json::json!({"type":"response_item", "payload": {
        "type":"function_call", "id":"native-tool", "call_id":"native-call",
        "name":"functions.exec_command", "arguments":"{\"cmd\":\"cargo check\"}"
    }});
    let result = serde_json::json!({"type":"response_item", "payload": {
        "type":"function_call_output", "call_id":"native-call",
        "output":"PRIVATE_CODEX_RESULT", "is_error":false
    }});
    let mut lines = vec![
        event("task_started", "n1"),
        format!("{call}\n"),
        format!("{result}\n"),
    ];
    if closes_on_body {
        lines.push(body("answer-id", "answer"));
    }
    lines.push(event("task_complete", "n1"));
    append(&verified.a.path, &lines);
    episode_span(
        verified.writer.store().init().channel,
        &verified.a,
        1,
        (start, len(&verified.a)),
    )
}

fn permit(span: &CodexEpisodeSpan) -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        span.episode.turn_nonce.clone(),
    )));
    issue_for_tests(span.execution.clone(), span.episode.clone(), &token).unwrap();
    token
}

fn queued_card(verified: &Verified) -> OwedWork {
    let cards: Vec<_> = verified
        .collector
        .log
        .work
        .iter()
        .filter(|work| {
            matches!(&work.derived,
            Derived::Piece(piece) if piece.unit_key.native_key == "native-tool#card")
        })
        .collect();
    assert_eq!(cards.len(), 1, "{:?}", verified.collector.log.work);
    cards[0].clone()
}

fn assert_no_card_send(verified: &mut Verified) {
    assert!(verified.port.posts().is_empty());
    let ledger = verified.writer.store().ledger();
    assert_eq!(ledger.next_serial(), 0, "held card must not prepare");
    assert!(ledger.unresolved().is_none());
    assert_eq!(ledger.violation(), None);
    assert!(!verified.writer.is_stopped());
    let queued = queued_card(verified);
    assert!(matches!(queued.derived, Derived::Piece(_)));
}

#[tokio::test(start_paused = true)]
async fn codex_body_and_idle_cards_flow_through_real_owed_origin_and_exact_permit() {
    for (channel, closes_on_body) in [(584_631, true), (584_632, false)] {
        let mut verified = Verified::new(channel, true);
        cards_on(&mut verified);
        let span = append_tool_turn(&mut verified, closes_on_body);
        verified
            .writer
            .store()
            .persist_codex_span(span.clone())
            .unwrap();
        let _token = permit(&span);
        verified.read();
        let queued = queued_card(&verified);
        let Derived::Piece(piece) = &queued.derived else {
            unreachable!()
        };
        assert_eq!(piece.unit_key.provider, ShadowProvider::Codex);
        assert_eq!(piece.unit_key.kind, UnitKind::Tool);
        assert!(piece.payload.starts_with("-# ▸ "));
        assert!(piece.payload.ends_with('✓'));
        assert!(!piece.payload.contains("PRIVATE_CODEX_RESULT"));
        assert_eq!(queued.source, verified.a);
        assert!(queued.range.start >= span.start && queued.range.end <= span.end.unwrap());
        assert!(
            queued.origin.is_none(),
            "capture cannot create a permit origin"
        );
        crate::services::tui_o::writer::owed::reevaluate(
            verified.writer.store(),
            &mut verified.collector.log.work,
        )
        .unwrap();
        let origin = queued_card(&verified).origin.unwrap();
        assert_eq!(origin.span, span);
        assert_eq!(origin.range, queued.range);
        assert!(admission::lookup(&origin).is_ok());
        verified.deliver().await;
        verified.deliver().await;
        let posts = verified.port.posts();
        assert_eq!(posts[0], piece.payload);
        assert_eq!(posts.len(), if closes_on_body { 2 } else { 1 });
        if closes_on_body {
            assert_eq!(posts[1], "answer");
        }
        assert!(verified.collector.log.work.is_empty());
        let ledger = verified.writer.store().ledger();
        assert_eq!(ledger.piece(0).unwrap().unit_key, piece.unit_key);
        assert!(matches!(
            ledger.piece(0).unwrap().outcome,
            Some(PieceOutcome::Posted(_))
        ));
        assert_eq!(
            ledger.piece(0).unwrap().local_retry,
            None,
            "exact permits keep retry authority"
        );
        assert_eq!(ledger.violation(), None);
    }
}

#[tokio::test(start_paused = true)]
async fn codex_card_with_a_permit_but_without_closed_origin_stays_owed() {
    let mut verified = Verified::new(584_633, true);
    cards_on(&mut verified);
    let span = append_tool_turn(&mut verified, true);
    let _token = permit(&span);
    verified.read();
    assert!(queued_card(&verified).origin.is_none());
    for _ in 0..2 {
        verified.deliver().await;
        assert_no_card_send(&mut verified);
        assert!(queued_card(&verified).origin.is_none());
    }
    verified.writer.store().persist_codex_span(span).unwrap();
    verified.deliver().await;
    let posts = verified.port.posts();
    assert_eq!(
        posts.len(),
        2,
        "the same held card sends only after origin exists"
    );
    assert!(posts[0].starts_with("-# ▸ "));
    assert_eq!(posts[1], "answer");
}

#[tokio::test(start_paused = true)]
async fn codex_card_cannot_use_another_episodes_permit_or_bypass_exact_admission() {
    let mut verified = Verified::new(584_634, true);
    cards_on(&mut verified);
    let span = append_tool_turn(&mut verified, true);
    verified
        .writer
        .store()
        .persist_codex_span(span.clone())
        .unwrap();
    let mut foreign = span.clone();
    foreign.episode.user_message_id = 2;
    foreign.episode.turn_nonce = "turn-2".into();
    foreign.episode.native_turn_id = "n2".into();
    let _token = permit(&foreign);
    verified.read();
    crate::services::tui_o::writer::owed::reevaluate(
        verified.writer.store(),
        &mut verified.collector.log.work,
    )
    .unwrap();
    let queued = queued_card(&verified);
    let origin = queued.origin.as_ref().unwrap();
    assert_eq!(
        admission::lookup(origin).err(),
        Some("episode_permit_unavailable")
    );
    let mut foreign_origin = origin.clone();
    foreign_origin.span = foreign;
    let wrong_record = admission::lookup(&foreign_origin).unwrap();
    assert_eq!(
        verified
            .writer
            .deliver_admitted(&queued.derived, Some((&wrong_record, origin)),)
            .await,
        Step::Withheld
    );
    for _ in 0..2 {
        verified.deliver().await;
        assert_no_card_send(&mut verified);
        assert_eq!(queued_card(&verified).origin.unwrap().span, span);
    }
}

#[tokio::test(start_paused = true)]
async fn codex_card_with_revoked_exact_permit_stays_owed_without_prepare_or_post() {
    let mut verified = Verified::new(584_635, true);
    cards_on(&mut verified);
    let span = append_tool_turn(&mut verified, false);
    verified
        .writer
        .store()
        .persist_codex_span(span.clone())
        .unwrap();
    let _token = permit(&span);
    verified.read();
    admission::revoke_channel(584_635, "tool_card_test_revoked");
    for _ in 0..2 {
        verified.deliver().await;
        assert_no_card_send(&mut verified);
        let queued = queued_card(&verified);
        assert_eq!(queued.origin.unwrap().span, span);
    }
}
