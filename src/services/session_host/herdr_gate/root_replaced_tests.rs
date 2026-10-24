use super::*;
use crate::services::session_host::herdr::contract::{HerdrOutcome, Witnessed};
use crate::services::session_host::herdr::model::HerdrResult;

const RESTORED_SHELL: u32 = 40;
const RESTORED_PROVIDER: u32 = 50;

#[derive(Clone, Copy, Debug)]
enum OriginalProcess {
    Absent,
    ExactAlive,
    Replaced,
    ExistsUnreadable,
    StartUnreadable,
}

struct ProofOs {
    inner: FakeOs,
    original: [OriginalProcess; 2],
    existence_reads: Mutex<Vec<u32>>,
}

impl ProofOs {
    fn new(rig: &Rig, original: [OriginalProcess; 2]) -> Arc<Self> {
        let mut inner = FakeOs::launched(env_naming(&rig.context))
            .starting(RESTORED_SHELL, &[start(2_000)])
            .starting(RESTORED_PROVIDER, &[start(2_001)]);
        inner.parents.insert(RESTORED_SHELL, 1);
        inner.parents.insert(RESTORED_PROVIDER, RESTORED_SHELL);
        Arc::new(Self {
            inner,
            original,
            existence_reads: Mutex::default(),
        })
    }

    fn original(&self, pid: u32) -> Option<OriginalProcess> {
        match pid {
            SHELL => Some(self.original[0]),
            PROVIDER => Some(self.original[1]),
            _ => None,
        }
    }

    fn reads(&self) -> Vec<u32> {
        self.existence_reads.lock().unwrap().clone()
    }
}

impl ProcessOs for ProofOs {
    fn exists(&self, pid: u32) -> Result<bool, RestoreUnverified> {
        self.existence_reads.lock().unwrap().push(pid);
        match self
            .original(pid)
            .expect("only original identities need absence evidence")
        {
            OriginalProcess::Absent => Ok(false),
            OriginalProcess::ExistsUnreadable => Err(RestoreUnverified::ProcessUnreadable),
            _ => Ok(true),
        }
    }

    fn start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified> {
        match self.original(pid) {
            Some(OriginalProcess::Replaced) => Ok(start(9_000)),
            Some(OriginalProcess::StartUnreadable) => Err(RestoreUnverified::ProcessUnreadable),
            _ => self.inner.start(pid),
        }
    }

    fn parent(&self, pid: u32) -> Result<u32, RestoreUnverified> {
        self.inner.parent(pid)
    }

    fn environ(&self, pid: u32) -> Result<Vec<String>, RestoreUnverified> {
        self.inner.environ(pid)
    }

    fn exec_path(&self, pid: u32) -> Option<String> {
        self.inner.exec_path(pid)
    }
}

/// Keeps the socket and witnessed reply path while simulating a complete missing-pane snapshot.
struct MissingPane(Arc<dyn HerdrTransport>);

impl HerdrTransport for MissingPane {
    fn call(&self, call: &HerdrCall) -> (HerdrOutcome, Witnessed) {
        assert!(matches!(call.request, HerdrRequest::SessionSnapshot {}));
        let (mut outcome, witness) = self.0.call(call);
        let Ok(reply) = &mut outcome else {
            panic!("socket snapshot failed")
        };
        let Ok(HerdrResult::SessionSnapshot { snapshot }) = &mut reply.body else {
            panic!("socket returned a non-snapshot")
        };
        snapshot.panes.clear();
        (outcome, witness)
    }

    fn call_with_witness(&self, _: &HerdrCall, _: &ServerWitness) -> HerdrOutcome {
        panic!("replaced-root proof must never write to Herdr")
    }

    fn server_witness(&self) -> Witnessed {
        self.0.server_witness()
    }
}

fn pane(rig: &Rig, foreground: u32) {
    *rig.server.process_info.lock().unwrap() = Some(json!({
        "result": {"type": "pane_process_info", "process_info": {
            "pane_id": PANE, "shell_pid": RESTORED_SHELL,
            "foreground_process_group_id": foreground,
            "foreground_processes": [{"pid": foreground}]
        }}
    }));
}

fn target(
    rig: &Rig,
    os: Arc<ProofOs>,
    peers: Vec<u32>,
    restore: Restore,
    missing: bool,
) -> HerdrTarget {
    let transport = transport(&rig.server, peers);
    let transport = if missing {
        Arc::new(MissingPane(transport)) as Arc<dyn HerdrTransport>
    } else {
        transport
    };
    HerdrTarget::new(
        endpoint(&rig.server),
        transport,
        &stored(&rig.server, HostedState::Bound),
    )
    .unwrap()
    .with_reads(restore, os)
}

