use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sha2::Digest as _;

use super::adk_resources::ResourceObservation;
use super::profile::{ExecutedTest, TestOutcome};
use super::verification_run::{
    ArtifactRef, BoundInput, CommandPolicy, CommandRecord, CommandSpec, Digest, ExpectedArtifact,
    ListedTest, NotSentReason, PhaseInitiation, PhaseKind, PhaseOutput, PhaseResult,
    ProcessIdentity, Termination,
};

/// Set on every phase child so a later observation can match the exact process, not just its PID.
pub const PHASE_NONCE_ENV: &str = "ADK_CAMPAIGN_PHASE_NONCE";

// Worker record version scripts/ci/nextest_ci.py audits (WORKER_VERSION).
const WORKER_RECORD_VERSION: u64 = 1;

pub enum Launch {
    NotSent(PhaseInitiation),
    Started(Box<RunningPhase>),
}

// A wait or interruption is terminal only once the leader exited and its process group is gone;
// otherwise the phase is handed back for a later retry or a direct reconciliation.
pub enum Settled {
    Finished(Box<PhaseResult>),
    Lingering {
        phase: Box<RunningPhase>,
        evidence: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeState {
    Alive,
    Gone,
    Unknown,
}

// What the test phase leaves for the audit helper: the worker record the native CI path writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestHandoff {
    pub record_dir: PathBuf,
    pub worker_json: String,
    pub junit_sources: Vec<PathBuf>,
}

pub struct LaunchContext<'a> {
    pub check_id: &'a str,
    pub log_dir: &'a Path,
    pub host: &'a str,
    pub now: DateTime<Utc>,
    pub handoff: Option<TestHandoff>,
}

// A phase child in its own process group, tracked from spawn to confirmed exit.
pub struct RunningPhase {
    phase_id: String,
    check_id: String,
    kind: PhaseKind,
    child: Child,
    process: ProcessIdentity,
    receipt: String,
    command: CommandSpec,
    started_instant: Instant,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    outputs: Vec<ExpectedArtifact>,
    handoff: Option<TestHandoff>,
}

pub fn file_digest(path: &Path) -> io::Result<Digest> {
    std::fs::read(path).map(|bytes| blake3::hash(&bytes).to_hex().to_string())
}

// The archive identity nextest_ci.py records in plan.json and checks against worker.json.
pub fn file_sha256(path: &Path) -> io::Result<String> {
    std::fs::read(path).map(|bytes| format!("{:x}", sha2::Sha256::digest(&bytes)))
}

fn not_sent(reason: NotSentReason, evidence: String, now: DateTime<Utc>) -> Launch {
    Launch::NotSent(PhaseInitiation::NotSent {
        reason,
        evidence,
        observed_at: now,
    })
}

// List has no file output of its own: its stdout is the universe the next phases read.
fn stdout_artifact(kind: PhaseKind) -> Option<&'static str> {
    (kind == PhaseKind::List).then_some("universe")
}

