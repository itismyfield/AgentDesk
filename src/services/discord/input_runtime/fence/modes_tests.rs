use super::super::{Gate, PopulationScope, health_reasons, lookup, test_health};
use super::*;
use crate::services::turn_orchestrator::QueuePersistenceContext;
use std::path::Path;
use std::sync::Arc;

fn sandbox() -> tempfile::TempDir {
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
    std::fs::create_dir_all(&parent).unwrap();
    tempfile::tempdir_in(parent).unwrap()
}

fn health(closing: &Closing) -> Option<String> {
    closing.gate.health.lock().unwrap().clone()
}

#[tokio::test(start_paused = true)]
async fn e1_drain_timeout_holds_closing_and_keeps_the_admitted_permit_valid() {
    let channel = 6_325_501;
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = test_health::Clear::new(&gate);
    let permit = gate.admit().unwrap();
    let closing = gate.close().unwrap();
    assert_eq!(
        closing.drain_within(Duration::from_secs(5)).await,
        Err(Failure::Busy)
    );
    assert_eq!(gate.mode(), Mode::Closing);
    assert!(health_reasons().contains(&format!(
        "turn_transition_held provider=claude channel={channel}"
    )));
    let root = sandbox();
    assert!(matches!(
        closing.frozen_population(root.path()),
        Err(Failure::Busy)
    ));
    // The effect admitted before the close can still finish and take its population lock.
    assert_eq!(permit.validate(&ProviderKind::Claude, channel), Ok(()));
    let scope =
        PopulationScope::writer(root.path(), &ProviderKind::Claude, channel, &permit).unwrap();
    drop(scope);
    assert!(matches!(gate.admit(), Err(Failure::Mode(Mode::Closing))));
    drop(permit);
    assert_eq!(closing.drain_within(Duration::from_secs(5)).await, Ok(()));
}

#[test]
fn e1_committed_move_opens_the_ledger_and_handback_release_keeps_the_order_barrier() {
    struct Env(Option<std::ffi::OsString>);
    impl Drop for Env {
        fn drop(&mut self) {
            unsafe {
                match &self.0 {
                    Some(old) => std::env::set_var("AGENTDESK_ROOT_DIR", old),
                    None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
                }
            }
        }
    }
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = sandbox();
    let _env = Env(std::env::var_os("AGENTDESK_ROOT_DIR"));
    unsafe {
        std::env::set_var("AGENTDESK_ROOT_DIR", root.path());
    }
    let runtime = root.path().join("runtime");
    let channel = 6_325_502;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
            let _health = test_health::Clear::new(&gate);
            let closing = Arc::new(gate.close().unwrap());
            let transition = shared
                .freeze_legacy_queue(
                    closing.clone(),
                    QueuePersistenceContext::new(&ProviderKind::Claude, "session", None),
                )
                .await
                .unwrap();
            assert_eq!(gate.mode(), Mode::Frozen);
            closing.open_ledger().unwrap();
            assert_eq!(gate.mode(), Mode::LedgerOpen);
            assert!(matches!(gate.admit(), Err(Failure::Mode(Mode::LedgerOpen))));
            assert!(matches!(closing.population(&runtime), Err(Failure::Busy)));
            assert_eq!(closing.release_after_handback(), Err(Failure::Busy));
            assert!(
                !order_barrier(&ProviderKind::Claude, channel),
                "a refused release leaves no barrier"
            );
            closing.begin_handback().unwrap();
            drop(closing.population(&runtime).unwrap());
            closing.release_after_handback().unwrap();
            assert!(lookup(&ProviderKind::Claude, channel).is_none());
            assert!(order_barrier(&ProviderKind::Claude, channel));
            assert_eq!(closing.release_after_handback(), Err(Failure::Busy));
            assert!(
                order_barrier(&ProviderKind::Claude, channel),
                "a stale release must preserve an already installed catch-up barrier"
            );
            let mut order = AdmissionOrder::new(ProviderKind::Claude, channel, 1, 0);
            let cap = order.begin_scan(1, vec![], 0, true).unwrap();
            let cap = order.complete(cap, 1).unwrap();
            assert_eq!(settle_order_barrier(&order, cap, 1), Ok(true));
            assert!(!order_barrier(&ProviderKind::Claude, channel));
            drop(transition);
        });
}

