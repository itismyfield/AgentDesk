use std::collections::BTreeMap;

use super::*;
use crate::services::campaign_execution::adk_resources::{ResourceFact, resource};
use crate::services::campaign_execution::model::{Action, ActionState};
use crate::services::campaign_execution::profile::{
    Observation, PlannedTest, ResourceKind, TestOutcome,
};
use crate::services::campaign_execution::verification_run::tests as run_tests;
use crate::services::campaign_execution::verification_run::{
    PhaseInput, SelectionRule, VerificationCollection, VerificationRun, VerificationSpec, digest_of,
};
#[cfg(unix)]
use crate::services::campaign_execution::verification_run::{Slot, VerificationError};

fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-29T09:00:00Z")
        .unwrap()
        .to_utc()
}

fn sh(script: &str, cwd: &Path) -> CommandSpec {
    CommandSpec {
        program: "sh".into(),
        argv: vec!["-c".into(), script.into()],
        cwd: cwd.to_string_lossy().into_owned(),
        env_identity: BTreeMap::new(),
    }
}

fn bound(command: CommandSpec) -> BoundInput {
    BoundInput::new(PhaseInput {
        subject_sha: "h1".into(),
        checkout_path: command.cwd.clone(),
        command,
        predecessors: BTreeMap::new(),
        preflight: vec![],
        outputs: vec![],
        resources: vec![],
    })
    .unwrap()
}

fn ctx<'a>(log_dir: &'a Path, handoff: Option<TestHandoff>) -> LaunchContext<'a> {
    LaunchContext {
        check_id: "lib-tests",
        log_dir,
        host: "host",
        now: at(),
        handoff,
        nonce: uuid::Uuid::new_v4().to_string(),
    }
}

fn observed(kind: ResourceKind, identity: &str, available: bool) -> ResourceObservation {
    let resource = resource(kind, "host", identity);
    let observation = if available {
        Observation::Observed {
            evidence: ResourceFact {
                identity: identity.into(),
                path: None,
                version: None,
            },
            observed_at: at(),
        }
    } else {
        Observation::NotYet {
            reason: "token timeout 75".into(),
            observed_at: at(),
        }
    };
    ResourceObservation {
        resource,
        observation,
    }
}

fn started(launch: Launch) -> RunningPhase {
    match launch {
        Launch::Started(phase) => *phase,
        Launch::NotSent(initiation) => panic!("expected a start, got {initiation:?}"),
    }
}

fn finished(settled: Settled) -> PhaseResult {
    match settled {
        Settled::Finished(result) => *result,
        Settled::Lingering { evidence, .. } => panic!("expected a finished phase: {evidence}"),
    }
}

fn expected(name: &str, path: &Path) -> ExpectedArtifact {
    ExpectedArtifact {
        name: name.into(),
        path: path.to_string_lossy().into_owned(),
    }
}

fn identity() -> AdkBuildIdentity {
    AdkBuildIdentity {
        rust_version: "1.94.1".into(),
        nextest_version: "0.9.100".into(),
        target_triple: "aarch64-apple-darwin".into(),
        run_id: "run:v-1".into(),
    }
}

fn policy() -> CommandPolicy {
    CommandPolicy {
        nextest_profile: "ci".into(),
        no_tests_fail: true,
        workers: 1,
    }
}

