//! The provenance rule against scripted pane readings and a fake OS.
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use super::*;
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};

const SHELL: u32 = 10;
const PROVIDER: u32 = 20;
const CHILD: u32 = 30;

fn witness(pid: u32) -> ServerWitness {
    ServerWitness::for_test(pid)
}

fn at(seconds: u64) -> ProcessStart {
    ProcessStart {
        identity: StartIdentity::Darwin { seconds, micros: 0 },
        wall_clock: UNIX_EPOCH + Duration::from_secs(seconds),
    }
}

fn listed(pids: &[u32]) -> PaneProcesses {
    from(1, pids)
}

fn from(server: u32, pids: &[u32]) -> PaneProcesses {
    let foreground = ForegroundProcesses::Listed(pids.to_vec());
    (Ok((Some(SHELL), foreground)), Ok(witness(server)))
}

/// Readings in order; the last one repeats. Counts every read.
struct Pane {
    readings: RefCell<VecDeque<PaneProcesses>>,
    reads: Cell<usize>,
}

impl Pane {
    fn new(readings: Vec<PaneProcesses>) -> Self {
        Self {
            readings: RefCell::new(readings.into()),
            reads: Cell::new(0),
        }
    }

    fn read(&self) -> PaneProcesses {
        self.reads.set(self.reads.get() + 1);
        let mut readings = self.readings.borrow_mut();
        if readings.len() > 1 {
            readings.pop_front().unwrap()
        } else {
            readings[0].clone()
        }
    }
}

/// Parents and environments by pid; starts in order per pid, the last one repeating.
pub(crate) struct FakeOs {
    pub(crate) parents: HashMap<u32, u32>,
    starts: Mutex<HashMap<u32, VecDeque<ProcessStart>>>,
    pub(crate) environ: Result<Vec<String>, RestoreUnverified>,
}

impl FakeOs {
    /// Shell 10 runs provider 20, which runs child 30, all started after the launch.
    pub(crate) fn launched(environ: Vec<String>) -> Self {
        let starts = [
            (SHELL, at(1_001)),
            (PROVIDER, at(1_002)),
            (CHILD, at(1_003)),
        ];
        Self {
            parents: HashMap::from([(SHELL, 1), (PROVIDER, SHELL), (CHILD, PROVIDER)]),
            starts: Mutex::new(
                starts
                    .into_iter()
                    .map(|(pid, start)| (pid, VecDeque::from([start])))
                    .collect(),
            ),
            environ: Ok(environ),
        }
    }

    pub(crate) fn starting(self, pid: u32, starts: &[ProcessStart]) -> Self {
        self.starts
            .lock()
            .unwrap()
            .insert(pid, starts.iter().copied().collect());
        self
    }
}

impl ProcessOs for FakeOs {
    fn parent(&self, pid: u32) -> Result<u32, RestoreUnverified> {
        self.parents
            .get(&pid)
            .copied()
            .ok_or(RestoreUnverified::ProcessUnreadable)
    }

    fn start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified> {
        let mut starts = self.starts.lock().unwrap();
        let queue = starts
            .get_mut(&pid)
            .ok_or(RestoreUnverified::ProcessUnreadable)?;
        Ok(if queue.len() > 1 {
            queue.pop_front().unwrap()
        } else {
            queue[0]
        })
    }

    fn environ(&self, _pid: u32) -> Result<Vec<String>, RestoreUnverified> {
        self.environ.clone()
    }

    fn exec_path(&self, _pid: u32) -> Option<String> {
        Some("/opt/claude/2.1.288".into())
    }
}

/// Launch context `nonce`'s file under the test runtime root, as a launch publishes it.
pub(crate) fn context(nonce: &str) -> PathBuf {
    let context = BindingContext {
        schema: 1,
        provider: "claude".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: nonce.into(),
        tmux_session: "AgentDesk-claude-probe".into(),
        channel_id: Some(1),
        owner_runtime_root: "test".into(),
        host: None,
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: None,
    };
    PreparedIncarnation::create(context).unwrap().path
}

pub(crate) const NONCE: &str = "0123456789abcdef0123456789abcdef";
const OTHER: &str = "fedcba9876543210fedcba9876543210";

