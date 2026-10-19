//! A fresh turn's admission boundary excludes older Legacy sends without excluding its own sends.

use super::*;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::channel_policy::Candidate;
use crate::services::tui_o::writer::binding::BindingEvents;

fn candidate(channel: u64) -> Candidate {
    test_override::with_channels(|boot| boot.unwrap().candidate(channel).unwrap().clone())
}

fn binding_seq(channel: u64) -> Result<u64, String> {
    ChannelBindingLog::new(channel, ShadowProvider::Codex)
        .binding_events_since(channel, 0)
        .map(|events| events.last().map_or(0, |event| event.seq))
}

fn mark(channel: u64) -> Candidate {
    let candidate = candidate(channel);
    assert!(candidate.mark_turn_admitted(channel, || binding_seq(channel)));
    candidate
}

/// Runs ready host work without advancing the paused clock through its retry timers.
async fn ready_work() {
    for _ in 0..24 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn r1_fresh_turn_mark_excludes_a_completed_previous_turn_body() {
    const CODEX: u64 = 61_850;
    let Takeover {
        harness,
        io,
        tasks: _tasks,
        _reserved,
    } = hosted_over_old(CODEX).await;
    let previous = candidate(CODEX).claim_body(CODEX);
    assert!(!previous.owned);
    drop(previous);
    let candidate = mark(CODEX);
    assert_eq!(candidate.turn_mark().unwrap().sends, (1, 1));

    let (path, source) = rollout(&harness, "fresh-turn.jsonl", &prompted("s-codex"));
    bind_late(CODEX, &path);
    ready_work().await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    assert_eq!(init_sources(&harness, CODEX), [(source, 0)]);
    assert!(candidate.claim_body(CODEX).send.is_none());
}

#[tokio::test(start_paused = true)]
async fn r2_a_body_started_after_the_turn_mark_still_skips_the_rollout() {
    const CODEX: u64 = 61_851;
    let Takeover {
        harness,
        io: _io,
        tasks: _tasks,
        _reserved,
    } = hosted_over_old(CODEX).await;
    let candidate = mark(CODEX);
    let admitted = candidate.turn_mark().unwrap();
    let same_turn = candidate.claim_body(CODEX);
    assert!(!same_turn.owned);
    drop(same_turn);
    assert_eq!(candidate.turn_mark().unwrap().sends, admitted.sends);

    let (path, _) = rollout(&harness, "same-turn.jsonl", &prompted("s-codex"));
    bind_late(CODEX, &path);
    ready_work().await;
    assert_eq!(adoption(CODEX), Adoption::Deferred);
    assert!(!harness.store.has_channel_dir(CODEX));
    assert!(
        !candidate.claim_body(CODEX).owned,
        "Legacy retains the turn"
    );
}

#[tokio::test(start_paused = true)]
async fn r3_a_previous_turn_body_in_flight_retries_then_commits_after_it_finishes() {
    const CODEX: u64 = 61_852;
    let Takeover {
        harness,
        io,
        tasks: _tasks,
        _reserved,
    } = hosted_over_old(CODEX).await;
    let previous = candidate(CODEX).claim_body(CODEX);
    let candidate = mark(CODEX);
    assert_eq!(candidate.turn_mark().unwrap().sends, (1, 0));
    let (path, source) = rollout(&harness, "in-flight.jsonl", &prompted("s-codex"));
    bind_late(CODEX, &path);
    ready_work().await;
    assert!(
        io.calls().contains(&("fence", CODEX)),
        "the first commit was tried"
    );
    assert_eq!(adoption(CODEX), Adoption::Deferred);
    assert!(
        !harness.store.has_channel_dir(CODEX),
        "the old body must finish first"
    );

    drop(previous);
    tokio::time::advance(Duration::from_millis(250)).await;
    ready_work().await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    assert_eq!(init_sources(&harness, CODEX), [(source, 0)]);
}

#[tokio::test(start_paused = true)]
async fn r4_sends_are_captured_before_the_unlocked_binding_read() {
    const CODEX: u64 = 61_853;
    let Takeover {
        harness,
        io: _io,
        tasks: _tasks,
        _reserved,
    } = hosted_over_old(CODEX).await;
    let candidate = candidate(CODEX);
    let (path, _) = rollout(&harness, "during-mark.jsonl", &prompted("s-codex"));
    assert!(candidate.mark_turn_admitted(CODEX, || {
        assert!(
            !candidate.locked_for_test(),
            "binding IO holds no adoption lock"
        );
        drop(candidate.claim_body(CODEX));
        bind_late(CODEX, &path);
        binding_seq(CODEX)
    }));
    let captured = candidate.turn_mark().unwrap();
    assert_eq!(captured.sends, (0, 0), "the sends read precedes binding IO");
    assert_eq!(captured.seq, binding_seq(CODEX).unwrap());
    let attempted = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&attempted);
    test_hook::set(CODEX, Step::BeforeFence, move || {
        observed.store(true, Ordering::SeqCst);
        Ok(())
    });
    ready_work().await;
    assert!(
        !attempted.load(Ordering::SeqCst),
        "a bind included by the mark is not a target"
    );
    assert_eq!(adoption(CODEX), Adoption::Deferred);
    assert!(!harness.store.has_channel_dir(CODEX));
}