// Resources and preflight digests are checked before any spawn; a wait leaves the phase unstarted.
pub fn launch(
    kind: PhaseKind,
    phase_id: &str,
    bound: &BoundInput,
    resources: &[ResourceObservation],
    ctx: LaunchContext<'_>,
) -> io::Result<Launch> {
    let input = &bound.input;
    for required in &input.resources {
        let observation = resources.iter().find(|r| r.resource == *required);
        if !observation.is_some_and(ResourceObservation::is_available) {
            let evidence = match observation {
                Some(o) => format!("{:?}", o.observation),
                None => "resource not observed".into(),
            };
            return Ok(not_sent(
                NotSentReason::ResourceWait {
                    resource: required.clone(),
                },
                evidence,
                ctx.now,
            ));
        }
    }
    for artifact in &input.preflight {
        let actual = file_digest(Path::new(&artifact.path)).ok();
        if actual.as_deref() != Some(artifact.digest.as_str()) {
            return Ok(not_sent(
                NotSentReason::PreflightMismatch {
                    artifact: artifact.name.clone(),
                },
                format!(
                    "{} digest {:?} != bound {}",
                    artifact.path, actual, artifact.digest
                ),
                ctx.now,
            ));
        }
    }
    std::fs::create_dir_all(ctx.log_dir)?;
    // The verified identity is recorded before the tests start, as verify-archive does on CI.
    if let Some(handoff) = &ctx.handoff {
        std::fs::create_dir_all(&handoff.record_dir)?;
        std::fs::write(handoff.record_dir.join("worker.json"), &handoff.worker_json)?;
    }
    let stdout_path = ctx.log_dir.join(format!("{phase_id}.stdout"));
    let stderr_path = ctx.log_dir.join(format!("{phase_id}.stderr"));
    let nonce = uuid::Uuid::new_v4().to_string();
    let mut command = Command::new(&input.command.program);
    command
        .args(&input.command.argv)
        .current_dir(&input.command.cwd)
        .envs(&input.command.env_identity)
        .env(PHASE_NONCE_ENV, &nonce)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout_path)?)
        .stderr(std::fs::File::create(&stderr_path)?);
    crate::services::process::configure_child_process_group(&mut command);
    let child = command.spawn()?;
    let pid = child.id();
    let process = ProcessIdentity {
        host: ctx.host.into(),
        pid,
        process_group: cfg!(unix).then_some(pid),
        started_at: ctx.now,
        nonce: nonce.clone(),
    };
    Ok(Launch::Started(Box::new(RunningPhase {
        phase_id: phase_id.into(),
        check_id: ctx.check_id.into(),
        kind,
        child,
        receipt: format!("{phase_id}:{pid}:{nonce}"),
        process,
        command: input.command.clone(),
        started_instant: Instant::now(),
        stdout_path,
        stderr_path,
        outputs: input.outputs.clone(),
        handoff: ctx.handoff,
    })))
}