pub(crate) fn env_naming(path: &Path) -> Vec<String> {
    vec![
        "HOME=/Users/x".into(),
        format!("AGENTDESK_BINDING_CONTEXT={}", path.display()),
    ]
}

fn run(pane: &Pane, os: &FakeOs, window: Duration) -> Result<ExpectedExecution, EvidenceGap> {
    let server = witness(1);
    let request = ProbeRequest {
        provider: "claude",
        nonce: NONCE,
        launched_at: UNIX_EPOCH + Duration::from_secs(1_000),
        witness: &server,
        window,
    };
    probe(&|| pane.read(), os, &request)
}

// The provider's own child shares the foreground group; only the shell's child is the provider.
#[test]
fn probe_takes_the_root_shells_child_from_a_list_with_its_grandchild() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let os = FakeOs::launched(env_naming(&context(NONCE)));
    let pane = Pane::new(vec![listed(&[CHILD, PROVIDER])]);
    let expected = ExpectedExecution {
        binding_provider: "claude".into(),
        binding_nonce: NONCE.into(),
        root: ProcessStamp {
            pid: SHELL,
            start: "darwin:1001.000000".into(),
        },
        provider_process: ProcessStamp {
            pid: PROVIDER,
            start: "darwin:1002.000000".into(),
        },
        provenance: "herdr_launch:ppid+env;exec=/opt/claude/2.1.288".into(),
    };
    assert_eq!(run(&pane, &os, PROVIDER_WINDOW), Ok(expected));
    assert_eq!(
        pane.reads.get(),
        3,
        "candidate, starts and the recheck each read the pane"
    );
}

// The environment must name this execution's own context file, which must name it back.
#[test]
fn probe_refuses_a_context_of_another_nonce() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let own = context(NONCE);
    let other = context(OTHER);
    let cases = [
        other.clone(),
        own.with_file_name("elsewhere.json"),
        PathBuf::from("/tmp").join(own.file_name().unwrap()),
    ];
    for path in cases {
        let os = FakeOs::launched(env_naming(&path));
        let pane = Pane::new(vec![listed(&[PROVIDER])]);
        let result = run(&pane, &os, PROVIDER_WINDOW);
        assert!(
            matches!(result, Err(EvidenceGap::OtherNonce(_))),
            "{path:?}: {result:?}"
        );
    }
    // The own path whose file names another execution.
    std::fs::copy(&other, &own).unwrap();
    let os = FakeOs::launched(env_naming(&own));
    let result = run(&Pane::new(vec![listed(&[PROVIDER])]), &os, PROVIDER_WINDOW);
    assert!(
        matches!(result, Err(EvidenceGap::OtherNonce(_))),
        "{result:?}"
    );
}

#[test]
fn probe_refuses_a_provider_that_sees_herdr_pane_variables() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut environ = env_naming(&context(NONCE));
    environ.push("HERDR_PANE_ID=w1:p1".into());
    let os = FakeOs::launched(environ);
    let result = run(&Pane::new(vec![listed(&[PROVIDER])]), &os, PROVIDER_WINDOW);
    assert_eq!(
        result,
        Err(EvidenceGap::HerdrEnvPresent("HERDR_PANE_ID".into()))
    );
}

// A pid reused between the readings has a new start; the pair is no longer the one checked.
#[test]
fn probe_refuses_a_process_whose_start_changed_between_readings() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let environ = env_naming(&context(NONCE));
    for pid in [PROVIDER, SHELL] {
        let os = FakeOs::launched(environ.clone()).starting(pid, &[at(1_002), at(1_009)]);
        let result = run(&Pane::new(vec![listed(&[PROVIDER])]), &os, PROVIDER_WINDOW);
        assert_eq!(result, Err(EvidenceGap::Replaced), "pid {pid}");
    }
    // The shell or provider gone from the pane at the recheck.
    for last in [
        listed(&[CHILD]),
        (
            Ok((Some(11), ForegroundProcesses::Listed(vec![PROVIDER]))),
            Ok(witness(1)),
        ),
    ] {
        let pane = Pane::new(vec![listed(&[PROVIDER]), listed(&[PROVIDER]), last]);
        let os = FakeOs::launched(environ.clone());
        assert_eq!(run(&pane, &os, PROVIDER_WINDOW), Err(EvidenceGap::Replaced));
    }
}

