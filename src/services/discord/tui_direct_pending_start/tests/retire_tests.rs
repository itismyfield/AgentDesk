//! A pending-start record whose turn already finished must never construct a
//! new episode: neither its waiting worker nor a restart restore may claim it.
use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

const CHANNEL: u64 = 1_479_671_298_497_183_835;
const TMUX: &str = "AgentDesk-claude-adk-cc";

struct Rig {
    _worker: std::sync::MutexGuard<'static, ()>,
    _env_lock: std::sync::MutexGuard<'static, ()>,
    _env: EnvReset,
    _temp: tempfile::TempDir,
}

impl Rig {
    fn new(idle: bool) -> Self {
        let worker = worker_test_lock();
        let env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        reset_present_for_tests();
        POST_ABORT_PROMOTE_CALLS.store(0, Ordering::SeqCst);
        *super::super::restore_gate::PROVIDER_IDLE_PROBE_FOR_TESTS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = (idle, 0);
        Self {
            _worker: worker,
            _env_lock: env_lock,
            _env: env,
            _temp: temp,
        }
    }

    fn probe_calls(&self) -> u32 {
        super::super::restore_gate::PROVIDER_IDLE_PROBE_FOR_TESTS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .1
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        reset_present_for_tests();
        POST_ABORT_PROMOTE_CALLS.store(0, Ordering::SeqCst);
        *super::super::restore_gate::PROVIDER_IDLE_PROBE_FOR_TESTS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = (false, 0);
    }
}

fn shared_at_generation(generation: u64) -> Arc<SharedData> {
    let mut shared = super::super::super::make_shared_data_for_tests();
    Arc::get_mut(&mut shared)
        .expect("fresh shared state")
        .restart
        .current_generation = generation;
    shared
}

fn adk_record(anchor: u64, generation: u64) -> TuiDirectPendingStart {
    TuiDirectPendingStart {
        tmux_session_name: TMUX.to_string(),
        generation,
        attempt_count: PENDING_START_MAX_CLAIM_ATTEMPTS,
        ..record("claude", CHANNEL, anchor)
    }
}

fn recording_claim(claimed: bool) -> (ClaimFn, Arc<Mutex<Vec<u64>>>) {
    let claims = Arc::new(Mutex::new(Vec::new()));
    let claims_for_fn = claims.clone();
    let claim: ClaimFn = Box::new(move |_shared, record| {
        let claims = claims_for_fn.clone();
        let anchor = record.anchor_message_id;
        Box::pin(async move {
            claims.lock().unwrap().push(anchor);
            claimed
        })
    });
    (claim, claims)
}

fn finalized_view() -> ViewFn {
    Box::new(|_shared, _record| Box::pin(async move { Some(obs(base_view())) }))
}

/// Mailbox held by another turn with no inflight row: the live 20:53 shape.
fn mailbox_blocked_view() -> ViewFn {
    Box::new(|_shared, _record| {
        Box::pin(async move {
            Some(obs(PriorTurnView {
                mailbox_blocking_turn_present: true,
                ..base_view()
            }))
        })
    })
}

fn durable_anchors() -> Vec<u64> {
    records_for_channel("claude", CHANNEL)
        .into_iter()
        .map(|record| record.anchor_message_id)
        .collect()
}

/// Restart with the three finished 2026-09-26 records while the provider is
/// idle: no claim, no ABORT, and the next real turn is the only claimant.
#[tokio::test(start_paused = true)]
async fn restored_finished_records_retire_without_claim_or_abort() {
    let rig = Rig::new(true);
    let shared = shared_at_generation(140);
    let anchors = [
        1_553_362_498_563_084_318u64,
        1_553_366_388_964_466_719,
        1_553_373_830_309_744_762,
    ];
    for anchor in anchors {
        persist(&adk_record(anchor, 139)).unwrap();
    }
    let (claim, claims) = recording_claim(true);
    let claim = Arc::new(claim);
    let (abort_cleanup, abort_calls, _) = recording_abort_cleanup();
    let abort_cleanup = Arc::new(abort_cleanup);
    for record in load_all() {
        let claim = claim.clone();
        let abort_cleanup = abort_cleanup.clone();
        run_worker(
            shared.clone(),
            record,
            finalized_view(),
            Box::new(move |shared, record| claim(shared, record)),
            Box::new(move |shared, record, foreign| abort_cleanup(shared, record, foreign)),
            never_reclaim_orphan(),
        )
        .await;
    }

    assert!(
        claims.lock().unwrap().is_empty(),
        "a finished turn must not construct an episode"
    );
    assert_eq!(abort_calls.load(Ordering::SeqCst), 0, "no ABORT cascade");
    assert_eq!(POST_ABORT_PROMOTE_CALLS.load(Ordering::SeqCst), 0);
    assert!(
        durable_anchors().is_empty(),
        "retired through the runtime path"
    );
    assert!(!pending_synthetic_start_present("claude", CHANNEL));
    assert_eq!(
        rig.probe_calls(),
        3,
        "one idle proof per record claim instant"
    );

    let next_turn = 1_553_380_238_036_312_147u64;
    persist(&adk_record(next_turn, 140)).unwrap();
    run_worker(
        shared.clone(),
        adk_record(next_turn, 140),
        finalized_view(),
        Box::new(move |shared, record| claim(shared, record)),
        Box::new(move |shared, record, foreign| abort_cleanup(shared, record, foreign)),
        never_reclaim_orphan(),
    )
    .await;
    assert_eq!(
        *claims.lock().unwrap(),
        vec![next_turn],
        "the next turn is the only claimant"
    );
    assert_eq!(abort_calls.load(Ordering::SeqCst), 0);
}

