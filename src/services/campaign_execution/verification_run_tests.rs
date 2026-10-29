use super::*;
use crate::services::campaign_execution::definition;
use crate::services::campaign_execution::definition::{PROFILE_VERSION, WORKFLOW_VERSION};
use crate::services::campaign_execution::model::{Role, SessionIdentity};
use crate::services::campaign_execution::profile::{Checkout, ResourceKind};

pub(crate) fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-29T07:00:00Z")
        .unwrap()
        .to_utc()
}

pub(crate) fn gate() -> HostGate {
    HostGate {
        host: "macbook".into(),
        gate: "verify".into(),
        incarnation: "boot-1".into(),
    }
}

pub(crate) fn binding() -> AttemptBinding {
    AttemptBinding {
        campaign_id: "campaign".into(),
        run_id: "run".into(),
        node_id: "a".into(),
        series: 1,
        stage: Stage::Implementation,
        attempt_id: "attempt-a".into(),
        role: Role::Implementation,
        session: SessionIdentity {
            gate: gate(),
            session: "work".into(),
            incarnation: "s-1".into(),
        },
        repository: Repository::Adk,
        hashes: definition::DefinitionHashes {
            requirements: "req".into(),
            acceptance: "ac".into(),
            profile: definition::profile_hash(Repository::Adk, PROFILE_VERSION).unwrap(),
        },
        workflow_version: WORKFLOW_VERSION.into(),
        base_sha: "h0-base".into(),
        candidate_sha: "h0".into(),
        control_version: 1,
    }
}

fn template(id: &str, kind: PhaseKind, requires: &[&str]) -> PhaseTemplate {
    PhaseTemplate {
        id: id.into(),
        kind,
        requires: requires.iter().map(|r| r.to_string()).collect(),
    }
}

pub(crate) fn spec() -> VerificationSpec {
    VerificationSpec {
        version: "adk-v1".into(),
        repository: Repository::Adk,
        checks: vec![SpecCheck {
            id: "lib-tests".into(),
            kind: CheckKind::Test,
            required: true,
            acceptance_ids: vec![],
            selection: SelectionRule::NonPgClassifier,
            phases: vec![
                template("archive", PhaseKind::Archive, &[]),
                template("list", PhaseKind::List, &["archive"]),
                template("plan", PhaseKind::Plan, &["list"]),
                template("test", PhaseKind::Test, &["archive", "plan"]),
            ],
        }],
        environment: vec![EnvironmentComponent::Toolchain],
        command_policy: CommandPolicy {
            nextest_profile: "ci".into(),
            no_tests_fail: true,
            workers: 1,
        },
    }
}

pub(crate) fn registration() -> RegistrationInput {
    RegistrationInput {
        owner_binding: binding(),
        repair_cycle: 0,
        registered_node_epoch: 3,
        pin_hash: "pin".into(),
        spec: spec(),
        check_id: "lib-tests".into(),
        gate: gate(),
        registered_at: at(),
    }
}

type Registered = (VerificationRun, BTreeMap<String, Action>);

pub(crate) fn registered() -> Registered {
    let (run, actions) = VerificationRun::register(registration()).unwrap();
    (
        run,
        actions.into_iter().map(|a| (a.id.clone(), a)).collect(),
    )
}

pub(crate) fn subject() -> BoundSubject {
    BoundSubject {
        reference: VerificationSubjectRef::SealedOutput {
            owner_attempt: "attempt-a".into(),
            seal_id: "seal-1".into(),
        },
        repository: Repository::Adk,
        base_sha: "h0-base".into(),
        output_sha: "h1".into(),
    }
}

fn command() -> CommandSpec {
    CommandSpec {
        program: "true".into(),
        argv: vec![],
        cwd: "/verify".into(),
        env_identity: BTreeMap::new(),
    }
}