#[tokio::test(start_paused = true)]
async fn r5_a_skip_baseline_supersedes_the_older_turn_mark() {
    const CODEX: u64 = 61_854;
    let Takeover {
        harness,
        io,
        tasks: _tasks,
        _reserved,
    } = hosted_over_old(CODEX).await;
    let candidate = mark(CODEX);
    drop(candidate.claim_body(CODEX));
    let (skipped, _) = rollout(&harness, "skip-first.jsonl", &prompted("s-codex"));
    bind_late(CODEX, &skipped);
    ready_work().await;
    assert_eq!(adoption(CODEX), Adoption::Deferred);
    assert!(!harness.store.has_channel_dir(CODEX));

    let (next, source) = rollout(&harness, "after-skip.jsonl", &prompted("s-codex"));
    bind_late(CODEX, &next);
    ready_work().await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    assert_eq!(init_sources(&harness, CODEX), [(source, 0)]);
}

#[tokio::test(start_paused = true)]
async fn r6_a_reused_codex_session_does_not_mark_or_discard_legacy_history() {
    const CODEX: u64 = 61_855;
    let Takeover {
        harness,
        io: _io,
        tasks: _tasks,
        _reserved,
    } = hosted_over_old(CODEX).await;
    let candidate = candidate(CODEX);
    drop(candidate.claim_body(CODEX));
    for session in ["s-codex", "", " "] {
        cutover::mark_codex_turn_admitted(CODEX, &ProviderKind::Codex, Some(session));
        assert!(
            candidate.turn_mark().is_none(),
            "resumed session {session:?}"
        );
    }
    let (path, _) = rollout(&harness, "reuse.jsonl", &prompted("s-codex"));
    bind_late(CODEX, &path);
    ready_work().await;
    assert_eq!(adoption(CODEX), Adoption::Deferred);
    assert!(!harness.store.has_channel_dir(CODEX));
    assert!(!candidate.claim_body(CODEX).owned);
}

