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

pub(super) fn tmux_session_pane_liveness(session: &str, state: &InflightTurnState) -> PaneLiveness {
    PROBES.with(|slot| {
        if let Some(probes) = slot.borrow_mut().as_mut() {
            probes.calls.push(("pane", session.to_owned()));
            return probes.pane;
        }
        super::local_pane_liveness(session, state)
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

// The warm sweeper's dead-watcher reap reads the stored rows before it unlinks: a found legacy
// row, or no row with no other-host trace, loses its orphan; a failed pane probe keeps it.
#[tokio::test]
async fn dead_watcher_rebind_reap_unlinks_only_a_row_the_host_guard_admits_pg() {
    use crate::services::discord::host_teardown_gate::test_support::{
        Stored, channel_key, seed, shared_on,
    };
    use crate::services::session_host::test_support::InjectedLivenessGuard;
    use crate::services::session_host::{HostLiveness, HostSessionRef};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let probe_error = (Stored::Legacy, HostLiveness::ProbeError);
    let cases = Stored::ALL
        .into_iter()
        .map(|stored| (stored, HostLiveness::DeadOrAbsent))
        .chain([probe_error]);
    for (n, (stored, pane)) in cases.enumerate() {
        let channel = 1_479_671_301_387_080_000 + n as u64;
        let name = provider.build_tmux_session_name(&format!("p4b1-reap-{n}"));
        seed(&pool, &channel_key(&shared, &name), &name, channel, stored).await;
        let _pane = InjectedLivenessGuard::set(HostSessionRef::tmux(&name), pane);
        let mut row = InflightTurnState::new(
            provider.clone(),
            channel,
            None,
            0,
            0,
            0,
            String::new(),
            None,
            Some(name.clone()),
            None,
            None,
            0,
        );
        row.rebind_origin = true;
        row.turn_source = TurnSource::ExternalAdopted;
        row.set_relay_owner_kind(RelayOwnerKind::Watcher);
        row.turn_start_offset = Some(0);
        row.rebind_origin_birth_generation = Some(1);
        let root = inflight_runtime_root().expect("runtime root");
        save_inflight_state_in_root(&root, &row).expect("persist the orphan");

        let reaped = sweep_reap_dead_watcher_rebind_origin(&shared, &provider, &row, 0, 2).await;
        let admitted = pane == HostLiveness::DeadOrAbsent
            && matches!(stored, Stored::Legacy | Stored::Missing);
        let label = format!("{stored:?} {pane:?}");
        assert_eq!(reaped, admitted, "{label}");
        let left = load_inflight_state(&provider, channel).is_some();
        assert_eq!(left, !admitted, "{label}");
    }
    pool.close().await;
    db.drop().await;
}
