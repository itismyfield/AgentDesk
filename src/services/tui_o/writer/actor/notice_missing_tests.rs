use super::*;
use crate::services::tui_o::store::ledger::input_notices::NoticeEvidence;

fn piece(evidence: &NoticeEvidence) -> Derived {
    Derived::Piece(PieceWork {
        output_origin: None,
        unit_key: evidence.unit_key.clone().unwrap(),
        index: 0,
        payload: evidence.payload.clone().unwrap(),
        notice_origin: evidence.origin.clone(),
        reply: None,
    })
}

fn calls() -> (usize, usize) {
    crate::services::tui_o::writer::input_notices::admission::NOTICE_CALLS.with(|c| c.get())
}

fn reset_calls() {
    crate::services::tui_o::writer::input_notices::admission::NOTICE_CALLS.with(|c| c.set((0, 0)));
}

#[tokio::test(start_paused = true)]
async fn missing_evidence_waits_across_polls_and_a_fresh_writer_until_a_real_change() {
    let runtime_guard = crate::config::TestRuntimeRootGuard::new();
    let runtime = tempfile::tempdir().unwrap();
    let (o, evidence, binding) = rerender_fixture(runtime.path());
    let port = Arc::new(Port::default());
    let progress = crate::services::tui_o::writer::progress::Publisher::new(CHANNEL);
    for incarnation in 0..2 {
        let mut actor = host(&o, port.clone(), Alarms::default(), u64::MAX);
        actor.owed_mut_for_test().push_back(piece(&evidence));
        reset_calls();
        actor.deliver_owed_for_test(&progress).await;
        assert_eq!(calls(), (1, 1), "boot observes the debt once");
        for poll in 0..20 {
            tokio::time::advance(std::time::Duration::from_secs(1)).await;
            actor.prepare_notices_for_test(&[]).await;
            actor.deliver_owed_for_test(&progress).await;
            assert_eq!(
                calls(),
                (1, 1),
                "unchanged poll {poll}, incarnation {incarnation}"
            );
        }
        assert!(port.posts.lock().unwrap().is_empty());
        assert_eq!(
            actor.owed_for_test().len(),
            1,
            "unknown evidence must not be lost"
        );
    }
    drop(binding);
    drop(runtime_guard);
}

#[tokio::test(start_paused = true)]
async fn missing_evidence_arrival_wakes_and_posts_exactly_once() {
    let runtime_guard = crate::config::TestRuntimeRootGuard::new();
    let runtime = tempfile::tempdir().unwrap();
    let (o, evidence, binding) = rerender_fixture(runtime.path());
    let port = Arc::new(Port::default());
    let mut actor = host(&o, port.clone(), Alarms::default(), u64::MAX);
    start_migration(&mut actor);
    actor.owed_mut_for_test().push_back(piece(&evidence));
    let progress = crate::services::tui_o::writer::progress::Publisher::new(CHANNEL);
    actor.deliver_owed_for_test(&progress).await;
    assert!(port.posts.lock().unwrap().is_empty());
    actor
        .writer_mut_for_test()
        .record_for_test(LedgerEntry::InputNotice {
            entry: Box::new(NoticeEntry::Materialized {
                evidence: Box::new(evidence.clone()),
            }),
        })
        .unwrap();
    actor.prepare_notices_for_test(&[]).await;
    actor.deliver_owed_for_test(&progress).await;
    assert_eq!(
        *port.posts.lock().unwrap(),
        [evidence.payload.clone().unwrap()]
    );
    assert!(actor.owed_for_test().is_empty());
    for _ in 0..10 {
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        actor.prepare_notices_for_test(&[]).await;
        actor.deliver_owed_for_test(&progress).await;
    }
    assert_eq!(port.posts.lock().unwrap().len(), 1);
    drop(actor);
    let mut actor = host(&o, port.clone(), Alarms::default(), u64::MAX);
    resume(&mut actor);
    let fresh = actor.read_sources_for_test();
    actor.prepare_notices_for_test(&fresh).await;
    actor.deliver_owed_for_test(&progress).await;
    assert!(actor.owed_for_test().is_empty());
    assert_eq!(
        port.posts.lock().unwrap().len(),
        1,
        "durable POST survives restart and native recapture"
    );
    drop(binding);
    drop(runtime_guard);
}

