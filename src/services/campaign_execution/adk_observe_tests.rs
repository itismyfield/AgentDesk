use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use serde_json::json;

use super::*;

pub(crate) const SLUG: &str = "itismyfield/AgentDesk";

pub(crate) fn at(minute: u32) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&format!("2026-10-30T10:{minute:02}:00Z"))
        .unwrap()
        .to_utc()
}

pub(crate) fn sha(c: char) -> String {
    std::iter::repeat_n(c, 40).collect()
}

// Canned gh: `api <path>` answers from `api`, `run download --name N --dir D` writes `files[N]` into D.
#[derive(Default)]
pub(crate) struct FakeGh {
    pub api: BTreeMap<String, Result<Value, String>>,
    pub files: BTreeMap<String, (String, Value)>,
    pub calls: Mutex<Vec<Vec<String>>>,
}

impl FakeGh {
    pub fn with(mut self, path: &str, value: Value) -> Self {
        self.api.insert(path.into(), Ok(value));
        self
    }

    pub fn failing(mut self, path: &str, error: &str) -> Self {
        self.api.insert(path.into(), Err(error.into()));
        self
    }

    pub fn file(mut self, artifact: &str, rel: &str, value: Value) -> Self {
        self.files.insert(artifact.into(), (rel.into(), value));
        self
    }
}

impl GhExec for FakeGh {
    fn run<'a>(
        &'a self,
        args: Vec<String>,
        _timeout: Duration,
        timeout_context: String,
    ) -> ExecFuture<'a> {
        self.calls.lock().unwrap().push(args.clone());
        let arg = |flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1))
                .cloned()
                .unwrap_or_default()
        };
        let result = match args.first().map(String::as_str) {
            Some("api") => match self.api.get(&args[1]) {
                Some(Ok(value)) => Ok(value.to_string()),
                Some(Err(error)) if error == "timeout" => Err(timeout_context),
                Some(Err(error)) => Err(error.clone()),
                None => Err(format!("gh exited with 1: HTTP 404 {}", args[1])),
            },
            Some("run") => match self.files.get(&arg("--name")) {
                Some((rel, value)) => {
                    let path = PathBuf::from(arg("--dir")).join(rel);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, value.to_string()).unwrap();
                    Ok(String::new())
                }
                None => Err("gh exited with 1: no artifact".into()),
            },
            _ => Err("unexpected gh call".into()),
        };
        Box::pin(async move { result })
    }
}

pub(crate) fn pr_json(head: &str, merged: Option<&str>) -> Value {
    json!({
        "number": 77, "state": if merged.is_some() { "closed" } else { "open" },
        "head": {"sha": head, "ref": "feat/x"},
        "base": {"sha": sha('b'), "ref": "main", "repo": {"full_name": SLUG}},
        "merged": merged.is_some(),
        "merge_commit_sha": merged.map(str::to_owned).unwrap_or_else(|| sha('e')),
        "merged_at": merged.map(|_| "2026-10-30T10:05:00Z"),
        "updated_at": "2026-10-30T10:06:00Z",
    })
}

#[tokio::test]
async fn open_pr_test_merge_sha_is_not_a_landing() {
    let gh = FakeGh::default().with(
        "repos/itismyfield/AgentDesk/pulls/77",
        pr_json(&sha('a'), None),
    );
    let observed = observe_pr(&gh, Repository::Adk, SLUG, 77, at(10)).await;
    let pr = observed.observed_evidence().expect("observed");
    assert!(!pr.merged);
    assert_eq!(pr.merge_sha, None);
    assert_eq!(pr.head_sha, sha('a'));

    let gh = FakeGh::default().with(
        "repos/itismyfield/AgentDesk/pulls/77",
        pr_json(&sha('a'), Some(&sha('c'))),
    );
    let observed = observe_pr(&gh, Repository::Adk, SLUG, 77, at(10)).await;
    assert_eq!(
        observed.observed_evidence().unwrap().merge_sha,
        Some(sha('c'))
    );
}

