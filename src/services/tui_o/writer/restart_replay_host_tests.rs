use super::*;
use crate::services::tui_o::shadow::CaptureBatch;
use crate::services::tui_o::store::ledger::{LocalRetryMark, PieceDisposition};
use crate::services::tui_o::writer::rotation::Owe;
use crate::services::tui_o::writer::rotation::Sources;
use crate::services::tui_o::writer::{boot, replay};
use std::io::BufWriter;
use std::time::{Duration, Instant};

const SETTLED: usize = 20_000;
// Large startup fixtures share their I/O budget; the library still uses its default parallelism.
static STARTUP_FIXTURE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn fixture(settled: usize) -> (Harness, SourceId, PieceWork) {
    fixture_records(settled, None)
}

fn fixture_records(settled: usize, open: Option<bool>) -> (Harness, SourceId, PieceWork) {
    let (harness, path, source) = switched_over(b"");
    let mut raw = Vec::new();
    let mut records = Vec::new();
    let mut entries = Vec::new();
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    let mut fresh_piece = None;
    let at = Utc::now();
    for index in 0..=settled + usize::from(open.is_some()) {
        let is_open = open.is_some() && index == settled;
        let text = if index < settled {
            format!("settled body {index}")
        } else if is_open {
            "open already posted".to_owned()
        } else {
            "new piece".to_owned()
        };
        let line = row(&format!("restart-{index}"), &text);
        let start = raw.len() as u64;
        raw.extend_from_slice(&line);
        let record = CapturedRecord {
            start,
            end: raw.len() as u64,
            line: line[..line.len() - 1].to_vec(),
        };
        let derived = deriver.derive(&record);
        let [Derived::Piece(piece)] = derived.as_slice() else {
            panic!("fixture must derive one body piece: {derived:?}");
        };
        if index < settled {
            entries.push(LedgerEntry::Prepared {
                notice_origin: None,
                serial: index as u64,
                unit_key: piece.unit_key.clone(),
                piece_index: piece.index,
                payload: piece.payload.clone(),
                anchor_id: if index == 0 { 100 } else { 999 + index as u64 },
                epoch: 1,
                local_retry: None,
            });
            entries.push(LedgerEntry::Posted {
                serial: index as u64,
                msg_id: 1000 + index as u64,
            });
            harness.port.say(BOT, &text);
        } else if is_open {
            entries.push(LedgerEntry::Prepared {
                notice_origin: None,
                serial: settled as u64,
                unit_key: piece.unit_key.clone(),
                piece_index: piece.index,
                payload: piece.payload.clone(),
                anchor_id: 999 + settled as u64,
                epoch: 1,
                local_retry: open.unwrap().then_some(LocalRetryMark::Budget),
            });
            harness.port.say(BOT, &text);
        } else {
            fresh_piece = Some(piece.clone());
        }
        records.push(record);
    }
    std::fs::write(&path, &raw).unwrap();
    let mut channel = harness.channel();
    let batch = CaptureBatch {
        source: source.clone(),
        records,
        captured_through: raw.len() as u64,
    };
    channel
        .append_spool(&batch, &hex::encode(Sha256::digest(&raw)))
        .unwrap();
    drop(channel);
    let ledger = harness.port.ledger.lock().unwrap().clone().unwrap();
    let mut output = BufWriter::new(std::fs::File::create(ledger).unwrap());
    for entry in entries {
        serde_json::to_writer(&mut output, &serde_json::json!({"at": at, "entry": entry})).unwrap();
        output.write_all(b"\n").unwrap();
    }
    output.flush().unwrap();
    output.get_ref().sync_all().unwrap();
    drop(output);
    let channel = harness.channel();
    assert_eq!(
        channel.ledger().next_serial(),
        settled as u64 + u64::from(open.is_some())
    );
    assert_eq!(channel.ledger().violation(), None);
    drop(channel);
    p5_log(harness._runtime.path(), CHANNEL, &startup(source.clone()));
    // The latency probe measures the host, without the fake port reading the entire ledger.
    *harness.port.ledger.lock().unwrap() = None;
    (harness, source, fresh_piece.unwrap())
}