fn absent(rig: &Rig) -> Arc<ProofOs> {
    ProofOs::new(rig, [OriginalProcess::Absent; 2])
}

fn normal_control(rig: &Rig, missing: bool) {
    pane(rig, RESTORED_SHELL);
    let os = absent(rig);
    let target = target(rig, os.clone(), vec![SERVER], off_where_dialled, missing);
    let row = stored(&rig.server, HostedState::Bound);
    let first = target.replaced_root_end(&row, None).unwrap();
    assert_eq!(os.reads(), [SHELL, PROVIDER]);
    assert!(target.replaced_root_end(&row, Some(&first)).is_ok());
    assert_eq!(os.reads(), [SHELL, PROVIDER, SHELL, PROVIDER]);
    assert!(rig.server.mutations().is_empty());
}

#[test]
fn replaced_root_end_requires_two_exact_absence_reads_for_shell_and_missing_pane() {
    let rig = rig();
    for missing in [false, true] {
        normal_control(&rig, missing);
    }
}

#[test]
fn replaced_root_end_refuses_every_nonabsent_original_root() {
    let rig = rig();
    for state in [
        OriginalProcess::ExactAlive,
        OriginalProcess::Replaced,
        OriginalProcess::ExistsUnreadable,
        OriginalProcess::StartUnreadable,
    ] {
        for missing in [false, true] {
            normal_control(&rig, missing);
            let os = ProofOs::new(&rig, [state, OriginalProcess::Absent]);
            let target = target(&rig, os.clone(), vec![SERVER], off_where_dialled, missing);
            let error = target
                .replaced_root_end(&stored(&rig.server, HostedState::Bound), None)
                .unwrap_err();
            assert!(
                error.contains("original root is not proven absent"),
                "{state:?}: {error}"
            );
            assert_eq!(os.reads(), [SHELL], "{state:?}");
        }
    }
    assert!(rig.server.mutations().is_empty());
}

#[test]
fn replaced_root_end_refuses_every_nonabsent_original_provider() {
    let rig = rig();
    for state in [
        OriginalProcess::ExactAlive,
        OriginalProcess::Replaced,
        OriginalProcess::ExistsUnreadable,
        OriginalProcess::StartUnreadable,
    ] {
        for missing in [false, true] {
            normal_control(&rig, missing);
            let os = ProofOs::new(&rig, [OriginalProcess::Absent, state]);
            let target = target(&rig, os.clone(), vec![SERVER], off_where_dialled, missing);
            let error = target
                .replaced_root_end(&stored(&rig.server, HostedState::Bound), None)
                .unwrap_err();
            assert!(
                error.contains("original provider is not proven absent"),
                "{state:?}: {error}"
            );
            assert_eq!(os.reads(), [SHELL, PROVIDER], "{state:?}");
        }
    }
    assert!(rig.server.mutations().is_empty());
}

#[test]
fn replaced_root_end_refuses_new_provider_and_unverified_pane() {
    let rig = rig();
    for verified in [true, false] {
        normal_control(&rig, false);
        pane(&rig, RESTORED_PROVIDER);
        let mut os = absent(&rig);
        if !verified {
            Arc::get_mut(&mut os).unwrap().inner.environ =
                Err(RestoreUnverified::ProcessUnreadable);
        }
        let row = stored(&rig.server, HostedState::Bound);
        let reading = HerdrPaneView::new(
            &endpoint(&rig.server),
            transport(&rig.server, vec![SERVER]),
            &row,
        )
        .unwrap()
        .with_os(os.clone())
        .read_execution();
        assert!(
            matches!(
                (verified, &reading),
                (
                    true,
                    PaneReading::Present {
                        provider: PaneProvider::Execution(_),
                        ..
                    }
                ) | (
                    false,
                    PaneReading::Present {
                        provider: PaneProvider::Unverified(_),
                        ..
                    }
                )
            ),
            "fixture must reach the intended provider observation: {reading:?}"
        );
        let target = target(&rig, os.clone(), vec![SERVER], off_where_dialled, false);
        assert!(
            target
                .replaced_root_end(&row, None)
                .unwrap_err()
                .contains("replaced pane is not a lone shell")
        );
        assert!(os.reads().is_empty());
    }
    assert!(rig.server.mutations().is_empty());
}

fn restore_on(_: &dyn HerdrTransport, _: &HerdrEndpoint) -> RestoreResume {
    RestoreResume::On
}