#[tokio::test]
async fn pr_errors_and_foreign_targets_are_unknown() {
    let mut foreign = pr_json(&sha('a'), None);
    foreign["base"]["repo"]["full_name"] = json!("someone/AgentDesk");
    for gh in [
        FakeGh::default().with("repos/itismyfield/AgentDesk/pulls/77", foreign),
        FakeGh::default().failing("repos/itismyfield/AgentDesk/pulls/77", "timeout"),
        FakeGh::default().failing(
            "repos/itismyfield/AgentDesk/pulls/77",
            "gh exited with 1: HTTP 403 Resource not accessible",
        ),
    ] {
        let observed = observe_pr(&gh, Repository::Adk, SLUG, 77, at(10)).await;
        assert!(
            matches!(observed, Observation::Unknown { .. }),
            "{observed:?}"
        );
    }
    let gh = FakeGh::default();
    let observed = observe_pr(&gh, Repository::Adk, "x/../y", 77, at(10)).await;
    assert!(matches!(observed, Observation::Unknown { .. }));
    assert!(gh.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn issue_observation_keeps_update_time_and_rejects_prs() {
    let issue = json!({"number": 7438, "html_url": "https://github.com/itismyfield/AgentDesk/issues/7438",
        "updated_at": "2026-10-30T09:00:00Z", "body": "AC1 works"});
    let gh = FakeGh::default().with("repos/itismyfield/AgentDesk/issues/7438", issue.clone());
    let observed = observe_issue(&gh, Repository::Adk, SLUG, 7438, at(10)).await;
    let evidence = observed.observed_evidence().expect("observed");
    assert_eq!(
        evidence.issue_updated_at,
        Some(
            DateTime::parse_from_rfc3339("2026-10-30T09:00:00Z")
                .unwrap()
                .to_utc()
        )
    );
    assert_eq!(evidence.body, "AC1 works");

    let mut pr = issue;
    pr["pull_request"] = json!({});
    let gh = FakeGh::default().with("repos/itismyfield/AgentDesk/issues/7438", pr);
    let observed = observe_issue(&gh, Repository::Adk, SLUG, 7438, at(10)).await;
    assert!(matches!(observed, Observation::Unknown { .. }));
}

pub(crate) fn sh_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

pub(crate) fn commit(dir: &Path, file: &str, content: &str) -> String {
    std::fs::write(dir.join(file), content).unwrap();
    sh_git(dir, &["add", "."]);
    sh_git(dir, &["commit", "-q", "-m", file]);
    sh_git(dir, &["rev-parse", "HEAD"])
}

// A bare "remote" and a work clone; its identity is the bare path (not a GitHub URL).
pub(crate) fn remote_and_clone() -> (tempfile::TempDir, PathBuf, String, String) {
    let temp = tempfile::tempdir().unwrap();
    let bare = temp.path().join("origin.git");
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh_git(
        temp.path(),
        &["init", "-q", "--bare", "-b", "main", "origin.git"],
    );
    sh_git(&work, &["init", "-q", "-b", "main"]);
    sh_git(&work, &["remote", "add", "origin", &bare.to_string_lossy()]);
    let base = commit(&work, "base.txt", "base");
    sh_git(&work, &["push", "-q", "origin", "main"]);
    let identity = repository_identity(&work).unwrap();
    (temp, work, identity, base)
}

#[test]
fn main_observation_reads_the_remote_not_local_main() {
    let (_temp, work, identity, base) = remote_and_clone();
    let local_only = commit(&work, "local.txt", "unpushed");
    let observed = observe_main(&work, &identity, "main", at(10));
    let main = observed.observed_evidence().expect("observed");
    assert_eq!(main.main_sha, base);
    assert_ne!(main.main_sha, local_only);
    assert_eq!(main.observed_ref, "refs/adk-observe/main");
    assert_eq!(main.fetched_at, at(10));

    assert!(matches!(
        observe_main(&work, "someone/AgentDesk", "main", at(10)),
        Observation::Unknown { .. }
    ));
    assert!(matches!(
        observe_main(&work, &identity, "--upload-pack=x", at(10)),
        Observation::Unknown { .. }
    ));
    assert!(matches!(
        observe_main(&work, &identity, "absent", at(10)),
        Observation::Unknown { .. }
    ));
}

fn expectation() -> DeploymentExpectation {
    DeploymentExpectation {
        action_id: "deploy-1".into(),
        host: "mac-mini".into(),
        expected_sha: sha('a'),
        expected_binary_digest: Some("sha256:new".into()),
        previous_incarnation: "boot-old".into(),
    }
}

fn health(incarnation: &str, source: Option<&str>) -> Value {
    let mut health = json!({
        "boot_incarnation": incarnation,
        "release_source": {"node_hostname": "mac-mini", "deployed_repo_head": sha('a')},
    });
    if let Some(source) = source {
        health["running_binary"] = json!({"source_sha": source, "binary_digest": "sha256:new"});
    }
    health
}

#[test]
fn deployment_needs_a_new_boot_reporting_the_expected_source() {
    let expect = expectation();
    let ok = observe_deployment(&expect, Ok(&health("boot-new", Some(&sha('a')))), at(10));
    let ok = ok.observed_evidence().expect("observed");
    assert!(ok.is_expected_running(expect.expected_binary_digest.as_deref()));
    assert!(!ok.is_expected_running(Some("sha256:other")));

    // Observed is not success: the new boot runs another source.
    let wrong = observe_deployment(&expect, Ok(&health("boot-new", Some(&sha('d')))), at(10));
    let wrong = wrong.observed_evidence().expect("observed");
    assert_eq!(wrong.actual_source_sha, sha('d'));
    assert!(!wrong.is_expected_running(None));

    assert!(matches!(
        observe_deployment(&expect, Ok(&health("boot-old", Some(&sha('a')))), at(10)),
        Observation::NotYet { .. }
    ));
    // The release manifest head alone is not running-binary evidence.
    assert!(matches!(
        observe_deployment(&expect, Ok(&health("boot-new", None)), at(10)),
        Observation::Unknown { .. }
    ));
    let mut other_host = health("boot-new", Some(&sha('a')));
    other_host["release_source"]["node_hostname"] = json!("other");
    assert!(matches!(
        observe_deployment(&expect, Ok(&other_host), at(10)),
        Observation::Unknown { .. }
    ));
    assert!(matches!(
        observe_deployment(&expect, Err("connect refused".into()), at(10)),
        Observation::Unknown { .. }
    ));
}