#[tokio::test(start_paused = true)]
async fn missing_evidence_terminal_ledger_wakes_without_materialization() {
    let runtime_guard = crate::config::TestRuntimeRootGuard::new();
    let runtime = tempfile::tempdir().unwrap();
    let (o, evidence, binding) = rerender_fixture(runtime.path());
    let port = Arc::new(Port::default());
    let mut actor = host(&o, port.clone(), Alarms::default(), u64::MAX);
    actor.owed_mut_for_test().push_back(piece(&evidence));
    let progress = crate::services::tui_o::writer::progress::Publisher::new(CHANNEL);
    actor.deliver_owed_for_test(&progress).await;
    actor
        .writer_mut_for_test()
        .record_for_test(LedgerEntry::Excluded {
            unit_key: evidence.unit_key.clone().unwrap(),
            reason: "input_notice_source".into(),
        })
        .unwrap();
    actor.deliver_owed_for_test(&progress).await;
    assert!(actor.owed_for_test().is_empty());
    assert!(port.posts.lock().unwrap().is_empty());
    assert_eq!(actor.progress_for_test().debt, 0);
    drop(binding);
    drop(runtime_guard);
}

#[tokio::test(start_paused = true)]
async fn proven_floor_replay_without_a_live_session_closes_and_clears_health() {
    let runtime_guard = crate::config::TestRuntimeRootGuard::new();
    let runtime = tempfile::tempdir().unwrap();
    let (o, evidence, binding) = rerender_fixture(runtime.path());
    let port = Arc::new(Port::default());
    let router = Arc::new(crate::services::tui_o::alarm::AlarmRouter::for_process(
        None, None,
    ));
    let alarms = Alarms(Default::default(), Some(router.clone()));
    let mut previous = host(&o, port.clone(), Alarms::default(), u64::MAX);
    *previous.deriver_mut_for_test() = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    resume(&mut previous);
    previous.read_sources_for_test();
    let captured = previous
        .writer_mut_for_test()
        .store()
        .cursor(&evidence.record.source)
        .unwrap()
        .clone();
    assert_eq!(captured.captured_through, evidence.record.end);
    assert!(
        previous
            .writer_mut_for_test()
            .store()
            .retained_segments(&evidence.record.source)
            > 0
    );
    assert!(previous.owed_for_test().is_empty());
    drop(previous);
    let mut actor = host(&o, port.clone(), alarms.clone(), u64::MAX);
    actor
        .writer_mut_for_test()
        .record_for_test(LedgerEntry::InputNotice {
            entry: Box::new(NoticeEntry::MigrationStarted {
                snapshot: MigrationSnapshot {
                    sources: vec![MigrationSource {
                        source: evidence.record.source.clone(),
                        lower: captured.captured_through,
                        upper: captured.captured_through,
                        prefix_hash: captured.prefix_hash,
                    }],
                    binding_checkpoint: 1,
                    history: MigrationHistory::Fixed {
                        lower: 10,
                        upper: 10,
                    },
                },
            }),
        })
        .unwrap();
    let progress = crate::services::tui_o::writer::progress::Publisher::new(CHANNEL);
    for incarnation in 0..2 {
        if incarnation == 1 {
            drop(actor);
            actor = host(&o, port.clone(), alarms.clone(), u64::MAX);
        }
        router.raise(
            CHANNEL,
            WriterAlarm::WaitingTooLong {
                incident: format!("before-floor-{incarnation}"),
                ready: 1,
                prepared: None,
            },
        );
        let waiting_reason = format!("tui_o:waiting_too_long:{CHANNEL}");
        assert!(crate::services::tui_o::alarm::health_reasons().contains(&waiting_reason));
        resume(&mut actor);
        actor.read_sources_for_test();
        actor.prepare_notices_for_test(&[]).await;
        actor.deliver_owed_for_test(&progress).await;
        assert_eq!(
            actor
                .writer_mut_for_test()
                .store()
                .ledger()
                .excluded(evidence.unit_key.as_ref().unwrap()),
            Some("input_notice_migration_floor"),
            "incarnation {incarnation}"
        );
        assert!(actor.owed_for_test().is_empty());
        assert_eq!(actor.progress_for_test().debt, 0);
        tokio::time::advance(WAIT_LIMIT_FOR_TEST + std::time::Duration::from_secs(1)).await;
        actor.observe_waiting_for_test(true);
        assert!(actor.waiting_ready_for_test().is_empty());
        assert!(actor.waiting_incident_for_test().is_none());
        assert!(!crate::services::tui_o::alarm::health_reasons().contains(&waiting_reason));
        assert_eq!(actor.undelivered_for_test(1).unwrap().notice_owed, 0);
    }
    assert!(port.posts.lock().unwrap().is_empty());
    assert!(
        alarms
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|alarm| !matches!(alarm, WriterAlarm::WaitingTooLong { .. }))
    );
    drop(binding);
    drop(runtime_guard);
}

