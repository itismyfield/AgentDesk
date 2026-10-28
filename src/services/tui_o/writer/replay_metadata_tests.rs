use super::*;
use crate::services::tui_o::writer::pieces::SummaryInputs;

fn projections(verified: &mut Verified) -> UnitDeriver {
    let channel = verified.writer.channel();
    let ledger = verified.writer.store().ledger();
    UnitDeriver::new(channel, ShadowProvider::Codex)
        .with_tool_cards(ToolCardInputs {
            known: ledger.non_card_tool_keys(),
            recorded_cards: ledger.card_tool_keys(),
            switch: Arc::new(|| true),
        })
        .with_turn_summary(SummaryInputs {
            turn_marks: ledger.summary_turn_marks(),
            disposed: ledger.disposed_unit_hashes(),
            context_window: None,
        })
}

#[tokio::test(start_paused = true)]
async fn restart_replay_verified_metadata_survives_filter_and_later_exact_permits() {
    let mut verified = Verified::new(584_659, true);
    verified.collector.deriver = projections(&mut verified);
    let first_start = len(&verified.a);
    append(
        &verified.a.path,
        &[
            event("task_started", "n1"),
            tool("old"),
            body("old-body", "old answer"),
            event("task_complete", "n1"),
        ],
    );
    let first = episode_span(584_659, &verified.a, 1, (first_start, len(&verified.a)));
    verified
        .writer
        .store()
        .persist_codex_span(first.clone())
        .unwrap();
    let _first_token = grant(&first);
    verified.read();
    verified.deliver().await;
    assert_eq!(
        verified.port.posts().len(),
        3,
        "card, body, summary must actually post"
    );
    let second_start = len(&verified.a);
    append(
        &verified.a.path,
        &[event("task_started", "n2"), tool("pending")],
    );
    verified.read();
    verified.deliver().await;
    let bindings = Bindings::on(584_659);
    bindings.bind(None, &verified.a, BindingCause::Startup);
    let mut sources = Sources::new(584_659, ShadowProvider::Codex, bindings);
    let mut deriver = projections(&mut verified);
    let mut log = OwedLog::new(584_659, ShadowProvider::Codex);
    sources
        .resume(&mut verified.writer, &mut deriver, &mut log)
        .unwrap();
    assert!(
        sources.replayed.posted >= 3,
        "resume did not remove completed pieces"
    );
    assert!(sources.replayed.excluded > 0);
    let metadata = log
        .replay_metadata
        .as_ref()
        .expect("finish_replay was bypassed");
    assert!(!metadata.associations.is_empty());
    assert!(!metadata.cards.is_empty());
    assert!(!metadata.pending.is_empty());
    assert_eq!(metadata.associations, log.associations);
    assert_eq!(metadata.cards, log.card_members);
    assert_eq!(metadata.pending, log.pending_card_members);
    verified.collector = Collector {
        sources,
        deriver,
        log,
    };
    append(
        &verified.a.path,
        &[body("new-body", "new answer"), event("task_complete", "n2")],
    );
    let second = episode_span(584_659, &verified.a, 2, (second_start, len(&verified.a)));
    verified
        .writer
        .store()
        .persist_codex_span(second.clone())
        .unwrap();
    verified.read();
    verified.deliver().await;
    assert_eq!(
        verified.port.posts().len(),
        3,
        "unpermitted card/body/summary must stay owed"
    );
    assert_eq!(verified.collector.log.work.len(), 3);
    assert!(
        verified
            .collector
            .log
            .work
            .iter()
            .all(|work| work.origin.is_some())
    );
    let _second_token = grant(&second);
    verified.deliver().await;
    let posts = verified.port.posts();
    assert_eq!(posts.len(), 6);
    assert!(posts[3].starts_with("-# ▸ "), "pending tool lost its card");
    assert_eq!(&posts[4..], ["new answer", "-# ✅ 완료"]);
    assert!(verified.collector.log.work.is_empty());
}

#[tokio::test(start_paused = true)]
async fn restart_replay_verified_waits_for_origin_before_settling_and_refilters_once() {
    let channel = 584_680;
    let mut verified = Verified::new(channel, true);
    let _old = verified.turn(1, &["old"], true).unwrap();
    verified.read();
    verified.deliver().await;
    let fresh = verified.turn(2, &["fresh"], false);
    assert!(fresh.is_none());
    verified.read();
    let ledger = verified.writer.store().ledger();
    let (serial, anchor_id) = (ledger.next_serial(), ledger.anchor());
    verified
        .writer
        .store()
        .append_ledger(LedgerEntry::Prepared {
            serial,
            unit_key: UnitKey {
                channel_id: channel,
                provider: ShadowProvider::Codex,
                native_key: "open-history".into(),
                kind: UnitKind::Body,
            },
            piece_index: 0,
            payload: "open exact".into(),
            anchor_id,
            epoch: 1,
            local_retry: None,
        })
        .unwrap();
    verified
        .port
        .posts
        .lock()
        .unwrap()
        .push("open exact".into());
    *verified.port.history_override.lock().unwrap() = Some(vec![SeenMessage {
        id: 1002,
        author_id: 42,
        content: "open exact".into(),
    }]);
    let bindings = Bindings::on(channel);
    bindings.bind(None, &verified.a, BindingCause::Startup);
    let mut sources = Sources::new(channel, ShadowProvider::Codex, bindings);
    let mut deriver = UnitDeriver::new(channel, ShadowProvider::Codex);
    let mut log = OwedLog::new(channel, ShadowProvider::Codex);
    sources
        .resume(&mut verified.writer, &mut deriver, &mut log)
        .unwrap();
    assert!(sources.replayed.deferred);
    assert_eq!(log.work.len(), 2);
    verified.collector = Collector {
        sources,
        deriver,
        log,
    };
    let file = channel_file_on(&verified._runtime, channel, CODEX_PROVENANCE_FILE);
    let held = file.with_extension("held");
    std::fs::rename(&file, &held).unwrap();
    verified.deliver().await;
    assert!(
        verified.writer.store().ledger().unresolved().is_some(),
        "Unknown origin must not force history settlement"
    );
    assert_eq!(verified.port.history_reads.load(Ordering::SeqCst), 0);
    std::fs::rename(&held, &file).unwrap();
    verified.deliver().await;
    assert!(verified.writer.store().ledger().unresolved().is_none());
    assert_eq!(verified.port.history_reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        verified.collector.log.work.len(),
        1,
        "old Posted should be filtered once"
    );
    assert_eq!(payload(&verified.collector.log.work[0]), "fresh");
    assert_eq!(verified.port.posts(), ["old", "open exact"]);
    assert!(!verified.collector.log.replay_deferred);
    let span = present(verified.writer.store())
        .spans
        .into_iter()
        .find(|span| span.episode.user_message_id == 2)
        .unwrap();
    let _fresh = grant(&span);
    verified.deliver().await;
    assert_eq!(verified.port.posts(), ["old", "open exact", "fresh"]);
    assert!(verified.collector.log.work.is_empty());
}