async fn observe_host(settled: usize) -> (Duration, usize) {
    let _fixture = STARTUP_FIXTURE.lock().await;
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (harness, _, _) = fixture(settled);
    harness.gate.acquired();
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let first = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&first);
    let started = Instant::now();
    *harness.port.on_post.lock().unwrap() = Some(Box::new(move || {
        let calls = RESTART_POLICY_CALLS.with(|calls| calls.get().unwrap().1);
        seen.lock()
            .unwrap()
            .get_or_insert((started.elapsed(), calls));
    }));
    RESTART_POLICY_CALLS.with(|calls| calls.set(Some((CHANNEL, 0))));
    boot::capture();
    let mut handles = managed(&harness, &io, &ready);
    assert_eq!(handles.len(), 1, "actual managed host must be selected");
    let timeout = tokio::time::timeout(Duration::from_secs(30), async {
        while first.lock().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    handles.pop().unwrap().stop_and_join().await.unwrap();
    RESTART_POLICY_CALLS.with(|calls| calls.set(None));
    p5::set_test_root(None);
    timeout.expect("managed host never posted the fresh piece");
    assert_eq!(harness.port.posts(), ["new piece"], "settled reposts = 0");
    let channel = harness.channel();
    assert_eq!(channel.ledger().violation(), None);
    assert_eq!(channel.ledger().next_serial(), settled as u64 + 1);
    let observed = first.lock().unwrap().unwrap();
    assert!(observed.1 > 0, "counter missed the actor runtime thread");
    let events = boot::events();
    for stage in [
        "recover",
        "wait_metadata",
        "port",
        "hosted",
        "resume",
        "resume_replay",
        "first_drain",
        "first_post",
        "first_posted",
    ] {
        assert!(
            events.iter().any(|event| event.channel == CHANNEL
                && event.stage == stage
                && event.status == "ok"),
            "missing stage {stage}: {events:?}"
        );
    }
    let resumed = events
        .iter()
        .find(|event| event.stage == "resume_replay")
        .unwrap();
    assert_eq!(resumed.details.records, settled as u64 + 1);
    assert_eq!(resumed.details.replay.posted, settled);
    assert_eq!(
        resumed.details.replay.kept,
        if settled == 0 { 2 } else { 1 }
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.stage == "first_post")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.stage == "first_posted")
            .count(),
        1
    );
    eprintln!(
        "restart_replay_host settled={settled} first_post_ms={} policy_calls_before_post={} post_count=1",
        observed.0.as_millis(),
        observed.1
    );
    observed
}

#[tokio::test]
async fn restart_replay_host_settled_20000_first_post_within_two_seconds_once() {
    let (elapsed, _) = observe_host(SETTLED).await;
    assert!(
        elapsed < Duration::from_secs(2),
        "first POST took {elapsed:?}"
    );
}

#[tokio::test]
async fn restart_replay_host_policy_calls_do_not_scale_with_settled_units() {
    let (_, empty_calls) = observe_host(0).await;
    assert!(empty_calls > 0);
    let (_, settled_calls) = observe_host(SETTLED).await;
    assert!(settled_calls > 0);
    assert!(
        settled_calls <= empty_calls + 8,
        "policy calls scaled with settled pieces: empty={empty_calls}, settled={settled_calls}"
    );
}

fn pending_fixture(state: &str) -> (Harness, PieceWork) {
    let (harness, _, piece) = fixture(2);
    let mut channel = harness.channel();
    let excluded = channel.ledger().piece(1).unwrap().unit_key.clone();
    channel
        .append_ledger(LedgerEntry::Excluded {
            unit_key: excluded,
            reason: "fixture exclusion".into(),
        })
        .unwrap();
    channel
        .append_ledger(LedgerEntry::Prepared {
            notice_origin: None,
            serial: 2,
            unit_key: piece.unit_key.clone(),
            piece_index: piece.index,
            payload: piece.payload.clone(),
            anchor_id: channel.ledger().anchor(),
            epoch: 1,
            local_retry: None,
        })
        .unwrap();
    let outcome = match state {
        "unresolved" => Some(LedgerEntry::Unresolved {
            serial: 2,
            reason: "history unavailable".into(),
        }),
        "ambiguous" => Some(LedgerEntry::Ambiguous {
            serial: 2,
            candidates: vec![2000, 2001],
        }),
        "not_found" => Some(LedgerEntry::NotFound { serial: 2 }),
        "not_sent" => Some(LedgerEntry::NotSent { serial: 2 }),
        "authorized" => Some(LedgerEntry::Rejected {
            serial: 2,
            status: 403,
        }),
        "open" => None,
        _ => panic!("unknown fixture state"),
    };
    if let Some(outcome) = outcome {
        channel.append_ledger(outcome).unwrap();
    }
    drop(channel);
    if state == "authorized" {
        harness
            .store
            .record_operator_resume(CHANNEL, 2, "fixture", "retry after refusal")
            .unwrap();
    }
    (harness, piece)
}