#[tokio::test(start_paused = true)]
async fn missing_evidence_does_not_block_the_later_body() {
    let runtime_guard = crate::config::TestRuntimeRootGuard::new();
    let runtime = tempfile::tempdir().unwrap();
    let (o, evidence, binding) = rerender_fixture(runtime.path());
    let port = Arc::new(Port::default());
    let mut actor = host(&o, port.clone(), Alarms::default(), u64::MAX);
    actor.owed_mut_for_test().push_back(piece(&evidence));
    let mut body = match piece(&evidence) {
        Derived::Piece(piece) => piece,
        _ => unreachable!(),
    };
    body.unit_key.kind = UnitKind::Body;
    body.unit_key.native_key = "body-after-missing".into();
    body.notice_origin = None;
    body.payload = "later answer".into();
    actor.owed_mut_for_test().push_back(Derived::Piece(body));
    let progress = crate::services::tui_o::writer::progress::Publisher::new(CHANNEL);
    reset_calls();
    actor.deliver_owed_for_test(&progress).await;
    assert_eq!(*port.posts.lock().unwrap(), ["later answer"]);
    assert_eq!(actor.owed_for_test().len(), 1);
    actor.prepare_notices_for_test(&[]).await;
    actor.deliver_owed_for_test(&progress).await;
    assert_eq!(calls(), (1, 1));
    drop(binding);
    drop(runtime_guard);
}

#[tokio::test(start_paused = true)]
async fn floored_evidence_does_not_exclude_a_piece_with_another_origin() {
    let runtime_guard = crate::config::TestRuntimeRootGuard::new();
    let runtime = tempfile::tempdir().unwrap();
    let (o, evidence, binding) = rerender_fixture(runtime.path());
    let mut actor = host(&o, Arc::new(Port::default()), Alarms::default(), u64::MAX);
    actor
        .writer_mut_for_test()
        .record_for_test(LedgerEntry::InputNotice {
            entry: Box::new(NoticeEntry::MigrationStarted {
                snapshot: MigrationSnapshot {
                    sources: vec![MigrationSource {
                        source: evidence.record.source.clone(),
                        lower: evidence.record.end,
                        upper: evidence.record.end,
                        prefix_hash: hex::encode(Sha256::digest(b"")),
                    }],
                    binding_checkpoint: 1,
                    history: MigrationHistory::Fixed {
                        lower: 10,
                        upper: 10,
                    },
                },
            }),
        })
        .unwrap();
    let record = CapturedRecord {
        start: evidence.record.start,
        end: evidence.record.end,
        line: evidence.raw_record.clone(),
    };
    let derived = actor.derive_scoped_for_test(
        &record,
        &evidence.record.source,
        crate::services::tui_o::shadow::IDENTITY_VERSION,
        Ok(*evidence.origin.clone().unwrap()),
        || None,
    );
    *actor.owed_mut_for_test() = derived.into();
    assert_eq!(actor.owed_for_test().len(), 1);
    let Derived::Piece(queued) = actor.owed_mut_for_test().front_mut().unwrap() else {
        panic!("notice piece required");
    };
    queued
        .notice_origin
        .as_mut()
        .unwrap()
        .logical_session
        .push_str("-other-owner");
    actor.persist_notice_evidence_for_test();
    assert!(
        actor
            .writer_mut_for_test()
            .store()
            .ledger()
            .excluded(evidence.unit_key.as_ref().unwrap())
            .is_none()
    );
    assert_eq!(actor.owed_for_test().len(), 1);
    drop(binding);
    drop(runtime_guard);
}