#[test]
fn resource_wait_and_preflight_mismatch_leave_the_phase_unspawned() {
    let temp = tempfile::tempdir().unwrap();
    let logs = temp.path().join("logs");
    let mut input = bound(sh("echo should-not-run > ran.txt", temp.path()));
    input
        .input
        .resources
        .push(resource(ResourceKind::Build, "host", "token"));
    let waiting = [observed(ResourceKind::Build, "token", false)];
    let waited = launch(
        PhaseKind::Archive,
        "archive",
        &input,
        &waiting,
        ctx(&logs, None),
    )
    .unwrap();
    assert!(matches!(
        waited,
        Launch::NotSent(PhaseInitiation::NotSent {
            reason: NotSentReason::ResourceWait { .. },
            ..
        })
    ));
    assert!(matches!(
        launch(PhaseKind::Archive, "archive", &input, &[], ctx(&logs, None)).unwrap(),
        Launch::NotSent(PhaseInitiation::NotSent {
            reason: NotSentReason::ResourceWait { .. },
            ..
        })
    ));
    assert!(!temp.path().join("ran.txt").exists() && !logs.exists());
    let archive = temp.path().join("archive.tar");
    std::fs::write(&archive, b"compiled").unwrap();
    input.input.preflight.push(ArtifactRef {
        name: "archive".into(),
        path: archive.to_string_lossy().into_owned(),
        digest: file_digest(&archive).unwrap(),
    });
    let ready = [observed(ResourceKind::Build, "token", true)];
    std::fs::write(&archive, b"rebuilt elsewhere").unwrap();
    assert!(matches!(
        launch(PhaseKind::Test, "test", &input, &ready, ctx(&logs, None)).unwrap(),
        Launch::NotSent(PhaseInitiation::NotSent {
            reason: NotSentReason::PreflightMismatch { artifact },
            ..
        }) if artifact == "archive"
    ));
    assert!(!temp.path().join("ran.txt").exists());
    std::fs::write(&archive, b"compiled").unwrap();
    let running =
        started(launch(PhaseKind::Test, "test", &input, &ready, ctx(&logs, None)).unwrap());
    let result = finished(running.wait(Duration::from_secs(10), at()));
    assert_eq!(result.record.exit_code, Some(0));
    assert!(temp.path().join("ran.txt").exists());
}

#[test]
fn started_phase_has_a_receipt_and_its_exit_is_recorded_verbatim() {
    let temp = tempfile::tempdir().unwrap();
    let logs = temp.path().join("logs");
    let input = bound(sh("echo out; echo err 1>&2; exit 3", temp.path()));
    let running =
        started(launch(PhaseKind::Archive, "archive", &input, &[], ctx(&logs, None)).unwrap());
    let PhaseInitiation::Started { receipt, process } = running.initiation() else {
        panic!("started phase must carry a receipt");
    };
    assert!(receipt.starts_with(&format!("archive:{}:", process.pid)));
    assert_eq!(process.host, "host");
    assert_eq!(process.process_group, cfg!(unix).then_some(process.pid));
    let result = finished(running.wait(Duration::from_secs(10), at()));
    assert_eq!(result.termination, Termination::Exited);
    assert_eq!(result.record.exit_code, Some(3));
    assert_eq!(result.record.phase_id, "archive");
    assert_eq!(result.receipt, receipt);
    assert_eq!(
        std::fs::read_to_string(&result.record.stdout_ref).unwrap(),
        "out\n"
    );
    assert_eq!(
        std::fs::read_to_string(&result.record.stderr_ref).unwrap(),
        "err\n"
    );
    assert_eq!(result.output, PhaseOutput::None);
    let second =
        started(launch(PhaseKind::Archive, "archive", &input, &[], ctx(&logs, None)).unwrap());
    assert_ne!(
        second.initiation(),
        PhaseInitiation::Started { receipt, process }
    );
    finished(second.wait(Duration::from_secs(10), at()));
}

#[cfg(unix)]
#[test]
fn interrupt_ends_the_whole_process_group_before_reporting() {
    let temp = tempfile::tempdir().unwrap();
    let logs = temp.path().join("logs");
    let input = bound(sh("sleep 30 & sleep 30 & wait", temp.path()));
    let mut running =
        started(launch(PhaseKind::Archive, "archive", &input, &[], ctx(&logs, None)).unwrap());
    let process = running.process().clone();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(process_tree_state(&process), TreeState::Alive);
    assert!(running.poll().unwrap().is_none());
    let result = finished(running.interrupt("immediate stop".into(), at()));
    assert!(
        matches!(result.termination, Termination::Interrupted { ref evidence } if evidence == "immediate stop")
    );
    assert_eq!(process_tree_state(&process), TreeState::Gone);
    assert!(result.record.exit_code.is_none() || result.record.exit_code != Some(0));
}

