//! The provenance rule against scripted pane readings and a fake OS.
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use super::test_support::{CHILD, FakeOs, NONCE, PROVIDER, SHELL, at, context, env_naming};
use super::*;
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};

/// A child the shell's own login startup runs before the launch command.
const STARTUP: u32 = 40;

fn witness(pid: u32) -> ServerWitness {
    ServerWitness::for_test(pid)
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

const OTHER: &str = "fedcba9876543210fedcba9876543210";

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
// A zero window is the input gate's single reading.
#[test]
fn probe_refuses_a_process_whose_start_changed_between_readings() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let environ = env_naming(&context(NONCE));
    for pid in [PROVIDER, SHELL] {
        let os = FakeOs::launched(environ.clone()).starting(pid, &[at(1_002), at(1_009)]);
        let result = run(&Pane::new(vec![listed(&[PROVIDER])]), &os, Duration::ZERO);
        assert_eq!(result, Err(EvidenceGap::Replaced), "pid {pid}");
    }
    // A start that changes at every reading ends the window as replaced.
    let starts: Vec<_> = (1_002..1_040).map(at).collect();
    let os = FakeOs::launched(environ.clone()).starting(PROVIDER, &starts);
    let window = Duration::from_millis(700);
    let result = run(&Pane::new(vec![listed(&[PROVIDER])]), &os, window);
    assert_eq!(result, Err(EvidenceGap::Replaced));
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
        assert_eq!(run(&pane, &os, Duration::ZERO), Err(EvidenceGap::Replaced));
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

// With no window, as the input gate reads, two children of the shell end the first reading.
#[test]
fn probe_ends_at_the_first_ambiguous_reading() {
    let pane = Pane::new(vec![listed(&[PROVIDER, 40]), listed(&[PROVIDER])]);
    let mut os = FakeOs::launched(Vec::new());
    os.parents.insert(40, SHELL);
    let started = Instant::now();
    assert_eq!(
        run(&pane, &os, Duration::ZERO),
        Err(EvidenceGap::Ambiguous(vec![PROVIDER, 40]))
    );
    assert_eq!(pane.reads.get(), 1);
    assert!(started.elapsed() < Duration::from_secs(1));
}

// A login-startup pipeline runs two children of the shell at once; the provider after it is
// taken, and a pair that outlasts the window ends it as the same ambiguity.
#[test]
fn probe_waits_out_a_startup_pipeline_within_its_window() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pipeline = [STARTUP, STARTUP + 1];
    let mut os = FakeOs::launched(env_naming(&context(NONCE)));
    for pid in pipeline {
        os.parents.insert(pid, SHELL);
    }
    let pane = Pane::new(vec![listed(&pipeline), listed(&[PROVIDER])]);
    let result = run(&pane, &os, PROVIDER_WINDOW);
    assert_eq!(result.map(|e| e.provider_process.pid), Ok(PROVIDER));

    let window = Duration::from_millis(700);
    let pane = Pane::new(vec![listed(&pipeline)]);
    let started = Instant::now();
    assert_eq!(
        run(&pane, &os, window),
        Err(EvidenceGap::Ambiguous(pipeline.to_vec()))
    );
    assert!(started.elapsed() < window + PROVIDER_POLL * 2);
    assert!(pane.reads.get() > 1, "read {} times", pane.reads.get());
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

// A shell-startup child shows the same gaps, so each is read again until the window closes.
#[test]
fn probe_refuses_an_unreadable_or_contextless_environment() {
    let window = Duration::from_millis(700);
    let refused = |os: &FakeOs, gap: EvidenceGap| {
        let pane = Pane::new(vec![listed(&[PROVIDER])]);
        let started = Instant::now();
        assert_eq!(run(&pane, os, window), Err(gap));
        assert!(started.elapsed() < window + PROVIDER_POLL * 2);
        assert!(pane.reads.get() > 3, "read {} times", pane.reads.get());
    };
    let mut os = FakeOs::launched(Vec::new());
    refused(&os, EvidenceGap::EnvUnreadable);
    os.environ = Err(RestoreUnverified::PlatformUnsupported);
    refused(&os, EvidenceGap::EnvUnreadable);
    os.environ = Ok(vec!["HOME=/Users/x".into(), "HERDR_ENV=1".into()]);
    refused(&os, EvidenceGap::NonceMissing);
}

// Login startup runs root-shell children before the typed launch command: hidden environment,
// no launch context, or exited between readings. The provider that follows is still taken.
#[test]
fn probe_reads_past_shell_startup_children_to_the_provider() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let environ = env_naming(&context(NONCE));
    let pane_env = vec!["HOME=/Users/x".into(), "HERDR_PANE_ID=w1:p1".into()];
    let gone_early = STARTUP + 1;
    let cases = [
        ("hidden environment", Vec::new(), listed(&[STARTUP])),
        ("no binding context", pane_env, listed(&[STARTUP])),
        ("exited before the recheck", Vec::new(), listed(&[])),
    ];
    for (case, startup_env, second) in cases {
        let os = FakeOs::launched(environ.clone()).startup_child(STARTUP, startup_env);
        let pane = Pane::new(vec![listed(&[STARTUP]), second, listed(&[PROVIDER])]);
        let result = run(&pane, &os, PROVIDER_WINDOW);
        assert_eq!(
            result.map(|e| e.provider_process.pid),
            Ok(PROVIDER),
            "{case}"
        );
    }
    // Exited before its start was read.
    let mut os = FakeOs::launched(environ);
    os.parents.insert(gone_early, SHELL);
    let pane = Pane::new(vec![
        listed(&[gone_early]),
        listed(&[gone_early]),
        listed(&[PROVIDER]),
    ]);
    let result = run(&pane, &os, PROVIDER_WINDOW);
    assert_eq!(result.map(|e| e.provider_process.pid), Ok(PROVIDER));
}