pub(crate) fn input(predecessors: &[(&str, Digest)]) -> PhaseInput {
    PhaseInput {
        subject_sha: "h1".into(),
        checkout_path: "/verify".into(),
        command: command(),
        predecessors: predecessors
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        preflight: vec![],
        outputs: vec![],
        resources: vec![],
    }
}

pub(crate) fn authority(
    fence: CurrentFence,
    source: AuthoritySource,
) -> VerificationStartAuthority {
    let (consumer_attempt, consumer_stage) = match &source {
        AuthoritySource::CurrentWork => ("attempt-a".to_string(), Stage::Implementation),
        _ => ("attempt-r".to_string(), Stage::Review),
    };
    VerificationStartAuthority {
        execution_id: registered().0.origin.execution_id,
        original_binding_digest: digest_of(&binding()).unwrap(),
        consumer_binding_digest: format!("{consumer_attempt}-digest"),
        consumer_attempt,
        consumer_stage,
        consumer_node_epoch: fence.node_epoch,
        run_control_version: fence.run_control_version,
        source,
    }
}

fn started(phase: &str, pid: u32) -> PhaseInitiation {
    PhaseInitiation::Started {
        receipt: format!("{phase}:{pid}:nonce"),
        process: ProcessIdentity {
            host: "macbook".into(),
            pid,
            process_group: Some(pid),
            started_at: at(),
            nonce: "nonce".into(),
        },
    }
}

pub(crate) fn result(phase: &str, exit_code: i32, output: PhaseOutput) -> PhaseResult {
    PhaseResult {
        record: CommandRecord {
            executable: "true".into(),
            cwd: "/verify".into(),
            argv: vec![],
            env_identity: BTreeMap::new(),
            phase_id: phase.into(),
            started_at: at(),
            finished_at: at(),
            exit_code: Some(exit_code),
            signal: None,
            stdout_ref: format!("/log/{phase}.stdout"),
            stderr_ref: format!("/log/{phase}.stderr"),
        },
        termination: Termination::Exited,
        artifacts: vec![],
        output,
    }
}

// Drive one phase through claim → start → finish on both the V and its action.
fn finish_phase(
    run: &mut VerificationRun,
    actions: &mut BTreeMap<String, Action>,
    phase: &str,
    exit_code: i32,
    output: PhaseOutput,
) {
    let action_id = run.phase(phase).unwrap().action_id.clone();
    let action = actions.get_mut(&action_id).unwrap();
    action.state = ActionState::Started;
    action.initiation_receipt = Some(format!("{phase}:7:nonce"));
    run.record_initiation(phase, started(phase, 7)).unwrap();
    run.record_result(phase, result(phase, exit_code, output))
        .unwrap();
    actions.get_mut(&action_id).unwrap().state = ActionState::Reconciled;
}

fn listed(name: &str, ignored: bool) -> ListedTest {
    ListedTest {
        binary_id: "agentdesk".into(),
        name: name.into(),
        ignored,
    }
}

fn executed(name: &str, outcome: TestOutcome) -> ExecutedTest {
    ExecutedTest {
        id: format!("agentdesk {name}"),
        check_id: "lib-tests".into(),
        outcome,
        log_refs: vec![],
    }
}

fn prepared(universe: Vec<ListedTest>) -> Registered {
    let (mut run, mut actions) = registered();
    run.bind_subject(subject()).unwrap();
    run.bind_phase_input("archive", input(&[])).unwrap();
    finish_phase(
        &mut run,
        &mut actions,
        "archive",
        0,
        PhaseOutput::Archive {
            digest: "archive-digest".into(),
        },
    );
    let archive = digest_of(
        &run.phase("archive")
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .output,
    )
    .unwrap();
    run.bind_phase_input("list", input(&[("archive", archive.clone())]))
        .unwrap();
    finish_phase(
        &mut run,
        &mut actions,
        "list",
        0,
        PhaseOutput::Universe { tests: universe },
    );
    let list = digest_of(&run.phase("list").unwrap().result.as_ref().unwrap().output).unwrap();
    run.bind_phase_input("plan", input(&[("list", list)]))
        .unwrap();
    finish_phase(
        &mut run,
        &mut actions,
        "plan",
        0,
        PhaseOutput::Plan {
            plan_digest: "plan-digest".into(),
            config_digest: "config-digest".into(),
        },
    );
    (run, actions)
}