#[cfg(unix)]
#[test]
fn wait_ends_only_when_the_whole_process_group_is_gone() {
    let temp = tempfile::tempdir().unwrap();
    let logs = temp.path().join("logs");
    // The leader exits at once and leaves a child behind in its group.
    let input = bound(sh("sleep 1 & exit 0", temp.path()));
    let running =
        started(launch(PhaseKind::Archive, "archive", &input, &[], ctx(&logs, None)).unwrap());
    let process = running.process().clone();
    let clock = Instant::now();
    let result = finished(running.wait(Duration::from_secs(10), at()));
    assert_eq!(result.record.exit_code, Some(0));
    assert!(
        clock.elapsed() >= Duration::from_millis(800),
        "{:?}",
        clock.elapsed()
    );
    assert_eq!(process_tree_state(&process), TreeState::Gone);
    // At the deadline the straggler is ended, and the result still carries the leader's exit.
    let input = bound(sh("sleep 30 & exit 0", temp.path()));
    let running =
        started(launch(PhaseKind::Archive, "archive", &input, &[], ctx(&logs, None)).unwrap());
    let process = running.process().clone();
    let clock = Instant::now();
    let result = finished(running.wait(Duration::from_millis(500), at()));
    assert_eq!(result.record.exit_code, Some(0));
    assert!(clock.elapsed() < Duration::from_secs(10));
    assert_eq!(process_tree_state(&process), TreeState::Gone);
}