#[tokio::test(start_paused = true)]
async fn an_eligible_mark_retries_a_transient_commit_failure_before_the_five_second_wake() {
    const CODEX: u64 = 61_856;
    let Takeover {
        harness,
        io,
        tasks: _tasks,
        _reserved,
    } = hosted_over_old(CODEX).await;
    mark(CODEX);
    let tried = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&tried);
    test_hook::set(CODEX, Step::BeforeFence, move || {
        observed.store(true, Ordering::SeqCst);
        Err("one transient intake fence failure".into())
    });
    let (path, source) = rollout(&harness, "retry.jsonl", &prompted("s-codex"));
    bind_late(CODEX, &path);
    ready_work().await;
    assert!(tried.load(Ordering::SeqCst));
    assert_eq!(adoption(CODEX), Adoption::Deferred);
    assert!(!harness.store.has_channel_dir(CODEX));
    let before_retry = tokio::time::Instant::now();
    tokio::time::advance(Duration::from_millis(249)).await;
    ready_work().await;
    assert_eq!(adoption(CODEX), Adoption::Deferred);
    tokio::time::advance(Duration::from_millis(1)).await;
    ready_work().await;
    assert_eq!(
        adoption(CODEX),
        Adoption::Committed,
        "{:?}",
        io.alarms.halted()
    );
    assert_eq!(
        tokio::time::Instant::now() - before_retry,
        Duration::from_millis(250)
    );
    assert_eq!(init_sources(&harness, CODEX), [(source, 0)]);
}

#[tokio::test(start_paused = true)]
async fn a_failed_later_admission_read_restores_the_host_baseline_after_a_retry() {
    const CODEX: u64 = 61_860;
    let Takeover {
        harness,
        io: _io,
        tasks: _tasks,
        _reserved,
    } = hosted_over_old(CODEX).await;
    drop(candidate(CODEX).claim_body(CODEX));
    let candidate = mark(CODEX);
    assert_eq!(candidate.turn_mark().unwrap().sends, (1, 1));
    let tried = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&tried);
    test_hook::set(CODEX, Step::BeforeFence, move || {
        observed.store(true, Ordering::SeqCst);
        Err("one transient commit failure before the later admission".into())
    });
    let (path, _) = rollout(&harness, "failed-later-mark.jsonl", &prompted("s-codex"));
    bind_late(CODEX, &path);
    ready_work().await;
    assert!(tried.load(Ordering::SeqCst));
    assert_eq!(adoption(CODEX), Adoption::Deferred);
    assert!(!candidate.mark_turn_admitted(CODEX, || Err("later binding read failed".into())));
    assert!(candidate.turn_mark().is_none());

    tokio::time::advance(Duration::from_millis(250)).await;
    ready_work().await;
    assert_eq!(adoption(CODEX), Adoption::Deferred);
    assert!(
        !harness.store.has_channel_dir(CODEX),
        "a discarded mark cannot remain cached as the host baseline"
    );
    assert!(
        !candidate.claim_body(CODEX).owned,
        "Legacy retains the rollout"
    );
}

#[test]
fn only_a_reserved_deferred_candidate_marks_and_failed_reads_clear_the_mark() {
    const CODEX: u64 = 61_857;
    for state in [
        Adoption::Pending,
        Adoption::Deferred,
        Adoption::Committed,
        Adoption::Held,
        Adoption::Released,
    ] {
        let ordinary = Candidate::new(state);
        assert!(
            !ordinary
                .mark_turn_admitted(CODEX, || panic!("ordinary candidates read no binding log"))
        );
        assert!(ordinary.turn_mark().is_none());
        if state != Adoption::Deferred {
            let decided = Candidate::reserved_late();
            *decided.lock() = state;
            assert!(!decided.mark_turn_admitted(CODEX, || panic!(
                "non-Deferred candidates read no binding log"
            )));
            assert!(decided.turn_mark().is_none());
        }
    }
    let candidate = Candidate::reserved_late();
    assert!(candidate.mark_turn_admitted(CODEX, || Ok(4)));
    assert!(candidate.turn_mark().is_some());
    assert!(!candidate.mark_turn_admitted(CODEX, || Err("binding read failed".into())));
    assert!(
        candidate.turn_mark().is_none(),
        "an earlier mark cannot survive a failed fresh read"
    );
    assert!(!candidate.mark_turn_admitted(CODEX, || {
        *candidate.lock() = Adoption::Released;
        Ok(5)
    }));
    assert!(
        candidate.turn_mark().is_none(),
        "state is checked again before publishing"
    );
}