fn test_input(run: &VerificationRun) -> PhaseInput {
    let digest = |phase: &str| {
        digest_of(&run.phase(phase).unwrap().result.as_ref().unwrap().output).unwrap()
    };
    let mut input = input(&[("archive", digest("archive")), ("plan", digest("plan"))]);
    for (name, digest) in [
        ("plan", "plan-digest"),
        ("archive", "archive-digest"),
        ("config", "config-digest"),
    ] {
        input.preflight.push(ArtifactRef {
            name: name.into(),
            path: format!("/scratch/{name}"),
            digest: digest.into(),
        });
    }
    input
}

fn environment() -> ExecutionEnvironment {
    ExecutionEnvironment {
        os: "macos".into(),
        checkout: Checkout {
            host: "macbook".into(),
            path: "/verify".into(),
            observed_sha: "h1".into(),
            observed_at: at(),
        },
        components: vec![],
        allocations: vec![],
    }
}

fn fence() -> CurrentFence {
    CurrentFence {
        run_control_version: 1,
        node_epoch: 3,
    }
}

#[test]
fn registration_is_deterministic_and_pre_registers_every_phase_unstarted() {
    let (run, actions) = registered();
    let (again, _) = registered();
    assert_eq!(run, again);
    assert_eq!(run.origin.owner_attempt, "attempt-a");
    assert_eq!(run.origin.owner_binding.candidate_sha, "h0");
    assert_eq!(run.origin.registered_node_epoch, 3);
    assert_eq!(run.origin.phase_ids, ["archive", "list", "plan", "test"]);
    assert_eq!(run.subject, Slot::Unbound);
    assert_eq!(
        run.state(|id| actions.get(id)),
        VerificationRunState::Registered
    );
    for phase in &run.phases {
        let action = &actions[&phase.action_id];
        assert_eq!(action.kind, ActionKind::VerificationPhase);
        assert_eq!(action.state, ActionState::Planned);
        assert_eq!(action.binding, binding());
        assert!(action.initiation_receipt.is_none());
    }
    let other_check = RegistrationInput {
        check_id: "missing".into(),
        ..registration()
    };
    assert!(matches!(
        VerificationRun::register(other_check),
        Err(VerificationError::UnknownPhase(_))
    ));
    let restored: VerificationRun =
        serde_json::from_slice(&serde_json::to_vec(&run).unwrap()).unwrap();
    assert_eq!(restored, run);
}

#[test]
fn subject_and_inputs_bind_once_in_dependency_order() {
    let (mut run, actions) = registered();
    assert_eq!(
        run.bind_phase_input("archive", input(&[])),
        Err(VerificationError::Unbound("subject".into()))
    );
    let mut wrong_base = subject();
    wrong_base.base_sha = "other".into();
    assert_eq!(
        run.bind_subject(wrong_base),
        Err(VerificationError::Conflict)
    );
    run.bind_subject(subject()).unwrap();
    run.bind_subject(subject()).unwrap();
    let mut other_output = subject();
    other_output.output_sha = "h2".into();
    assert_eq!(
        run.bind_subject(other_output),
        Err(VerificationError::AlreadyBound)
    );
    assert_eq!(
        run.bind_phase_input("list", input(&[("archive", "guess".into())])),
        Err(VerificationError::PredecessorPending("archive".into()))
    );
    run.bind_phase_input("archive", input(&[])).unwrap();
    run.bind_phase_input("archive", input(&[])).unwrap();
    let mut changed = input(&[]);
    changed.command.argv.push("--release".into());
    assert_eq!(
        run.bind_phase_input("archive", changed),
        Err(VerificationError::AlreadyBound)
    );
    let admit = |run: &VerificationRun, phase: &str| {
        let action = &actions[&run.phase(phase).unwrap().action_id];
        run.admit_phase_start(
            phase,
            action,
            &authority(fence(), AuthoritySource::CurrentWork),
            fence(),
        )
    };
    assert_eq!(
        admit(&run, "list"),
        Err(VerificationError::Unbound("list".into()))
    );
    assert!(matches!(
        admit(&run, "archive"),
        Ok(PhaseAdmission::Start { .. })
    ));
}