#[cfg(unix)]
fn signal_group(pgid: u32, signal: &str) -> bool {
    Command::new("kill")
        .args([signal, "--", &format!("-{pgid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

// Absence is only reported when kill(1) says the group has no process; any other outcome is Unknown.
#[cfg(unix)]
pub fn process_tree_state(process: &ProcessIdentity) -> TreeState {
    let Some(pgid) = process.process_group else {
        return TreeState::Unknown;
    };
    match Command::new("kill")
        .args(["-0", "--", &format!("-{pgid}")])
        .stdout(Stdio::null())
        .output()
    {
        Ok(out) if out.status.success() => TreeState::Alive,
        Ok(out) if String::from_utf8_lossy(&out.stderr).contains("No such process") => {
            TreeState::Gone
        }
        _ => TreeState::Unknown,
    }
}

#[cfg(not(unix))]
pub fn process_tree_state(process: &ProcessIdentity) -> TreeState {
    match Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", process.pid)])
        .output()
    {
        Ok(o) if String::from_utf8_lossy(&o.stdout).contains(&process.pid.to_string()) => {
            TreeState::Alive
        }
        Ok(_) => TreeState::Gone,
        Err(_) => TreeState::Unknown,
    }
}

pub fn process_tree_alive(process: &ProcessIdentity) -> bool {
    process_tree_state(process) == TreeState::Alive
}

impl RunningPhase {
    pub fn initiation(&self) -> PhaseInitiation {
        PhaseInitiation::Started {
            receipt: self.receipt.clone(),
            process: self.process.clone(),
        }
    }

    pub fn process(&self) -> &ProcessIdentity {
        &self.process
    }

    pub fn poll(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    // Waits for the leader and then for its whole group within the same timeout; stragglers are
    // ended at the deadline, and a group that does not go away hands the phase back unfinished.
    pub fn wait(mut self, timeout: Duration, now: DateTime<Utc>) -> Settled {
        let deadline = self.started_instant + timeout;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    return self.terminate(Termination::TimedOut, now);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => {
                    return self.terminate(
                        Termination::Interrupted {
                            evidence: "wait failed".into(),
                        },
                        now,
                    );
                }
            }
        };
        while process_tree_state(&self.process) == TreeState::Alive && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if process_tree_state(&self.process) != TreeState::Gone {
            self.end_group();
        }
        self.settle(Some(status), Termination::Exited, now)
    }

    // Interruption ends the whole process group and only reports once the group is gone.
    pub fn interrupt(self, evidence: String, now: DateTime<Utc>) -> Settled {
        self.terminate(Termination::Interrupted { evidence }, now)
    }

    fn terminate(mut self, termination: Termination, now: DateTime<Utc>) -> Settled {
        self.end_group();
        let _ = self.child.kill();
        let status = self.child.wait().ok();
        self.settle(status, termination, now)
    }

    #[cfg(unix)]
    fn end_group(&self) {
        let Some(pgid) = self.process.process_group else {
            return;
        };
        signal_group(pgid, "-TERM");
        let grace = Instant::now();
        while process_tree_state(&self.process) == TreeState::Alive
            && grace.elapsed() < Duration::from_secs(2)
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        if process_tree_state(&self.process) != TreeState::Gone {
            signal_group(pgid, "-KILL");
        }
    }

    #[cfg(not(unix))]
    fn end_group(&self) {}

    fn settle(
        self,
        status: Option<ExitStatus>,
        termination: Termination,
        now: DateTime<Utc>,
    ) -> Settled {
        let settle = Instant::now();
        while process_tree_state(&self.process) != TreeState::Gone
            && settle.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        match process_tree_state(&self.process) {
            TreeState::Gone => Settled::Finished(Box::new(self.finish(status, termination, now))),
            state => Settled::Lingering {
                evidence: format!(
                    "process group {:?} is {state:?} after termination",
                    self.process.process_group
                ),
                phase: Box::new(self),
            },
        }
    }

    fn finish(
        self,
        status: Option<ExitStatus>,
        termination: Termination,
        now: DateTime<Utc>,
    ) -> PhaseResult {
        // The handoffs the native CI path did between its steps: List stdout becomes the universe
        // file; the test's exit code and JUnit go into the worker record the audit reads.
        if let Some(expected) =
            stdout_artifact(self.kind).and_then(|name| self.outputs.iter().find(|o| o.name == name))
        {
            let _ = std::fs::copy(&self.stdout_path, &expected.path);
        }
        let exit_code = status.and_then(|s| s.code());
        if let Some(handoff) = &self.handoff {
            let code = exit_code.or_else(|| status.and_then(unix_signal).map(|s| 128 + s));
            if let Some(code) = code {
                let _ = std::fs::write(handoff.record_dir.join("exit-code"), format!("{code}\n"));
            }
            if let Some(junit) = handoff.junit_sources.iter().find(|p| p.is_file()) {
                let _ = std::fs::copy(junit, handoff.record_dir.join("junit.xml"));
            }
        }
        let mut artifacts = Vec::new();
        for expected in &self.outputs {
            if let Ok(digest) = file_digest(Path::new(&expected.path)) {
                artifacts.push(ArtifactRef {
                    name: expected.name.clone(),
                    path: expected.path.clone(),
                    digest,
                });
            }
        }
        // nextest exits 100 when tests failed: that JUnit is still the raw result the audit judges.
        let parsable =
            exit_code == Some(0) || (self.kind == PhaseKind::Test && exit_code == Some(100));
        let output = if termination == Termination::Exited && parsable {
            parse_output(self.kind, &artifacts, &self.check_id)
        } else {
            PhaseOutput::None
        };
        PhaseResult {
            receipt: self.receipt.clone(),
            record: CommandRecord {
                executable: self.command.program.clone(),
                cwd: self.command.cwd.clone(),
                argv: self.command.argv.clone(),
                env_identity: self.command.env_identity.clone(),
                phase_id: self.phase_id.clone(),
                started_at: self.process.started_at,
                finished_at: now,
                exit_code,
                signal: status.and_then(unix_signal),
                stdout_ref: self.stdout_path.to_string_lossy().into_owned(),
                stderr_ref: self.stderr_path.to_string_lossy().into_owned(),
            },
            termination,
            artifacts,
            output,
        }
    }
}

#[cfg(unix)]
fn unix_signal(status: ExitStatus) -> Option<i32> {
    std::os::unix::process::ExitStatusExt::signal(&status)
}

#[cfg(not(unix))]
fn unix_signal(_status: ExitStatus) -> Option<i32> {
    None
}

fn artifact<'a>(artifacts: &'a [ArtifactRef], name: &str) -> Option<&'a ArtifactRef> {
    artifacts.iter().find(|a| a.name == name)
}

fn parse_output(kind: PhaseKind, artifacts: &[ArtifactRef], check_id: &str) -> PhaseOutput {
    let read =
        |name: &str| artifact(artifacts, name).and_then(|a| std::fs::read_to_string(&a.path).ok());
    match kind {
        PhaseKind::Archive => artifact(artifacts, "archive")
            .map(|a| PhaseOutput::Archive {
                digest: a.digest.clone(),
            })
            .unwrap_or(PhaseOutput::None),
        PhaseKind::List => read("universe")
            .and_then(|text| parse_universe(&text).ok())
            .map(|tests| PhaseOutput::Universe { tests })
            .unwrap_or(PhaseOutput::None),
        PhaseKind::Plan => match (artifact(artifacts, "plan"), artifact(artifacts, "config")) {
            (Some(plan), Some(config)) => std::fs::read_to_string(&plan.path)
                .ok()
                .and_then(|text| parse_plan(&text).ok())
                .map(|(plan_digest, selected)| PhaseOutput::Plan {
                    plan_digest,
                    plan_artifact_digest: plan.digest.clone(),
                    config_digest: config.digest.clone(),
                    selected,
                })
                .unwrap_or(PhaseOutput::None),
            _ => PhaseOutput::None,
        },
        PhaseKind::Test => read("junit")
            .map(|xml| PhaseOutput::Tests {
                executed: parse_junit(&xml, check_id),
            })
            .unwrap_or(PhaseOutput::None),
        PhaseKind::SourceCompare | PhaseKind::Classify | PhaseKind::Audit => PhaseOutput::None,
    }
}

// nextest_ci.py plan.json: `plan_digest` and the `[binary_id, name]` pairs its workers will run.
pub fn parse_plan(text: &str) -> Result<(Digest, Vec<ListedTest>), String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let plan_digest = value
        .get("plan_digest")
        .and_then(|d| d.as_str())
        .ok_or("no plan_digest")?
        .to_owned();
    let workers = value
        .get("workers")
        .and_then(|w| w.as_array())
        .ok_or("no workers")?;
    let mut selected = Vec::new();
    for worker in workers {
        let tests = worker
            .get("tests")
            .and_then(|t| t.as_array())
            .ok_or("worker without tests")?;
        for test in tests {
            let pair = test.as_array().filter(|p| p.len() == 2);
            let (Some(binary_id), Some(name)) = (
                pair.and_then(|p| p[0].as_str()),
                pair.and_then(|p| p[1].as_str()),
            ) else {
                return Err("test entry is not a [binary_id, name] pair".into());
            };
            selected.push(ListedTest {
                binary_id: binary_id.to_owned(),
                name: name.to_owned(),
                ignored: false,
            });
        }
    }
    Ok((plan_digest, selected))
}

