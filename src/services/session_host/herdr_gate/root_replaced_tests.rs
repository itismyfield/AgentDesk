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
    boot: Option<StartIdentity>,
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
            boot: None,
        })
    }

    /// As `new` on a host that booted at `boot` seconds (`None`: boot time unread).
    fn booted(rig: &Rig, original: [OriginalProcess; 2], boot: Option<u64>) -> Arc<Self> {
        let mut os = Arc::into_inner(Self::new(rig, original)).unwrap();
        os.boot = boot.map(|seconds| StartIdentity::Darwin { seconds, micros: 0 });
        Arc::new(os)
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

    fn boot_time(&self) -> Option<StartIdentity> {
        self.boot
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

/// A user_default target whose nonce's grant admitted server `granted`; the server answering now
/// is `SERVER`, with startup proof.
fn granted_target(rig: &Rig, os: Arc<ProofOs>, granted: u32, missing: bool) -> HerdrTarget {
    use crate::services::session_host::herdr::policy::permit::{
        ExistingMutationEvidence, grant_for_test, startup_for_test,
    };
    let transport = transport(&rig.server, vec![SERVER]);
    let transport = if missing {
        Arc::new(MissingPane(transport)) as Arc<dyn HerdrTransport>
    } else {
        transport
    };
    let configured =
        endpoint(&rig.server).with_mode(crate::config::session_hosts::EndpointMode::UserDefault);
    let launch = crate::services::herdr_launch::HerdrLaunchEndpoint {
        execution_node: configured.execution_node().into(),
        config_key: configured.config_key().into(),
        socket_addr: rig.server.path.to_string_lossy().into_owned(),
        herdr_session: configured.herdr_session().into(),
    };
    let server = |pid| ServerWitness {
        socket: rig.server.path.clone(),
        pid,
        start: start(1_000).identity,
    };
    let evidence = ExistingMutationEvidence {
        startup: Some(startup_for_test(server(SERVER))),
        grant: Some(grant_for_test(&launch, NONCE, server(granted))),
    };
    let stored = stored(&rig.server, HostedState::Bound);
    HerdrTarget::new_with_evidence(configured, transport, &stored, evidence)
        .unwrap()
        .with_reads(off_where_dialled, os)
}

/// The lone shell a restored pane shows in place of the recorded root.
fn restored_shell() -> PaneReading {
    PaneReading::Present {
        root: ProcessStamp {
            pid: RESTORED_SHELL,
            start: "darwin:2000.000000".into(),
        },
        provider: PaneProvider::Exited,
    }
}

const OLD_SERVER: u32 = 99;
/// A boot after both recorded starts (1_001 and 1_002).
const BOOTED_AFTER: u64 = 5_000;

#[test]
fn a_replaced_server_takes_an_original_pid_reused_since_the_boot_as_gone() {
    use OriginalProcess::{Absent, Replaced};
    let rig = rig();
    pane(&rig, RESTORED_SHELL);
    let row = stored(&rig.server, HostedState::Bound);
    for original in [[Replaced, Absent], [Absent, Replaced], [Replaced, Replaced]] {
        for missing in [false, true] {
            let os = ProofOs::booted(&rig, original, Some(BOOTED_AFTER));
            let target = granted_target(&rig, os, OLD_SERVER, missing);
            let first = target.replaced_root_end(&row, None);
            let first = first.unwrap_or_else(|e| panic!("{original:?} missing={missing}: {e}"));
            assert!(target.replaced_root_end(&row, Some(&first)).is_ok());
            // The row-end read of the same execution reaches the same answer.
            assert!(
                target.ended_reading(&row, &restored_shell()),
                "{original:?}"
            );
            assert!(target.ended_reading(&row, &PaneReading::Missing));
        }
    }
    assert!(rig.server.mutations().is_empty());
}

#[test]
fn a_reused_original_pid_is_alive_unless_the_server_was_replaced_and_it_predates_the_boot() {
    use OriginalProcess::{Absent, ExactAlive, ExistsUnreadable, Replaced, StartUnreadable};
    let rig = rig();
    pane(&rig, RESTORED_SHELL);
    let row = stored(&rig.server, HostedState::Bound);
    let cases = [
        ("the granted server", SERVER, Replaced, Some(BOOTED_AFTER)),
        (
            "a boot at the recorded start",
            OLD_SERVER,
            Replaced,
            Some(1_001),
        ),
        ("an unread boot", OLD_SERVER, Replaced, None),
        (
            "a live original",
            OLD_SERVER,
            ExactAlive,
            Some(BOOTED_AFTER),
        ),
        (
            "an unread pid",
            OLD_SERVER,
            ExistsUnreadable,
            Some(BOOTED_AFTER),
        ),
        (
            "an unread start",
            OLD_SERVER,
            StartUnreadable,
            Some(BOOTED_AFTER),
        ),
    ];
    for (case, granted, root, boot) in cases {
        let os = ProofOs::booted(&rig, [root, Absent], boot);
        let target = granted_target(&rig, os, granted, false);
        let error = target.replaced_root_end(&row, None).unwrap_err();
        assert!(
            error.contains("original root is not proven absent"),
            "{case}: {error}"
        );
        assert!(!target.ended_reading(&row, &restored_shell()), "{case}");
        // A pane that is gone needs no process read, on any server.
        assert!(target.ended_reading(&row, &PaneReading::Missing), "{case}");
    }
    assert!(rig.server.mutations().is_empty());
}