#[test]
fn list_and_test_outputs_are_parsed_from_the_artifacts_the_phase_wrote() {
    let temp = tempfile::Builder::new()
        .prefix("verification artifacts ' ")
        .tempdir()
        .unwrap();
    let logs = temp.path().join("logs");
    let universe = temp.path().join("universe.json");
    // List prints the universe; the runner hands its stdout to the declared artifact.
    let mut input = bound(sh(
        "printf '%s' '{\"rust-suites\":{\"agentdesk\":{\"binary-id\":\"agentdesk\",\"testcases\":{\"a::new_test\":{\"ignored\":false},\"a::old\":{\"ignored\":true}}}}}'",
        temp.path(),
    ));
    input.input.outputs.push(expected("universe", &universe));
    let result = finished(
        started(launch(PhaseKind::List, "list", &input, &[], ctx(&logs, None)).unwrap())
            .wait(Duration::from_secs(10), at()),
    );
    assert_eq!(result.artifacts.len(), 1);
    assert!(universe.is_file());
    assert_eq!(
        result.output,
        PhaseOutput::Universe {
            tests: vec![
                ListedTest {
                    binary_id: "agentdesk".into(),
                    name: "a::new_test".into(),
                    ignored: false
                },
                ListedTest {
                    binary_id: "agentdesk".into(),
                    name: "a::old".into(),
                    ignored: true
                },
            ]
        }
    );
    assert!(
        parse_universe(
            r#"{"rust-suites":{"s":{"binary-id":"b","testcases":{"t":{"ignored":true}}}}}"#
        )
        .is_err()
    );
    let junit = temp.path().join("junit.xml");
    let xml = concat!(
        "<?xml version=\"1.0\"?><testsuites><testsuite name=\"agentdesk\" tests=\"3\">",
        "<testcase name=\"a::passes\" classname=\"agentdesk\" time=\"0.1\"/>",
        "<testcase name=\"a::fails\" classname=\"agentdesk\"><failure message=\"boom &lt;x&gt;\">trace</failure></testcase>",
        "<testcase name=\"a::skipped\" classname=\"agentdesk\"><skipped/></testcase>",
        "</testsuite></testsuites>"
    );
    let mut input = bound(sh(
        &format!("printf '%s' '{xml}' > junit.xml; exit 100"),
        temp.path(),
    ));
    input.input.outputs.push(expected("junit", &junit));
    let running = started(launch(PhaseKind::Test, "test", &input, &[], ctx(&logs, None)).unwrap());
    let (failed_pid, failed_nonce) = (running.process().pid, running.process().nonce.clone());
    let failed = finished(running.wait(Duration::from_secs(10), at()));
    assert_eq!(failed.record.exit_code, Some(100));
    assert_eq!(failed.record.phase_id, "test");
    let PhaseOutput::Tests { executed } = &failed.output else {
        panic!("nextest's test-failure exit still yields the raw JUnit results");
    };
    assert_eq!(executed.len(), 3);
    assert!(executed.iter().all(|t| t.check_id == "lib-tests"));
    assert_eq!(executed[0].id, "agentdesk a::passes");
    assert_eq!(executed[0].outcome, TestOutcome::Passed);
    assert_eq!(executed[1].outcome, TestOutcome::Failed);
    assert!(matches!(executed[2].outcome, TestOutcome::Skipped { .. }));
    assert_eq!(failed.receipt, format!("test:{failed_pid}:{failed_nonce}"));
    let mut broken = bound(sh(
        &format!("printf '%s' '{xml}' > junit.xml; exit 101"),
        temp.path(),
    ));
    broken.input.outputs.push(expected("junit", &junit));
    let broken = finished(
        started(launch(PhaseKind::Test, "test", &broken, &[], ctx(&logs, None)).unwrap())
            .wait(Duration::from_secs(10), at()),
    );
    assert_eq!(
        broken.output,
        PhaseOutput::None,
        "any other failure exit keeps the raw artifact only"
    );
    assert!(
        parse_plan(
            r#"{"plan_digest":"x","identity":{"scope":"s"},"workers":[{"index":0,"tests":[["only-one"]]}]}"#
        )
        .is_err()
    );
    assert!(parse_plan(r#"{"plan_digest":"x","workers":[]}"#).is_err());
}

#[test]
fn adk_phase_commands_run_in_the_verification_checkout_with_a_private_target() {
    let checkout = Path::new("/ws/campaigns/c/run/a/s1/verify/v-1");
    let target = Path::new("/ws/campaigns/c/run/a/s1/target/v-1");
    let scratch = Path::new("/ws/campaigns/c/run/a/s1/scratch/v-1");
    let paths = AdkPhasePaths {
        checkout,
        target,
        scratch,
        sha: "h1",
    };
    let identity = identity();
    let policy = policy();
    for kind in [
        PhaseKind::Archive,
        PhaseKind::List,
        PhaseKind::SourceCompare,
        PhaseKind::Classify,
        PhaseKind::Plan,
        PhaseKind::Test,
        PhaseKind::Audit,
    ] {
        let command = adk_phase_command(
            kind,
            &paths,
            &identity,
            &policy,
            SelectionRule::NonPgClassifier,
        );
        assert_eq!(command.cwd, checkout.to_string_lossy(), "{kind:?}");
        assert_eq!(
            command
                .env_identity
                .get("CARGO_TARGET_DIR")
                .map(String::as_str),
            Some(target.to_str().unwrap())
        );
        // Nothing points outside this execution's checkout, scratch and target.
        for arg in command.argv.iter().filter(|a| a.starts_with('/')) {
            assert!(
                arg.starts_with(checkout.to_str().unwrap())
                    || arg.starts_with(scratch.to_str().unwrap())
                    || arg.starts_with(target.to_str().unwrap()),
                "{kind:?}: {arg}"
            );
        }
        if matches!(kind, PhaseKind::List | PhaseKind::Test) {
            let remap = command
                .argv
                .iter()
                .position(|a| a == "--workspace-remap")
                .map(|i| command.argv[i + 1].as_str());
            assert_eq!(remap, Some(checkout.to_str().unwrap()), "{kind:?}");
        }
    }
    let test = adk_phase_command(
        PhaseKind::Test,
        &paths,
        &identity,
        &policy,
        SelectionRule::NonPgClassifier,
    );
    let argv = test.argv.join(" ");
    assert!(argv.contains("--archive-file /ws/campaigns/c/run/a/s1/scratch/v-1/lib-tests.tar.zst"));
    assert!(argv.contains("--profile ci") && argv.ends_with("--no-tests fail"));
    let plan_for = |selection: SelectionRule| {
        adk_phase_command(PhaseKind::Plan, &paths, &identity, &policy, selection)
            .argv
            .join(" ")
    };
    let plan = plan_for(SelectionRule::NonPgClassifier);
    for flag in [
        "--sha h1",
        "--run-id run:v-1",
        "--rust-version 1.94.1",
        "--nextest-version 0.9.100",
        "--target aarch64-apple-darwin",
        "--scope campaign:non-pg",
        "--only /ws/campaigns/c/run/a/s1/scratch/v-1/selection.json",
    ] {
        assert!(plan.contains(flag), "{flag}: {plan}");
    }
    // The whole listed universe is planned without the classifier's selection file.
    let universe = plan_for(SelectionRule::ListedUniverse);
    assert!(universe.contains("--scope campaign:universe") && !universe.contains("--only"));
    let handoff = adk_test_handoff(&paths, &identity, &policy, "pd-1", "aa11");
    assert_eq!(handoff.record_dir, scratch.join("results/worker-0"));
    assert_eq!(handoff.extract_dir, scratch.join("extract"));
    assert!(handoff.junit_sources[0].starts_with(scratch.join("extract")));
    assert!(handoff.junit_sources[1].starts_with(target));
}

// Drives one registered phase: claim the action, launch, wait, and record the V's own result.
fn drive(
    run: &mut VerificationRun,
    actions: &mut BTreeMap<String, Action>,
    phase: &str,
    kind: PhaseKind,
    input: PhaseInput,
    logs: &Path,
    handoff: Option<TestHandoff>,
) -> PhaseResult {
    run.bind_phase_input(phase, input).unwrap();
    let bound = run.phase(phase).unwrap().input.bound().unwrap().clone();
    let running = started(launch(kind, phase, &bound, &[], ctx(logs, handoff)).unwrap());
    let initiation = running.initiation();
    let PhaseInitiation::Started { receipt, .. } = &initiation else {
        panic!()
    };
    let action_id = run.phase(phase).unwrap().action_id.clone();
    let action = actions.get_mut(&action_id).unwrap();
    action.state = ActionState::Started;
    action.initiation_receipt = Some(receipt.clone());
    let action = action.clone();
    run.record_initiation(phase, initiation).unwrap();
    let result = finished(running.wait(Duration::from_secs(60), at()));
    let stderr = std::fs::read_to_string(&result.record.stderr_ref).unwrap_or_default();
    assert_eq!(result.record.exit_code, Some(0), "{phase}: {stderr}");
    run.record_result(phase, &action, result.clone()).unwrap();
    actions.get_mut(&action_id).unwrap().state = ActionState::Reconciled;
    result
}

fn phase_input(
    run: &VerificationRun,
    command: CommandSpec,
    requires: &[&str],
    outputs: Vec<ExpectedArtifact>,
    preflight: Vec<ArtifactRef>,
) -> PhaseInput {
    PhaseInput {
        subject_sha: "h1".into(),
        checkout_path: command.cwd.clone(),
        command,
        predecessors: requires
            .iter()
            .map(|id| {
                let output = &run.phase(id).unwrap().result.as_ref().unwrap().output;
                (id.to_string(), digest_of(output).unwrap())
            })
            .collect(),
        preflight,
        outputs,
        resources: vec![],
    }
}

fn pinned(name: &str, path: &Path) -> ArtifactRef {
    ArtifactRef {
        name: name.into(),
        path: path.to_string_lossy().into_owned(),
        digest: file_digest(path).unwrap(),
    }
}

// The cargo steps are stand-ins; plan and audit run the real scripts/ci/nextest_ci.py.
#[cfg(unix)]
#[test]
fn adk_phases_hand_their_files_to_the_real_helper_and_collect_the_planned_check() {
    let temp = tempfile::tempdir().unwrap();
    let scratch = temp.path().join("scratch");
    let target = temp.path().join("target");
    let logs = temp.path().join("logs");
    std::fs::create_dir_all(&scratch).unwrap();
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"));
    let paths = AdkPhasePaths {
        checkout,
        target: &target,
        scratch: &scratch,
        sha: "h1",
    };
    let (identity, policy) = (identity(), policy());
    let spec = run_tests::spec_with_audit();
    let (mut run, mut actions) = run_tests::registered_with(spec.clone());
    run.bind_subject(run_tests::subject()).unwrap();
    let archive = scratch.join("lib-tests.tar.zst");
    let archive_input = phase_input(
        &run,
        sh(
            &format!("printf 'archive-bytes' > {}", archive.display()),
            checkout,
        ),
        &[],
        vec![expected("archive", &archive)],
        vec![],
    );
    drive(
        &mut run,
        &mut actions,
        "archive",
        PhaseKind::Archive,
        archive_input,
        &logs,
        None,
    );
    let universe = scratch.join("universe.json");
    let list_command = adk_phase_command(
        PhaseKind::List,
        &paths,
        &identity,
        &policy,
        SelectionRule::NonPgClassifier,
    );
    let listed = sh(
        "printf '%s' '{\"rust-suites\":{\"agentdesk\":{\"binary-id\":\"agentdesk\",\"testcases\":{\"campaign::probe\":{\"ignored\":false},\"campaign::pg_only\":{\"ignored\":false}}}}}'",
        Path::new(&list_command.cwd),
    );
    let list_input = phase_input(
        &run,
        listed,
        &["archive"],
        vec![expected("universe", &universe)],
        vec![],
    );
    let list = drive(
        &mut run,
        &mut actions,
        "list",
        PhaseKind::List,
        list_input,
        &logs,
        None,
    );
    assert!(matches!(&list.output, PhaseOutput::Universe { tests } if tests.len() == 2));
    // The classifier stands in for pg_test_classifier.py and writes the selection the plan reads.
    let selection = scratch.join("selection.json");
    let classify_input = phase_input(
        &run,
        sh(
            &format!(
                "printf '%s' '{{\"non_pg\":[\"campaign::probe\"]}}' > {}",
                selection.display()
            ),
            checkout,
        ),
        &["list"],
        vec![expected("selection", &selection)],
        vec![],
    );
    drive(
        &mut run,
        &mut actions,
        "classify",
        PhaseKind::Classify,
        classify_input,
        &logs,
        None,
    );
    let plan_json = scratch.join("plan/plan.json");
    let config = scratch.join("plan/worker-0.toml");
    let plan_input = phase_input(
        &run,
        adk_phase_command(
            PhaseKind::Plan,
            &paths,
            &identity,
            &policy,
            SelectionRule::NonPgClassifier,
        ),
        &["list", "classify"],
        vec![expected("plan", &plan_json), expected("config", &config)],
        vec![],
    );
    let plan = drive(
        &mut run,
        &mut actions,
        "plan",
        PhaseKind::Plan,
        plan_input,
        &logs,
        None,
    );
    let PhaseOutput::Plan {
        plan_digest,
        plan_artifact_digest,
        scope,
        selected,
        ..
    } = &plan.output
    else {
        panic!("real plan output: {:?}", plan.output);
    };
    assert_eq!(scope, "campaign:non-pg");
    assert_eq!(
        selected.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["campaign::probe"]
    );
    assert_eq!(plan_artifact_digest, &file_digest(&plan_json).unwrap());
    assert_ne!(plan_digest, plan_artifact_digest);
    run.bind_plan(&spec).unwrap();
    assert_eq!(
        run.plan.bound().unwrap().manifest,
        [PlannedTest {
            id: "agentdesk campaign::probe".into(),
            check_id: "lib-tests".into(),
            required: true
        }]
    );
    let handoff = adk_test_handoff(
        &paths,
        &identity,
        &policy,
        plan_digest,
        &file_sha256(&archive).unwrap(),
    );
    let junit_target = scratch.join("extract/target/nextest/ci/junit.xml");
    let junit = concat!(
        "<?xml version=\"1.0\"?><testsuites><testsuite name=\"agentdesk\" tests=\"1\">",
        "<testcase name=\"campaign::probe\" classname=\"agentdesk\" time=\"0.01\"/>",
        "</testsuite></testsuites>"
    );
    // nextest extracts only into an existing directory: the runner, not the command, prepares it.
    let test_command = sh(
        &format!(
            "test -d {} && mkdir -p {} && printf '%s' '{junit}' > {}",
            handoff.extract_dir.display(),
            junit_target.parent().unwrap().display(),
            junit_target.display()
        ),
        checkout,
    );
    let preflight = vec![
        pinned("archive", &archive),
        pinned("plan", &plan_json),
        pinned("config", &config),
    ];
    let test_input = phase_input(
        &run,
        test_command,
        &["archive", "plan"],
        vec![expected("junit", &handoff.record_dir.join("junit.xml"))],
        preflight,
    );
    run.bind_phase_input("test", test_input.clone()).unwrap();
    // A plan file whose bytes changed after binding does not start, even with the same identity.
    let original = std::fs::read(&plan_json).unwrap();
    let mut edited = original.clone();
    edited.extend_from_slice(b"\n");
    std::fs::write(&plan_json, &edited).unwrap();
    let bound = run.phase("test").unwrap().input.bound().unwrap().clone();
    assert!(matches!(
        launch(PhaseKind::Test, "test", &bound, &[], ctx(&logs, Some(handoff.clone()))).unwrap(),
        Launch::NotSent(PhaseInitiation::NotSent {
            reason: NotSentReason::PreflightMismatch { artifact },
            ..
        }) if artifact == "plan"
    ));
    std::fs::write(&plan_json, &original).unwrap();
    let test = drive(
        &mut run,
        &mut actions,
        "test",
        PhaseKind::Test,
        test_input,
        &logs,
        Some(handoff.clone()),
    );
    let PhaseOutput::Tests { executed } = &test.output else {
        panic!("{:?}", test.output);
    };
    assert_eq!(executed[0].id, "agentdesk campaign::probe");
    assert_eq!(executed[0].check_id, "lib-tests");
    assert_eq!(
        std::fs::read_to_string(handoff.record_dir.join("exit-code")).unwrap(),
        "0\n"
    );
    assert!(handoff.record_dir.join("worker.json").is_file());
    let verdict = scratch.join("verdict.json");
    let audit_input = phase_input(
        &run,
        adk_phase_command(
            PhaseKind::Audit,
            &paths,
            &identity,
            &policy,
            SelectionRule::NonPgClassifier,
        ),
        &["test"],
        vec![expected("verdict", &verdict)],
        vec![],
    );
    drive(
        &mut run,
        &mut actions,
        "audit",
        PhaseKind::Audit,
        audit_input,
        &logs,
        None,
    );
    let verdict: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&verdict).unwrap()).unwrap();
    assert_eq!(verdict["verdict"], "pass", "{verdict}");
    let collected = run
        .collect(|id| actions.get(id), run_tests::environment(), vec![])
        .unwrap();
    let VerificationCollection::Executed { audit, evidence } = collected else {
        panic!("{collected:?}");
    };
    assert!(audit.complete && audit.all_passed, "{audit:?}");
    assert_eq!(audit.audit_exit_code, Some(0));
    assert_eq!(evidence.planned.len(), 1);
}

