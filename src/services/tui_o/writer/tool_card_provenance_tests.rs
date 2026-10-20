use super::*;
use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource, UnitKind};
use crate::services::tui_o::writer::pieces::{TOOL_CALL_CARD, ToolCardInputs};
use crate::services::tui_o::writer::rotation::Owe;
use std::sync::atomic::AtomicBool;

fn cards_on(verified: &mut Verified) -> Arc<AtomicBool> {
    let enabled = Arc::new(AtomicBool::new(true));
    let switch = enabled.clone();
    let channel = verified.writer.channel();
    let ledger = verified.writer.store().ledger();
    verified.collector.deriver =
        UnitDeriver::new(channel, ShadowProvider::Codex).with_tool_cards(ToolCardInputs {
            known: ledger.non_card_tool_keys(),
            recorded_cards: ledger.card_tool_keys(),
            switch: Arc::new(move || switch.load(Ordering::SeqCst)),
        });
    enabled
}

fn tool(id: &str) -> String {
    let value = serde_json::json!({"type":"response_item", "payload": {
        "type":"function_call", "id":id, "call_id":format!("call-{id}"),
        "name":"exec_command", "arguments":"{\"cmd\":\"cargo check\"}"
    }});
    format!("{value}\n")
}

fn capture(verified: &mut Verified, source: &SourceId) {
    let store = verified.writer.store();
    if store.cursor(source).is_none() {
        store.attach_source(source).unwrap();
    }
    let cursor = store.cursor(source).unwrap().clone();
    let mut capture = crate::services::tui_o::shadow::capture::SourceCapture::reopen(
        source.clone(),
        cursor.captured_through,
        &cursor.prefix_hash,
    )
    .unwrap();
    let CaptureOutcome::Batch(batch) = capture.poll(1 << 20) else {
        panic!("capture failed")
    };
    store.append_spool(&batch, &capture.prefix_hash()).unwrap();
    for record in batch.records {
        let derived = verified.collector.deriver.derive(&record);
        verified.collector.log.owe(source, &record, derived);
    }
}

fn queued_card(verified: &Verified) -> &OwedWork {
    let cards = verified.collector.log.work.iter().filter(|work| {
        matches!(&work.derived, Derived::Piece(piece)
            if piece.unit_key.kind == UnitKind::Tool && piece.unit_key.native_key.ends_with("#card"))
    }).collect::<Vec<_>>();
    assert_eq!(cards.len(), 1, "{:?}", verified.collector.log.work);
    cards[0]
}

fn grant(span: &CodexEpisodeSpan) -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        span.episode.turn_nonce.clone(),
    )));
    issue_for_tests(span.execution.clone(), span.episode.clone(), &token).unwrap();
    token
}

fn assert_held(verified: &mut Verified) {
    assert!(
        verified.port.posts().is_empty(),
        "a foreign origin authorized a card"
    );
    assert_eq!(verified.writer.store().ledger().next_serial(), 0);
    assert!(!verified.writer.is_stopped());
    assert!(queued_card(verified).origin.is_none());
}

#[tokio::test(start_paused = true)]
async fn an_old_pending_card_cannot_borrow_the_next_turns_closed_origin_when_off() {
    let mut verified = Verified::new(584_651, true);
    let enabled = cards_on(&mut verified);
    append(
        &verified.a.path,
        &[event("task_started", "n1"), tool("old")],
    );
    verified.read();
    verified.deliver().await;
    let native = UnitKey {
        channel_id: 584_651,
        provider: ShadowProvider::Codex,
        native_key: "old".into(),
        kind: UnitKind::Tool,
    };
    assert_eq!(
        verified.writer.store().ledger().excluded(&native),
        Some(TOOL_CALL_CARD)
    );
    enabled.store(false, Ordering::SeqCst);
    let start = len(&verified.a);
    append(
        &verified.a.path,
        &[
            event("task_started", "n2"),
            body("new", "new answer"),
            event("task_complete", "n2"),
        ],
    );
    let span = episode_span(584_651, &verified.a, 2, (start, len(&verified.a)));
    verified
        .writer
        .store()
        .persist_codex_span(span.clone())
        .unwrap();
    let _token = grant(&span);
    verified.read();
    assert!(
        matches!(&queued_card(&verified).derived, Derived::Piece(piece)
        if piece.unit_key.native_key == "old#card")
    );
    for _ in 0..2 {
        verified.deliver().await;
        assert_held(&mut verified);
    }
}