#[test]
fn test_phase_input_requires_the_bound_plan_from_this_execution() {
    let (mut run, actions) = prepared(vec![listed("new_test", false)]);
    assert_eq!(
        run.bind_phase_input("test", test_input(&run)),
        Err(VerificationError::Unbound("plan".into()))
    );
    let check = spec().checks[0].clone();
    run.bind_plan(&check).unwrap();
    run.bind_plan(&check).unwrap();
    let plan = run.plan.bound().unwrap();
    assert_eq!(plan.archive_digest, "archive-digest");
    assert_eq!(plan.manifest.len(), 1);
    assert_eq!(plan.manifest[0].id, "agentdesk new_test");
    assert!(plan.manifest[0].required);
    assert_eq!(
        run.state(|id| actions.get(id)),
        VerificationRunState::Materialized
    );
    let mut stale_archive = test_input(&run);
    stale_archive
        .predecessors
        .insert("archive".into(), "old".into());
    assert!(run.bind_phase_input("test", stale_archive).is_err());
    let mut no_preflight = test_input(&run);
    no_preflight.preflight.clear();
    assert_eq!(
        run.bind_phase_input("test", no_preflight),
        Err(VerificationError::Unbound("plan".into()))
    );
    run.bind_phase_input("test", test_input(&run)).unwrap();
    assert!(run.phase("test").unwrap().input.bound().is_some());
}

#[test]
fn stale_authority_is_refused_without_cancelling_the_phase_and_payload_ignores_the_consumer() {
    let (mut run, mut actions) = prepared(vec![listed("t", false)]);
    run.bind_plan(&spec().checks[0]).unwrap();
    run.bind_phase_input("test", test_input(&run)).unwrap();
    let action_id = run.phase("test").unwrap().action_id.clone();
    let current = CurrentFence {
        run_control_version: 2,
        node_epoch: 4,
    };
    let stale = authority(fence(), AuthoritySource::CurrentWork);
    assert_eq!(
        run.admit_phase_start("test", &actions[&action_id], &stale, current),
        Err(VerificationError::StaleAuthority)
    );
    assert_eq!(actions[&action_id].state, ActionState::Planned);
    let review = authority(
        current,
        AuthoritySource::ReviewPrerequisite {
            prerequisite_id: "prereq-1".into(),
        },
    );
    let Ok(PhaseAdmission::Start {
        action_id: admitted,
        payload_digest,
    }) = run.admit_phase_start("test", &actions[&action_id], &review, current)
    else {
        panic!("current review authority must admit the same phase");
    };
    assert_eq!(admitted, action_id);
    let Ok(PhaseAdmission::Start {
        payload_digest: work_payload,
        ..
    }) = run.admit_phase_start("test", &actions[&action_id], &stale, fence())
    else {
        panic!("work authority under its own fence must admit");
    };
    assert_eq!(payload_digest, work_payload);
    let mut other = review.clone();
    other.execution_id = "v-other".into();
    assert_eq!(
        run.admit_phase_start("test", &actions[&action_id], &other, current),
        Err(VerificationError::StaleAuthority)
    );
    let mut wrong_action = actions[&action_id].clone();
    wrong_action.binding.attempt_id = "attempt-r".into();
    assert!(matches!(
        run.admit_phase_start("test", &wrong_action, &review, current),
        Err(VerificationError::ActionMismatch(_))
    ));
    actions.get_mut(&action_id).unwrap().state = ActionState::Cancelled;
    assert_eq!(
        run.admit_phase_start("test", &actions[&action_id], &review, current),
        Err(VerificationError::Cancelled)
    );
    assert_eq!(
        run.state(|id| actions.get(id)),
        VerificationRunState::Materialized
    );
}

