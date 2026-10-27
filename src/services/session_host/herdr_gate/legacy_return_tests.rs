use super::*;
use crate::db::dispatched_sessions::hosted_execution::tests::{owner, record};
use crate::services::session_host::herdr::contract::{HerdrOutcome, Witnessed};
use crate::services::session_host::herdr::model::{HerdrReply, HerdrResult, HerdrSnapshot};
use crate::services::session_host::herdr::provenance::{ProcessStart, StartIdentity};

struct MissingPane;
impl HerdrTransport for MissingPane {
    fn call_with_witness(&self, _: &HerdrCall, _: &ServerWitness) -> HerdrOutcome {
        panic!("legacy return must never mutate Herdr")
    }

    fn server_witness(&self) -> Witnessed {
        Ok(ServerWitness::for_test(99))
    }
    fn call(&self, call: &HerdrCall) -> (HerdrOutcome, Witnessed) {
        assert!(matches!(call.request, HerdrRequest::SessionSnapshot {}));
        (
            Ok(HerdrReply {
                id: call.id.clone(),
                body: Ok(HerdrResult::SessionSnapshot {
                    snapshot: HerdrSnapshot {
                        version: "0.9.3".into(),
                        protocol: 22,
                        panes: vec![],
                    },
                }),
            }),
            self.server_witness(),
        )
    }
}
struct Os(Result<bool, RestoreUnverified>);
impl ProcessOs for Os {
    fn exists(&self, pid: u32) -> Result<bool, RestoreUnverified> {
        assert_eq!(pid, 101);
        self.0
    }
    fn start(&self, _: u32) -> Result<ProcessStart, RestoreUnverified> {
        Ok(ProcessStart {
            identity: StartIdentity::LinuxTicks(1),
            wall_clock: UNIX_EPOCH,
        })
    }
    fn parent(&self, _: u32) -> Result<u32, RestoreUnverified> {
        unreachable!()
    }
    fn environ(&self, _: u32) -> Result<Vec<String>, RestoreUnverified> {
        unreachable!()
    }
    fn exec_path(&self, _: u32) -> Option<String> {
        unreachable!()
    }
}
fn endpoint() -> HerdrEndpoint {
    HerdrEndpoint::new(
        "test-node",
        "herdr.default",
        // The endpoint needs an absolute socket path, which takes a drive prefix on Windows.
        std::path::Path::new(if cfg!(windows) {
            r"C:\adk\herdr.sock"
        } else {
            "/adk/herdr.sock"
        }),
        "agentdesk",
    )
    .unwrap()
}

pub(crate) fn ended(record: &HostedExecution) -> RetiredEnd {
    RetiredEnd::observe_with_os(
        &endpoint(),
        Arc::new(MissingPane),
        record,
        Arc::new(Os(Ok(false))),
    )
    .unwrap()
}

pub(crate) fn provider_alive(record: &HostedExecution) -> Result<RetiredEnd, ReturnEndRefusal> {
    RetiredEnd::observe_with_os(
        &endpoint(),
        Arc::new(MissingPane),
        record,
        Arc::new(Os(Ok(true))),
    )
}

