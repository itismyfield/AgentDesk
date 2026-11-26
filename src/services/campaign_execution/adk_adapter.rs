use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::domain::campaign::workspace::WorkspaceObservation;

use super::adk_resources::AdkResources;
use super::adk_workspace::{self, AdkWorkspaceKey, WorkspaceError};
use super::model::HostGate;
use super::profile::{
    Checkout, ComponentObservation, EnvironmentComponent, EnvironmentFact, ExecutionEnvironment,
    Observation,
};
use super::verification_run::{
    ArtifactRef, ExpectedArtifact, PhaseInput, PhaseKind, Termination, VerificationError,
    VerificationPhase, VerificationRun, VerificationSpec, digest_of, execution_id,
};
use super::verification_runner::{
    AdkBuildIdentity, AdkPhasePaths, TestHandoff, adk_phase_command, adk_test_handoff,
};

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Verification(#[from] VerificationError),
    #[error("verification checkout, artifact or tool identity could not be verified")]
    Identity,
}

pub struct AdkAdapter<'a> {
    pub repository: &'a Path,
    pub workspace_root: &'a Path,
    pub resources: &'a AdkResources,
}

pub(super) struct PhaseMaterialization {
    pub input: PhaseInput,
    pub environment: ExecutionEnvironment,
    pub handoff: Option<TestHandoff>,
    pub logs: PathBuf,
}

