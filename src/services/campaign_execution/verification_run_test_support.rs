use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::domain::campaign::{
    model::Repository,
    profile::{CheckKind, Checkout, EnvironmentComponent, ExecutionEnvironment},
    verification_run::{
        CommandPolicy, CommandRecord, CommandSpec, Digest, PhaseInput, PhaseKind, PhaseOutput,
        PhaseResult, PhaseTemplate, SelectionRule, SpecCheck, Termination, VerificationSpec,
    },
};

pub fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-29T07:00:00Z")
        .unwrap()
        .to_utc()
}

fn template(id: &str, kind: PhaseKind, requires: &[&str]) -> PhaseTemplate {
    PhaseTemplate {
        id: id.into(),
        kind,
        requires: requires.iter().map(|r| r.to_string()).collect(),
    }
}

pub fn spec() -> VerificationSpec {
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
pub fn spec_with_audit() -> VerificationSpec {
    let mut spec = spec();
    spec.checks[0]
        .phases
        .push(template("audit", PhaseKind::Audit, &["test"]));
    spec
}

fn command() -> CommandSpec {
    CommandSpec {
        program: "true".into(),
        argv: vec![],
        cwd: "/verify".into(),
        env_identity: BTreeMap::new(),
    }
}

pub fn input(predecessors: &[(&str, Digest)]) -> PhaseInput {
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

pub(super) fn receipt(phase: &str, pid: u32) -> String {
    format!("{phase}:{pid}:nonce")
}

pub fn result(phase: &str, exit_code: i32, output: PhaseOutput) -> PhaseResult {
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

pub fn environment() -> ExecutionEnvironment {
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