#[tokio::test(start_paused = true)]
async fn floor_boundaries_keep_late_input_without_an_old_record_proof() {
    let runtime_guard = crate::config::TestRuntimeRootGuard::new();
    for (known_source, timestamp, checkpoint) in [
        (true, None, 1),
        (false, None, 0),
        (false, Some("2099-01-01T00:00:00Z"), 1),
    ] {
        let runtime = tempfile::tempdir().unwrap();
        let (o, mut evidence, binding) = rerender_fixture(runtime.path());
        if let Some(timestamp) = timestamp {
            let mut value: serde_json::Value =
                serde_json::from_slice(&evidence.raw_record).unwrap();
            value["timestamp"] = timestamp.into();
            evidence.raw_record = serde_json::to_vec(&value).unwrap();
            evidence.record.end = evidence.raw_record.len() as u64 + 1;
            evidence.origin.as_mut().unwrap().record = evidence.record.clone();
            let mut native = evidence.raw_record.clone();
            native.push(b'\n');
            std::fs::write(&evidence.record.source.path, native).unwrap();
        }
        let port = Arc::new(Port::default());
        let mut actor = host(&o, port.clone(), Alarms::default(), u64::MAX);
        actor
            .writer_mut_for_test()
            .record_for_test(LedgerEntry::InputNotice {
                entry: Box::new(NoticeEntry::MigrationStarted {
                    snapshot: MigrationSnapshot {
                        sources: if known_source {
                            vec![MigrationSource {
                                source: evidence.record.source.clone(),
                                lower: 0,
                                upper: 0,
                                prefix_hash: hex::encode(Sha256::digest(b"")),
                            }]
                        } else {
                            Vec::new()
                        },
                        binding_checkpoint: checkpoint,
                        history: MigrationHistory::Fixed {
                            lower: 10,
                            upper: 10,
                        },
                    },
                }),
            })
            .unwrap();
        let record = CapturedRecord {
            start: evidence.record.start,
            end: evidence.record.end,
            line: evidence.raw_record.clone(),
        };
        *actor.owed_mut_for_test() = actor
            .derive_scoped_for_test(
                &record,
                &evidence.record.source,
                crate::services::tui_o::shadow::IDENTITY_VERSION,
                Ok(*evidence.origin.clone().unwrap()),
                || None,
            )
            .into();
        assert_eq!(actor.owed_for_test().len(), 1);
        actor.persist_notice_evidence_for_test();
        let progress = crate::services::tui_o::writer::progress::Publisher::new(CHANNEL);
        actor.deliver_owed_for_test(&progress).await;
        assert_eq!(
            *port.posts.lock().unwrap(),
            [evidence.payload.unwrap()],
            "known_source={known_source}, timestamp={timestamp:?}, checkpoint={checkpoint}"
        );
        assert!(
            actor
                .writer_mut_for_test()
                .store()
                .ledger()
                .excluded(evidence.unit_key.as_ref().unwrap())
                .is_none()
        );
        drop(actor);
        drop(binding);
    }
    drop(runtime_guard);
}

#[tokio::test(start_paused = true)]
async fn an_old_binding_without_a_timestamp_does_not_prove_a_new_rows_floor() {
    let runtime_guard = crate::config::TestRuntimeRootGuard::new();
    let runtime = tempfile::tempdir().unwrap();
    let (o, evidence, binding) = rerender_fixture(runtime.path());
    std::fs::write(&evidence.record.source.path, b"").unwrap();
    let port = Arc::new(Port::default());
    let mut actor = host(&o, port.clone(), Alarms::default(), u64::MAX);
    actor
        .writer_mut_for_test()
        .record_for_test(LedgerEntry::InputNotice {
            entry: Box::new(NoticeEntry::MigrationStarted {
                snapshot: MigrationSnapshot {
                    sources: Vec::new(),
                    binding_checkpoint: 1,
                    history: MigrationHistory::Fixed {
                        lower: 10,
                        upper: 10,
                    },
                },
            }),
        })
        .unwrap();
    let mut native = evidence.raw_record.clone();
    native.push(b'\n');
    append(&evidence.record.source.path, &native);
    resume(&mut actor);
    actor.read_sources_for_test();
    assert_eq!(
        actor.owed_for_test().len(),
        1,
        "the row was actually captured after the freeze"
    );
    let key = evidence.unit_key.as_ref().unwrap();
    assert!(
        actor
            .writer_mut_for_test()
            .store()
            .ledger()
            .excluded(key)
            .is_none()
    );
    let progress = crate::services::tui_o::writer::progress::Publisher::new(CHANNEL);
    reset_calls();
    actor.deliver_owed_for_test(&progress).await;
    for _ in 0..20 {
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        actor.prepare_notices_for_test(&[]).await;
        actor.deliver_owed_for_test(&progress).await;
    }
    assert_eq!(calls(), (1, 1));
    assert!(
        actor
            .writer_mut_for_test()
            .store()
            .ledger()
            .excluded(key)
            .is_none()
    );
    assert_eq!(
        actor.owed_for_test().len(),
        1,
        "uncertain historical input must stay recoverable"
    );
    assert!(port.posts.lock().unwrap().is_empty());
    drop(binding);
    drop(runtime_guard);
}
