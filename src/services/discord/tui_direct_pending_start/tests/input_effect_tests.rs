//! The pending-start worker under the input fence: a closed gate starts no worker and keeps the
//! durable record; an admitted worker holds the drain through claim and terminal cleanup.

use super::*;
use crate::services::discord::input_runtime::{self, fence::Gate};
use crate::services::discord::make_shared_data_for_tests;
use crate::services::provider::ProviderKind;
use futures::FutureExt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

#[tokio::test(start_paused = true)]
async fn c2_boot_pending_start_restore_skips_protected_legacy_open_and_closing() {
    use crate::services::discord::tui_prompt_relay::synthetic_start::{
        RESTORE_CLAIM_FOR_TEST, RESTORE_VIEW_FOR_TEST, restore_pending_starts,
    };
    let _guard = worker_test_lock();
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let shared = make_shared_data_for_tests();
    let protected_cases = [
        (record("claude", 6_325_608, 6_325_618), false),
        (record("claude", 6_325_610, 6_325_620), true),
    ];
    let legacy = record("claude", 6_325_609, 6_325_619);
    persist(&legacy).unwrap();
    let mut protected_rows = Vec::new();
    let mut health_guards = Vec::new();
    let mut closing_guards = Vec::new();
    for (protected, closing) in &protected_cases {
        persist(protected).unwrap();
        let path = super::super::state::root().unwrap().join(format!(
            "{}_{}_{}.json",
            protected.provider, protected.channel_id, protected.anchor_message_id
        ));
        protected_rows.push((path.clone(), std::fs::read(&path).unwrap()));
        let gate = Gate::protect(ProviderKind::Claude, protected.channel_id).unwrap();
        health_guards.push(input_runtime::fence::test_health::Clear::new(&gate));
        if *closing {
            closing_guards.push(gate.close().unwrap());
        }
    }
    reset_present_for_tests();
    // Boot skips all protection modes before presence, unlike runtime admission.
    let claims = Arc::new(AtomicU32::new(0));
    RESTORE_VIEW_FOR_TEST
        .with(|slot| *slot.borrow_mut() = Some(gated_view(Arc::new(AtomicBool::new(true)))));
    RESTORE_CLAIM_FOR_TEST.with(|slot| *slot.borrow_mut() = Some(counting_claim(claims.clone())));
    restore_pending_starts(&shared, &ProviderKind::Claude);
    for (protected, closing) in &protected_cases {
        assert!(
            !pending_synthetic_start_present("claude", protected.channel_id),
            "boot restored the protected presence index: closing={closing}"
        );
    }
    assert!(pending_synthetic_start_present("claude", legacy.channel_id));
    settle().await;
    assert_eq!(
        claims.load(Ordering::SeqCst),
        1,
        "unprotected restore claims once"
    );
    for ((protected, closing), (path, before)) in protected_cases.iter().zip(&protected_rows) {
        assert_eq!(std::fs::read(path).unwrap(), *before);
        assert!(
            !pending_synthetic_start_present("claude", protected.channel_id),
            "settled boot restored protected presence: closing={closing}"
        );
    }
    reset_present_for_tests();
}

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