// Only the shell: read again until the window closes; a provider showing up in time is taken.
#[test]
fn probe_waits_for_the_provider_only_within_its_window() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let environ = env_naming(&context(NONCE));
    let pane = Pane::new(vec![listed(&[SHELL])]);
    let os = FakeOs::launched(environ.clone());
    let window = Duration::from_millis(700);
    let started = Instant::now();
    assert_eq!(run(&pane, &os, window), Err(EvidenceGap::NoneYet));
    assert!(started.elapsed() < window + PROVIDER_POLL * 2);
    assert!(pane.reads.get() >= 3, "read {} times", pane.reads.get());

    let pane = Pane::new(vec![
        listed(&[SHELL]),
        listed(&[SHELL]),
        listed(&[SHELL, PROVIDER]),
    ]);
    let result = run(&pane, &os, window);
    assert_eq!(result.map(|e| e.provider_process.pid), Ok(PROVIDER));
}

// Two children of the shell are never waited out.
#[test]
fn probe_ends_at_the_first_ambiguous_reading() {
    let pane = Pane::new(vec![listed(&[PROVIDER, 40]), listed(&[PROVIDER])]);
    let mut os = FakeOs::launched(Vec::new());
    os.parents.insert(40, SHELL);
    let started = Instant::now();
    assert_eq!(
        run(&pane, &os, PROVIDER_WINDOW),
        Err(EvidenceGap::Ambiguous(vec![PROVIDER, 40]))
    );
    assert_eq!(pane.reads.get(), 1);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn probe_refuses_a_provider_started_before_the_launch() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let environ = env_naming(&context(NONCE));
    let os = FakeOs::launched(environ.clone()).starting(PROVIDER, &[at(998)]);
    let result = run(&Pane::new(vec![listed(&[PROVIDER])]), &os, PROVIDER_WINDOW);
    assert_eq!(result, Err(EvidenceGap::StartedBeforeLaunch));
    // Within the slack a Linux start may read early by.
    let os = FakeOs::launched(environ).starting(PROVIDER, &[at(999)]);
    let result = run(&Pane::new(vec![listed(&[PROVIDER])]), &os, PROVIDER_WINDOW);
    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn probe_refuses_an_unreadable_or_contextless_environment() {
    let pane = || Pane::new(vec![listed(&[PROVIDER])]);
    let mut os = FakeOs::launched(Vec::new());
    assert_eq!(
        run(&pane(), &os, PROVIDER_WINDOW),
        Err(EvidenceGap::EnvUnreadable)
    );
    os.environ = Err(RestoreUnverified::PlatformUnsupported);
    assert_eq!(
        run(&pane(), &os, PROVIDER_WINDOW),
        Err(EvidenceGap::EnvUnreadable)
    );
    os.environ = Ok(vec!["HOME=/Users/x".into(), "HERDR_ENV=1".into()]);
    assert_eq!(
        run(&pane(), &os, PROVIDER_WINDOW),
        Err(EvidenceGap::NonceMissing)
    );
}

// Every reading must come from the server the launch was sent to.
#[test]
fn probe_refuses_readings_from_another_server() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let environ = env_naming(&context(NONCE));
    for at_reading in 0..3 {
        let mut readings = vec![listed(&[PROVIDER]); 3];
        readings[at_reading] = from(2, &[PROVIDER]);
        let pane = Pane::new(readings);
        let os = FakeOs::launched(environ.clone());
        assert_eq!(
            run(&pane, &os, PROVIDER_WINDOW),
            Err(EvidenceGap::ServerChanged),
            "reading {at_reading}"
        );
    }
    let pane = Pane::new(vec![(
        Ok((Some(SHELL), ForegroundProcesses::Listed(vec![PROVIDER]))),
        Err(RestoreUnverified::NoPeer),
    )]);
    let os = FakeOs::launched(environ);
    assert_eq!(
        run(&pane, &os, PROVIDER_WINDOW),
        Err(EvidenceGap::ServerChanged)
    );
}