/// A restored record queued behind a live pre-restart turn is probed at each claim
/// instant, never while waiting; with the provider busy running it, it claims.
#[tokio::test(start_paused = true)]
async fn restored_record_behind_live_prior_is_probed_once_at_claim_and_claims() {
    let rig = Rig::new(false);
    let shared = shared_at_generation(140);
    let anchor = 1_553_373_830_309_744_762u64;
    persist(&adk_record(anchor, 139)).unwrap();
    let polls = Arc::new(AtomicU32::new(0));
    let probed_while_blocked = Arc::new(AtomicU32::new(0));
    let view: ViewFn = {
        let polls = polls.clone();
        let probed_while_blocked = probed_while_blocked.clone();
        Box::new(move |_shared, _record| {
            let blocked = polls.fetch_add(1, Ordering::SeqCst) < 20;
            if blocked
                && super::super::restore_gate::PROVIDER_IDLE_PROBE_FOR_TESTS
                    .lock()
                    .unwrap()
                    .1
                    > 0
            {
                probed_while_blocked.fetch_add(1, Ordering::SeqCst);
            }
            Box::pin(async move {
                Some(obs(PriorTurnView {
                    inflight_present: blocked,
                    mailbox_blocking_turn_present: blocked,
                    ..base_view()
                }))
            })
        })
    };
    let (claim, claims) = recording_claim(true);
    let (abort_cleanup, abort_calls, _) = recording_abort_cleanup();
    run_worker(
        shared,
        adk_record(anchor, 139),
        view,
        claim,
        abort_cleanup,
        never_reclaim_orphan(),
    )
    .await;

    assert!(polls.load(Ordering::SeqCst) > 20);
    assert_eq!(probed_while_blocked.load(Ordering::SeqCst), 0);
    assert_eq!(
        rig.probe_calls(),
        1,
        "evaluated at each claim instant, not while waiting"
    );
    assert_eq!(
        *claims.lock().unwrap(),
        vec![anchor],
        "legitimate retry kept"
    );
    assert_eq!(abort_calls.load(Ordering::SeqCst), 0);
    assert!(durable_anchors().is_empty(), "deleted after the claim");
}

/// The idle proof only refuses records from a previous process; a record of
/// this process keeps its claim even when the provider looks idle.
#[tokio::test(start_paused = true)]
async fn same_generation_record_claims_even_when_provider_idle() {
    let rig = Rig::new(true);
    let shared = shared_at_generation(140);
    let anchor = 1_553_380_394_479_517_887u64;
    persist(&adk_record(anchor, 140)).unwrap();
    let (claim, claims) = recording_claim(true);
    let (abort_cleanup, _, _) = recording_abort_cleanup();
    run_worker(
        shared,
        adk_record(anchor, 140),
        finalized_view(),
        claim,
        abort_cleanup,
        never_reclaim_orphan(),
    )
    .await;

    assert_eq!(*claims.lock().unwrap(), vec![anchor]);
    assert_eq!(rig.probe_calls(), 0);
}

/// Retiring one anchor stops its waiting worker before any claim, keeps its
/// file deleted, and leaves a sibling record's gate on the same channel.
#[tokio::test(start_paused = true)]
async fn retired_waiting_worker_exits_without_claim_and_keeps_sibling_gate() {
    let _rig = Rig::new(false);
    let shared = shared_at_generation(140);
    let finished = adk_record(1_553_373_830_309_744_762, 140);
    let sibling = adk_record(1_553_381_382_921_781_299, 140);
    persist(&finished).unwrap();
    persist(&sibling).unwrap();
    let (claim, claims) = recording_claim(true);
    let (abort_cleanup, abort_calls, _) = recording_abort_cleanup();
    let worker = tokio::spawn(run_worker(
        shared,
        finished.clone(),
        mailbox_blocked_view(),
        claim,
        abort_cleanup,
        never_reclaim_orphan(),
    ));
    tokio::time::advance(PENDING_START_POLL * 3).await;
    tokio::task::yield_now().await;

    assert!(retire_completed(finished.key(), TMUX));
    for _ in 0..(PENDING_START_MAX_BACKSTOP_CYCLES + 1) {
        tokio::time::advance(PENDING_START_BACKSTOP + PENDING_START_POLL * 2).await;
        tokio::task::yield_now().await;
    }
    worker.await.unwrap();

    assert!(claims.lock().unwrap().is_empty());
    assert_eq!(abort_calls.load(Ordering::SeqCst), 0);
    assert_eq!(durable_anchors(), vec![sibling.anchor_message_id]);
    assert!(
        pending_synthetic_start_present("claude", CHANNEL),
        "the sibling's gate survives the retire"
    );
}