// `cargo nextest list --message-format json`: rust-suites → binary-id and testcases with `ignored`.
pub fn parse_universe(text: &str) -> Result<Vec<ListedTest>, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let suites = value
        .get("rust-suites")
        .and_then(|s| s.as_object())
        .ok_or("no rust-suites")?;
    let mut tests = Vec::new();
    for suite in suites.values() {
        let binary_id = suite
            .get("binary-id")
            .and_then(|b| b.as_str())
            .ok_or("suite without binary-id")?;
        let cases = suite
            .get("testcases")
            .and_then(|c| c.as_object())
            .ok_or("suite without testcases")?;
        for (name, case) in cases {
            let ignored = case
                .get("ignored")
                .and_then(|i| i.as_bool())
                .ok_or("testcase without ignored flag")?;
            tests.push(ListedTest {
                binary_id: binary_id.to_owned(),
                name: name.clone(),
                ignored,
            });
        }
    }
    if tests.iter().all(|t| t.ignored) {
        return Err("list contains no runnable tests".into());
    }
    Ok(tests)
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let start = tag.find(&format!(" {name}=\""))? + name.len() + 3;
    let end = tag[start..].find('"')? + start;
    Some(
        tag[start..end]
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&amp;", "&"),
    )
}

// nextest JUnit: <testsuite name=binary-id><testcase name=...> with failure/error/skipped children.
pub fn parse_junit(xml: &str, check_id: &str) -> Vec<ExecutedTest> {
    let mut executed = Vec::new();
    let mut suite = String::new();
    let mut rest = xml;
    while let Some(open) = rest.find('<') {
        let tag_end = match rest[open..].find('>') {
            Some(end) => open + end,
            None => break,
        };
        let tag = &rest[open..=tag_end];
        if tag.starts_with("<testsuite ") {
            suite = attribute(tag, "name").unwrap_or_default();
        } else if tag.starts_with("<testcase ") {
            let name = attribute(tag, "name").unwrap_or_default();
            let (body, next) = if tag.ends_with("/>") {
                ("", tag_end + 1)
            } else {
                let close = rest[tag_end..]
                    .find("</testcase>")
                    .map(|c| tag_end + c)
                    .unwrap_or(rest.len());
                (&rest[tag_end + 1..close], close)
            };
            let outcome = if body.contains("<skipped") {
                TestOutcome::Skipped {
                    reason: "junit skipped".into(),
                }
            } else if body.contains("<failure") || body.contains("<error") {
                TestOutcome::Failed
            } else {
                TestOutcome::Passed
            };
            executed.push(ExecutedTest {
                id: format!("{suite} {name}"),
                check_id: check_id.into(),
                outcome,
                log_refs: Vec::new(),
            });
            rest = &rest[next..];
            continue;
        }
        rest = &rest[tag_end + 1..];
    }
    executed
}

