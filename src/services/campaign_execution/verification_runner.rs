use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::adk_resources::ResourceObservation;
use super::profile::{ExecutedTest, TestOutcome};
use super::verification_run::{
    ArtifactRef, BoundInput, CommandPolicy, CommandRecord, CommandSpec, Digest, ExpectedArtifact,
    ListedTest, NotSentReason, PhaseInitiation, PhaseKind, PhaseOutput, PhaseResult,
    ProcessIdentity, Termination,
};

/// Set on every phase child so a later observation can match the exact process, not just its PID.
pub const PHASE_NONCE_ENV: &str = "ADK_CAMPAIGN_PHASE_NONCE";

pub enum Launch {
    NotSent(PhaseInitiation),
    Started(Box<RunningPhase>),
}

// A phase child in its own process group, tracked from spawn to confirmed exit.
pub struct RunningPhase {
    phase_id: String,
    kind: PhaseKind,
    child: Child,
    process: ProcessIdentity,
    receipt: String,
    command: CommandSpec,
    started_instant: Instant,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    outputs: Vec<ExpectedArtifact>,
}

pub fn file_digest(path: &Path) -> io::Result<Digest> {
    std::fs::read(path).map(|bytes| blake3::hash(&bytes).to_hex().to_string())
}

fn not_sent(reason: NotSentReason, evidence: String, now: DateTime<Utc>) -> Launch {
    Launch::NotSent(PhaseInitiation::NotSent {
        reason,
        evidence,
        observed_at: now,
    })
}

// Resources and preflight digests are checked before any spawn; a wait leaves the phase unstarted.
pub fn launch(
    kind: PhaseKind,
    phase_id: &str,
    bound: &BoundInput,
    resources: &[ResourceObservation],
    log_dir: &Path,
    host: &str,
    now: DateTime<Utc>,
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
                now,
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
                now,
            ));
        }
    }
    std::fs::create_dir_all(log_dir)?;
    let stdout_path = log_dir.join(format!("{phase_id}.stdout"));
    let stderr_path = log_dir.join(format!("{phase_id}.stderr"));
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
        host: host.into(),
        pid,
        process_group: cfg!(unix).then_some(pid),
        started_at: now,
        nonce: nonce.clone(),
    };
    Ok(Launch::Started(Box::new(RunningPhase {
        phase_id: phase_id.into(),
        kind,
        child,
        receipt: format!("{phase_id}:{pid}:{nonce}"),
        process,
        command: input.command.clone(),
        started_instant: Instant::now(),
        stdout_path,
        stderr_path,
        outputs: input.outputs.clone(),
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

#[cfg(unix)]
pub fn process_tree_alive(process: &ProcessIdentity) -> bool {
    process
        .process_group
        .is_some_and(|pgid| signal_group(pgid, "-0"))
}

#[cfg(not(unix))]
pub fn process_tree_alive(process: &ProcessIdentity) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", process.pid)])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&process.pid.to_string()))
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

    pub fn wait(mut self, timeout: Duration, now: DateTime<Utc>) -> PhaseResult {
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return self.finish(Some(status), Termination::Exited, now),
                Ok(None) if self.started_instant.elapsed() >= timeout => {
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
        }
    }

    // Interruption ends the whole process group and only returns once the group is gone.
    pub fn interrupt(self, evidence: String, now: DateTime<Utc>) -> PhaseResult {
        self.terminate(Termination::Interrupted { evidence }, now)
    }

    fn terminate(mut self, termination: Termination, now: DateTime<Utc>) -> PhaseResult {
        #[cfg(unix)]
        if let Some(pgid) = self.process.process_group {
            signal_group(pgid, "-TERM");
            let grace = Instant::now();
            while process_tree_alive(&self.process) && grace.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_millis(50));
            }
            if process_tree_alive(&self.process) {
                signal_group(pgid, "-KILL");
            }
        }
        let _ = self.child.kill();
        let status = self.child.wait().ok();
        let settle = Instant::now();
        while process_tree_alive(&self.process) && settle.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.finish(status, termination, now)
    }

    fn finish(
        self,
        status: Option<ExitStatus>,
        termination: Termination,
        now: DateTime<Utc>,
    ) -> PhaseResult {
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
        let exit_code = status.and_then(|s| s.code());
        let output = if termination == Termination::Exited && exit_code == Some(0) {
            parse_output(self.kind, &artifacts, &self.phase_id)
        } else {
            PhaseOutput::None
        };
        PhaseResult {
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
        PhaseKind::Plan => match (read("plan"), artifact(artifacts, "config")) {
            (Some(text), Some(config)) => serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("plan_digest")?.as_str().map(str::to_owned))
                .map(|plan_digest| PhaseOutput::Plan {
                    plan_digest,
                    config_digest: config.digest.clone(),
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

pub struct AdkPhasePaths<'a> {
    pub checkout: &'a Path,
    pub target: &'a Path,
    pub scratch: &'a Path,
    pub workspace_root: &'a Path,
    pub sha: &'a str,
}

// The same preparation and run steps as scripts/ci/nextest-native.sh, one process per phase.
pub fn adk_phase_command(
    kind: PhaseKind,
    paths: &AdkPhasePaths<'_>,
    policy: &CommandPolicy,
) -> CommandSpec {
    let scratch = paths.scratch.to_string_lossy().into_owned();
    let archive = format!("{scratch}/lib-tests.tar.zst");
    let universe = format!("{scratch}/universe.json");
    let workspace = paths.workspace_root.to_string_lossy().into_owned();
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

#[cfg(test)]
#[path = "verification_runner_tests.rs"]
mod tests;