#[tokio::test]
async fn restart_replay_retains_open_unresolved_authorized_pieces() {
    for state in [
        "unresolved",
        "ambiguous",
        "not_found",
        "not_sent",
        "authorized",
    ] {
        let (harness, piece) = pending_fixture(state);
        p5::set_test_root(Some(harness._runtime.path()));
        let mut writer = harness.writer();
        assert!(writer.store().ledger().unresolved().is_none());
        assert_eq!(writer.store().ledger().violation(), None);
        if state == "authorized" {
            assert!(matches!(
                writer
                    .store()
                    .ledger()
                    .disposition(&piece.unit_key, piece.index),
                PieceDisposition::Authorized { .. }
            ));
        }
        let bindings = Arc::new(ChannelBindingLog::new(CHANNEL, ShadowProvider::Claude));
        let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings);
        let mut owed = VecDeque::new();
        sources
            .resume(
                &mut writer,
                &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
                &mut owed,
            )
            .unwrap();
        assert_eq!(sources.replayed.posted, 1, "{state}");
        assert_eq!(sources.replayed.excluded, 1, "{state}");
        assert!(!sources.replayed.deferred);
        assert_eq!(
            owed,
            [Derived::Piece(piece)],
            "{state}: exact key/index lost"
        );
        eprintln!("restart_replay_retains state={state} removed=2 owed=1 missing=0");
        p5::set_test_root(None);
    }
}

#[tokio::test]
async fn restart_replay_open_defers_the_whole_queue() {
    let (harness, piece) = pending_fixture("open");
    p5::set_test_root(Some(harness._runtime.path()));
    let mut writer = harness.writer();
    let bindings = Arc::new(ChannelBindingLog::new(CHANNEL, ShadowProvider::Claude));
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings);
    let mut owed = VecDeque::new();
    sources
        .resume(
            &mut writer,
            &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
            &mut owed,
        )
        .unwrap();
    assert!(sources.replayed.deferred);
    assert_eq!(
        (
            sources.replayed.posted,
            sources.replayed.excluded,
            owed.len()
        ),
        (0, 0, 3)
    );
    assert!(owed.contains(&Derived::Piece(piece)));
    p5::set_test_root(None);
}

#[tokio::test(start_paused = true)]
async fn restart_replay_host_preserves_exact_uncertain_keys_at_the_resume_boundary() {
    for state in [
        "open",
        "unresolved",
        "authorized",
        "ambiguous",
        "not_found",
        "not_sent",
    ] {
        let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
        let (harness, piece) = pending_fixture(state);
        harness.gate.acquired();
        let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
        replay::capture();
        let mut handles = managed(&harness, &io, &ready);
        assert_eq!(handles.len(), 1);
        polls(3).await;
        handles.pop().unwrap().stop_and_join().await.unwrap();
        let snapshots = replay::snapshots();
        let (summary, kept) = snapshots
            .first()
            .expect("managed host never finished replay");
        assert!(
            kept.contains(&Derived::Piece(piece)),
            "{state}: lost exact key/index"
        );
        if state == "open" {
            assert!(summary.deferred);
            assert_eq!(kept.len(), 3);
        } else {
            assert!(!summary.deferred);
            assert_eq!(summary.posted + summary.excluded, 2);
            assert_eq!(kept.len(), 1);
        }
        p5::set_test_root(None);
    }
}