#[test]
fn test_children_do_not_inherit_the_operational_root() {
    let temp = tempfile::tempdir().unwrap();
    let logs = temp.path().join("logs");
    let mut command = sh(
        "printf '%s|%s' \"${AGENTDESK_ROOT_DIR:-unset}\" \"${CARGO_TARGET_DIR:-unset}\"",
        temp.path(),
    );
    command.env_identity.insert(
        "AGENTDESK_ROOT_DIR".into(),
        temp.path().join("release").to_string_lossy().into_owned(),
    );
    command
        .env_identity
        .insert("CARGO_TARGET_DIR".into(), "/t/v-1".into());
    let input = bound(command);
    let result = finished(
        started(launch(PhaseKind::Test, "test", &input, &[], ctx(&logs, None)).unwrap())
            .wait(Duration::from_secs(10), at()),
    );
    assert_eq!(
        std::fs::read_to_string(&result.record.stdout_ref).unwrap(),
        "unset|/t/v-1"
    );
}

// Plans the two-test universe with the real helper under the listed-universe policy. `partial`
// forges a plan command that claims the universe scope while reading a one-test selection file.
#[cfg(unix)]
fn universe_plan_with_real_helper(partial: bool) -> (VerificationRun, VerificationSpec, usize) {
    let temp = tempfile::tempdir().unwrap();
    let scratch = temp.path().join("scratch");
    let target = temp.path().join("target");
    let logs = temp.path().join("logs");
    std::fs::create_dir_all(&scratch).unwrap();
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"));
    let paths = AdkPhasePaths {
        checkout,
        target: &target,
        scratch: &scratch,
        sha: "h1",
    };
    let (identity, policy) = (identity(), policy());
    let mut spec: VerificationSpec = run_tests::spec();
    spec.checks[0].selection = SelectionRule::ListedUniverse;
    let (mut run, mut actions) = run_tests::registered_with(spec.clone());
    run.bind_subject(run_tests::subject()).unwrap();
    let archive = scratch.join("lib-tests.tar.zst");
    let archive_input = phase_input(
        &run,
        sh(
            &format!("printf 'archive-bytes' > {}", archive.display()),
            checkout,
        ),
        &[],
        vec![expected("archive", &archive)],
        vec![],
    );
    drive(
        &mut run,
        &mut actions,
        "archive",
        PhaseKind::Archive,
        archive_input,
        &logs,
        None,
    );
    let universe = scratch.join("universe.json");
    let list_input = phase_input(
        &run,
        sh(
            "printf '%s' '{\"rust-suites\":{\"agentdesk\":{\"binary-id\":\"agentdesk\",\"testcases\":{\"campaign::probe\":{\"ignored\":false},\"campaign::pg_only\":{\"ignored\":false}}}}}'",
            checkout,
        ),
        &["archive"],
        vec![expected("universe", &universe)],
        vec![],
    );
    drive(
        &mut run,
        &mut actions,
        "list",
        PhaseKind::List,
        list_input,
        &logs,
        None,
    );
    let selection = scratch.join("selection.json");
    let classify_script = if partial {
        format!(
            "printf '%s' '{{\"non_pg\":[\"campaign::probe\"]}}' > {}",
            selection.display()
        )
    } else {
        "true".to_owned()
    };
    let classify_input = phase_input(
        &run,
        sh(&classify_script, checkout),
        &["list"],
        vec![],
        vec![],
    );
    drive(
        &mut run,
        &mut actions,
        "classify",
        PhaseKind::Classify,
        classify_input,
        &logs,
        None,
    );
    let plan_json = scratch.join("plan/plan.json");
    let config = scratch.join("plan/worker-0.toml");
    let mut plan_command = adk_phase_command(
        PhaseKind::Plan,
        &paths,
        &identity,
        &policy,
        if partial {
            SelectionRule::NonPgClassifier
        } else {
            SelectionRule::ListedUniverse
        },
    );
    if partial {
        let scope = plan_command
            .argv
            .iter()
            .position(|a| a == "--scope")
            .unwrap();
        plan_command.argv[scope + 1] = "campaign:universe".into();
        assert!(plan_command.argv.iter().any(|a| a == "--only"));
    }
    let plan_input = phase_input(
        &run,
        plan_command,
        &["list", "classify"],
        vec![expected("plan", &plan_json), expected("config", &config)],
        vec![],
    );
    let plan = drive(
        &mut run,
        &mut actions,
        "plan",
        PhaseKind::Plan,
        plan_input,
        &logs,
        None,
    );
    let PhaseOutput::Plan {
        scope, selected, ..
    } = &plan.output
    else {
        panic!("real plan output: {:?}", plan.output);
    };
    assert_eq!(scope, "campaign:universe");
    (run, spec, selected.len())
}

