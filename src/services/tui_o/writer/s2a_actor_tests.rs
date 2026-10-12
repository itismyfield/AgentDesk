use super::*;
use crate::services::provider::CancelToken;
use crate::services::tui_o::store::codex_provenance::submit_floor::{
    SubmitFloor, tests as fixture,
};
use crate::services::tui_o::store::fault::{self, Step};
use crate::services::tui_o::writer::actor::codex_submission::Operation;
use crate::services::tui_o::writer::host::{ManagedWriterHandle, start_managed};
use crate::services::tui_prompt_dedupe::codex_verified::provenance::admission;

#[tokio::test(start_paused = true)]
async fn s2a_same_claim_restart_rejects_delayed_close_reply_before_issue_and_attach() {
    let _selected = test_override::force_candidates(&[(CHANNEL, CodexTui)]);
    let mut source = None;
    let harness = Harness::build_channel(CHANNEL, |root| {
        let native = fixture::native_file(root);
        let bytes = std::fs::read(&native.path).unwrap();
        source = Some(native.clone());
        vec![InitSource {
            source_id: native,
            delivery_start: bytes.len() as u64,
            prefix_hash: hex::encode(Sha256::digest(&bytes)),
        }]
    });
    let source = source.unwrap();
    p5_log(
        harness._runtime.path(),
        CHANNEL,
        &p5_event(CHANNEL, "codex", p5::BindingTarget::Source(source.clone())),
    );
    harness.gate.acquired();
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    p5::set_test_root(Some(harness._runtime.path()));
    let mut handles = start_managed(
        ShadowProvider::Codex,
        true,
        cutover::boot_ownership(),
        || HostParts {
            io: Arc::clone(&io),
            runtime_root: Some(root(&harness)),
            gate: Arc::clone(&harness.gate),
            readiness: Arc::clone(&ready),
        },
    );
    assert_eq!(handles.len(), 1);
    polls(3).await;
    assert!(ready.accepts(CHANNEL), "{:?}", io.alarms.halted());
    let a = ready
        .submission(CHANNEL)
        .expect("actual Hosted::run installed A");
    let channel = harness.channel();
    let floor = fixture::native_floor(
        fixture::launch(CHANNEL, &source),
        channel.cursor(&source).unwrap(),
    );
    a.request(Operation::Prepare(SubmitFloor::NativeFloor(Box::new(
        floor.clone(),
    ))))
    .await
    .unwrap();
    let span = fixture::terminal(&floor);
    // Keep A's delivered reply unconsumed across the recovery of the same hosting claim.
    let delayed = a
        .request(Operation::Close(Box::new(span.clone())))
        .await
        .unwrap();
    let generation = handles[0].generation();
    let mut next_launch = fixture::launch(CHANNEL, &source);
    next_launch.episode.user_message_id += 1;
    next_launch.episode.turn_nonce = "next-turn".into();
    let next = fixture::native_floor(next_launch, channel.cursor(&source).unwrap());
    let failed = fault::plant(
        root(&harness).as_path(),
        Step::Write,
        std::io::ErrorKind::StorageFull,
        Some(1),
    );
    assert!(
        a.request(Operation::Prepare(SubmitFloor::NativeFloor(Box::new(next))))
            .await
            .is_err()
    );
    drop(failed);
    let mut b = None;
    for _ in 0..60 {
        polls(1).await;
        if let Some(port) = ready.submission(CHANNEL)
            && !port.same_run(&a)
            && ready.accepts(CHANNEL)
        {
            b = Some(port);
            break;
        }
    }
    let b = b.unwrap_or_else(|| panic!("A/B recovery: {:?}", io.alarms.0.lock().unwrap()));
    assert_eq!(
        handles[0].generation(),
        generation,
        "the hosting claim stayed the same"
    );
    let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        span.episode.turn_nonce.clone(),
    )));
    let issued = AtomicUsize::new(0);
    let result = ready.with_closed_reply(CHANNEL, delayed, |closed| {
        issued.fetch_add(1, Ordering::SeqCst);
        admission::issue_for_tests(
            closed.span().execution.clone(),
            closed.span().episode.clone(),
            &token,
        )
    });
    assert!(
        result.is_err(),
        "a same-generation stale reply must not reach issuance"
    );
    assert_eq!(
        issued.load(Ordering::SeqCst),
        0,
        "issuance and its token attach were never called"
    );
    let current = b
        .request(Operation::Close(Box::new(span.clone())))
        .await
        .unwrap();
    let record = ready
        .with_closed_reply(CHANNEL, current, |closed| {
            issued.fetch_add(1, Ordering::SeqCst);
            admission::issue_for_tests(
                closed.span().execution.clone(),
                closed.span().episode.clone(),
                &token,
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        issued.load(Ordering::SeqCst),
        1,
        "a current reply really reaches the existing issuer"
    );
    assert_eq!(Arc::strong_count(&record), 3);
    assert!(harness.port.posts().is_empty());
    admission::forget_channel_for_tests(CHANNEL);
    handles.pop().unwrap().stop_and_join().await.unwrap();
}

async fn s2b1a_host() -> (
    Harness,
    Arc<TestIo>,
    Arc<Readiness>,
    Vec<ManagedWriterHandle>,
    crate::services::tui_o::shadow::SourceId,
) {
    let mut source = None;
    let harness = Harness::build_channel(CHANNEL, |root| {
        let native = fixture::native_file(root);
        let bytes = std::fs::read(&native.path).unwrap();
        source = Some(native.clone());
        vec![InitSource {
            source_id: native,
            delivery_start: bytes.len() as u64,
            prefix_hash: hex::encode(Sha256::digest(&bytes)),
        }]
    });
    let source = source.unwrap();
    p5_log(
        harness._runtime.path(),
        CHANNEL,
        &p5_event(CHANNEL, "codex", p5::BindingTarget::Source(source.clone())),
    );
    harness.gate.acquired();
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    p5::set_test_root(Some(harness._runtime.path()));
    let handles = start_managed(
        ShadowProvider::Codex,
        true,
        cutover::boot_ownership(),
        || HostParts {
            io: Arc::clone(&io),
            runtime_root: Some(root(&harness)),
            gate: Arc::clone(&harness.gate),
            readiness: Arc::clone(&ready),
        },
    );
    assert_eq!(handles.len(), 1);
    polls(3).await;
    assert!(ready.accepts(CHANNEL));
    (harness, io, ready, handles, source)
}

#[tokio::test(start_paused = true)]
async fn s2b1a_same_claim_restart_rejects_delayed_typed_prepare_before_submit() {
    let _selected = test_override::force_candidates(&[(CHANNEL, CodexTui)]);
    let (harness, io, ready, mut handles, source) = s2b1a_host().await;
    let a = ready.submission(CHANNEL).unwrap();
    let launch = fixture::launch(CHANNEL, &source);
    let floor = fixture::native_floor(launch.clone(), harness.channel().cursor(&source).unwrap());
    let delayed = a
        .request(Operation::Capture(Box::new((
            launch.clone(),
            floor.execution().clone(),
        ))))
        .await
        .unwrap();
    let generation = handles[0].generation();
    let failed = fault::plant(
        root(&harness).as_path(),
        Step::Write,
        std::io::ErrorKind::StorageFull,
        Some(1),
    );
    let mut next_launch = launch.clone();
    next_launch.episode.user_message_id += 1;
    next_launch.episode.turn_nonce = "successor-turn".into();
    assert!(
        a.request(Operation::Capture(Box::new((
            next_launch,
            floor.execution().clone()
        ))))
        .await
        .is_err()
    );
    drop(failed);
    let mut b = None;
    for _ in 0..60 {
        polls(1).await;
        if let Some(port) = ready.submission(CHANNEL)
            && !port.same_run(&a)
            && ready.accepts(CHANNEL)
        {
            b = Some(port);
            break;
        }
    }
    let b = b.unwrap_or_else(|| panic!("recovery: {:?}", io.alarms.0.lock().unwrap()));
    assert_eq!(handles[0].generation(), generation);
    let submits = AtomicUsize::new(0);
    assert!(
        ready
            .with_prepared_reply(CHANNEL, delayed, |_| {
                submits.fetch_add(1, Ordering::SeqCst);
            })
            .is_err(),
        "same-generation A prepare cannot authorize B input"
    );
    assert_eq!(submits.load(Ordering::SeqCst), 0);
    let current = b
        .request(Operation::Capture(Box::new((
            launch,
            floor.execution().clone(),
        ))))
        .await
        .unwrap();
    ready
        .with_prepared_reply(CHANNEL, current, |prepared| {
            assert_eq!(prepared.floor().execution(), floor.execution());
            assert_eq!(prepared.floor().checkpoint(), floor.checkpoint());
            submits.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
    assert_eq!(submits.load(Ordering::SeqCst), 1);
    assert!(harness.port.posts().is_empty());
    handles.pop().unwrap().stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn s2b1a_dropping_command_sender_does_not_stop_hosted_actor() {
    let _selected = test_override::force_candidates(&[(CHANNEL, CodexTui)]);
    let (harness, _io, ready, mut handles, _source) = s2b1a_host().await;
    ready.drop_submission_for_tests(CHANNEL);
    polls(3).await;
    assert!(
        ready.accepts(CHANNEL),
        "closed command input must fall back to the existing poll loop"
    );
    assert!(harness.port.posts().is_empty());
    handles.pop().unwrap().stop_and_join().await.unwrap();
}