// The shell's child execs the provider in place: same pid and start, a new environment.
#[test]
fn probe_takes_the_provider_its_launch_command_execs_into() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let environ = env_naming(&context(NONCE));
    let shell_env = vec!["HOME=/Users/x".into(), "HERDR_PANE_ID=w1:p1".into()];
    for before in [Vec::new(), shell_env] {
        let os = FakeOs::launched(Vec::new()).reading(PROVIDER, &[before, environ.clone()]);
        let pane = Pane::new(vec![listed(&[PROVIDER])]);
        let result = run(&pane, &os, PROVIDER_WINDOW);
        let provider = ProcessStamp {
            pid: PROVIDER,
            start: "darwin:1002.000000".into(),
        };
        assert_eq!(result.map(|e| e.provider_process), Ok(provider));
        assert_eq!(
            pane.reads.get(),
            5,
            "a refused reading, then the full proof"
        );
    }
}

// A refusal no startup child can cause ends the probe at its first reading.
#[test]
fn probe_ends_a_refusal_no_startup_child_causes_at_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let own = env_naming(&context(NONCE));
    let mut herdr = own.clone();
    herdr.push("HERDR_PANE_ID=w1:p1".into());
    let cases = [
        (FakeOs::launched(env_naming(&context(OTHER))), "other nonce"),
        (FakeOs::launched(herdr), "herdr variable"),
        (
            FakeOs::launched(own).starting(PROVIDER, &[at(998)]),
            "started before the launch",
        ),
    ];
    for (os, case) in cases {
        let pane = Pane::new(vec![listed(&[PROVIDER])]);
        let started = Instant::now();
        assert!(run(&pane, &os, PROVIDER_WINDOW).is_err(), "{case}");
        assert!(
            pane.reads.get() <= 2,
            "{case}: read {} times",
            pane.reads.get()
        );
        assert!(started.elapsed() < Duration::from_secs(1), "{case}");
    }
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