/// A retire that lands while the last backstop cycle awaits the orphan reclaim
/// must not leave an abort marker on the already completed anchor.
#[tokio::test(start_paused = true)]
async fn retire_during_final_orphan_reclaim_skips_the_abort() {
    let _rig = Rig::new(false);
    let shared = shared_at_generation(140);
    let finished = adk_record(1_553_373_830_309_744_762, 140);
    persist(&finished).unwrap();
    let view: ViewFn = Box::new(|_shared, _record| {
        Box::pin(async move {
            Some(PriorTurnObservation {
                view: PriorTurnView {
                    inflight_present: true,
                    mailbox_blocking_turn_present: true,
                    ..base_view()
                },
                foreign_inflight_identity: Some((1_552_939_872_535_318_579, "old".to_string())),
            })
        })
    });
    let reclaims = Arc::new(AtomicU32::new(0));
    let reclaim: ReclaimOrphanFn = {
        let reclaims = reclaims.clone();
        Box::new(move |_shared, record| {
            let call = reclaims.fetch_add(1, Ordering::SeqCst) + 1;
            if call == PENDING_START_MAX_BACKSTOP_CYCLES {
                assert!(retire_completed(record.key(), TMUX));
            }
            Box::pin(async move { ReclaimStaleForeignOutcome::None })
        })
    };
    let (claim, claims) = recording_claim(true);
    let (abort_cleanup, abort_calls, _) = recording_abort_cleanup();
    let worker = tokio::spawn(run_worker(
        shared,
        finished,
        view,
        claim,
        abort_cleanup,
        reclaim,
    ));
    for _ in 0..(PENDING_START_MAX_BACKSTOP_CYCLES + 1) {
        tokio::time::advance(PENDING_START_BACKSTOP + PENDING_START_POLL * 2).await;
        tokio::task::yield_now().await;
    }
    worker.await.unwrap();

    assert_eq!(
        reclaims.load(Ordering::SeqCst),
        PENDING_START_MAX_BACKSTOP_CYCLES
    );
    assert!(claims.lock().unwrap().is_empty());
    assert_eq!(
        abort_calls.load(Ordering::SeqCst),
        0,
        "no abort marker on a ✅ anchor"
    );
    assert_eq!(POST_ABORT_PROMOTE_CALLS.load(Ordering::SeqCst), 0);
    assert!(durable_anchors().is_empty());
}

/// The visible row-absent completion the watcher runs for an anchor retires
/// that anchor's record, and its live worker never claims afterwards.
#[tokio::test(start_paused = true)]
async fn visibly_completed_anchor_retires_record_and_stops_its_worker() {
    let _rig = Rig::new(false);
    let shared = shared_at_generation(140);
    let finished = adk_record(1_553_373_830_309_744_762, 140);
    let captured = TuiDirectPendingStart {
        captured_source: Some(("transcript.jsonl".to_string(), 42)),
        ..adk_record(1_553_381_551_168_036_906, 140)
    };
    persist(&finished).unwrap();
    persist(&captured).unwrap();
    let (claim, claims) = recording_claim(true);
    let (abort_cleanup, abort_calls, _) = recording_abort_cleanup();
    let worker = tokio::spawn(run_worker(
        shared,
        finished.clone(),
        mailbox_blocked_view(),
        claim,
        abort_cleanup,
        never_reclaim_orphan(),
    ));
    tokio::time::advance(PENDING_START_POLL * 3).await;
    tokio::task::yield_now().await;

    for anchor in [finished.anchor_message_id, captured.anchor_message_id] {
        super::super::super::tui_direct_abort_marker::resolve_own_claim_markers_for_visibly_completed_anchor(
            "claude", TMUX, CHANNEL, anchor,
        );
    }
    tokio::time::advance(PENDING_START_BACKSTOP + PENDING_START_POLL * 2).await;
    tokio::task::yield_now().await;
    worker.await.unwrap();

    assert!(claims.lock().unwrap().is_empty());
    assert_eq!(abort_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        durable_anchors(),
        vec![captured.anchor_message_id],
        "a captured-source restart obligation is not retired"
    );
}