#[test]
fn legacy_return_end_requires_both_observations() {
    let r = record(&owner("return-end"), "n1", HostedState::Retired);
    assert!(ended(&r).matches(&r));
    assert!(HerdrTarget::new(endpoint(), Arc::new(MissingPane), &r).is_none());
    for (exists, process) in [
        (Ok(true), pane_probe::RecordedProcess::Replaced),
        (
            Err(RestoreUnverified::ProcessUnreadable),
            pane_probe::RecordedProcess::Unreadable,
        ),
    ] {
        assert_eq!(
            RetiredEnd::observe_with_os(
                &endpoint(),
                Arc::new(MissingPane),
                &r,
                Arc::new(Os(exists))
            )
            .unwrap_err(),
            ReturnEndRefusal::ProviderUnverified(process)
        );
    }
    let mut live = r.clone();
    live.expected.as_mut().unwrap().provider_process.start = "linux-ticks:1".into();
    assert_eq!(
        RetiredEnd::observe_with_os(
            &endpoint(),
            Arc::new(MissingPane),
            &live,
            Arc::new(Os(Ok(true)))
        )
        .unwrap_err(),
        ReturnEndRefusal::ProviderUnverified(pane_probe::RecordedProcess::ExactAlive)
    );
    for pane in [
        PaneReading::Unreadable("no observation".into()),
        PaneReading::Present {
            root: r.expected.as_ref().unwrap().root.clone(),
            provider: PaneProvider::Execution(
                r.expected.as_ref().unwrap().provider_process.clone(),
            ),
        },
    ] {
        assert_eq!(
            RetiredEnd::from_reads(&r, pane, &Os(Ok(false))).unwrap_err(),
            ReturnEndRefusal::PaneUnverified
        );
    }
    let pane = PaneReading::Present {
        root: r.expected.as_ref().unwrap().root.clone(),
        provider: PaneProvider::Exited,
    };
    assert!(RetiredEnd::from_reads(&r, pane, &Os(Ok(false))).is_ok());
    // The production OS entry is exercised with a PID outside its signed platform range.
    let mut unobservable = r.clone();
    unobservable.expected.as_mut().unwrap().provider_process.pid = u32::MAX;
    assert!(RetiredEnd::observe(&endpoint(), Arc::new(MissingPane), &unobservable).is_err());
}

#[test]
fn legacy_return_end_rejects_nonretired_incomplete_and_other_endpoint() {
    let r = record(&owner("return-end"), "n1", HostedState::Retired);
    for state in [HostedState::Pending, HostedState::Bound] {
        let mut next = r.clone();
        next.state = state;
        assert_eq!(
            RetiredEnd::observe_with_os(
                &endpoint(),
                Arc::new(MissingPane),
                &next,
                Arc::new(Os(Ok(false)))
            )
            .unwrap_err(),
            ReturnEndRefusal::NotRetired
        );
    }
    let mut next = r.clone();
    next.schema = 2;
    assert_eq!(
        RetiredEnd::observe_with_os(
            &endpoint(),
            Arc::new(MissingPane),
            &next,
            Arc::new(Os(Ok(false)))
        )
        .unwrap_err(),
        ReturnEndRefusal::NotRetired
    );
    for missing_location in [false, true] {
        let mut next = r.clone();
        if missing_location {
            next.location = None;
        } else {
            next.expected = None;
        }
        assert_eq!(
            RetiredEnd::observe_with_os(
                &endpoint(),
                Arc::new(MissingPane),
                &next,
                Arc::new(Os(Ok(false)))
            )
            .unwrap_err(),
            ReturnEndRefusal::Incomplete
        );
    }
    let mut next = r.clone();
    next.location.as_mut().unwrap().socket_addr = "/other.sock".into();
    assert_eq!(
        RetiredEnd::observe_with_os(
            &endpoint(),
            Arc::new(MissingPane),
            &next,
            Arc::new(Os(Ok(false)))
        )
        .unwrap_err(),
        ReturnEndRefusal::EndpointMismatch
    );
}

/// Reads each pid as its `RecordedProcess` names, against a record whose starts are
/// `linux-ticks:<pid>`; a pid not named is unreadable. Counts the existence reads.
struct PidOs(HashMap<u32, pane_probe::RecordedProcess>, AtomicU64);
impl PidOs {
    fn new(root: pane_probe::RecordedProcess, provider: pane_probe::RecordedProcess) -> Self {
        Self(
            HashMap::from([(100, root), (101, provider)]),
            AtomicU64::new(0),
        )
    }
}
impl ProcessOs for PidOs {
    fn exists(&self, pid: u32) -> Result<bool, RestoreUnverified> {
        use pane_probe::RecordedProcess::{ProvenAbsent, Unreadable};
        self.1.fetch_add(1, Ordering::SeqCst);
        match self.0.get(&pid) {
            Some(ProvenAbsent) => Ok(false),
            None | Some(Unreadable) => Err(RestoreUnverified::ProcessUnreadable),
            Some(_) => Ok(true),
        }
    }
    fn start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified> {
        let ticks = match self.0.get(&pid) {
            Some(pane_probe::RecordedProcess::ExactAlive) => pid,
            Some(pane_probe::RecordedProcess::Replaced) => pid + 1_000,
            _ => return Err(RestoreUnverified::ProcessUnreadable),
        };
        Ok(ProcessStart {
            identity: StartIdentity::LinuxTicks(ticks.into()),
            wall_clock: UNIX_EPOCH,
        })
    }
    fn parent(&self, _: u32) -> Result<u32, RestoreUnverified> {
        unreachable!()
    }
    fn environ(&self, _: u32) -> Result<Vec<String>, RestoreUnverified> {
        unreachable!()
    }
    fn exec_path(&self, _: u32) -> Option<String> {
        unreachable!()
    }
}