impl AdkAdapter<'_> {
    pub(super) fn materialize(
        &self,
        run: &VerificationRun,
        phase: &VerificationPhase,
        spec: &VerificationSpec,
        host: &HostGate,
        now: DateTime<Utc>,
    ) -> Result<PhaseMaterialization, AdapterError> {
        let origin = &run.origin;
        let Some(subject) = run.subject.bound() else {
            return Err(AdapterError::Identity);
        };
        if &origin.gate != host
            || spec.hash()? != origin.spec_hash
            || !self.repository.is_absolute()
            || !self.workspace_root.is_absolute()
            || origin.execution_id
                != execution_id(&origin.owner_binding, origin.repair_cycle, &origin.check_id)
            || [&origin.campaign_id, &origin.run_id, &origin.node_id]
                .into_iter()
                .any(|id| matches!(id.as_str(), "." | ".."))
        {
            return Err(AdapterError::Identity);
        }
        let key = AdkWorkspaceKey {
            campaign_id: origin.campaign_id.clone(),
            run_id: origin.run_id.clone(),
            node_id: origin.node_id.clone(),
            series: origin.series,
        };
        key.validate()?;
        let checkout = key.verification_dir(self.workspace_root, &origin.execution_id);
        let target = key.target_dir(self.workspace_root, &origin.execution_id);
        let scratch = key.scratch_dir(self.workspace_root, &origin.execution_id);
        let witness = adk_workspace::prepare_verification(
            self.repository,
            self.workspace_root,
            &key,
            subject,
            &host.host,
            &origin.execution_id,
            now,
        )?;
        let paths = AdkPhasePaths {
            checkout: &checkout,
            target: &target,
            scratch: &scratch,
            sha: &witness.head_sha,
        };
        let build = observe_build(&checkout, &origin.execution_id)?;
        let check = spec.check(&origin.check_id).ok_or(AdapterError::Identity)?;
        let mut command = adk_phase_command(
            phase.kind,
            &paths,
            &build,
            &spec.command_policy,
            check.selection,
        );
        self.resources.bind_cargo(&mut command)?;
        let predecessors = phase
            .requires
            .iter()
            .map(|id| {
                let result = run
                    .phase(id)?
                    .result
                    .as_ref()
                    .ok_or(AdapterError::Identity)?;
                if result.termination != Termination::Exited
                    || (result.record.exit_code != Some(0)
                        && !(phase.kind == PhaseKind::Audit
                            && run.phase(id)?.kind == PhaseKind::Test))
                {
                    return Err(AdapterError::Identity);
                }
                Ok((
                    id.clone(),
                    digest_of(&result.output).map_err(io::Error::other)?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>, AdapterError>>()?;
        let lookup = |name: &str| {
            let path = scratch.join(artifact_path(name));
            run.phases
                .iter()
                .filter_map(|p| p.result.as_ref())
                .flat_map(|r| &r.artifacts)
                .find(|a| a.name == name && Path::new(&a.path) == path)
                .cloned()
                .ok_or(AdapterError::Identity)
        };
        use PhaseKind::*;
        let inputs: &[&str] = match phase.kind {
            Archive => &[],
            List => &["archive"],
            SourceCompare | Classify => &["universe"],
            Plan => &["archive", "universe", "selection"],
            Test => &["archive", "plan", "config"],
            Audit => &["plan", "junit", "worker", "exit-code"],
        };
        let mut preflight = inputs
            .iter()
            .map(|name| lookup(name))
            .collect::<Result<Vec<_>, _>>()?;
        if command.program == "python3" {
            let script = checkout.join(&command.argv[0]);
            preflight.push(ArtifactRef {
                name: "command-script".into(),
                digest: super::verification_runner::file_digest(&script)?,
                path: script.to_string_lossy().into_owned(),
            });
        }
        let outputs: &[&str] = match phase.kind {
            Archive => &["archive"],
            List => &["universe"],
            SourceCompare => &["compare"],
            Classify => &["selection"],
            Plan => &["plan", "config"],
            Test => &["junit", "worker", "exit-code"],
            Audit => &["verdict", "summary"],
        };
        let handoff = if phase.kind == Test {
            let plan = run.plan.bound().ok_or(AdapterError::Identity)?;
            Some(adk_test_handoff(
                &paths,
                &build,
                &spec.command_policy,
                &plan.plan_digest,
                &super::verification_runner::file_sha256(Path::new(&lookup("archive")?.path))?,
            ))
        } else {
            None
        };
        Ok(PhaseMaterialization {
            input: PhaseInput {
                subject_sha: witness.head_sha.clone(),
                checkout_path: witness.path.clone(),
                command,
                predecessors,
                preflight,
                resources: Vec::new(),
                outputs: outputs
                    .iter()
                    .map(|name| ExpectedArtifact {
                        name: (*name).into(),
                        path: scratch
                            .join(artifact_path(name))
                            .to_string_lossy()
                            .into_owned(),
                    })
                    .collect(),
            },
            environment: execution_environment(witness, build, now),
            handoff,
            logs: scratch.join("logs"),
        })
    }
}

fn artifact_path(name: &str) -> &'static str {
    match name {
        "archive" => "lib-tests.tar.zst",
        "universe" => "universe.json",
        "compare" => "compare.json",
        "selection" => "selection.json",
        "plan" => "plan/plan.json",
        "config" => "plan/worker-0.toml",
        "junit" => "results/worker-0/junit.xml",
        "worker" => "results/worker-0/worker.json",
        "exit-code" => "results/worker-0/exit-code",
        "verdict" => "verdict.json",
        _ => "summary.md",
    }
}

fn observe_build(checkout: &Path, id: &str) -> Result<AdkBuildIdentity, AdapterError> {
    let read = |program: &str, args: &[&str]| -> Result<String, AdapterError> {
        let output = std::process::Command::new(program)
            .current_dir(checkout)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()?;
        if !output.status.success() {
            return Err(AdapterError::Identity);
        }
        String::from_utf8(output.stdout).map_err(|_| AdapterError::Identity)
    };
    let rust = read("rustc", &["-vV"])?;
    let field = |prefix: &str| {
        rust.lines()
            .find_map(|line| line.strip_prefix(prefix))
            .filter(|s| !s.is_empty())
            .ok_or(AdapterError::Identity)
            .map(str::to_owned)
    };
    let nextest = read("cargo-nextest", &["nextest", "--version"])?;
    let nextest_version = nextest
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("cargo-nextest "))
        .and_then(|value| value.split_whitespace().next())
        .ok_or(AdapterError::Identity)?;
    Ok(AdkBuildIdentity {
        rust_version: field("release: ")?,
        target_triple: field("host: ")?,
        nextest_version: nextest_version.into(),
        run_id: id.into(),
    })
}

fn execution_environment(
    witness: WorkspaceObservation,
    build: AdkBuildIdentity,
    now: DateTime<Utc>,
) -> ExecutionEnvironment {
    let observed = |component, identity: &str, version| ComponentObservation {
        component,
        observation: Observation::Observed {
            evidence: EnvironmentFact {
                identity: identity.into(),
                version,
                path: None,
            },
            observed_at: now,
        },
    };
    ExecutionEnvironment {
        os: std::env::consts::OS.into(),
        checkout: Checkout {
            host: witness.host,
            path: witness.path,
            observed_sha: witness.head_sha,
            observed_at: witness.observed_at,
        },
        components: vec![
            observed(
                EnvironmentComponent::Toolchain,
                "rustc",
                Some(build.rust_version),
            ),
            observed(EnvironmentComponent::RustTarget, &build.target_triple, None),
        ],
        allocations: Vec::new(),
    }
}
