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
            assert_eq!(closing.release_after_handback(&[], 0), Err(Failure::Busy));
            assert!(
                !order_barrier(&ProviderKind::Claude, channel),
                "a refused release leaves no barrier"
            );
            closing.begin_handback().unwrap();
            drop(closing.population(&runtime).unwrap());
            closing.release_after_handback(&[], 0).unwrap();
            assert!(lookup(&ProviderKind::Claude, channel).is_none());
            assert!(order_barrier(&ProviderKind::Claude, channel));
            assert_eq!(closing.release_after_handback(&[], 0), Err(Failure::Busy));
            assert!(
                order_barrier(&ProviderKind::Claude, channel),
                "a stale release must preserve an already installed catch-up barrier"
            );
            assert_eq!(
                settle_order_barrier(complete_handback(channel, 0)),
                Ok(true)
            );
            assert!(!order_barrier(&ProviderKind::Claude, channel));
            drop(transition);
        });
}

fn scan_handback(channel: u64, sources: Vec<u64>, horizon: u64) -> ordering::ScanCapability {
    let ticket = ordering::handback_fetch_ticket(&ProviderKind::Claude, channel).unwrap();
    ordering::handback_begin_scan(ticket, sources, horizon, true).unwrap()
}

fn complete_handback(channel: u64, horizon: u64) -> ValidatedOrderCapability {
    ordering::handback_complete(scan_handback(channel, vec![], horizon)).unwrap()
}

#[test]
fn g1a_handback_settle_rechecks_current_capability_and_retains_barrier_on_refusal() {
    let channel = 6_325_714;
    let closing = Closing::frozen_for_test(ProviderKind::Claude, channel);
    closing.begin_handback().unwrap();
    closing.release_after_handback(&[], 7).unwrap();
    let cap = complete_handback(channel, 0);
    let hint = ordering::handback_pending(&ProviderKind::Claude, channel, &[11]).unwrap();
    assert_eq!(hint.dirty_generation, 8);
    assert_eq!(settle_order_barrier(cap), Err(Failure::StalePermit));
    assert!(order_barrier(&ProviderKind::Claude, channel));
    let cap = scan_handback(channel, vec![11], 11);
    let cap = ordering::handback_settle_sources(cap, &[11]).unwrap();
    assert!(ordering::handback_complete(cap).is_err());
    assert!(order_barrier(&ProviderKind::Claude, channel));
    assert_eq!(
        settle_order_barrier(complete_handback(channel, 11)),
        Ok(true)
    );
}

#[test]
fn g1a_a_later_handback_rejects_the_previous_complete_capability() {
    let channel = 6_325_715;
    let first = Closing::frozen_for_test(ProviderKind::Claude, channel);
    first.begin_handback().unwrap();
    first.release_after_handback(&[], 0).unwrap();
    let old = complete_handback(channel, 0);
    let second = Closing::frozen_for_test(ProviderKind::Claude, channel);
    second.begin_handback().unwrap();
    second.release_after_handback(&[], 0).unwrap();
    assert_eq!(settle_order_barrier(old), Err(Failure::StalePermit));
    assert!(order_barrier(&ProviderKind::Claude, channel));
    assert_eq!(
        settle_order_barrier(complete_handback(channel, 0)),
        Ok(true)
    );
}

#[test]
fn g1a_unrelated_order_cannot_issue_a_post_release_settle_capability() {
    let channel = 6_325_716;
    let mut original = ordering::AdmissionOrder::new(ProviderKind::Claude, channel, 7, 0);
    original.pending(&[11]);
    let snapshot = original.pending_snapshot();
    let closing = Closing::frozen_for_test(ProviderKind::Claude, channel);
    closing.begin_handback().unwrap();
    closing
        .release_after_handback(&snapshot.sources, snapshot.dirty_generation)
        .unwrap();
    drop(original);
    let mut other = ordering::AdmissionOrder::new(ProviderKind::Claude, channel, 2, 0);
    let ticket = other.fetch_ticket(2).unwrap();
    let cap = other.begin_scan(ticket, 2, vec![], 0, true).unwrap();
    let cap = other.complete(cap, 2).unwrap();
    assert_eq!(settle_order_barrier(cap), Err(Failure::StalePermit));
    assert!(order_barrier(&ProviderKind::Claude, channel));
    let cap = scan_handback(channel, vec![11], 11);
    let cap = ordering::handback_settle_sources(cap, &[11]).unwrap();
    assert!(ordering::handback_complete(cap).is_err());
    assert_eq!(
        settle_order_barrier(complete_handback(channel, 11)),
        Ok(true)
    );
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