#[test]
fn started_or_indeterminate_phases_reconcile_instead_of_restarting_after_reload() {
    let (mut run, mut actions) = registered();
    run.bind_subject(subject()).unwrap();
    run.bind_phase_input("archive", input(&[])).unwrap();
    let action_id = run.phase("archive").unwrap().action_id.clone();
    let work = authority(fence(), AuthoritySource::CurrentWork);
    run.record_initiation("archive", started("archive", 41))
        .unwrap();
    run.record_initiation("archive", started("archive", 41))
        .unwrap();
    assert_eq!(
        run.record_initiation("archive", started("archive", 42)),
        Err(VerificationError::Conflict)
    );
    let reloaded: VerificationRun =
        serde_json::from_slice(&serde_json::to_vec(&run).unwrap()).unwrap();
    let (again, _) = registered();
    assert_eq!(
        reloaded.phase("archive").unwrap().action_id,
        again.phase("archive").unwrap().action_id
    );
    assert_eq!(
        reloaded.admit_phase_start("archive", &actions[&action_id], &work, fence()),
        Ok(PhaseAdmission::Reconcile)
    );
    actions.get_mut(&action_id).unwrap().state = ActionState::Started;
    actions.get_mut(&action_id).unwrap().initiation_receipt = Some("archive:41:nonce".into());
    assert_eq!(
        reloaded.admit_phase_start("archive", &actions[&action_id], &work, fence()),
        Ok(PhaseAdmission::AlreadyStarted {
            receipt: "archive:41:nonce".into()
        })
    );
    assert_eq!(
        reloaded.state(|id| actions.get(id)),
        VerificationRunState::Preparing
    );
    let (mut fresh, mut fresh_actions) = registered();
    fresh.bind_subject(subject()).unwrap();
    fresh.bind_phase_input("archive", input(&[])).unwrap();
    fresh
        .record_initiation(
            "archive",
            PhaseInitiation::Indeterminate {
                evidence: "spawn reply lost".into(),
                observed_at: at(),
            },
        )
        .unwrap();
    fresh_actions.get_mut(&action_id).unwrap().state = ActionState::Unknown;
    assert_eq!(
        fresh.admit_phase_start("archive", &fresh_actions[&action_id], &work, fence()),
        Ok(PhaseAdmission::Reconcile)
    );
    assert_eq!(
        fresh.state(|id| fresh_actions.get(id)),
        VerificationRunState::Unknown
    );
    assert_eq!(
        fresh.cancel(&fresh_actions, "immediate".into(), at()),
        Err(VerificationError::Started)
    );
}

#[test]
fn resource_wait_keeps_the_phase_unstarted_and_admittable() {
    let (mut run, actions) = registered();
    run.bind_subject(subject()).unwrap();
    run.bind_phase_input("archive", input(&[])).unwrap();
    let wait = PhaseInitiation::NotSent {
        reason: NotSentReason::ResourceWait {
            resource: ResourceIdentity {
                kind: ResourceKind::Build,
                host: "macbook".into(),
                identity: "/tmp/adk-build-token.lock".into(),
            },
        },
        evidence: "token timeout 75".into(),
        observed_at: at(),
    };
    run.record_initiation("archive", wait).unwrap();
    assert_eq!(
        run.record_result("archive", result("archive", 75, PhaseOutput::None)),
        Err(VerificationError::NotFinished)
    );
    let action = &actions[&run.phase("archive").unwrap().action_id];
    assert!(matches!(
        run.admit_phase_start(
            "archive",
            action,
            &authority(fence(), AuthoritySource::CurrentWork),
            fence()
        ),
        Ok(PhaseAdmission::Start { .. })
    ));
    assert_eq!(
        run.state(|id| actions.get(id)),
        VerificationRunState::Registered
    );
    assert_eq!(
        run.collect(environment(), vec![]),
        Err(VerificationError::NotFinished)
    );
}