// Every path is inside this execution's verification checkout, scratch and target.
pub struct AdkPhasePaths<'a> {
    pub checkout: &'a Path,
    pub target: &'a Path,
    pub scratch: &'a Path,
    pub sha: &'a str,
}

// Build and run identity the plan records; the caller observes these, nothing is copied from CI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdkBuildIdentity {
    pub rust_version: String,
    pub nextest_version: String,
    pub target_triple: String,
    pub run_id: String,
}

// The same preparation and run steps as scripts/ci/nextest-native.sh, one process per phase.
pub fn adk_phase_command(
    kind: PhaseKind,
    paths: &AdkPhasePaths<'_>,
    identity: &AdkBuildIdentity,
    policy: &CommandPolicy,
) -> CommandSpec {
    let scratch = paths.scratch.to_string_lossy().into_owned();
    let archive = format!("{scratch}/lib-tests.tar.zst");
    let universe = format!("{scratch}/universe.json");
    let workspace = paths.checkout.to_string_lossy().into_owned();
    let py = |script: &str, args: &[String]| -> (String, Vec<String>) {
        let mut argv = vec![format!("scripts/ci/{script}")];
        argv.extend(args.iter().cloned());
        ("python3".into(), argv)
    };
    let (program, argv) = match kind {
        PhaseKind::Archive => (
            "cargo".into(),
            ["nextest", "archive", "--lib", "--archive-file", &archive]
                .map(String::from)
                .to_vec(),
        ),
        PhaseKind::List => (
            "cargo".into(),
            [
                "nextest",
                "list",
                "--archive-file",
                &archive,
                "--workspace-remap",
                &workspace,
                "--ignore-default-filter",
                "--message-format",
                "json",
            ]
            .map(String::from)
            .to_vec(),
        ),
        PhaseKind::SourceCompare => py(
            "rust_test_sources.py",
            &[
                "compare".into(),
                "--platform".into(),
                std::env::consts::OS.into(),
                "--list".into(),
                universe.clone(),
                "--json-out".into(),
                format!("{scratch}/compare.json"),
            ],
        ),
        PhaseKind::Classify => py(
            "pg_test_classifier.py",
            &[
                "classify".into(),
                "--list".into(),
                universe.clone(),
                "--json-out".into(),
                format!("{scratch}/selection.json"),
            ],
        ),
        PhaseKind::Plan => py(
            "nextest_ci.py",
            &[
                "plan".into(),
                "--universe".into(),
                universe.clone(),
                "--only".into(),
                format!("{scratch}/selection.json"),
                "--archive".into(),
                archive.clone(),
                "--workers".into(),
                policy.workers.to_string(),
                "--sha".into(),
                paths.sha.into(),
                "--scope".into(),
                "campaign".into(),
                "--run-id".into(),
                identity.run_id.clone(),
                "--rust-version".into(),
                identity.rust_version.clone(),
                "--nextest-version".into(),
                identity.nextest_version.clone(),
                "--target".into(),
                identity.target_triple.clone(),
                "--nextest-profile".into(),
                policy.nextest_profile.clone(),
                "--output".into(),
                format!("{scratch}/plan"),
            ],
        ),
        PhaseKind::Test => {
            let mut argv = [
                "nextest",
                "run",
                "--archive-file",
                &archive,
                "--workspace-remap",
                &workspace,
                "--extract-to",
                &format!("{scratch}/extract"),
                "--extract-overwrite",
                "--config-file",
                &format!("{scratch}/plan/worker-0.toml"),
                "--profile",
                &policy.nextest_profile,
            ]
            .map(String::from)
            .to_vec();
            if policy.no_tests_fail {
                argv.extend(["--no-tests", "fail"].map(String::from));
            }
            ("cargo".into(), argv)
        }
        PhaseKind::Audit => py(
            "nextest_ci.py",
            &[
                "audit".into(),
                "--plan".into(),
                format!("{scratch}/plan/plan.json"),
                "--results".into(),
                format!("{scratch}/results"),
                "--json-out".into(),
                format!("{scratch}/verdict.json"),
                "--summary-out".into(),
                format!("{scratch}/summary.md"),
            ],
        ),
    };
    let mut env_identity = BTreeMap::new();
    env_identity.insert(
        "CARGO_TARGET_DIR".into(),
        paths.target.to_string_lossy().into_owned(),
    );
    CommandSpec {
        program,
        argv,
        cwd: paths.checkout.to_string_lossy().into_owned(),
        env_identity,
    }
}

// The worker-0 record under scratch/results, from the plan this V bound and the verified archive.
pub fn adk_test_handoff(
    paths: &AdkPhasePaths<'_>,
    identity: &AdkBuildIdentity,
    policy: &CommandPolicy,
    plan_digest: &str,
    archive_sha256: &str,
) -> TestHandoff {
    let worker = serde_json::json!({
        "archive_sha256": archive_sha256,
        "plan_digest": plan_digest,
        "run_attempt": "1",
        "run_id": identity.run_id,
        "sha": paths.sha,
        "version": WORKER_RECORD_VERSION,
        "worker": 0,
    });
    let junit = Path::new("nextest")
        .join(&policy.nextest_profile)
        .join("junit.xml");
    TestHandoff {
        record_dir: paths.scratch.join("results").join("worker-0"),
        worker_json: format!("{worker}\n"),
        junit_sources: vec![
            paths.scratch.join("extract").join("target").join(&junit),
            paths.target.join(&junit),
        ],
    }
}

#[cfg(test)]
#[path = "verification_runner_tests.rs"]
mod tests;
