use super::*;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Case {
    Gc,
    CurrentWaiting,
    CarriedWaiting,
}

fn old_segments(harness: &Harness, source: &SourceId) -> Vec<(String, Vec<u8>)> {
    let key = crate::services::tui_o::store::spool::source_key(source);
    let path = harness
        ._runtime
        .path()
        .join("o_store")
        .join(CANARY_CHANNEL.to_string())
        .join("spool");
    let mut files: Vec<_> = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&key)
        })
        .map(|path| {
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read(path).unwrap(),
            )
        })
        .collect();
    files.sort();
    files
}

async fn wake(stop: &watch::Sender<bool>, elapsed: Duration) {
    tokio::time::advance(elapsed).await;
    stop.send_replace(false);
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}

async fn scenario(case: Case) {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe_lock = DedupeGuard::acquire();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _scope = ScopeFixture::new();
    let _fake_tmux =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    let (_, old) = proven(root.path());
    assert!(dedupe::clear_relaunch::fresh_scope_eligible(CANARY_CHANNEL));
    let harness = Harness::build_channel(CANARY_CHANNEL, |_| {
        vec![InitSource {
            source_id: old.clone(),
            delivery_start: 0,
            prefix_hash: hex::encode(Sha256::digest(b"")),
        }]
    });
    let mut store = harness.channel();
    let mut capture = SourceCapture::open(old.clone(), 0).unwrap();
    let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
        panic!("setup must capture old metadata");
    };
    store.append_spool(&batch, &capture.prefix_hash()).unwrap();
    let checkpoint = dedupe::binding_events::binding_events_since(CANARY_CHANNEL, 0)
        .unwrap()
        .last()
        .unwrap()
        .seq;
    store.set_binding_checkpoint(checkpoint).unwrap();
    let old_cursor = store.cursor(&old).unwrap().clone();
    let old_rotation = store.rotation().unwrap();
    let segments = old_segments(&harness, &old);
    assert!(!segments.is_empty(), "old retained spool must be real");
    if case == Case::Gc {
        store.set_segment_max_for_tests(1);
    }
    let old_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(&old.path);
    let (_, current) = proven(root.path());
    let writer = ChannelWriter::new(
        store,
        harness.gate.clone(),
        harness.port.clone(),
        harness.lease.clone(),
        harness.alarms.clone(),
    );
    let selection = writer.suspension.clone();
    if case != Case::CurrentWaiting {
        harness.gate.acquired();
    }
    let (stop, stopped) = watch::channel(false);
    let (resumed, ready) = watch::channel(false);
    let task = tokio::spawn(run_channel(
        writer,
        ShadowProvider::Codex,
        Arc::new(ChannelBindingLog::new(
            CANARY_CHANNEL,
            ShadowProvider::Codex,
        )),
        stopped,
        resumed,
    ));
    wake(&stop, Duration::ZERO).await;
    polls(4).await;
    assert!(
        *ready.borrow(),
        "the actual actor must complete filtered resume"
    );
    assert!(selection.contains(&old), "the old source must be suspended");
    assert_eq!(
        harness.alarms.cleared.load(Ordering::SeqCst),
        0,
        "empty scoped restoration must not clear a carried waiting incident"
    );
    match case {
        Case::CarriedWaiting => {
            polls(4).await;
            assert_eq!(harness.alarms.cleared.load(Ordering::SeqCst), 0);
        }
        Case::Gc => {
            answer(&current, "gc first");
            answer(&current, "gc second");
            polls(10).await;
            assert_eq!(harness.port.posts(), ["gc first", "gc second"]);
            let current_store = harness.channel();
            assert!(
                !current_store.ledger().gc_segments(&current).is_empty(),
                "scoped current source must record actual GC despite suspended old source"
            );
            assert_eq!(current_store.retained_segments(&current), 1);
        }
        Case::CurrentWaiting => {
            answer(&current, "current waited");
            polls(5).await;
            assert!(
                harness.port.posts().is_empty(),
                "unowned gateway must retain the current piece"
            );
            assert!(crate::services::tui_o::writer::actor::work_for_tests().0 > 0);
            wake(&stop, Duration::from_millis(300_001)).await;
            assert!(
                harness.alarms.raised.lock().unwrap().iter().any(
                    |alarm| matches!(alarm, WriterAlarm::WaitingTooLong { ready, .. } if *ready > 0)
                ),
                "the actual current owed piece must raise its own waiting incident"
            );
            assert_eq!(harness.alarms.cleared.load(Ordering::SeqCst), 0);
            harness.gate.acquired();
            wake(&stop, Duration::ZERO).await;
            polls(4).await;
            assert_eq!(harness.port.posts(), ["current waited"]);
            assert_eq!(
                harness.alarms.cleared.load(Ordering::SeqCst),
                1,
                "posting current work must clear the actor's own incident while S remains nonempty"
            );
            assert!(selection.contains(&old));
        }
    }
    let final_store = harness.channel();
    assert_eq!(final_store.cursor(&old), Some(&old_cursor));
    assert_eq!(
        final_store.rotation().unwrap().link(&old),
        old_rotation.link(&old)
    );
    assert_eq!(
        old_segments(&harness, &old),
        segments,
        "old retained segments must not be GC'd"
    );
    assert_eq!(old_io.stats().read_bytes, 0);
    stop.send(true).unwrap();
    assert_eq!(task.await.unwrap(), None);
    dedupe::binding_events::forget_channel_for_tests(CANARY_CHANNEL);
    dedupe::binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
}

#[tokio::test(start_paused = true)]
async fn scoped_current_source_gc_runs_without_collecting_old_segments() {
    if crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::scoped_current_source_gc_runs_without_collecting_old_segments"
    )) {
        scenario(Case::Gc).await;
    }
}

#[tokio::test(start_paused = true)]
async fn current_waiting_incident_clears_after_post_with_suspended_old_source() {
    if crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::current_waiting_incident_clears_after_post_with_suspended_old_source"
    )) {
        scenario(Case::CurrentWaiting).await;
    }
}

#[tokio::test(start_paused = true)]
async fn scoped_empty_restore_keeps_carried_waiting_clear_unpublished() {
    if crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::scoped_empty_restore_keeps_carried_waiting_clear_unpublished"
    )) {
        scenario(Case::CarriedWaiting).await;
    }
}