#[test]
fn unstarted_registered_and_materialized_runs_cancel_and_stay_cancelled() {
    let (mut run, actions) = registered();
    let cancelled = run.cancel(&actions, "immediate".into(), at()).unwrap();
    assert_eq!(cancelled.len(), 4);
    assert_eq!(
        run.state(|id| actions.get(id)),
        VerificationRunState::Cancelled
    );
    let (mut run, mut actions) = prepared(vec![listed("t", false)]);
    run.bind_plan(&spec().checks[0]).unwrap();
    let cancelled = run.cancel(&actions, "immediate".into(), at()).unwrap();
    assert_eq!(cancelled, [run.phase("test").unwrap().action_id.clone()]);
    for id in &cancelled {
        actions.get_mut(id).unwrap().state = ActionState::Cancelled;
    }
    assert_eq!(
        run.state(|id| actions.get(id)),
        VerificationRunState::Cancelled
    );
    run.bind_phase_input("test", test_input(&run)).unwrap();
    let review = authority(
        fence(),
        AuthoritySource::ReviewPrerequisite {
            prerequisite_id: "prereq-1".into(),
        },
    );
    let test_action = &actions[&run.phase("test").unwrap().action_id];
    assert_eq!(
        run.admit_phase_start("test", test_action, &review, fence()),
        Err(VerificationError::Cancelled)
    );
}

#[test]
fn collection_keeps_preparation_failures_and_interruptions_apart_from_test_results() {
    let (mut run, mut actions) = registered();
    run.bind_subject(subject()).unwrap();
    run.bind_phase_input("archive", input(&[])).unwrap();
    finish_phase(&mut run, &mut actions, "archive", 101, PhaseOutput::None);
    assert!(matches!(
        run.collect(environment(), vec![]),
        Ok(VerificationCollection::PreparationFailed { phase_id, record })
            if phase_id == "archive" && record.exit_code == Some(101)
    ));
    assert_eq!(
        run.bind_phase_input("list", input(&[])),
        Err(VerificationError::PredecessorPending("archive".into()))
    );
    let (mut run, _) = registered();
    run.bind_subject(subject()).unwrap();
    run.bind_phase_input("archive", input(&[])).unwrap();
    run.record_initiation("archive", started("archive", 9))
        .unwrap();
    let mut interrupted = result("archive", 143, PhaseOutput::None);
    interrupted.termination = Termination::Interrupted {
        evidence: "process group gone".into(),
    };
    run.record_result("archive", interrupted).unwrap();
    assert!(matches!(
        run.collect(environment(), vec![]),
        Ok(VerificationCollection::Interrupted { phase_id, .. }) if phase_id == "archive"
    ));
}

fn executed_run(
    universe: &[&str],
    executed: Vec<ExecutedTest>,
    exit: i32,
) -> (Registered, VerificationCollection) {
    let (mut run, mut actions) = prepared(universe.iter().map(|n| listed(n, false)).collect());
    run.bind_plan(&spec().checks[0]).unwrap();
    run.bind_phase_input("test", test_input(&run)).unwrap();
    finish_phase(
        &mut run,
        &mut actions,
        "test",
        exit,
        PhaseOutput::Tests { executed },
    );
    let collection = run
        .collect(environment(), vec!["/log/test.stdout".into()])
        .unwrap();
    run.collection = Some(collection.clone());
    assert_eq!(
        run.state(|id| actions.get(id)),
        VerificationRunState::Collected
    );
    ((run, actions), collection)
}

