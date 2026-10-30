use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::adk_workspace::repository_identity;
use super::execution_pin::IssueObservation;
use super::model::Repository;
use super::profile::Observation;
use crate::services::git::GitCommand;

pub mod ci;

const GH_TIMEOUT: Duration = Duration::from_secs(30);
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

pub type ExecFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

// Read-only gh execution. Production goes through the CLI adapter's timed async run; tests inject output.
pub trait GhExec: Send + Sync {
    fn run<'a>(
        &'a self,
        args: Vec<String>,
        timeout: Duration,
        timeout_context: String,
    ) -> ExecFuture<'a>;
}

pub struct GhCli;

impl GhExec for GhCli {
    fn run<'a>(
        &'a self,
        args: Vec<String>,
        timeout: Duration,
        timeout_context: String,
    ) -> ExecFuture<'a> {
        crate::github::run_async(args, timeout, timeout_context)
    }
}

pub(crate) fn unknown<T>(reason: impl Into<String>) -> Observation<T> {
    Observation::Unknown {
        reason: reason.into(),
        last_observed_at: None,
    }
}

pub(crate) fn is_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

// owner/name only: any other shape could steer `gh api` to a different endpoint.
pub fn valid_slug(slug: &str) -> bool {
    let mut parts = slug.split('/');
    let ok = |part: Option<&str>| {
        part.is_some_and(|p| {
            !p.is_empty()
                && !p.starts_with('.')
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

pub(crate) fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with(['-', '/'])
        && !branch.contains("..")
        && branch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/'))
}

// Timeouts, permission errors and malformed output all end as Err, which callers report as Unknown.
pub(crate) async fn gh_api(gh: &dyn GhExec, path: String) -> Result<Value, String> {
    let context = format!("gh api {path} timed out after {}s", GH_TIMEOUT.as_secs());
    let raw = gh
        .run(vec!["api".into(), path.clone()], GH_TIMEOUT, context)
        .await?;
    serde_json::from_str(&raw).map_err(|e| format!("gh api {path}: unparsable JSON: {e}"))
}

pub(crate) fn str_at<'a>(value: &'a Value, pointer: &str) -> Result<&'a str, String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("response lacks {pointer}"))
}

pub(crate) fn sha_at(value: &Value, pointer: &str) -> Result<String, String> {
    let sha = str_at(value, pointer)?;
    if !is_sha(sha) {
        return Err(format!("{pointer} is not a full commit SHA: {sha:?}"));
    }
    Ok(sha.to_owned())
}

pub(crate) fn time_at(value: &Value, pointer: &str) -> Result<DateTime<Utc>, String> {
    let raw = str_at(value, pointer)?;
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.to_utc())
        .map_err(|e| format!("{pointer} is not RFC 3339: {e}"))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrObservation {
    pub repository: Repository,
    pub repository_identity: String,
    pub number: u64,
    pub state: String,
    pub head_sha: String,
    pub head_ref: String,
    pub base_ref: String,
    pub base_sha: String,
    pub merged: bool,
    // Only set once merged: an open PR's merge_commit_sha is GitHub's test merge, not a landing.
    pub merge_sha: Option<String>,
    pub merged_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

fn parse_pr(
    repository: Repository,
    slug: &str,
    number: u64,
    value: &Value,
) -> Result<PrObservation, String> {
    if value.get("number").and_then(Value::as_u64) != Some(number) {
        return Err(format!("response is not PR #{number}"));
    }
    let base_repo = str_at(value, "/base/repo/full_name")?;
    if !base_repo.eq_ignore_ascii_case(slug) {
        return Err(format!("PR #{number} belongs to {base_repo}, not {slug}"));
    }
    let merged = value
        .get("merged")
        .and_then(Value::as_bool)
        .ok_or("response lacks /merged")?;
    let (merge_sha, merged_at) = if merged {
        (
            Some(sha_at(value, "/merge_commit_sha")?),
            Some(time_at(value, "/merged_at")?),
        )
    } else {
        (None, None)
    };
    Ok(PrObservation {
        repository,
        repository_identity: slug.to_owned(),
        number,
        state: str_at(value, "/state")?.to_owned(),
        head_sha: sha_at(value, "/head/sha")?,
        head_ref: str_at(value, "/head/ref")?.to_owned(),
        base_ref: str_at(value, "/base/ref")?.to_owned(),
        base_sha: sha_at(value, "/base/sha")?,
        merged,
        merge_sha,
        merged_at,
        updated_at: time_at(value, "/updated_at")?,
    })
}

// Observed reports the PR as it is; open, closed and merged are all facts, none of them a landing.
pub async fn observe_pr(
    gh: &dyn GhExec,
    repository: Repository,
    slug: &str,
    number: u64,
    observed_at: DateTime<Utc>,
) -> Observation<PrObservation> {
    if !valid_slug(slug) {
        return unknown(format!("invalid repository slug {slug:?}"));
    }
    match gh_api(gh, format!("repos/{slug}/pulls/{number}"))
        .await
        .and_then(|value| parse_pr(repository, slug, number, &value))
    {
        Ok(evidence) => Observation::Observed {
            evidence,
            observed_at,
        },
        Err(reason) => unknown(reason),
    }
}

// Requirements read: issue view lacks updatedAt, so the REST issue is used. AC locations stay empty here.
pub async fn observe_issue(
    gh: &dyn GhExec,
    repository: Repository,
    slug: &str,
    number: u64,
    observed_at: DateTime<Utc>,
) -> Observation<IssueObservation> {
    if !valid_slug(slug) {
        return unknown(format!("invalid repository slug {slug:?}"));
    }
    let parsed = gh_api(gh, format!("repos/{slug}/issues/{number}"))
        .await
        .and_then(|value| {
            if value.get("number").and_then(Value::as_u64) != Some(number) {
                return Err(format!("response is not issue #{number}"));
            }
            if value.get("pull_request").is_some() {
                return Err(format!("#{number} is a pull request, not an issue"));
            }
            Ok(IssueObservation {
                repository,
                repository_identity: slug.to_owned(),
                issue_number: number,
                issue_url: str_at(&value, "/html_url")?.to_owned(),
                observed_at,
                issue_updated_at: Some(time_at(&value, "/updated_at")?),
                body: value
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                acceptance: Vec::new(),
            })
        });
    match parsed {
        Ok(evidence) => Observation::Observed {
            evidence,
            observed_at,
        },
        Err(reason) => unknown(reason),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MainObservation {
    pub repository_identity: String,
    pub remote: String,
    pub branch: String,
    pub observed_ref: String,
    pub main_sha: String,
    // Taken before the fetch starts, so the fetched tip is at least this fresh.
    pub fetched_at: DateTime<Utc>,
}

fn git_text(repo: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    GitCommand::new()
        .repo(repo)
        .args(args)
        .timeout(timeout)
        .run_text()
        .map(|out| out.trim().to_owned())
        .map_err(|e| e.to_string())
}

// The tip comes from a fetch of the remote branch into a private ref; local main is never read.
pub fn observe_main(
    repo: &Path,
    expected_identity: &str,
    branch: &str,
    fetched_at: DateTime<Utc>,
) -> Observation<MainObservation> {
    if !valid_branch(branch) {
        return unknown(format!("invalid branch {branch:?}"));
    }
    match repository_identity(repo) {
        Ok(identity) if identity == expected_identity => {}
        Ok(identity) => return unknown(format!("origin is {identity}, not {expected_identity}")),
        Err(error) => return unknown(error.to_string()),
    }
    let observed_ref = format!("refs/adk-observe/{branch}");
    let refspec = format!("+refs/heads/{branch}:{observed_ref}");
    let fetched = git_text(
        repo,
        &["fetch", "--no-tags", "--quiet", "origin", &refspec],
        FETCH_TIMEOUT,
    )
    .and_then(|_| {
        git_text(
            repo,
            &[
                "rev-parse",
                "--verify",
                &format!("{observed_ref}^{{commit}}"),
            ],
            GH_TIMEOUT,
        )
    });
    match fetched {
        Ok(main_sha) if is_sha(&main_sha) => Observation::Observed {
            evidence: MainObservation {
                repository_identity: expected_identity.to_owned(),
                remote: "origin".into(),
                branch: branch.to_owned(),
                observed_ref,
                main_sha,
                fetched_at,
            },
            observed_at: fetched_at,
        },
        Ok(other) => unknown(format!("fetched ref resolved to {other:?}")),
        Err(reason) => unknown(reason),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentExpectation {
    pub action_id: String,
    pub host: String,
    pub expected_sha: String,
    pub expected_binary_digest: Option<String>,
    pub previous_incarnation: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentObservation {
    pub action_id: String,
    pub host: String,
    pub expected_sha: String,
    pub actual_source_sha: String,
    pub binary_identity: Option<String>,
    pub boot_incarnation: String,
    pub previous_incarnation: String,
}

impl DeploymentObservation {
    // A new boot running the expected source; a reported digest must match the staged one when expected.
    pub fn is_expected_running(&self, expected_binary_digest: Option<&str>) -> bool {
        self.boot_incarnation != self.previous_incarnation
            && self.actual_source_sha == self.expected_sha
            && expected_binary_digest.is_none_or(|d| self.binary_identity.as_deref() == Some(d))
    }
}

// Reads a detailed health payload. The release manifest head is not running-binary evidence
// (release_source.rs writes it after boot), so only a binary-reported source SHA is accepted.
pub fn observe_deployment(
    expectation: &DeploymentExpectation,
    health: Result<&Value, String>,
    observed_at: DateTime<Utc>,
) -> Observation<DeploymentObservation> {
    let health = match health {
        Ok(health) => health,
        Err(reason) => return unknown(reason),
    };
    match health
        .pointer("/release_source/node_hostname")
        .and_then(Value::as_str)
    {
        Some(host) if host == expectation.host => {}
        Some(host) => return unknown(format!("health answered from {host}")),
        None => return unknown("health does not name its host"),
    }
    let Some(incarnation) = health
        .get("boot_incarnation")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return unknown("health does not report a boot incarnation");
    };
    if incarnation == expectation.previous_incarnation {
        return Observation::NotYet {
            reason: "service still runs the pre-deploy boot incarnation".into(),
            observed_at,
        };
    }
    let Some(actual) = health
        .pointer("/running_binary/source_sha")
        .and_then(Value::as_str)
        .filter(|s| is_sha(s))
    else {
        return unknown("running binary does not report its source SHA");
    };
    Observation::Observed {
        evidence: DeploymentObservation {
            action_id: expectation.action_id.clone(),
            host: expectation.host.clone(),
            expected_sha: expectation.expected_sha.clone(),
            actual_source_sha: actual.to_owned(),
            binary_identity: health
                .pointer("/running_binary/binary_digest")
                .and_then(Value::as_str)
                .map(str::to_owned),
            boot_incarnation: incarnation.to_owned(),
            previous_incarnation: expectation.previous_incarnation.clone(),
        },
        observed_at,
    }
}

#[cfg(test)]
#[path = "adk_observe_tests.rs"]
pub(crate) mod tests;