#[tokio::test(start_paused = true)]
async fn a_card_with_members_from_distinct_source_episodes_stays_owed() {
    let mut verified = Verified::new(584_652, true);
    cards_on(&mut verified);
    let a = verified.a.clone();
    let start = len(&a);
    append(&a.path, &[tool("first")]);
    capture(&mut verified, &a);
    let first = episode_span(584_652, &a, 1, (start, len(&a)));
    let b = transcript(
        &a.path,
        "b.jsonl",
        "s2",
        &[tool("last"), body("answer", "answer")],
    );
    capture(&mut verified, &b);
    let mut second = episode_span(584_652, &b, 2, (0, len(&b)));
    second.execution.proof_seq += 1;
    second.execution.execution_nonce = "second-execution".into();
    for span in [&first, &second] {
        verified
            .writer
            .store()
            .persist_codex_span(span.clone())
            .unwrap();
    }
    let (_first, _second) = (grant(&first), grant(&second));
    for _ in 0..2 {
        verified.deliver().await;
        assert_held(&mut verified);
    }
}

#[tokio::test(start_paused = true)]
async fn missing_or_ambiguous_native_member_coordinates_cannot_authorize_a_card() {
    for (channel, ambiguous) in [(584_653, false), (584_654, true)] {
        let mut verified = Verified::new(channel, true);
        cards_on(&mut verified);
        let a = verified.a.clone();
        let start = len(&a);
        append(
            &a.path,
            &[tool("first"), tool("last"), body("answer", "answer")],
        );
        capture(&mut verified, &a);
        let span = episode_span(channel, &a, 1, (start, len(&a)));
        verified
            .writer
            .store()
            .persist_codex_span(span.clone())
            .unwrap();
        let _token = grant(&span);
        let key = UnitKey {
            channel_id: channel,
            provider: ShadowProvider::Codex,
            native_key: "first".into(),
            kind: UnitKind::Tool,
        };
        if ambiguous {
            let coordinates = verified.collector.log.associations.get_mut(&key).unwrap();
            let mut other = coordinates[0].clone();
            other.1.start += 1;
            coordinates.push(other);
        } else {
            verified.collector.log.associations.remove(&key);
        }
        verified.deliver().await;
        assert_held(&mut verified);
    }
}

#[tokio::test(start_paused = true)]
async fn members_with_one_exact_origin_allow_a_card_despite_repeated_identical_coordinates() {
    let mut verified = Verified::new(584_655, true);
    cards_on(&mut verified);
    let a = verified.a.clone();
    let start = len(&a);
    append(
        &a.path,
        &[tool("first"), tool("last"), body("answer", "answer")],
    );
    capture(&mut verified, &a);
    let span = episode_span(584_655, &a, 1, (start, len(&a)));
    verified
        .writer
        .store()
        .persist_codex_span(span.clone())
        .unwrap();
    let _token = grant(&span);
    for coordinates in verified.collector.log.associations.values_mut() {
        coordinates.push(coordinates[0].clone());
    }
    verified.deliver().await;
    assert_eq!(verified.port.posts().len(), 2);
    assert!(verified.port.posts()[0].starts_with("-# ▸ Bash"));
    assert_eq!(verified.port.posts()[1], "answer");
    assert!(verified.collector.log.work.is_empty());
    assert_eq!(verified.writer.store().ledger().violation(), None);
}
