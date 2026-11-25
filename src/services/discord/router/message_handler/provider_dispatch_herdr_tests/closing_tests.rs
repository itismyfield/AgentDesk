//! Once a termination chose a Pending execution, no start of it is recorded and no attach
//! publishes or binds it: its hook is refused for a retry and its turn writes nothing.
use super::*;
use crate::services::session_host::shut_execution_for_test;

/// A launched pane left Pending, its start hook sent when `start`; its nonce and provider.
fn launched(fx: &Fixture, launcher: &Arc<Launcher>, start: bool) -> (String, Started) {
    let mut ports = fx.ports(launcher);
    ports.cancel_after_attach = true;
    let started = Mutex::new(None);
    let (first, _) = fx.turn(&HostedRecord::Legacy, &ports, || {
        *started.lock().unwrap() = fx.start_provider(launcher, start);
        if !start {
            fx.cancel_now();
        }
    });
    assert!(first.is_err());
    assert_eq!(fx.row(), Some(HostedState::Pending));
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    (nonce, started.into_inner().unwrap().unwrap())
}

#[test]
fn a_start_hook_after_the_fence_shut_is_refused_and_the_pending_stays_unstarted_pg() {
    let fx = Fixture::new("closing-hook", None);
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = launched(&fx, &launcher, false);
    shut_execution_for_test(&nonce);
    let payload = json!({"session_id": started.session, "source": "startup",
        "transcript_path": started.path});
    assert_ne!(started.hook(&fx, "SessionStart", &payload), 202);
    let ports = fx.ports(&launcher);
    let (next, _) = fx.turn(&fx.record(), &ports, || {});
    let next = next.unwrap_err();
    assert!(next.contains("no logged start"), "{next}");
    assert!(ports.attaches.lock().unwrap().is_empty());
    assert!(fx.rig.sends().is_empty());
}

#[test]
fn an_attach_after_the_fence_shut_publishes_and_binds_nothing_pg() {
    let fx = Fixture::new("closing-attach", None);
    let launcher = Arc::new(Launcher::default());
    let (nonce, _started) = launched(&fx, &launcher, true);
    shut_execution_for_test(&nonce);
    let ports = fx.ports(&launcher);
    let (next, _) = fx.turn(&fx.record(), &ports, || {});
    assert!(next.is_err(), "{next:?}");
    assert_eq!(ports.attaches.lock().unwrap().len(), 1);
    assert!(fx.rig.sends().is_empty(), "nothing typed");
    assert_eq!(fx.row(), Some(HostedState::Pending));
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
}