/// An admitted worker is polled on the blocking pool: wait in real time, virtual time held.
async fn settle_until(done: impl Fn() -> bool) {
    for _ in 0..4_000 {
        if done() {
            return;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("the admitted worker never reached the expected state");
}

/// Advances virtual time one poll at a time until the admitted worker reaches the state.
async fn advance_until(done: impl Fn() -> bool) {
    for _ in 0..4_000 {
        if done() {
            return;
        }
        tokio::time::advance(PENDING_START_POLL).await;
        for _ in 0..2 {
            tokio::task::yield_now().await;
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    panic!("the admitted worker never reached the expected state");
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
        crate::services::tui_prompt_dedupe::record_prompt_anchor(
            &rec.provider,
            &rec.tmux_session_name,
            rec.channel_id,
            rec.anchor_message_id,
        );
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
    let rec = record("claude", fenced, fenced + 10);
    assert!(
        crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
            &rec.provider,
            &rec.tmux_session_name,
            rec.channel_id,
        )
        .is_none(),
        "refused worker retained its prompt-anchor slot"
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
    let claim_release = Arc::new(tokio::sync::Notify::new());
    let claim_started = Arc::new(AtomicBool::new(false));
    let claim_on_worker = Arc::new(AtomicBool::new(false));
    let blocked_claim: ClaimFn = {
        let claims = claims.clone();
        let release = claim_release.clone();
        let started = claim_started.clone();
        let on_worker = claim_on_worker.clone();
        Box::new(move |_shared, _record| {
            let claims = claims.clone();
            let release = release.clone();
            let started = started.clone();
            let on_worker = on_worker.clone();
            Box::pin(async move {
                on_worker.store(
                    input_runtime::fence::require_worker().is_ok(),
                    Ordering::SeqCst,
                );
                started.store(true, Ordering::SeqCst);
                release.notified().await;
                claims.fetch_add(1, Ordering::SeqCst);
                true
            })
        })
    };
    spawn_worker(
        shared.clone(),
        rec,
        gated_view(ready.clone()),
        blocked_claim,
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
    advance_until(|| claim_started.load(Ordering::SeqCst)).await;
    assert!(
        claim_on_worker.load(Ordering::SeqCst),
        "the admitted claim runs where its row writer is allowed"
    );
    assert!(
        closing.drain().now_or_never().is_none(),
        "claim in progress lost its permit"
    );
    claim_release.notify_one();
    settle_until(|| claims.load(Ordering::SeqCst) == 1 && closing.drain().now_or_never().is_some())
        .await;
    assert!(!pending_synthetic_start_present("claude", channel));
    reset_present_for_tests();
}

#[tokio::test(start_paused = true)]
async fn c2_pending_start_drain_waits_for_reclaim_and_abort_cleanup() {
    let _guard = worker_test_lock();
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
    let temp = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
    reset_present_for_tests();
    let shared = make_shared_data_for_tests();
    let channel = 6_325_573;
    let rec = record("claude", channel, channel + 10);
    persist(&rec).unwrap();
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = input_runtime::fence::test_health::Clear::new(&gate);
    let stage = Arc::new(AtomicU32::new(0));
    let release_reclaim = Arc::new(tokio::sync::Notify::new());
    let release_cleanup = Arc::new(tokio::sync::Notify::new());
    let reclaim: ReclaimOrphanFn = {
        let stage = stage.clone();
        let release = release_reclaim.clone();
        Box::new(move |_, _| {
            let stage = stage.clone();
            let release = release.clone();
            Box::pin(async move {
                if stage.swap(1, Ordering::SeqCst) == 0 {
                    release.notified().await;
                }
                ReclaimStaleForeignOutcome::None
            })
        })
    };
    let cleanup: AbortCleanupFn = {
        let stage = stage.clone();
        let release = release_cleanup.clone();
        Box::new(move |_, _, _| {
            let stage = stage.clone();
            let release = release.clone();
            Box::pin(async move {
                stage.store(2, Ordering::SeqCst);
                release.notified().await;
                stage.store(3, Ordering::SeqCst);
            })
        })
    };
    let view: ViewFn = Box::new(|_, _| {
        Box::pin(async {
            Some(obs(PriorTurnView {
                inflight_present: true,
                inflight_is_own_anchor: false,
                mailbox_blocking_turn_present: true,
                mailbox_turn_is_own_anchor: false,
                runtime_binding_present: true,
            }))
        })
    });
    let claims = Arc::new(AtomicU32::new(0));
    spawn_worker(
        shared,
        rec,
        view,
        counting_claim(claims.clone()),
        cleanup,
        reclaim,
    );
    // The reclaim boundary is reached once the first backstop window expires.
    advance_until(|| stage.load(Ordering::SeqCst) == 1).await;
    let closing = gate.close().unwrap();
    assert!(
        closing.drain().now_or_never().is_none(),
        "reclaim lost its permit"
    );
    release_reclaim.notify_one();
    // The remaining backstop cycles run to the abort cleanup boundary.
    advance_until(|| stage.load(Ordering::SeqCst) == 2).await;
    assert!(
        closing.drain().now_or_never().is_none(),
        "cleanup lost its permit"
    );
    assert!(pending_synthetic_start_present("claude", channel));
    release_cleanup.notify_one();
    settle_until(|| {
        stage.load(Ordering::SeqCst) == 3 && !pending_synthetic_start_present("claude", channel)
    })
    .await;
    assert_eq!(claims.load(Ordering::SeqCst), 0);
    settle_until(|| closing.drain().now_or_never().is_some()).await;
    reset_present_for_tests();
}
