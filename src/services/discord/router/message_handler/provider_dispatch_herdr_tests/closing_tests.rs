//! Once a termination chose a Pending execution, no start of it is recorded and no attach
//! publishes or binds it: its hook is refused for a retry and its turn writes nothing.
use super::*;

/// The turn after a termination chose a launched Pending pane whose start was logged or not;
/// returns that turn's error and how many attaches it tried.
fn turn_after_shut(tag: &str, start_logged: bool) -> (String, usize) {
    let fx = Fixture::new(tag, None);
    let launcher = Arc::new(Launcher::default());
    let mut ports = fx.ports(&launcher);
    ports.cancel_after_attach = true;
    let started = Mutex::new(None);
    let (first, _) = fx.turn(&HostedRecord::Legacy, &ports, || {
        *started.lock().unwrap() = fx.start_provider(&launcher, start_logged);
        if !start_logged {
            fx.cancel_now();
        }
    });
    assert!(first.is_err());
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    crate::services::session_host::shut_execution_for_test(&nonce);
    if !start_logged {
        let started = started.into_inner().unwrap().unwrap();
        let payload = json!({"session_id": started.session, "source": "startup",
            "transcript_path": started.path});
        assert_ne!(started.hook(&fx, "SessionStart", &payload), 202);
    }
    let ports = fx.ports(&launcher);
    let (next, _) = fx.turn(&fx.record(), &ports, || {});
    assert!(fx.rig.sends().is_empty(), "nothing typed");
    assert_eq!(fx.row(), Some(HostedState::Pending));
    (next.unwrap_err(), ports.attaches.lock().unwrap().len())
}

#[test]
fn a_start_hook_after_the_fence_shut_is_refused_and_the_pending_stays_unstarted_pg() {
    let (error, attaches) = turn_after_shut("closing-hook", false);
    assert!(error.contains("no logged start"), "{error}");
    assert_eq!(attaches, 0);
}

#[test]
fn an_attach_after_the_fence_shut_publishes_and_binds_nothing_pg() {
    let (error, attaches) = turn_after_shut("closing-attach", true);
    assert!(error.contains("source not attached"), "{error}");
    assert_eq!(attaches, 1);
}