#[cfg(unix)]
#[test]
fn listed_universe_plans_the_whole_list_with_the_real_helper() {
    let (mut run, spec, selected) = universe_plan_with_real_helper(false);
    assert_eq!(selected, 2);
    run.bind_plan(&spec).unwrap();
    assert_eq!(run.plan.bound().unwrap().manifest.len(), 2);
}

// The helper records whatever `--scope` it is given, so a partial plan can call itself the
// universe; the run refuses to bind it instead of trusting that self-declaration.
#[cfg(unix)]
#[test]
fn a_partial_plan_claiming_the_universe_scope_does_not_bind() {
    let (mut run, spec, selected) = universe_plan_with_real_helper(true);
    assert_eq!(selected, 1);
    assert_eq!(run.bind_plan(&spec), Err(VerificationError::Conflict));
    assert_eq!(run.plan, Slot::Unbound);
}

// The Windows tree observation is only trusted when the whole process listing parsed; a
// missing or malformed listing is Unknown, and Gone needs the leader and every descendant absent.
#[test]
fn a_windows_process_listing_must_be_complete_before_a_tree_is_gone() {
    assert_eq!(parse_process_edges(""), None);
    assert_eq!(parse_process_edges("\r\n\r\n"), None);
    assert_eq!(
        parse_process_edges("12,4\r\nProcessId,ParentProcessId\r\n"),
        None
    );
    assert_eq!(parse_process_edges("12,4\r\n13\r\n"), None);
    let edges = parse_process_edges("4,1\r\n12,4\r\n13,12\r\n20,1\r\n").unwrap();
    assert_eq!(edges, vec![(4, 1), (12, 4), (13, 12), (20, 1)]);
    assert_eq!(tree_state_from_edges(12, &edges), TreeState::Alive);
    assert_eq!(tree_state_from_edges(99, &edges), TreeState::Gone);
    // Once the leader has exited the tree is Gone only when nothing it spawned is listed either.
    let orphans = [(4u32, 1u32), (20, 1)];
    assert_eq!(tree_state_from_edges(12, &orphans), TreeState::Gone);
    let survivors = [(4u32, 1u32), (13, 12), (14, 13), (20, 1)];
    assert_eq!(tree_state_from_edges(12, &survivors), TreeState::Alive);
}
