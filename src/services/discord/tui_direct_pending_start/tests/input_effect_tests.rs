//! The pending-start worker under the input fence: a closed gate starts no worker and keeps the
//! durable record; an admitted worker holds the drain until it claims.

use super::*;
use crate::services::discord::input_runtime::{self, fence::Gate};
use crate::services::discord::make_shared_data_for_tests;
use crate::services::provider::ProviderKind;
use futures::FutureExt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Reports the prior turn finalized once `ready` is set.
fn gated_view(ready: Arc<AtomicBool>) -> ViewFn {
    Box::new(move |_shared, _record| {
        let finalized = ready.load(Ordering::SeqCst);
        Box::pin(async move {
            Some(obs(PriorTurnView {
                inflight_present: !finalized,
                ..base_view()
            }))
        })
    })
}

fn counting_claim(claims: Arc<AtomicU32>) -> ClaimFn {
    Box::new(move |_shared, _record| {
        let claims = claims.clone();
        Box::pin(async move {
            claims.fetch_add(1, Ordering::SeqCst);
            true
        })
    })
}

async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn c2_closed_input_gate_starts_no_pending_start_worker_and_keeps_the_record() {
    let _guard = worker_test_lock();
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
    let temp = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
    reset_present_for_tests();
    let shared = make_shared_data_for_tests();
    let (open, fenced) = (6_325_570_u64, 6_325_571_u64);
    let gate = Gate::protect(ProviderKind::Claude, fenced).unwrap();
    let _health = input_runtime::fence::test_health::Clear::new(&gate);
    let _closing = gate.close().unwrap();
    let claims = Arc::new(AtomicU32::new(0));
    for channel in [open, fenced] {
        let rec = record("claude", channel, channel + 10);
        persist(&rec).unwrap();
        let (cleanup, _, _) = recording_abort_cleanup();
        spawn_worker(
            shared.clone(),
            rec,
            gated_view(Arc::new(AtomicBool::new(true))),
            counting_claim(claims.clone()),
            cleanup,
            never_reclaim_orphan(),
        );
    }
    settle().await;

    assert_eq!(
        claims.load(Ordering::SeqCst),
        1,
        "only the open channel claims"
    );
    assert!(!pending_synthetic_start_present("claude", open));
    assert!(
        pending_synthetic_start_present("claude", fenced),
        "the fenced record is kept for its later owner"
    );
    assert!(
        input_runtime::health_reasons()
            .iter()
            .any(|reason| reason.contains(&format!("channel={fenced}"))),
        "the refusal is a health reason"
    );
    reset_present_for_tests();
}

#[tokio::test(start_paused = true)]
async fn c2_admitted_pending_start_worker_holds_the_input_drain_until_it_claims() {
    let _guard = worker_test_lock();
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
    let temp = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
    reset_present_for_tests();
    let shared = make_shared_data_for_tests();
    let channel = 6_325_572_u64;
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = input_runtime::fence::test_health::Clear::new(&gate);
    let rec = record("claude", channel, channel + 10);
    persist(&rec).unwrap();
    let claims = Arc::new(AtomicU32::new(0));
    let ready = Arc::new(AtomicBool::new(false));
    let (cleanup, _, _) = recording_abort_cleanup();
    spawn_worker(
        shared.clone(),
        rec,
        gated_view(ready.clone()),
        counting_claim(claims.clone()),
        cleanup,
        never_reclaim_orphan(),
    );
    settle().await;
    let closing = gate.close().unwrap();
    assert!(
        closing.drain().now_or_never().is_none(),
        "the waiting worker holds the drain"
    );
    ready.store(true, Ordering::SeqCst);
    tokio::time::advance(PENDING_START_POLL * 2).await;
    settle().await;
    assert_eq!(claims.load(Ordering::SeqCst), 1);
    assert!(
        closing.drain().now_or_never().is_some(),
        "the claim releases the drain"
    );
    assert!(!pending_synthetic_start_present("claude", channel));
    reset_present_for_tests();
}