#[test]
fn audit_judges_completeness_from_the_manifest_not_from_exit_or_pass_count() {
    let (_, complete_failure) = executed_run(
        &["a", "b"],
        vec![
            executed("a", TestOutcome::Passed),
            executed("b", TestOutcome::Failed),
        ],
        100,
    );
    let VerificationCollection::Executed { evidence, audit } = complete_failure else {
        panic!("complete execution is collected as Executed");
    };
    assert_eq!(evidence.binding, binding());
    assert_eq!(evidence.planned.len(), 2);
    assert!(audit.complete);
    assert!(!audit.all_passed);
    assert_eq!(audit.failed, ["agentdesk b"]);
    let (_, exit_zero_missing) =
        executed_run(&["a", "b"], vec![executed("a", TestOutcome::Passed)], 0);
    let VerificationCollection::Executed { audit, .. } = exit_zero_missing else {
        panic!();
    };
    assert!(!audit.complete && !audit.all_passed);
    assert_eq!(audit.missing, ["agentdesk b"]);
    let (_, skipped) = executed_run(
        &["a"],
        vec![executed(
            "a",
            TestOutcome::Skipped {
                reason: "filtered".into(),
            },
        )],
        0,
    );
    let VerificationCollection::Executed { audit, .. } = skipped else {
        panic!();
    };
    assert!(!audit.complete);
    let (_, duplicate) = executed_run(
        &["a"],
        vec![
            executed("a", TestOutcome::Passed),
            executed("a", TestOutcome::Passed),
        ],
        0,
    );
    let VerificationCollection::Executed { audit, .. } = duplicate else {
        panic!();
    };
    assert!(!audit.complete && audit.duplicate == ["agentdesk a"]);
    let (_, zero) = executed_run(&["a"], vec![], 0);
    let VerificationCollection::Executed { audit, .. } = zero else {
        panic!();
    };
    assert!(!audit.complete && audit.executed == 0);
    let (_, all_pass) = executed_run(&["a"], vec![executed("a", TestOutcome::Passed)], 0);
    let VerificationCollection::Executed { audit, .. } = all_pass else {
        panic!();
    };
    assert!(audit.complete && audit.all_passed);
}

#[test]
fn results_record_once_and_a_second_different_result_is_a_conflict() {
    let (mut run, mut actions) = registered();
    run.bind_subject(subject()).unwrap();
    run.bind_phase_input("archive", input(&[])).unwrap();
    assert_eq!(
        run.record_result("archive", result("archive", 0, PhaseOutput::None)),
        Err(VerificationError::NotFinished)
    );
    finish_phase(&mut run, &mut actions, "archive", 0, PhaseOutput::None);
    run.record_result("archive", result("archive", 0, PhaseOutput::None))
        .unwrap();
    assert_eq!(
        run.record_result("archive", result("archive", 1, PhaseOutput::None)),
        Err(VerificationError::Conflict)
    );
    assert_eq!(run.phases.iter().filter(|p| p.result.is_some()).count(), 1);
}

#[test]
fn spec_validation_rejects_unordered_phases_and_test_checks_without_a_test_phase() {
    let mut unordered = spec();
    unordered.checks[0].phases.swap(0, 1);
    assert!(unordered.validate().is_err());
    let mut no_test = spec();
    no_test.checks[0]
        .phases
        .retain(|p| p.kind != PhaseKind::Test);
    assert!(no_test.validate().is_err());
    let plan = spec().plan(
        Stage::Implementation,
        &Candidate {
            repository: Repository::Adk,
            base_sha: "h0-base".into(),
            head_sha: "h1".into(),
        },
        vec![],
    );
    assert_eq!(plan.checks.len(), 1);
    assert_eq!(
        plan.environment_requirements,
        [EnvironmentComponent::Toolchain]
    );
    assert!(plan.validate().is_ok());
}