#[test]
fn a_later_admission_wins_when_binding_reads_publish_in_reverse_order() {
    const CODEX: u64 = 61_858;
    let candidate = Candidate::reserved_late();
    let mut later = None;
    candidate.mark_turn_admitted(CODEX, || {
        assert!(!candidate.locked_for_test());
        assert!(candidate.mark_turn_admitted(CODEX, || Ok(20)));
        later = candidate.turn_mark();
        Ok(10)
    });
    let mark = candidate.turn_mark().unwrap();
    assert_eq!(
        mark.seq, 20,
        "the older binding read cannot overwrite the later admission"
    );
    assert_eq!(mark.order, later.unwrap().order);
    assert!(!candidate.mark_turn_admitted(CODEX, || {
        assert!(candidate.mark_turn_admitted(CODEX, || Ok(30)));
        Err("an older read failed after a later admission".into())
    }));
    assert_eq!(candidate.turn_mark().unwrap().seq, 30);
}

struct ClaimLog(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for ClaimLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn body_claim_logs_distinguish_previous_and_current_turns_without_body_content() {
    const CODEX: u64 = 61_859;
    const PRIVATE_BODY: &str = "private transport body that must not enter the claim log";
    let _reserved = reserve(CODEX);
    let candidate = candidate(CODEX);
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let writer = Arc::clone(&recorded);
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || ClaimLog(Arc::clone(&writer)))
        .finish();
    let _tracing = tracing::subscriber::set_default(subscriber);

    let previous = cutover::claim_then_send(
        Some(cutover::BodyClaim::new(CODEX, Some(CodexTui))),
        || async { Ok::<_, String>(PRIVATE_BODY) },
    )
    .await
    .unwrap();
    assert_eq!(previous, cutover::BodySend::Sent(Ok(PRIVATE_BODY)));
    assert!(candidate.mark_turn_admitted(CODEX, || Ok(3)));
    let current = cutover::claim_then_send(
        Some(cutover::BodyClaim::new(CODEX, Some(CodexTui))),
        || async { Err::<(), _>(PRIVATE_BODY) },
    )
    .await
    .unwrap();
    assert_eq!(current, cutover::BodySend::Sent(Err(PRIVATE_BODY)));
    let status = cutover::claim_then_send(None, || async { "status only" })
        .await
        .unwrap();
    assert_eq!(status, cutover::BodySend::Sent("status only"));
    *candidate.lock() = Adoption::Committed;
    let owned: cutover::BodySend<()> = cutover::claim_then_send(
        Some(cutover::BodyClaim::new(CODEX, Some(CodexTui))),
        || async { panic!("O owns the body") },
    )
    .await
    .unwrap();
    assert_eq!(owned, cutover::BodySend::OwnedByO);

    let captured = String::from_utf8(recorded.lock().unwrap().clone()).unwrap();
    let claims: Vec<_> = captured
        .lines()
        .filter(|line| line.contains("[tui_o] body claim"))
        .collect();
    assert_eq!(
        claims.len(),
        3,
        "status-only transport adds no body claim: {captured}"
    );
    for line in &claims {
        assert!(line.contains("channel=61859"), "{line}");
        assert!(line.contains(&format!("origin={}:", file!())), "{line}");
    }
    assert!(
        claims[0].contains("owned=false")
            && claims[0].contains("started=1 finished=0")
            && claims[0].contains("mark_present=false"),
        "{}",
        claims[0]
    );
    assert!(
        claims[1].contains("owned=false")
            && claims[1].contains("started=2 finished=1")
            && claims[1].contains("mark_present=true"),
        "{}",
        claims[1]
    );
    assert!(
        claims[2].contains("owned=true")
            && claims[2].contains("started=2 finished=2")
            && claims[2].contains("mark_present=true"),
        "{}",
        claims[2]
    );
    assert!(
        !captured.contains(PRIVATE_BODY),
        "body bytes never enter the structural log"
    );
}
