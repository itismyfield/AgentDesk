use super::*;
use std::cell::RefCell;

struct ProbeOverride {
    pane: PaneLiveness,
    activity_recent: bool,
    calls: Vec<(&'static str, String)>,
}

thread_local! {
    static PROBES: RefCell<Option<ProbeOverride>> = const { RefCell::new(None) };
}

struct ProbeGuard;

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        PROBES.with(|slot| slot.borrow_mut().take());
    }
}

pub(super) fn tmux_session_pane_liveness(session: &str) -> PaneLiveness {
    PROBES.with(|slot| {
        if let Some(probes) = slot.borrow_mut().as_mut() {
            probes.calls.push(("pane", session.to_owned()));
            return probes.pane;
        }
        crate::services::tmux_diagnostics::tmux_session_pane_liveness(session)
    })
}

pub(super) fn watcher_runtime_activity_recent(session: &str) -> bool {
    PROBES.with(|slot| {
        if let Some(probes) = slot.borrow_mut().as_mut() {
            probes.calls.push(("activity", session.to_owned()));
            return probes.activity_recent;
        }
        super::watcher_runtime_activity_recent(session)
    })
}

fn probe_runtime_watcher(
    session: Option<&str>,
    pane: PaneLiveness,
    activity_recent: bool,
) -> (bool, Vec<(&'static str, String)>) {
    PROBES.with(|slot| {
        assert!(slot.borrow().is_none());
        slot.replace(Some(ProbeOverride {
            pane,
            activity_recent,
            calls: Vec::new(),
        }));
    });
    let _guard = ProbeGuard;
    let state = InflightTurnState::new(
        ProviderKind::Codex,
        1,
        None,
        0,
        0,
        0,
        String::new(),
        None,
        session.map(str::to_owned),
        None,
        None,
        0,
    );
    let result = runtime_watcher_is_proven_dead(&state);
    let calls = PROBES.with(|slot| slot.borrow_mut().take().unwrap().calls);
    (result, calls)
}

#[test]
fn runtime_watcher_proven_dead_skips_probes_without_session() {
    for session in [None, Some(""), Some(" \t\r\n"), Some("\u{2003}")] {
        let (dead, calls) = probe_runtime_watcher(session, PaneLiveness::DeadOrAbsent, false);
        assert!(calls.is_empty(), "session {session:?}: {calls:?}");
        assert!(!dead, "session {session:?}");
    }
}

#[test]
fn runtime_watcher_proven_dead_preserves_probe_error() {
    for activity_recent in [false, true] {
        let (dead, calls) = probe_runtime_watcher(
            Some(" \twatcher-test\n"),
            PaneLiveness::ProbeError,
            activity_recent,
        );
        assert!(!dead, "activity_recent={activity_recent}");
        assert_eq!(calls, [("pane", "watcher-test".to_owned())]);
    }
}

#[test]
fn runtime_watcher_proven_dead_delegates_signals() {
    for pane in [PaneLiveness::Live, PaneLiveness::DeadOrAbsent] {
        for activity_recent in [false, true] {
            let (dead, calls) =
                probe_runtime_watcher(Some(" \twatcher-test\n"), pane, activity_recent);
            assert_eq!(dead, proven_dead_from_signals(pane, activity_recent));
            assert_eq!(
                dead, !activity_recent,
                "{pane:?}, activity_recent={activity_recent}"
            );
            assert_eq!(
                calls,
                [
                    ("pane", "watcher-test".to_owned()),
                    ("activity", "watcher-test".to_owned())
                ]
            );
        }
    }
}