fn restore_unknown_version(_: &dyn HerdrTransport, _: &HerdrEndpoint) -> RestoreResume {
    RestoreResume::Unverified(RestoreUnverified::VersionNotVerified)
}

#[test]
fn replaced_root_end_requires_verified_restore_off_before_pane_reads() {
    let rig = rig();
    for restore in [
        restore_on as Restore,
        not_canonical,
        restore_unknown_version,
        off_elsewhere,
    ] {
        normal_control(&rig, false);
        let os = absent(&rig);
        let target = target(&rig, os.clone(), vec![SERVER], restore, false);
        assert!(
            target
                .replaced_root_end(&stored(&rig.server, HostedState::Bound), None)
                .is_err()
        );
        assert!(os.reads().is_empty());
    }
    assert!(rig.server.mutations().is_empty());
}

#[test]
fn replaced_root_end_refuses_mutated_stored_launch_evidence() {
    let rig = rig();
    for changed in 0..5 {
        normal_control(&rig, false);
        let os = absent(&rig);
        let target = target(&rig, os.clone(), vec![SERVER], off_where_dialled, false);
        let mut row = stored(&rig.server, HostedState::Bound);
        match changed {
            0 => row.state = HostedState::Retired,
            1 => row
                .expected
                .as_mut()
                .unwrap()
                .root
                .start
                .push_str("-changed"),
            2 => row
                .expected
                .as_mut()
                .unwrap()
                .provider_process
                .start
                .push_str("-changed"),
            3 => row.execution_nonce.push_str("-changed"),
            4 => row.expected = None,
            _ => unreachable!(),
        }
        assert!(
            target
                .replaced_root_end(&row, None)
                .unwrap_err()
                .contains("stored launch evidence differs")
        );
        assert!(os.reads().is_empty());
    }
    assert!(rig.server.mutations().is_empty());
}

#[test]
fn replaced_root_end_requires_a_replaced_lone_shell() {
    let rig = rig();
    normal_control(&rig, false);
    *rig.server.process_info.lock().unwrap() = Some(json!({
        "result": {"type": "pane_process_info", "process_info": {
            "pane_id": PANE, "shell_pid": SHELL,
            "foreground_process_group_id": SHELL,
            "foreground_processes": [{"pid": SHELL}]
        }}
    }));
    let os = absent(&rig);
    let target = target(&rig, os.clone(), vec![SERVER], off_where_dialled, false);
    assert!(
        target
            .replaced_root_end(&stored(&rig.server, HostedState::Bound), None)
            .unwrap_err()
            .contains("replaced pane is not a lone shell")
    );
    assert!(os.reads().is_empty());
    assert!(rig.server.mutations().is_empty());
}

#[test]
fn replaced_root_end_rejects_changed_snapshot_process_and_final_witness() {
    let rig = rig();
    for (peers, expected, reads) in [
        (vec![SERVER, 99], "snapshot from another server", vec![]),
        (
            vec![SERVER, SERVER, 99],
            "processes from another server",
            vec![],
        ),
        (
            vec![SERVER, SERVER, SERVER, 99],
            "server changed after process observations",
            vec![SHELL, PROVIDER],
        ),
    ] {
        normal_control(&rig, false);
        let os = absent(&rig);
        let target = target(&rig, os.clone(), peers, off_where_dialled, false);
        let error = target
            .replaced_root_end(&stored(&rig.server, HostedState::Bound), None)
            .unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert_eq!(os.reads(), reads);
    }
    assert!(rig.server.mutations().is_empty());
}

#[test]
fn replaced_root_end_second_proof_requires_the_first_execution_and_server() {
    let rig = rig();
    for changed_server in [false, true] {
        normal_control(&rig, false);
        let os = absent(&rig);
        let peers = if changed_server {
            vec![SERVER, SERVER, SERVER, SERVER, 99]
        } else {
            vec![SERVER]
        };
        let target = target(&rig, os.clone(), peers, off_where_dialled, false);
        let mut row = stored(&rig.server, HostedState::Bound);
        let first = target.replaced_root_end(&row, None).unwrap();
        assert_eq!(os.reads(), [SHELL, PROVIDER]);
        if !changed_server {
            row.owner.runtime_root.push_str("-changed");
        }
        let error = target.replaced_root_end(&row, Some(&first)).unwrap_err();
        assert!(
            error.contains("execution or server changed between proofs"),
            "{error}"
        );
        assert_eq!(os.reads(), [SHELL, PROVIDER]);
    }
    assert!(rig.server.mutations().is_empty());
}
