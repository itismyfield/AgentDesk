use super::*;
use crate::db::dispatched_sessions::hosted_execution::tests::{owner, record};
use crate::services::session_host::herdr::contract::{HerdrOutcome, Witnessed};
use crate::services::session_host::herdr::model::{HerdrReply, HerdrResult, HerdrSnapshot};
use crate::services::session_host::herdr::provenance::{ProcessStart, StartIdentity};

struct MissingPane;
impl HerdrTransport for MissingPane {
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
        std::path::Path::new("/adk/herdr.sock"),
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
            root: ProcessStamp {
                pid: 200,
                start: "different".into(),
            },
            provider: PaneProvider::Exited,
        },
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