#[test]
fn g1a_handback_settle_rechecks_current_capability_and_retains_barrier_on_refusal() {
    let channel = 6_325_714;
    let closing = Closing::frozen_for_test(ProviderKind::Claude, channel);
    closing.begin_handback().unwrap();
    closing.release_after_handback().unwrap();
    let mut order = AdmissionOrder::new(ProviderKind::Claude, channel, 7, 10);
    order.pending(&[11]);
    let mut cap = order.begin_scan(7, vec![11], 11, true).unwrap();
    order.settle(&mut cap, 7, 11).unwrap();
    let cap = order.complete(cap, 7).unwrap();
    let mut other = AdmissionOrder::new(ProviderKind::Claude, channel, 7, 11);
    assert!(other.begin_scan(7, vec![], 11, true).is_err());
    order.pending(&[12]);
    assert_eq!(
        settle_order_barrier(&order, cap, 7),
        Err(Failure::StalePermit)
    );
    assert!(order_barrier(&ProviderKind::Claude, channel));
    let mut cap = order.begin_scan(7, vec![12], 12, true).unwrap();
    order.settle(&mut cap, 7, 12).unwrap();
    let cap = order.complete(cap, 7).unwrap();
    order.invalidate(8);
    assert_eq!(
        settle_order_barrier(&order, cap, 8),
        Err(Failure::StalePermit)
    );
    assert!(order_barrier(&ProviderKind::Claude, channel));
    let cap = order.begin_scan(8, vec![], 12, true).unwrap();
    let cap = order.complete(cap, 8).unwrap();
    assert_eq!(settle_order_barrier(&order, cap, 8), Ok(true));
}

#[test]
fn g1a_a_later_handback_rejects_the_previous_complete_capability() {
    let channel = 6_325_715;
    let first = Closing::frozen_for_test(ProviderKind::Claude, channel);
    first.begin_handback().unwrap();
    first.release_after_handback().unwrap();
    let mut order = AdmissionOrder::new(ProviderKind::Claude, channel, 7, 10);
    let cap = order.begin_scan(7, vec![], 10, true).unwrap();
    let old = order.complete(cap, 7).unwrap();
    let second = Closing::frozen_for_test(ProviderKind::Claude, channel);
    second.begin_handback().unwrap();
    second.release_after_handback().unwrap();
    let (_, _, owner) = old.scope();
    claim_order_barrier(&ProviderKind::Claude, channel, owner).unwrap();
    assert_eq!(
        settle_order_barrier(&order, old, 7),
        Err(Failure::StalePermit)
    );
    assert!(order_barrier(&ProviderKind::Claude, channel));
    let cap = order.begin_scan(7, vec![], 10, true).unwrap();
    let cap = order.complete(cap, 7).unwrap();
    assert_eq!(settle_order_barrier(&order, cap, 7), Ok(true));
}

#[test]
fn g1a_dropped_controller_releases_only_its_claim_and_keeps_the_handback_barrier() {
    let channel = 6_325_716;
    let closing = Closing::frozen_for_test(ProviderKind::Claude, channel);
    closing.begin_handback().unwrap();
    closing.release_after_handback().unwrap();
    let mut first = AdmissionOrder::new(ProviderKind::Claude, channel, 7, 10);
    let cap = first.begin_scan(7, vec![], 10, true).unwrap();
    let old = first.complete(cap, 7).unwrap();
    let mut second = AdmissionOrder::new(ProviderKind::Claude, channel, 7, 10);
    assert!(second.begin_scan(7, vec![], 10, true).is_err());
    drop(second);
    let mut second = AdmissionOrder::new(ProviderKind::Claude, channel, 7, 10);
    assert!(second.begin_scan(7, vec![], 10, true).is_err());
    drop(first);
    assert!(order_barrier(&ProviderKind::Claude, channel));
    let cap = second.begin_scan(7, vec![], 10, true).unwrap();
    let fresh = second.complete(cap, 7).unwrap();
    assert_eq!(
        settle_order_barrier(&second, old, 7),
        Err(Failure::StalePermit)
    );
    assert!(order_barrier(&ProviderKind::Claude, channel));
    assert_eq!(settle_order_barrier(&second, fresh, 7), Ok(true));
}

#[test]
fn e1_abort_returns_only_a_history_free_close_and_hold_refuses_both_owners() {
    let channel = 6_325_503;
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = test_health::Clear::new(&gate);
    let permit = gate.admit().unwrap();
    let closing = gate.close().unwrap();
    assert_eq!(closing.abort_close(true), Err(Failure::Mode(Mode::Closing)));
    assert_eq!(gate.mode(), Mode::Closing);
    closing.abort_close(false).unwrap();
    assert_eq!(gate.mode(), Mode::LegacyOpen);
    assert_eq!(permit.validate(&ProviderKind::Claude, channel), Ok(()));
    drop(permit);
    drop(gate.admit().unwrap());

    let frozen = Closing::frozen_for_test(ProviderKind::Claude, 6_325_504);
    assert_eq!(frozen.abort_close(true), Err(Failure::Mode(Mode::Frozen)));
    frozen.hold().unwrap();
    assert_eq!(frozen.gate.mode(), Mode::Held);
    assert_eq!(
        health(&frozen).as_deref(),
        Some("turn_transition_held provider=claude channel=6325504")
    );
    let root = sandbox();
    assert!(matches!(frozen.population(root.path()), Err(Failure::Busy)));
    assert_eq!(frozen.abort_close(false), Err(Failure::Mode(Mode::Held)));
    assert_eq!(frozen.hold(), Err(Failure::Mode(Mode::Held)));
    frozen.open_ledger().unwrap();
    assert_eq!(health(&frozen), None);
    frozen.begin_handback().unwrap();
    assert!(frozen.population(root.path()).is_ok());
}
