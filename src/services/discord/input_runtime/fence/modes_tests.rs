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
            assert!(settle_order_barrier(&ProviderKind::Claude, channel));
            assert!(!order_barrier(&ProviderKind::Claude, channel));
            drop(transition);
        });
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
