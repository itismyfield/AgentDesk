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
    fn new() -> Self {
        let worker = worker_test_lock();
        let env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        reset_present_for_tests();
        POST_ABORT_PROMOTE_CALLS.store(0, Ordering::SeqCst);
        Self {
            _worker: worker,
            _env_lock: env_lock,
            _env: env,
            _temp: temp,
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        reset_present_for_tests();
        POST_ABORT_PROMOTE_CALLS.store(0, Ordering::SeqCst);
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

/// Retiring one anchor stops its waiting worker before any claim, keeps its
/// file deleted, and leaves a sibling record's gate on the same channel.
#[tokio::test(start_paused = true)]
async fn retired_waiting_worker_exits_without_claim_and_keeps_sibling_gate() {
    let _rig = Rig::new();
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
    let _rig = Rig::new();
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
    let _rig = Rig::new();
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
