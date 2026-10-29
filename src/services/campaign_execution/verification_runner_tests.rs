use std::collections::BTreeMap;

use super::*;
use crate::services::campaign_execution::adk_resources::{ResourceFact, resource};
use crate::services::campaign_execution::profile::{Observation, ResourceKind, TestOutcome};
use crate::services::campaign_execution::verification_run::PhaseInput;

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
        &logs,
        "host",
        at(),
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
        launch(
            PhaseKind::Archive,
            "archive",
            &input,
            &[],
            &logs,
            "host",
            at()
        )
        .unwrap(),
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
        launch(PhaseKind::Test, "test", &input, &ready, &logs, "host", at()).unwrap(),
        Launch::NotSent(PhaseInitiation::NotSent {
            reason: NotSentReason::PreflightMismatch { artifact },
            ..
        }) if artifact == "archive"
    ));
    assert!(!temp.path().join("ran.txt").exists());
    std::fs::write(&archive, b"compiled").unwrap();
    let running =
        started(launch(PhaseKind::Test, "test", &input, &ready, &logs, "host", at()).unwrap());
    let result = running.wait(Duration::from_secs(10), at());
    assert_eq!(result.record.exit_code, Some(0));
    assert!(temp.path().join("ran.txt").exists());
}

#[test]
fn started_phase_has_a_receipt_and_its_exit_is_recorded_verbatim() {
    let temp = tempfile::tempdir().unwrap();
    let logs = temp.path().join("logs");
    let input = bound(sh("echo out; echo err 1>&2; exit 3", temp.path()));
    let running = started(
        launch(
            PhaseKind::Archive,
            "archive",
            &input,
            &[],
            &logs,
            "host",
            at(),
        )
        .unwrap(),
    );
    let PhaseInitiation::Started { receipt, process } = running.initiation() else {
        panic!("started phase must carry a receipt");
    };
    assert!(receipt.starts_with(&format!("archive:{}:", process.pid)));
    assert_eq!(process.host, "host");
    assert_eq!(process.process_group, cfg!(unix).then_some(process.pid));
    let result = running.wait(Duration::from_secs(10), at());
    assert_eq!(result.termination, Termination::Exited);
    assert_eq!(result.record.exit_code, Some(3));
    assert_eq!(result.record.phase_id, "archive");
    assert_eq!(
        std::fs::read_to_string(&result.record.stdout_ref).unwrap(),
        "out\n"
    );
    assert_eq!(
        std::fs::read_to_string(&result.record.stderr_ref).unwrap(),
        "err\n"
    );
    assert_eq!(result.output, PhaseOutput::None);
    let second = started(
        launch(
            PhaseKind::Archive,
            "archive",
            &input,
            &[],
            &logs,
            "host",
            at(),
        )
        .unwrap(),
    );
    assert_ne!(
        second.initiation(),
        PhaseInitiation::Started { receipt, process }
    );
    second.wait(Duration::from_secs(10), at());
}

#[cfg(unix)]
#[test]
fn interrupt_ends_the_whole_process_group_before_reporting() {
    let temp = tempfile::tempdir().unwrap();
    let logs = temp.path().join("logs");
    let input = bound(sh("sleep 30 & sleep 30 & wait", temp.path()));
    let mut running = started(
        launch(
            PhaseKind::Archive,
            "archive",
            &input,
            &[],
            &logs,
            "host",
            at(),
        )
        .unwrap(),
    );
    let process = running.process().clone();
    std::thread::sleep(Duration::from_millis(200));
    assert!(process_tree_alive(&process));
    assert!(running.poll().unwrap().is_none());
    let result = running.interrupt("immediate stop".into(), at());
    assert!(
        matches!(result.termination, Termination::Interrupted { ref evidence } if evidence == "immediate stop")
    );
    assert!(!process_tree_alive(&process));
    assert!(result.record.exit_code.is_none() || result.record.exit_code != Some(0));
}

#[test]
fn list_and_test_outputs_are_parsed_from_the_artifacts_the_phase_wrote() {
    let temp = tempfile::tempdir().unwrap();
    let logs = temp.path().join("logs");
    let universe = temp.path().join("universe.json");
    let script = format!(
        "printf '%s' '{{\"rust-suites\":{{\"agentdesk\":{{\"binary-id\":\"agentdesk\",\"testcases\":{{\"a::new_test\":{{\"ignored\":false}},\"a::old\":{{\"ignored\":true}}}}}}}}}}' > {}",
        universe.display()
    );
    let mut input = bound(sh(&script, temp.path()));
    input.input.outputs.push(ExpectedArtifact {
        name: "universe".into(),
        path: universe.to_string_lossy().into_owned(),
    });
    let result =
        started(launch(PhaseKind::List, "list", &input, &[], &logs, "host", at()).unwrap())
            .wait(Duration::from_secs(10), at());
    assert_eq!(result.artifacts.len(), 1);
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
        &format!("printf '%s' '{xml}' > {}; exit 100", junit.display()),
        temp.path(),
    ));
    input.input.outputs.push(ExpectedArtifact {
        name: "junit".into(),
        path: junit.to_string_lossy().into_owned(),
    });
    let failed = started(
        launch(
            PhaseKind::Test,
            "lib-tests",
            &input,
            &[],
            &logs,
            "host",
            at(),
        )
        .unwrap(),
    )
    .wait(Duration::from_secs(10), at());
    assert_eq!(failed.record.exit_code, Some(100));
    assert_eq!(
        failed.output,
        PhaseOutput::None,
        "a non-zero exit keeps the raw artifact only"
    );
    let parsed = parse_junit(xml, "lib-tests");
    assert_eq!(parsed.len(), 3);
    assert_eq!(parsed[0].id, "agentdesk a::passes");
    assert_eq!(parsed[0].outcome, TestOutcome::Passed);
    assert_eq!(parsed[1].outcome, TestOutcome::Failed);
    assert!(matches!(parsed[2].outcome, TestOutcome::Skipped { .. }));
    assert!(parsed.iter().all(|t| t.check_id == "lib-tests"));
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
        workspace_root: Path::new("/ws/campaigns/c/run/a/s1/editing"),
        sha: "h1",
    };
    let policy = CommandPolicy {
        nextest_profile: "ci".into(),
        no_tests_fail: true,
        workers: 1,
    };
    for kind in [
        PhaseKind::Archive,
        PhaseKind::List,
        PhaseKind::SourceCompare,
        PhaseKind::Classify,
        PhaseKind::Plan,
        PhaseKind::Test,
        PhaseKind::Audit,
    ] {
        let command = adk_phase_command(kind, &paths, &policy);
        assert_eq!(command.cwd, checkout.to_string_lossy(), "{kind:?}");
        assert_eq!(
            command
                .env_identity
                .get("CARGO_TARGET_DIR")
                .map(String::as_str),
            Some(target.to_str().unwrap())
        );
    }
    let test = adk_phase_command(PhaseKind::Test, &paths, &policy);
    let argv = test.argv.join(" ");
    assert!(argv.contains("--archive-file /ws/campaigns/c/run/a/s1/scratch/v-1/lib-tests.tar.zst"));
    assert!(argv.contains("--profile ci") && argv.ends_with("--no-tests fail"));
    let plan = adk_phase_command(PhaseKind::Plan, &paths, &policy);
    assert!(plan.argv.join(" ").contains("--sha h1"));
}
