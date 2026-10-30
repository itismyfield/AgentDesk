use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    GhExec, PrObservation, gh_api, is_sha, sha_at, str_at, time_at, unknown, valid_branch,
    valid_slug,
};
use crate::services::campaign_execution::profile::Observation;

const PLAN_ARTIFACT: &str = "nextest-plan";
const PAGE: usize = 100;
// The ADK profile's one judge context (scripts/ci/nextest_required.py GATE), required whatever the rules say.
const PROFILE_JUDGE: &str = "CI required";

// Same names as scripts/ci/nextest_required.py artifact_name() and the ci-pr.yml worker matrix.
fn worker_artifact(worker: u32, attempt: u32) -> String {
    format!("nextest-worker-{worker}-attempt-{attempt}")
}

fn worker_job(worker: u32) -> String {
    format!("Rust tests (worker {worker})")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiMode {
    Head,
    MergeRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowIdentity {
    pub path: String,
    pub name: String,
    pub event: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub context: String,
    pub check_run_id: u64,
    pub status: String,
    pub conclusion: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerAdoption {
    pub worker: u32,
    pub attempt: u32,
    pub artifact_id: u64,
    pub artifact_digest: Option<String>,
    // True when the worker was not rerun and its earlier attempt carries over (collect-results).
    pub inherited: bool,
}

// GitHub enforces rulesets and classic protection together; None marks a source that was not read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredSet {
    pub ruleset: Option<Vec<String>>,
    pub classic: Option<Vec<String>>,
    pub profile: Vec<String>,
}

impl RequiredSet {
    // Only a set read from every source is complete; being non-empty alone says nothing.
    pub fn contexts(&self) -> Result<Vec<String>, String> {
        let (Some(ruleset), Some(classic)) = (&self.ruleset, &self.classic) else {
            return Err(
                "required set was not read from both rulesets and branch protection".into(),
            );
        };
        let mut contexts: Vec<String> = ruleset
            .iter()
            .chain(classic)
            .chain(&self.profile)
            .cloned()
            .collect();
        contexts.sort();
        contexts.dedup();
        if contexts.is_empty() {
            return Err("required set is empty".into());
        }
        Ok(contexts)
    }
}

// What the plan and worker artifacts prove was tested; a pass needs all of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedTests {
    pub tested_sha: String,
    // Merge-ref mode only: first parent of the tested merge, checked against the recorded base.
    pub tested_base: Option<String>,
    pub mode: CiMode,
    pub plan_digest: String,
    pub workers: Vec<WorkerAdoption>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TestProvenance {
    Verified(VerifiedTests),
    // Check results were observed, but what they tested could not be bound to this run.
    Unverified { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiObservation {
    pub repository_identity: String,
    pub pr_number: u64,
    pub pr_head_sha: String,
    pub pr_base_ref: String,
    pub workflow: WorkflowIdentity,
    pub run_id: u64,
    pub run_attempt: u32,
    pub run_conclusion: Option<String>,
    pub required: RequiredSet,
    pub checks: Vec<CheckResult>,
    pub provenance: TestProvenance,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum CiVerdict {
    Passed,
    Failed { contexts: Vec<String> },
    Incomplete { reason: String },
}

impl CiObservation {
    // A definite required failure stands on its own; a pass also needs every context and verified tests.
    pub fn verdict(&self) -> CiVerdict {
        let required = match self.required.contexts() {
            Ok(required) => required,
            Err(reason) => return CiVerdict::Incomplete { reason },
        };
        let mut failed = Vec::new();
        let mut open = None;
        for context in &required {
            let Some(check) = self.checks.iter().find(|c| &c.context == context) else {
                if open.is_none() {
                    open = Some(format!(
                        "required context {context} has no check in this run"
                    ));
                }
                continue;
            };
            match (check.status.as_str(), check.conclusion.as_deref()) {
                ("completed", Some("success")) => {}
                ("completed", Some("failure" | "cancelled" | "timed_out" | "action_required")) => {
                    failed.push(context.clone())
                }
                (status, conclusion) if open.is_none() => {
                    open = Some(format!(
                        "required context {context} is {status}/{conclusion:?}"
                    ));
                }
                _ => {}
            }
        }
        if !failed.is_empty() {
            return CiVerdict::Failed { contexts: failed };
        }
        if let Some(reason) = open {
            return CiVerdict::Incomplete { reason };
        }
        match &self.provenance {
            TestProvenance::Verified(_) => CiVerdict::Passed,
            TestProvenance::Unverified { reason } => CiVerdict::Incomplete {
                reason: format!("tests are not bound to this run: {reason}"),
            },
        }
    }

    pub fn tests(&self) -> Option<&VerifiedTests> {
        match &self.provenance {
            TestProvenance::Verified(tests) => Some(tests),
            TestProvenance::Unverified { .. } => None,
        }
    }

    // CI binds the exact head it observed; a newer candidate head needs its own CI.
    pub fn binds_head(&self, candidate_head: &str) -> bool {
        self.pr_head_sha == candidate_head
    }
}

fn page<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    let items = value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("response lacks {key}"))?;
    let total = value.get("total_count").and_then(Value::as_u64);
    if total != Some(items.len() as u64) || items.len() >= PAGE {
        return Err(format!("{key} list is incomplete ({total:?} reported)"));
    }
    Ok(items)
}

async fn ruleset_required(
    gh: &dyn GhExec,
    slug: &str,
    branch: &str,
) -> Result<Vec<String>, String> {
    let rules = match gh_api(
        gh,
        format!("repos/{slug}/rules/branches/{branch}?per_page={PAGE}"),
    )
    .await?
    {
        Value::Array(rules) if rules.len() < PAGE => rules,
        _ => return Err("branch rules are not a complete list".into()),
    };
    let mut contexts = Vec::new();
    for rule in rules
        .iter()
        .filter(|r| r.get("type").and_then(Value::as_str) == Some("required_status_checks"))
    {
        let checks = rule
            .pointer("/parameters/required_status_checks")
            .and_then(Value::as_array)
            .ok_or("required_status_checks rule has no check list")?;
        for check in checks {
            contexts.push(context_of(check.get("context"))?);
        }
    }
    Ok(contexts)
}

fn context_of(value: Option<&Value>) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(context) if !context.is_empty() => Ok(context.to_owned()),
        _ => Err("required status check without a context".into()),
    }
}

// An unprotected branch has no classic checks; a protected one must show its list or stay unread.
async fn classic_required(
    gh: &dyn GhExec,
    slug: &str,
    branch: &str,
) -> Result<Vec<String>, String> {
    let info = gh_api(gh, format!("repos/{slug}/branches/{branch}")).await?;
    if info.get("name").and_then(Value::as_str) != Some(branch) {
        return Err(format!("branch response does not name {branch}"));
    }
    match info.get("protected").and_then(Value::as_bool) {
        Some(false) => return Ok(Vec::new()),
        Some(true) => {}
        None => return Err("branch response lacks protected".into()),
    }
    if info.pointer("/protection/enabled") == Some(&Value::Bool(false)) {
        return Ok(Vec::new());
    }
    let checks = info
        .pointer("/protection/required_status_checks")
        .ok_or("protected branch shows no required status checks")?;
    match checks.get("enforcement_level").and_then(Value::as_str) {
        Some("off") => return Ok(Vec::new()),
        Some(_) => {}
        None => return Err("required status checks lack an enforcement level".into()),
    }
    let mut contexts = checks
        .get("contexts")
        .and_then(Value::as_array)
        .ok_or("required status checks have no context list")?
        .iter()
        .map(|c| context_of(Some(c)))
        .collect::<Result<Vec<_>, _>>()?;
    for check in checks
        .get("checks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        contexts.push(context_of(check.get("context"))?);
    }
    Ok(contexts)
}

// Rulesets, classic protection and the profile judge together; any unread source keeps the set Unknown.
pub async fn observe_required_set(
    gh: &dyn GhExec,
    slug: &str,
    branch: &str,
    observed_at: DateTime<Utc>,
) -> Observation<RequiredSet> {
    if !valid_slug(slug) || !valid_branch(branch) {
        return unknown(format!("invalid target {slug:?} {branch:?}"));
    }
    let mut ruleset = match ruleset_required(gh, slug, branch).await {
        Ok(contexts) => contexts,
        Err(reason) => return unknown(format!("rulesets: {reason}")),
    };
    let mut classic = match classic_required(gh, slug, branch).await {
        Ok(contexts) => contexts,
        Err(reason) => return unknown(format!("branch protection: {reason}")),
    };
    for contexts in [&mut ruleset, &mut classic] {
        contexts.sort();
        contexts.dedup();
    }
    Observation::Observed {
        evidence: RequiredSet {
            ruleset: Some(ruleset),
            classic: Some(classic),
            profile: vec![PROFILE_JUDGE.to_owned()],
        },
        observed_at,
    }
}

fn text_field(meta: &Value, key: &str) -> Option<String> {
    match meta.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

struct Artifact {
    id: u64,
    digest: Option<String>,
}

fn unique_artifact(artifacts: &[Value], name: &str) -> Result<Artifact, String> {
    let mut found = artifacts
        .iter()
        .filter(|a| a.get("name").and_then(Value::as_str) == Some(name));
    let (Some(artifact), None) = (found.next(), found.next()) else {
        return Err(format!("artifact {name} is absent or ambiguous"));
    };
    if artifact.get("expired").and_then(Value::as_bool) != Some(false) {
        return Err(format!("artifact {name} is expired or unreadable"));
    }
    Ok(Artifact {
        id: artifact
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("artifact {name} has no id"))?,
        digest: artifact
            .get("digest")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

async fn download_json(
    gh: &dyn GhExec,
    slug: &str,
    run_id: u64,
    name: &str,
    file: &str,
) -> Result<Value, String> {
    let dir = tempfile::tempdir().map_err(|e| format!("download dir: {e}"))?;
    let dir_arg = dir.path().to_string_lossy().into_owned();
    let args = [
        "run",
        "download",
        &run_id.to_string(),
        "--repo",
        slug,
        "--name",
        name,
        "--dir",
        &dir_arg,
    ]
    .map(str::to_owned)
    .to_vec();
    gh.run(
        args,
        super::GH_TIMEOUT,
        format!("gh run download {name} timed out"),
    )
    .await?;
    let raw = std::fs::read_to_string(Path::new(&dir_arg).join(file))
        .map_err(|e| format!("{name}/{file} unreadable: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("{name}/{file} unparsable: {e}"))
}

pub struct CiTarget<'a> {
    pub slug: &'a str,
    pub pr: &'a PrObservation,
    pub run_id: u64,
    // The base recorded when this CI was requested; a merge-ref must sit on exactly it.
    pub recorded_base: &'a str,
}

enum Gate<T> {
    Go(T),
    NotYet(String),
}

fn bind_tested(
    tested: &str,
    pr_head: &str,
    parents: Option<&[String]>,
    recorded_base: &str,
) -> Result<(CiMode, Option<String>), String> {
    if tested == pr_head {
        return Ok((CiMode::Head, None));
    }
    match parents {
        Some([first, second]) if second == pr_head && first == recorded_base => {
            Ok((CiMode::MergeRef, Some(first.clone())))
        }
        _ => Err(format!(
            "tested {tested} is neither PR head {pr_head} nor its merge onto {recorded_base}"
        )),
    }
}

async fn observe_run(
    gh: &dyn GhExec,
    target: &CiTarget<'_>,
) -> Result<Gate<CiObservation>, String> {
    let CiTarget {
        slug, pr, run_id, ..
    } = *target;
    let run = gh_api(gh, format!("repos/{slug}/actions/runs/{run_id}")).await?;
    if run.get("id").and_then(Value::as_u64) != Some(run_id)
        || !str_at(&run, "/repository/full_name")?.eq_ignore_ascii_case(slug)
    {
        return Err(format!("run {run_id} is not a run of {slug}"));
    }
    let run_head = sha_at(&run, "/head_sha")?;
    if run_head != pr.head_sha {
        return Ok(Gate::NotYet(format!(
            "run {run_id} tested head {run_head}; PR head is now {}",
            pr.head_sha
        )));
    }
    if str_at(&run, "/status")? != "completed" {
        return Ok(Gate::NotYet(format!("run {run_id} is still in progress")));
    }
    let run_attempt = run
        .get("run_attempt")
        .and_then(Value::as_u64)
        .and_then(|a| u32::try_from(a).ok())
        .filter(|a| *a > 0)
        .ok_or("run has no attempt")?;

    // Checks bind this head and run by themselves, so a failure survives missing test artifacts.
    let checks = observe_checks(gh, slug, &pr.head_sha, run_id).await?;
    let provenance = match verify_tests(gh, target, &run, run_attempt).await {
        Ok(tests) => TestProvenance::Verified(tests),
        Err(reason) => TestProvenance::Unverified { reason },
    };
    Ok(Gate::Go(CiObservation {
        repository_identity: slug.to_owned(),
        pr_number: pr.number,
        pr_head_sha: pr.head_sha.clone(),
        pr_base_ref: pr.base_ref.clone(),
        workflow: WorkflowIdentity {
            path: str_at(&run, "/path")?.to_owned(),
            name: str_at(&run, "/name")?.to_owned(),
            event: str_at(&run, "/event")?.to_owned(),
        },
        run_id,
        run_attempt,
        run_conclusion: run
            .get("conclusion")
            .and_then(Value::as_str)
            .map(str::to_owned),
        required: RequiredSet::default(),
        checks,
        provenance,
    }))
}

async fn verify_tests(
    gh: &dyn GhExec,
    target: &CiTarget<'_>,
    run: &Value,
    run_attempt: u32,
) -> Result<VerifiedTests, String> {
    let CiTarget {
        slug, pr, run_id, ..
    } = *target;
    let artifacts_page = gh_api(
        gh,
        format!("repos/{slug}/actions/runs/{run_id}/artifacts?per_page={PAGE}"),
    )
    .await?;
    let artifacts = page(&artifacts_page, "artifacts")?;
    unique_artifact(artifacts, PLAN_ARTIFACT)?;
    let plan = download_json(gh, slug, run_id, PLAN_ARTIFACT, "plan/plan.json").await?;
    let tested_sha = sha_at(&plan, "/identity/sha")?;
    let plan_digest = str_at(&plan, "/plan_digest")?.to_owned();
    if plan
        .get("identity")
        .and_then(|identity| text_field(identity, "run_id"))
        != Some(run_id.to_string())
    {
        return Err("plan identity belongs to another run".into());
    }
    let worker_count = plan
        .get("workers")
        .and_then(Value::as_array)
        .map(Vec::len)
        .filter(|n| *n > 0)
        .ok_or("plan has no workers")?;

    let parents = if tested_sha == pr.head_sha {
        None
    } else {
        let commit = gh_api(gh, format!("repos/{slug}/commits/{tested_sha}")).await?;
        let parents = commit
            .get("parents")
            .and_then(Value::as_array)
            .ok_or("tested commit has no parent list")?
            .iter()
            .map(|p| sha_at(p, "/sha"))
            .collect::<Result<Vec<_>, _>>()?;
        Some(parents)
    };
    let (mode, tested_base) = bind_tested(
        &tested_sha,
        &pr.head_sha,
        parents.as_deref(),
        target.recorded_base,
    )?;

    let jobs_page = gh_api(
        gh,
        format!("repos/{slug}/actions/runs/{run_id}/attempts/{run_attempt}/jobs?per_page={PAGE}"),
    )
    .await?;
    let earlier = earlier_attempts(gh, slug, run_id, run_attempt).await?;
    let attempt = AttemptContext {
        gh,
        target,
        jobs: page(&jobs_page, "jobs")?,
        earlier: &earlier,
        artifacts,
        run_attempt,
        run_started_at: time_at(run, "/run_started_at")?,
        plan_digest: &plan_digest,
        tested_sha: &tested_sha,
    };
    let mut workers = Vec::new();
    for worker in 0..worker_count as u32 {
        workers.push(attempt.adopt_worker(worker).await?);
    }
    Ok(VerifiedTests {
        tested_sha,
        tested_base,
        mode,
        plan_digest,
        workers,
    })
}

struct EarlierAttempt {
    attempt: u32,
    started_at: DateTime<Utc>,
    jobs: Value,
}

// Newest first: the start time and job list of every attempt before the current one.
async fn earlier_attempts(
    gh: &dyn GhExec,
    slug: &str,
    run_id: u64,
    run_attempt: u32,
) -> Result<Vec<EarlierAttempt>, String> {
    let mut earlier = Vec::new();
    for attempt in (1..run_attempt).rev() {
        let path = format!("repos/{slug}/actions/runs/{run_id}/attempts/{attempt}");
        let run = gh_api(gh, path.clone()).await?;
        if run.get("id").and_then(Value::as_u64) != Some(run_id)
            || run.get("run_attempt").and_then(Value::as_u64) != Some(u64::from(attempt))
        {
            return Err(format!(
                "attempt {attempt} of run {run_id} answers for another run"
            ));
        }
        let jobs = gh_api(gh, format!("{path}/jobs?per_page={PAGE}")).await?;
        page(&jobs, "jobs")?;
        earlier.push(EarlierAttempt {
            attempt,
            started_at: time_at(&run, "/run_started_at")?,
            jobs,
        });
    }
    Ok(earlier)
}

fn unique_job<'a>(jobs: &'a [Value], name: &str, attempt: u32) -> Result<&'a Value, String> {
    let mut found = jobs
        .iter()
        .filter(|j| j.get("name").and_then(Value::as_str) == Some(name));
    match (found.next(), found.next()) {
        (Some(job), None) => Ok(job),
        _ => Err(format!(
            "job {name} is absent or ambiguous in attempt {attempt}"
        )),
    }
}

struct AttemptContext<'a> {
    gh: &'a dyn GhExec,
    target: &'a CiTarget<'a>,
    jobs: &'a [Value],
    earlier: &'a [EarlierAttempt],
    artifacts: &'a [Value],
    run_attempt: u32,
    run_started_at: DateTime<Utc>,
    plan_digest: &'a str,
    tested_sha: &'a str,
}

impl AttemptContext<'_> {
    // GitHub lists a job that was not rerun under each later attempt number with its original times, so
    // the attempt that ran it is the one whose start precedes those times and lists the same record.
    fn executed_attempt(&self, name: &str, job: &Value) -> Result<u32, String> {
        let times = |job: &Value| -> Result<_, String> {
            Ok((time_at(job, "/started_at")?, time_at(job, "/completed_at")?))
        };
        let ran = times(job).map_err(|e| format!("job {name}: {e}"))?;
        if ran.0 >= self.run_started_at {
            return Ok(self.run_attempt);
        }
        for earlier in self.earlier {
            let listed = unique_job(page(&earlier.jobs, "jobs")?, name, earlier.attempt)?;
            if times(listed).map_err(|e| format!("job {name}: {e}"))? != ran {
                return Err(format!(
                    "job {name} in attempt {} is not the run attempt {} recorded",
                    self.run_attempt, earlier.attempt
                ));
            }
            if ran.0 >= earlier.started_at {
                return Ok(earlier.attempt);
            }
        }
        Err(format!(
            "job {name} started before every attempt of this run"
        ))
    }

    // A worker run in this attempt needs this attempt's artifact; a carried-over worker only the artifact
    // of the attempt that ran it, and only when worker.json names this run/plan/SHA/worker.
    async fn adopt_worker(&self, worker: u32) -> Result<WorkerAdoption, String> {
        let run_attempt = self.run_attempt;
        let name = worker_job(worker);
        let job = unique_job(self.jobs, &name, run_attempt)?;
        job.get("run_attempt")
            .and_then(Value::as_u64)
            .and_then(|a| u32::try_from(a).ok())
            .filter(|a| (1..=run_attempt).contains(a))
            .ok_or_else(|| format!("job {name} reports no valid attempt"))?;
        let attempt = self.executed_attempt(&name, job)?;
        let artifact_name = worker_artifact(worker, attempt);
        let artifact = unique_artifact(self.artifacts, &artifact_name)?;
        let meta = download_json(
            self.gh,
            self.target.slug,
            self.target.run_id,
            &artifact_name,
            "worker.json",
        )
        .await?;
        let want = [
            ("worker", worker.to_string()),
            ("run_attempt", attempt.to_string()),
            ("plan_digest", self.plan_digest.to_owned()),
            ("sha", self.tested_sha.to_owned()),
            ("run_id", self.target.run_id.to_string()),
        ];
        if let Some((key, _)) = want
            .iter()
            .find(|(key, value)| text_field(&meta, key).as_deref() != Some(value.as_str()))
        {
            return Err(format!(
                "{artifact_name}/worker.json {key} does not match this run"
            ));
        }
        Ok(WorkerAdoption {
            worker,
            attempt,
            artifact_id: artifact.id,
            artifact_digest: artifact.digest,
            inherited: attempt < run_attempt,
        })
    }
}

async fn observe_checks(
    gh: &dyn GhExec,
    slug: &str,
    head: &str,
    run_id: u64,
) -> Result<Vec<CheckResult>, String> {
    let response = gh_api(
        gh,
        format!("repos/{slug}/commits/{head}/check-runs?per_page={PAGE}"),
    )
    .await?;
    let marker = format!("/actions/runs/{run_id}/");
    let mut latest: BTreeMap<String, CheckResult> = BTreeMap::new();
    for check in page(&response, "check_runs")? {
        if check.get("head_sha").and_then(Value::as_str) != Some(head)
            || !check
                .get("details_url")
                .and_then(Value::as_str)
                .is_some_and(|url| url.contains(&marker))
        {
            continue;
        }
        let result = CheckResult {
            context: str_at(check, "/name")?.to_owned(),
            check_run_id: check
                .get("id")
                .and_then(Value::as_u64)
                .ok_or("check run has no id")?,
            status: str_at(check, "/status")?.to_owned(),
            conclusion: check
                .get("conclusion")
                .and_then(Value::as_str)
                .map(str::to_owned),
        };
        // The newest check run of a context in this run is its current attempt.
        if latest
            .get(&result.context)
            .is_none_or(|old| old.check_run_id < result.check_run_id)
        {
            latest.insert(result.context.clone(), result);
        }
    }
    Ok(latest.into_values().collect())
}

pub async fn observe_ci(
    gh: &dyn GhExec,
    target: &CiTarget<'_>,
    observed_at: DateTime<Utc>,
) -> Observation<CiObservation> {
    if !valid_slug(target.slug)
        || target.slug != target.pr.repository_identity
        || !is_sha(target.recorded_base)
    {
        return unknown("CI target does not name a valid repository, PR and recorded base");
    }
    let required =
        match observe_required_set(gh, target.slug, &target.pr.base_ref, observed_at).await {
            Observation::Observed { evidence, .. } => evidence,
            Observation::NotYet { reason, .. } | Observation::Unknown { reason, .. } => {
                return unknown(format!("required set: {reason}"));
            }
        };
    match observe_run(gh, target).await {
        Ok(Gate::Go(mut evidence)) => {
            evidence.required = required;
            // Without verified tests only a definite required failure is evidence; the rest stays Unknown.
            if let TestProvenance::Unverified { reason } = &evidence.provenance
                && !matches!(evidence.verdict(), CiVerdict::Failed { .. })
            {
                return unknown(reason.clone());
            }
            Observation::Observed {
                evidence,
                observed_at,
            }
        }
        Ok(Gate::NotYet(reason)) => Observation::NotYet {
            reason,
            observed_at,
        },
        Err(reason) => unknown(reason),
    }
}

#[cfg(test)]
#[path = "ci_tests.rs"]
mod tests;