fn timed(state: HostedState) -> HostedExecution {
    let mut r = record(&owner("root-replaced"), "n1", state);
    let expected = r.expected.as_mut().unwrap();
    expected.root.start = "linux-ticks:100".into();
    expected.provider_process.start = "linux-ticks:101".into();
    r
}

/// The pane as a restarted server restores it: another shell, alone.
fn other_shell(provider: PaneProvider) -> PaneReading {
    let root = ProcessStamp {
        pid: 200,
        start: "linux-ticks:200".into(),
    };
    PaneReading::Present { root, provider }
}

// Another lone shell ends the execution only when its recorded root and provider are both proven
// absent; a live, reused or unreadable one, another pane state or no launch evidence keeps it.
#[test]
fn root_replaced_requires_two_proven_absences() {
    use pane_probe::RecordedProcess::{ExactAlive, ProvenAbsent, Replaced, Unreadable};
    let r = timed(HostedState::Bound);
    let ended = |root, provider, pane: &PaneReading| {
        let os = PidOs::new(root, provider);
        (confirmed_ended(&os, &r, pane), os.1.load(Ordering::SeqCst))
    };
    let restored = other_shell(PaneProvider::Exited);
    assert!(ended(ProvenAbsent, ProvenAbsent, &restored).0);
    for kept in [ExactAlive, Replaced, Unreadable] {
        assert!(!ended(kept, ProvenAbsent, &restored).0, "root {kept:?}");
        assert!(!ended(ProvenAbsent, kept, &restored).0, "provider {kept:?}");
    }
    let provider = r.expected.as_ref().unwrap().provider_process.clone();
    let other_nonce = format!("{:?}", EvidenceGap::OtherNonce("n2".into()));
    for pane in [
        other_shell(PaneProvider::Execution(provider)),
        other_shell(PaneProvider::Unverified(other_nonce)),
        other_shell(PaneProvider::Unverified("NoneYet".into())),
        PaneReading::Unreadable("snapshot from another server".into()),
    ] {
        assert!(!ended(ProvenAbsent, ProvenAbsent, &pane).0, "{pane:?}");
    }
    let mut unproven = r.clone();
    unproven.expected = None;
    let os = PidOs::new(ProvenAbsent, ProvenAbsent);
    assert!(!confirmed_ended(&os, &unproven, &restored));
    // A missing pane or the recorded root alone ends it as before, with no process read.
    let own = PaneReading::Present {
        root: r.expected.as_ref().unwrap().root.clone(),
        provider: PaneProvider::Exited,
    };
    for pane in [PaneReading::Missing, own] {
        assert_eq!(ended(ExactAlive, ExactAlive, &pane), (true, 0), "{pane:?}");
    }
}

// A retired execution whose pane came back as another lone shell is a legacy return's end once
// both recorded processes are proven absent, and not while its root lives.
#[test]
fn legacy_return_root_replaced_both_absent() {
    use pane_probe::RecordedProcess::{ExactAlive, ProvenAbsent};
    let r = timed(HostedState::Retired);
    let restored = || other_shell(PaneProvider::Exited);
    let end = RetiredEnd::from_reads(&r, restored(), &PidOs::new(ProvenAbsent, ProvenAbsent));
    assert!(end.unwrap().matches(&r));
    let alive = RetiredEnd::from_reads(&r, restored(), &PidOs::new(ExactAlive, ProvenAbsent));
    assert_eq!(alive.unwrap_err(), ReturnEndRefusal::PaneUnverified);
}