#[tokio::test]
async fn restart_replay_partial_unit_preserves_fresh_indices_and_blocked_work() {
    let (harness, _, mut first) = fixture(0);
    let mut writer = harness.writer();
    writer
        .store()
        .append_ledger(LedgerEntry::Prepared {
            notice_origin: None,
            serial: 0,
            unit_key: first.unit_key.clone(),
            piece_index: 0,
            payload: first.payload.clone(),
            anchor_id: 100,
            epoch: 1,
            local_retry: None,
        })
        .unwrap();
    writer
        .store()
        .append_ledger(LedgerEntry::Posted {
            serial: 0,
            msg_id: 101,
        })
        .unwrap();
    let mut items = VecDeque::from([Derived::Piece(first.clone())]);
    first.index = 1;
    items.push_back(Derived::Piece(first.clone()));
    first.index = 2;
    items.push_back(Derived::Piece(first.clone()));
    items.push_back(Derived::Blocked {
        reason: "schema changed".into(),
    });
    items.push_back(Derived::Excluded {
        unit_key: unit("exclude-intent"),
        reason: "not recorded".into(),
    });
    let before = items.clone();
    let summary = items.finish_replay(writer.store().ledger());
    assert_eq!((summary.posted, summary.kept), (1, 4));
    assert_eq!(items, before.into_iter().skip(1).collect::<VecDeque<_>>());
}

async fn open_host(marked: bool) {
    let _fixture = STARTUP_FIXTURE.lock().await;
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (harness, _, _) = fixture_records(SETTLED, Some(marked));
    harness.gate.acquired();
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    boot::capture();
    RESTART_POLICY_CALLS.with(|calls| calls.set(Some((CHANNEL, 0))));
    let calls_at_history = Arc::new(AtomicUsize::new(0));
    let history_count = Arc::clone(&calls_at_history);
    *harness.port.on_post.lock().unwrap() = Some(Box::new(move || {
        history_count.store(
            RESTART_POLICY_CALLS.with(|calls| calls.get().unwrap().1),
            Ordering::SeqCst,
        );
    }));
    let mut handles = managed(&harness, &io, &ready);
    assert_eq!(handles.len(), 1);
    polls(12).await;
    handles.pop().unwrap().stop_and_join().await.unwrap();
    let calls = calls_at_history.load(Ordering::SeqCst);
    RESTART_POLICY_CALLS.with(|calls| calls.set(None));
    p5::set_test_root(None);
    assert_eq!(harness.port.posts(), ["new piece"]);
    assert_eq!(harness.port.history_reads.load(Ordering::SeqCst), 1);
    assert!(
        calls > 0 && calls < 32,
        "marked={marked} policy calls={calls}"
    );
    let store = harness.channel();
    assert!(store.ledger().unresolved().is_none());
    assert!(matches!(
        store.ledger().piece(SETTLED as u64).unwrap().outcome,
        Some(PieceOutcome::Posted(_))
    ));
    assert_eq!(store.ledger().next_serial(), SETTLED as u64 + 2);
    let events = boot::events();
    let refilters = events
        .iter()
        .filter(|event| event.stage == "refilter")
        .collect::<Vec<_>>();
    assert_eq!(refilters.len(), 1, "one refilter after existing settlement");
    assert_eq!(refilters[0].details.replay.posted, SETTLED + 1);
    let drain = events
        .iter()
        .find(|event| event.stage == "first_drain" && event.status == "ok")
        .unwrap();
    assert!(
        drain.details.done <= 2,
        "old drain scaled: {}",
        drain.details.done
    );
    eprintln!(
        "open_host marked={marked} calls={calls} drain_steps={} refilter_skip={}",
        drain.details.done, refilters[0].details.replay.posted
    );
}

#[tokio::test(start_paused = true)]
async fn restart_replay_host_refilters_after_unmarked_open_history_settlement() {
    open_host(false).await;
}

#[tokio::test(start_paused = true)]
async fn restart_replay_host_refilters_after_marked_open_history_settlement() {
    open_host(true).await;
}

