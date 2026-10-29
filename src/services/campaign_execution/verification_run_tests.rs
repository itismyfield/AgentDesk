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
                template("classify", PhaseKind::Classify, &["list"]),
                template("plan", PhaseKind::Plan, &["list", "classify"]),
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

// The full fixed phase set: the audit consumes the test phase's raw result.
pub(crate) fn spec_with_audit() -> VerificationSpec {
    let mut spec = spec();
    spec.checks[0]
        .phases
        .push(template("audit", PhaseKind::Audit, &["test"]));
    spec
}

pub(crate) fn registration() -> RegistrationInput {
    registration_with(spec())
}

fn registration_with(spec: VerificationSpec) -> RegistrationInput {
    RegistrationInput {
        owner_binding: binding(),
        repair_cycle: 0,
        registered_node_epoch: 3,
        pin_hash: "pin".into(),
        spec,
        check_id: "lib-tests".into(),
        gate: gate(),
        registered_at: at(),
    }
}

type Registered = (VerificationRun, BTreeMap<String, Action>);

pub(crate) fn registered() -> Registered {
    registered_with(spec())
}

pub(crate) fn registered_with(spec: VerificationSpec) -> Registered {
    let (run, actions) = VerificationRun::register(registration_with(spec)).unwrap();
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

fn receipt(phase: &str, pid: u32) -> String {
    format!("{phase}:{pid}:nonce")
}

fn process(pid: u32) -> ProcessIdentity {
    ProcessIdentity {
        host: "macbook".into(),
        pid,
        process_group: Some(pid),
        started_at: at(),
        nonce: "nonce".into(),
    }
}

fn started(phase: &str, pid: u32) -> PhaseInitiation {
    PhaseInitiation::Started {
        receipt: receipt(phase, pid),
        process: process(pid),
    }
}

fn indeterminate(id: &str) -> PhaseInitiation {
    PhaseInitiation::Indeterminate {
        initiation_id: id.into(),
        evidence: "spawn reply lost".into(),
        observed_at: at(),
    }
}

fn observation(
    run: &VerificationRun,
    phase: &str,
    initiation_id: &str,
    outcome: ReconciledInitiation,
) -> InitiationObservation {
    InitiationObservation {
        execution_id: run.origin.execution_id.clone(),
        action_id: run.phase(phase).unwrap().action_id.clone(),
        phase_id: phase.into(),
        initiation_id: initiation_id.into(),
        outcome,
        evidence: "process table and nonce env read directly".into(),
        observed_at: at(),
    }
}

pub(crate) fn result(phase: &str, exit_code: i32, output: PhaseOutput) -> PhaseResult {
    PhaseResult {
        receipt: receipt(phase, 7),
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

// Mark the phase's action Started with pid 7's receipt, as the start transaction would.
fn start_action(actions: &mut BTreeMap<String, Action>, action_id: &str, phase: &str) -> Action {
    let action = actions.get_mut(action_id).unwrap();
    action.state = ActionState::Started;
    action.initiation_receipt = Some(receipt(phase, 7));
    action.clone()
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
    let action = start_action(actions, &action_id, phase);
    run.record_initiation(phase, started(phase, 7)).unwrap();
    run.record_result(phase, &action, result(phase, exit_code, output))
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

fn output_digest(run: &VerificationRun, phase: &str) -> Digest {
    digest_of(&run.phase(phase).unwrap().result.as_ref().unwrap().output).unwrap()
}

fn prepared(universe: Vec<ListedTest>) -> Registered {
    let selected = universe.iter().filter(|t| !t.ignored).cloned().collect();
    prepared_with(spec(), universe, selected)
}

fn prepared_with(
    spec: VerificationSpec,
    universe: Vec<ListedTest>,
    selected: Vec<ListedTest>,
) -> Registered {
    let scope = plan_scope(spec.checks[0].selection).to_owned();
    let (mut run, mut actions) = registered_with(spec);
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
    run.bind_phase_input(
        "list",
        input(&[("archive", output_digest(&run, "archive"))]),
    )
    .unwrap();
    finish_phase(
        &mut run,
        &mut actions,
        "list",
        0,
        PhaseOutput::Universe { tests: universe },
    );
    run.bind_phase_input("classify", input(&[("list", output_digest(&run, "list"))]))
        .unwrap();
    finish_phase(&mut run, &mut actions, "classify", 0, PhaseOutput::None);
    run.bind_phase_input(
        "plan",
        input(&[
            ("list", output_digest(&run, "list")),
            ("classify", output_digest(&run, "classify")),
        ]),
    )
    .unwrap();
    finish_phase(
        &mut run,
        &mut actions,
        "plan",
        0,
        PhaseOutput::Plan {
            plan_digest: "plan-digest".into(),
            plan_artifact_digest: "plan-file-digest".into(),
            config_digest: "config-digest".into(),
            scope,
            selected,
        },
    );
    (run, actions)
}

fn test_input(run: &VerificationRun) -> PhaseInput {
    let mut input = input(&[
        ("archive", output_digest(run, "archive")),
        ("plan", output_digest(run, "plan")),
    ]);
    for (name, digest) in [
        ("plan", "plan-file-digest"),
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

pub(crate) fn environment() -> ExecutionEnvironment {
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

fn collect(
    run: &VerificationRun,
    actions: &BTreeMap<String, Action>,
) -> Result<VerificationCollection, VerificationError> {
    run.collect(|id| actions.get(id), environment(), vec![])
}

#[test]
fn registration_is_deterministic_and_pre_registers_every_phase_unstarted() {
    let (run, actions) = registered();
    let (again, _) = registered();
    assert_eq!(run, again);
    assert_eq!(run.origin.owner_attempt, "attempt-a");
    assert_eq!(run.origin.owner_binding.candidate_sha, "h0");
    assert_eq!(run.origin.registered_node_epoch, 3);
    assert_eq!(
        run.origin.phase_ids,
        ["archive", "list", "classify", "plan", "test"]
    );
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
    // Another attempt's output on the same repository and base is not this V's subject.
    let mut other_owner = subject();
    other_owner.reference = VerificationSubjectRef::SealedOutput {
        owner_attempt: "attempt-b".into(),
        seal_id: "seal-b".into(),
    };
    other_owner.output_sha = "h1-b".into();
    assert_eq!(
        run.bind_subject(other_owner),
        Err(VerificationError::Conflict)
    );
    let candidate_of = |attempt: &str, sha: &str| BoundSubject {
        reference: VerificationSubjectRef::BoundCandidate {
            attempt_id: attempt.into(),
        },
        repository: Repository::Adk,
        base_sha: "h0-base".into(),
        output_sha: sha.into(),
    };
    assert_eq!(
        run.bind_subject(candidate_of("attempt-b", "h0")),
        Err(VerificationError::Conflict)
    );
    assert_eq!(
        run.bind_subject(candidate_of("attempt-a", "h0-other")),
        Err(VerificationError::Conflict)
    );
    assert_eq!(run.subject, Slot::Unbound);
    let (mut direct, _) = registered();
    direct
        .bind_subject(candidate_of("attempt-a", "h0"))
        .unwrap();
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
    run.bind_plan(&spec()).unwrap();
    run.bind_plan(&spec()).unwrap();
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
    let mut semantic_only = test_input(&run);
    semantic_only.preflight[0].digest = "plan-digest".into();
    assert_eq!(
        run.bind_phase_input("test", semantic_only),
        Err(VerificationError::Unbound("plan".into()))
    );
    run.bind_phase_input("test", test_input(&run)).unwrap();
    assert!(run.phase("test").unwrap().input.bound().is_some());
}

#[test]
fn plan_binds_the_tests_the_plan_selected_not_the_whole_universe() {
    let universe = vec![
        listed("unit_a", false),
        listed("pg_b", false),
        listed("old", true),
    ];
    let (mut run, mut actions) =
        prepared_with(spec(), universe.clone(), vec![listed("unit_a", false)]);
    run.bind_plan(&spec()).unwrap();
    let ids = |run: &VerificationRun| -> Vec<String> {
        run.plan
            .bound()
            .unwrap()
            .manifest
            .iter()
            .map(|p| p.id.clone())
            .collect()
    };
    assert_eq!(ids(&run), ["agentdesk unit_a"]);
    assert!(matches!(
        &run.phase("list").unwrap().result.as_ref().unwrap().output,
        PhaseOutput::Universe { tests } if tests.len() == 3
    ));
    run.bind_phase_input("test", test_input(&run)).unwrap();
    finish_phase(
        &mut run,
        &mut actions,
        "test",
        0,
        PhaseOutput::Tests {
            executed: vec![executed("unit_a", TestOutcome::Passed)],
        },
    );
    let VerificationCollection::Executed { audit, .. } = collect(&run, &actions).unwrap() else {
        panic!("the selected manifest ran completely");
    };
    assert!(audit.complete && audit.all_passed && audit.missing.is_empty());
    let (mut pg, _) = prepared_with(spec(), universe.clone(), vec![listed("pg_b", false)]);
    pg.bind_plan(&spec()).unwrap();
    assert_eq!(ids(&pg), ["agentdesk pg_b"]);
    // The listed universe binds only a plan that selected every runnable listed test.
    let mut whole = spec();
    whole.checks[0].selection = SelectionRule::ListedUniverse;
    let (mut partial, _) = prepared_with(
        whole.clone(),
        universe.clone(),
        vec![listed("unit_a", false)],
    );
    assert_eq!(partial.bind_plan(&whole), Err(VerificationError::Conflict));
    assert_eq!(partial.plan, Slot::Unbound);
    let (mut complete, _) = prepared_with(
        whole.clone(),
        universe.clone(),
        vec![listed("pg_b", false), listed("unit_a", false)],
    );
    complete.bind_plan(&whole).unwrap();
    assert_eq!(ids(&complete), ["agentdesk pg_b", "agentdesk unit_a"]);
    for selected in [
        vec![listed("ghost", false)],
        vec![listed("old", false)],
        vec![listed("unit_a", false), listed("unit_a", false)],
    ] {
        let (mut bad, _) = prepared_with(spec(), universe.clone(), selected);
        assert_eq!(bad.bind_plan(&spec()), Err(VerificationError::Conflict));
        assert_eq!(bad.plan, Slot::Unbound);
    }
    // A plan produced under another selection policy does not bind to this check.
    let (mut other_scope, _) =
        prepared_with(spec(), universe.clone(), vec![listed("unit_a", false)]);
    if let Some(PhaseOutput::Plan { scope, .. }) = other_scope
        .phases
        .iter_mut()
        .find(|p| p.kind == PhaseKind::Plan)
        .and_then(|p| p.result.as_mut())
        .map(|r| &mut r.output)
    {
        *scope = plan_scope(SelectionRule::ListedUniverse).into();
    }
    assert_eq!(
        other_scope.bind_plan(&spec()),
        Err(VerificationError::Conflict)
    );
    let mut optional = spec();
    optional.checks[0].required = false;
    let (mut other_policy, _) =
        prepared_with(spec(), universe.clone(), vec![listed("unit_a", false)]);
    assert_eq!(
        other_policy.bind_plan(&optional),
        Err(VerificationError::Conflict)
    );
    assert_eq!(other_policy.plan, Slot::Unbound);
}

#[test]
fn stale_authority_is_refused_without_cancelling_the_phase_and_payload_ignores_the_consumer() {
    let (mut run, mut actions) = prepared(vec![listed("t", false)]);
    run.bind_plan(&spec()).unwrap();
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
    actions.get_mut(&action_id).unwrap().initiation_receipt = Some(receipt("archive", 41));
    assert_eq!(
        reloaded.admit_phase_start("archive", &actions[&action_id], &work, fence()),
        Ok(PhaseAdmission::AlreadyStarted {
            receipt: receipt("archive", 41)
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
        .record_initiation("archive", indeterminate("i-1"))
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
    assert_eq!(
        fresh.record_initiation("archive", started("archive", 41)),
        Err(VerificationError::Conflict)
    );
    // Only a direct observation of this execution, action and process leaves Unknown.
    let seen = ReconciledInitiation::Started {
        receipt: receipt("archive", 41),
        process: process(41),
    };
    let mut other_execution = observation(&fresh, "archive", "i-1", seen.clone());
    other_execution.execution_id = "v-other".into();
    assert!(matches!(
        fresh.reconcile_initiation(other_execution),
        Err(VerificationError::ActionMismatch(_))
    ));
    let mut other_action = observation(&fresh, "archive", "i-1", ReconciledInitiation::Unstarted);
    other_action.action_id = fresh.phase("list").unwrap().action_id.clone();
    assert!(matches!(
        fresh.reconcile_initiation(other_action),
        Err(VerificationError::ActionMismatch(_))
    ));
    let other_process = ReconciledInitiation::Started {
        receipt: receipt("archive", 41),
        process: process(42),
    };
    assert_eq!(
        fresh.reconcile_initiation(observation(&fresh, "archive", "i-1", other_process)),
        Err(VerificationError::Conflict)
    );
    assert_eq!(
        fresh.state(|id| fresh_actions.get(id)),
        VerificationRunState::Unknown
    );
    let mut fresh: VerificationRun =
        serde_json::from_slice(&serde_json::to_vec(&fresh).unwrap()).unwrap();
    assert_eq!(
        fresh.reconcile_initiation(observation(&fresh, "archive", "i-1", seen)),
        Ok(ActionState::Started)
    );
    let archive = fresh_actions.get_mut(&action_id).unwrap();
    archive.state = ActionState::Started;
    archive.initiation_receipt = Some(receipt("archive", 41));
    assert_eq!(
        fresh.admit_phase_start("archive", &fresh_actions[&action_id], &work, fence()),
        Ok(PhaseAdmission::AlreadyStarted {
            receipt: receipt("archive", 41)
        })
    );
    assert_eq!(
        fresh.state(|id| fresh_actions.get(id)),
        VerificationRunState::Preparing
    );
    assert_eq!(
        fresh.reconcile_initiation(observation(
            &fresh,
            "archive",
            "i-1",
            ReconciledInitiation::Unstarted
        )),
        Err(VerificationError::Conflict)
    );
    let replay = ReconciledInitiation::Started {
        receipt: receipt("archive", 41),
        process: process(41),
    };
    assert_eq!(
        fresh.reconcile_initiation(observation(&fresh, "archive", "i-1", replay)),
        Ok(ActionState::Started)
    );
    let other_after = ReconciledInitiation::Started {
        receipt: receipt("archive", 44),
        process: process(44),
    };
    assert_eq!(
        fresh.reconcile_initiation(observation(&fresh, "archive", "i-1", other_after)),
        Err(VerificationError::Conflict)
    );
    let mut finished = result("archive", 0, PhaseOutput::None);
    finished.receipt = receipt("archive", 41);
    fresh
        .record_result("archive", &fresh_actions[&action_id], finished.clone())
        .unwrap();
    // A late copy of the resolving Started observation answers with the finished state and
    // leaves the recorded result alone instead of moving the action back to Started.
    let mut fresh: VerificationRun =
        serde_json::from_slice(&serde_json::to_vec(&fresh).unwrap()).unwrap();
    let late = ReconciledInitiation::Started {
        receipt: receipt("archive", 41),
        process: process(41),
    };
    assert_eq!(
        fresh.reconcile_initiation(observation(&fresh, "archive", "i-1", late)),
        Ok(ActionState::Reconciled)
    );
    assert_eq!(
        fresh.phase("archive").unwrap().result.as_ref(),
        Some(&finished)
    );
    let (mut idle, mut idle_actions) = registered();
    idle.bind_subject(subject()).unwrap();
    idle.bind_phase_input("archive", input(&[])).unwrap();
    idle.record_initiation("archive", indeterminate("i-1"))
        .unwrap();
    idle_actions.get_mut(&action_id).unwrap().state = ActionState::Unknown;
    assert_eq!(
        idle.reconcile_initiation(observation(
            &idle,
            "archive",
            "i-1",
            ReconciledInitiation::Unstarted
        )),
        Ok(ActionState::Planned)
    );
    idle_actions.get_mut(&action_id).unwrap().state = ActionState::Planned;
    assert!(matches!(
        idle.admit_phase_start("archive", &idle_actions[&action_id], &work, fence()),
        Ok(PhaseAdmission::Start { .. })
    ));
    assert_eq!(
        idle.state(|id| idle_actions.get(id)),
        VerificationRunState::Registered
    );
    // Replaying the resolving observation is idempotent; a stale Unstarted after a start is not.
    assert_eq!(
        idle.reconcile_initiation(observation(
            &idle,
            "archive",
            "i-1",
            ReconciledInitiation::Unstarted
        )),
        Ok(ActionState::Planned)
    );
    assert_eq!(
        idle.cancel(&idle_actions, "immediate".into(), at())
            .unwrap()
            .len(),
        5
    );
    let (mut relaunched, _) = registered();
    relaunched.bind_subject(subject()).unwrap();
    relaunched.bind_phase_input("archive", input(&[])).unwrap();
    relaunched
        .record_initiation("archive", indeterminate("i-1"))
        .unwrap();
    relaunched
        .reconcile_initiation(observation(
            &relaunched,
            "archive",
            "i-1",
            ReconciledInitiation::Unstarted,
        ))
        .unwrap();
    relaunched
        .record_initiation("archive", started("archive", 43))
        .unwrap();
    assert_eq!(
        relaunched.reconcile_initiation(observation(
            &relaunched,
            "archive",
            "i-1",
            ReconciledInitiation::Unstarted
        )),
        Err(VerificationError::Conflict)
    );
    // A second lost start is resolved only by an observation naming it: the earlier Unstarted,
    // an unknown id or a reused id cannot stand in for it, before or after a reload.
    let (mut relost, _) = registered();
    relost.bind_subject(subject()).unwrap();
    relost.bind_phase_input("archive", input(&[])).unwrap();
    relost
        .record_initiation("archive", indeterminate("i-1"))
        .unwrap();
    relost
        .reconcile_initiation(observation(
            &relost,
            "archive",
            "i-1",
            ReconciledInitiation::Unstarted,
        ))
        .unwrap();
    relost
        .record_initiation("archive", indeterminate("i-1"))
        .unwrap();
    assert_eq!(relost.phase("archive").unwrap().initiations.len(), 2);
    let rewritten = PhaseInitiation::Indeterminate {
        initiation_id: "i-1".into(),
        evidence: "spawn reply lost again".into(),
        observed_at: at(),
    };
    assert_eq!(
        relost.record_initiation("archive", rewritten),
        Err(VerificationError::Conflict)
    );
    relost
        .record_initiation("archive", indeterminate("i-2"))
        .unwrap();
    assert_eq!(
        relost.phase("archive").unwrap().unresolved_initiation(),
        Some("i-2")
    );
    let mut relost: VerificationRun =
        serde_json::from_slice(&serde_json::to_vec(&relost).unwrap()).unwrap();
    for stale in ["i-1", "i-9"] {
        assert_eq!(
            relost.reconcile_initiation(observation(
                &relost,
                "archive",
                stale,
                ReconciledInitiation::Unstarted
            )),
            Err(VerificationError::Conflict)
        );
        assert!(relost.phase("archive").unwrap().unresolved());
    }
    assert_eq!(
        relost.reconcile_initiation(observation(
            &relost,
            "archive",
            "i-2",
            ReconciledInitiation::Unstarted
        )),
        Ok(ActionState::Planned)
    );
    assert_eq!(
        relost.reconcile_initiation(observation(
            &relost,
            "archive",
            "i-2",
            ReconciledInitiation::Unstarted
        )),
        Ok(ActionState::Planned)
    );
    assert_eq!(relost.phase("archive").unwrap().initiations.len(), 4);
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
    let action = &actions[&run.phase("archive").unwrap().action_id];
    assert_eq!(
        run.record_result("archive", action, result("archive", 75, PhaseOutput::None)),
        Err(VerificationError::NotFinished)
    );
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
    assert_eq!(collect(&run, &actions), Err(VerificationError::NotFinished));
}

#[test]
fn unstarted_registered_and_materialized_runs_cancel_and_stay_cancelled() {
    let (mut run, actions) = registered();
    let cancelled = run.cancel(&actions, "immediate".into(), at()).unwrap();
    assert_eq!(cancelled.len(), 5);
    assert_eq!(
        run.state(|id| actions.get(id)),
        VerificationRunState::Cancelled
    );
    let (mut run, mut actions) = prepared(vec![listed("t", false)]);
    run.bind_plan(&spec()).unwrap();
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
        collect(&run, &actions),
        Ok(VerificationCollection::PreparationFailed { phase_id, record })
            if phase_id == "archive" && record.exit_code == Some(101)
    ));
    assert_eq!(
        run.bind_phase_input("list", input(&[])),
        Err(VerificationError::PredecessorPending("archive".into()))
    );
    // A preparation failure is not collected while another phase of this V is still live.
    let list_id = run.phase("list").unwrap().action_id.clone();
    run.record_initiation("list", started("list", 8)).unwrap();
    assert_eq!(collect(&run, &actions), Err(VerificationError::NotFinished));
    let (mut run, mut actions) = registered();
    run.bind_subject(subject()).unwrap();
    run.bind_phase_input("archive", input(&[])).unwrap();
    let archive_id = run.phase("archive").unwrap().action_id.clone();
    let archive_action = start_action(&mut actions, &archive_id, "archive");
    run.record_initiation("archive", started("archive", 7))
        .unwrap();
    let mut interrupted = result("archive", 143, PhaseOutput::None);
    interrupted.termination = Termination::Interrupted {
        evidence: "process group gone".into(),
    };
    run.record_result("archive", &archive_action, interrupted)
        .unwrap();
    assert!(matches!(
        collect(&run, &actions),
        Ok(VerificationCollection::Interrupted { phase_id, .. }) if phase_id == "archive"
    ));
    assert!(actions.contains_key(&list_id));
}

fn executed_run(
    universe: &[&str],
    executed: Vec<ExecutedTest>,
    exit: i32,
) -> (Registered, VerificationCollection) {
    let (mut run, mut actions) = prepared(universe.iter().map(|n| listed(n, false)).collect());
    run.bind_plan(&spec()).unwrap();
    run.bind_phase_input("test", test_input(&run)).unwrap();
    finish_phase(
        &mut run,
        &mut actions,
        "test",
        exit,
        PhaseOutput::Tests { executed },
    );
    let collection = run
        .collect(
            |id| actions.get(id),
            environment(),
            vec!["/log/test.stdout".into()],
        )
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
    // Results outside the plan mean the wrong set ran: not complete, so never a pass.
    let (_, extra) = executed_run(
        &["a"],
        vec![
            executed("a", TestOutcome::Passed),
            executed("b", TestOutcome::Passed),
        ],
        0,
    );
    let VerificationCollection::Executed { audit, .. } = extra else {
        panic!();
    };
    assert!(!audit.complete && !audit.all_passed);
    assert_eq!(audit.extra, ["agentdesk b"]);
    let mut other_check = executed("a", TestOutcome::Passed);
    other_check.check_id = "pg-tests".into();
    let (_, other_check) = executed_run(&["a"], vec![other_check], 0);
    let VerificationCollection::Executed { audit, .. } = other_check else {
        panic!();
    };
    assert!(!audit.complete);
    assert_eq!(audit.extra, ["agentdesk a"]);
    assert_eq!(audit.missing, ["agentdesk a"]);
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
    assert_eq!(audit.audit_exit_code, None);
}

#[test]
fn audit_follows_failed_tests_and_collection_waits_for_every_fixed_phase() {
    let universe = vec![listed("a", false), listed("b", false)];
    let (mut run, mut actions) =
        prepared_with(spec_with_audit(), universe.clone(), universe.clone());
    run.bind_plan(&spec_with_audit()).unwrap();
    run.bind_phase_input("test", test_input(&run)).unwrap();
    finish_phase(
        &mut run,
        &mut actions,
        "test",
        100,
        PhaseOutput::Tests {
            executed: vec![
                executed("a", TestOutcome::Passed),
                executed("b", TestOutcome::Failed),
            ],
        },
    );
    assert_eq!(collect(&run, &actions), Err(VerificationError::NotFinished));
    // The audit consumes the failed test phase's raw result; preparation phases still need exit 0.
    run.bind_phase_input("audit", input(&[("test", output_digest(&run, "test"))]))
        .unwrap();
    let audit_id = run.phase("audit").unwrap().action_id.clone();
    assert!(matches!(
        run.admit_phase_start(
            "audit",
            &actions[&audit_id],
            &authority(fence(), AuthoritySource::CurrentWork),
            fence()
        ),
        Ok(PhaseAdmission::Start { .. })
    ));
    run.record_initiation("audit", indeterminate("i-1"))
        .unwrap();
    actions.get_mut(&audit_id).unwrap().state = ActionState::Unknown;
    assert_eq!(collect(&run, &actions), Err(VerificationError::NotFinished));
    let seen = ReconciledInitiation::Started {
        receipt: receipt("audit", 7),
        process: process(7),
    };
    assert_eq!(
        run.reconcile_initiation(observation(&run, "audit", "i-1", seen)),
        Ok(ActionState::Started)
    );
    let audit_action = start_action(&mut actions, &audit_id, "audit");
    assert_eq!(
        run.state(|id| actions.get(id)),
        VerificationRunState::Running
    );
    assert_eq!(collect(&run, &actions), Err(VerificationError::NotFinished));
    run.record_result(
        "audit",
        &audit_action,
        result("audit", 1, PhaseOutput::None),
    )
    .unwrap();
    actions.get_mut(&audit_id).unwrap().state = ActionState::Reconciled;
    let VerificationCollection::Executed { evidence, audit } = collect(&run, &actions).unwrap()
    else {
        panic!("an audited failure is executed evidence, not a preparation failure");
    };
    assert!(audit.complete && !audit.all_passed);
    assert_eq!(audit.audit_exit_code, Some(1));
    assert_eq!(audit.failed, ["agentdesk b"]);
    assert_eq!(evidence.actual.len(), 2);
    // Every test passed but the audit phase disagreed: still not a pass.
    let (mut run, mut actions) =
        prepared_with(spec_with_audit(), universe.clone(), universe.clone());
    run.bind_plan(&spec_with_audit()).unwrap();
    run.bind_phase_input("test", test_input(&run)).unwrap();
    finish_phase(
        &mut run,
        &mut actions,
        "test",
        0,
        PhaseOutput::Tests {
            executed: vec![
                executed("a", TestOutcome::Passed),
                executed("b", TestOutcome::Passed),
            ],
        },
    );
    run.bind_phase_input("audit", input(&[("test", output_digest(&run, "test"))]))
        .unwrap();
    finish_phase(&mut run, &mut actions, "audit", 1, PhaseOutput::None);
    let VerificationCollection::Executed { audit, .. } = collect(&run, &actions).unwrap() else {
        panic!();
    };
    assert!(audit.complete && !audit.all_passed && audit.failed.is_empty());
    assert_eq!(audit.audit_exit_code, Some(1));
}

#[test]
fn results_bind_to_their_phase_action_and_receipt_and_record_once() {
    let (mut run, mut actions) = registered();
    run.bind_subject(subject()).unwrap();
    run.bind_phase_input("archive", input(&[])).unwrap();
    let archive_id = run.phase("archive").unwrap().action_id.clone();
    assert_eq!(
        run.record_result(
            "archive",
            &actions[&archive_id],
            result("archive", 0, PhaseOutput::None)
        ),
        Err(VerificationError::NotFinished)
    );
    finish_phase(&mut run, &mut actions, "archive", 0, PhaseOutput::None);
    let archive_action = actions[&archive_id].clone();
    run.record_result(
        "archive",
        &archive_action,
        result("archive", 0, PhaseOutput::None),
    )
    .unwrap();
    assert_eq!(
        run.record_result(
            "archive",
            &archive_action,
            result("archive", 1, PhaseOutput::None)
        ),
        Err(VerificationError::Conflict)
    );
    run.bind_phase_input(
        "list",
        input(&[("archive", output_digest(&run, "archive"))]),
    )
    .unwrap();
    let list_id = run.phase("list").unwrap().action_id.clone();
    let list_action = start_action(&mut actions, &list_id, "list");
    run.record_initiation("list", started("list", 7)).unwrap();
    let universe = || PhaseOutput::Universe {
        tests: vec![listed("t", false)],
    };
    // Another phase's result, another process's receipt or another action leave the slot empty.
    assert!(matches!(
        run.record_result(
            "list",
            &list_action,
            result("archive", 0, PhaseOutput::None)
        ),
        Err(VerificationError::ActionMismatch(_))
    ));
    let mut foreign = result("list", 0, universe());
    foreign.receipt = receipt("list", 99);
    assert!(matches!(
        run.record_result("list", &list_action, foreign),
        Err(VerificationError::ActionMismatch(_))
    ));
    assert!(matches!(
        run.record_result("list", &archive_action, result("list", 0, universe())),
        Err(VerificationError::ActionMismatch(_))
    ));
    assert!(run.phase("list").unwrap().result.is_none());
    run.record_result("list", &list_action, result("list", 0, universe()))
        .unwrap();
    run.record_result("list", &list_action, result("list", 0, universe()))
        .unwrap();
    assert_eq!(run.phases.iter().filter(|p| p.result.is_some()).count(), 2);
}

#[test]
fn started_phase_without_a_runner_handle_closes_only_on_a_confirmed_gone_tree() {
    let (mut run, mut actions) = registered();
    run.bind_subject(subject()).unwrap();
    run.bind_phase_input("archive", input(&[])).unwrap();
    let action_id = run.phase("archive").unwrap().action_id.clone();
    let action = start_action(&mut actions, &action_id, "archive");
    run.record_initiation("archive", started("archive", 7))
        .unwrap();
    let mut run: VerificationRun =
        serde_json::from_slice(&serde_json::to_vec(&run).unwrap()).unwrap();
    let execution_id = run.origin.execution_id.clone();
    let seen = |tree: TreeState| TerminationObservation {
        execution_id: execution_id.clone(),
        action_id: action_id.clone(),
        phase_id: "archive".into(),
        receipt: receipt("archive", 7),
        process: process(7),
        tree,
        evidence: "process table read after restart".into(),
        observed_at: at(),
    };
    assert_eq!(
        run.reconcile_termination(&action, seen(TreeState::Alive)),
        Err(VerificationError::NotFinished)
    );
    assert_eq!(
        run.reconcile_termination(&action, seen(TreeState::Unknown)),
        Err(VerificationError::NotFinished)
    );
    assert_eq!(collect(&run, &actions), Err(VerificationError::NotFinished));
    assert_eq!(
        run.cancel(&actions, "immediate".into(), at()),
        Err(VerificationError::Started)
    );
    let mut other = seen(TreeState::Gone);
    other.receipt = receipt("archive", 8);
    other.process = process(8);
    assert!(matches!(
        run.reconcile_termination(&action, other),
        Err(VerificationError::ActionMismatch(_))
    ));
    assert!(run.phase("archive").unwrap().result.is_none());
    run.reconcile_termination(&action, seen(TreeState::Gone))
        .unwrap();
    run.reconcile_termination(&action, seen(TreeState::Gone))
        .unwrap();
    let result = run.phase("archive").unwrap().result.as_ref().unwrap();
    assert_eq!(result.record.exit_code, None);
    assert!(matches!(
        result.termination,
        Termination::Interrupted { .. }
    ));
    actions.get_mut(&action_id).unwrap().state = ActionState::Reconciled;
    assert!(matches!(
        collect(&run, &actions),
        Ok(VerificationCollection::Interrupted { phase_id, .. }) if phase_id == "archive"
    ));
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
    // Only what the runner executes faithfully registers: one worker, no PG selection yet, and a
    // classifier selection only when a classify phase feeds the plan.
    let mut two_workers = spec();
    two_workers.command_policy.workers = 2;
    assert_eq!(
        two_workers.validate(),
        Err(VerificationError::InvalidSpec("workers".into()))
    );
    let mut pg = spec();
    pg.checks[0].selection = SelectionRule::PgClassifier;
    assert!(matches!(
        pg.validate(),
        Err(VerificationError::InvalidSpec(_))
    ));
    let mut unclassified = spec();
    unclassified.checks[0]
        .phases
        .retain(|p| p.kind != PhaseKind::Classify);
    for phase in &mut unclassified.checks[0].phases {
        phase.requires.retain(|r| r != "classify");
    }
    assert!(matches!(
        unclassified.validate(),
        Err(VerificationError::InvalidSpec(_))
    ));
    unclassified.checks[0].selection = SelectionRule::ListedUniverse;
    unclassified.validate().unwrap();
    spec().validate().unwrap();
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