#[tokio::test(start_paused = true)]
async fn restart_replay_first_poll_and_durable_posted_are_separate_on_append_failure() {
    use crate::services::tui_o::store::fault::{self, Keep, Step as At};
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (harness, _, _) = fixture(0);
    let ledger = harness
        ._runtime
        .path()
        .join("o_store")
        .join(CHANNEL.to_string())
        .join("ledger.jsonl");
    *harness.port.on_construct.lock().unwrap() = Some(Box::new(|| {
        assert!(
            !boot::events()
                .iter()
                .any(|event| event.stage == "first_post"),
            "request construction is not its first poll"
        );
    }));
    let armed = Arc::new(Mutex::new(None));
    let fault_holder = Arc::clone(&armed);
    *harness.port.on_post.lock().unwrap() = Some(Box::new(move || {
        let events = boot::events();
        assert!(events.iter().any(|event| event.stage == "first_post"));
        assert!(!events.iter().any(|event| event.stage == "first_posted"));
        let planted = fault::plant(
            &ledger,
            At::Append(Keep::Nothing),
            std::io::ErrorKind::PermissionDenied,
            Some(1),
        );
        *fault_holder.lock().unwrap() = Some(planted);
    }));
    harness.gate.acquired();
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    boot::capture();
    let mut handles = managed(&harness, &io, &ready);
    assert_eq!(handles.len(), 1);
    polls(3).await;
    handles.pop().unwrap().stop_and_join().await.unwrap();
    p5::set_test_root(None);
    assert_eq!(harness.port.posts(), ["new piece"]);
    assert!(harness.channel().ledger().unresolved().is_some());
    let events = boot::events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.stage == "first_post")
            .count(),
        1
    );
    assert!(!events.iter().any(|event| event.stage == "first_posted"));
}

#[tokio::test]
async fn restart_replay_cooperative_drain_observes_stop_before_a_fresh_backlog_finishes() {
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (harness, _, _) = fixture(128);
    let ledger = harness
        ._runtime
        .path()
        .join("o_store")
        .join(CHANNEL.to_string())
        .join("ledger.jsonl");
    std::fs::write(&ledger, b"").unwrap();
    harness.gate.acquired();
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let (send, receive) = tokio::sync::oneshot::channel();
    let send = Mutex::new(Some(send));
    *harness.port.on_post.lock().unwrap() = Some(Box::new(move || {
        if let Some(send) = send.lock().unwrap().take() {
            send.send(()).unwrap();
        }
    }));
    let mut handles = managed(&harness, &io, &ready);
    assert_eq!(handles.len(), 1);
    let handle = handles.pop().unwrap();
    let stopper = tokio::spawn(async move {
        receive.await.unwrap();
        handle.stop_and_join().await.unwrap();
    });
    tokio::time::timeout(Duration::from_secs(5), stopper)
        .await
        .unwrap()
        .unwrap();
    p5::set_test_root(None);
    let posted = harness.port.posts();
    assert!(!posted.is_empty());
    assert!(
        posted.len() < 129,
        "drain never yielded to its stop: {}",
        posted.len()
    );
    assert_eq!(
        harness.channel().ledger().next_serial(),
        posted.len() as u64
    );
}

#[tokio::test]
async fn restart_replay_deferred_filter_is_once_and_keeps_nonposted_settlement() {
    for outcome in ["unresolved", "ambiguous", "not_found"] {
        let (harness, piece) = pending_fixture("open");
        p5::set_test_root(Some(harness._runtime.path()));
        let mut writer = harness.writer();
        let mut sources = Sources::new(
            CHANNEL,
            ShadowProvider::Claude,
            Arc::new(ChannelBindingLog::new(CHANNEL, ShadowProvider::Claude)),
        );
        let mut items = VecDeque::new();
        sources
            .resume(
                &mut writer,
                &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
                &mut items,
            )
            .unwrap();
        let mut pending = sources.replayed.deferred;
        assert!(pending);
        let entry = match outcome {
            "unresolved" => LedgerEntry::Unresolved {
                serial: 2,
                reason: "uncertain".into(),
            },
            "ambiguous" => LedgerEntry::Ambiguous {
                serial: 2,
                candidates: vec![2000, 2001],
            },
            _ => LedgerEntry::NotFound { serial: 2 },
        };
        writer.store().append_ledger(entry).unwrap();
        assert!(replay::after_open(&mut pending, writer.store().ledger()));
        let summary = items.finish_replay(writer.store().ledger());
        assert_eq!(summary.posted + summary.excluded, 2);
        assert_eq!(items, [Derived::Piece(piece)]);
        assert!(!replay::after_open(&mut pending, writer.store().ledger()));
        p5::set_test_root(None);
    }
}
