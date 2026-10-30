use std::path::PathBuf;
use std::process::Command;

use super::*;
use crate::services::campaign_execution::verification_run::VerificationSubjectRef;

fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-29T08:00:00Z")
        .unwrap()
        .to_utc()
}

fn sh_git(dir: &Path, args: &[&str]) -> String {
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

fn commit(dir: &Path, file: &str, content: &str) -> String {
    std::fs::write(dir.join(file), content).unwrap();
    sh_git(dir, &["add", "."]);
    sh_git(dir, &["commit", "-q", "-m", file]);
    sh_git(dir, &["rev-parse", "HEAD"])
}

// A real repository with origin set to the canonical remote; nothing is fetched.
fn repo(remote: &str) -> (tempfile::TempDir, PathBuf, String) {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    sh_git(&repo, &["init", "-q", "-b", "main"]);
    sh_git(&repo, &["remote", "add", "origin", remote]);
    let h0 = commit(&repo, "base.txt", "h0");
    (temp, repo, h0)
}

fn key(node: &str) -> AdkWorkspaceKey {
    AdkWorkspaceKey {
        campaign_id: "campaign".into(),
        run_id: "run".into(),
        node_id: node.into(),
        series: 1,
    }
}

fn subject(base: &str, output: &str) -> BoundSubject {
    BoundSubject {
        reference: VerificationSubjectRef::SealedOutput {
            owner_attempt: "attempt-a".into(),
            seal_id: "seal-1".into(),
        },
        repository: Repository::Adk,
        base_sha: base.into(),
        output_sha: output.into(),
    }
}

const REMOTE: &str = "git@github.com:itismyfield/AgentDesk.git";

#[test]
fn two_nodes_on_one_issue_get_separate_editing_verification_and_target_spaces() {
    let (temp, repo, h0) = repo(REMOTE);
    let root = temp.path().join("root");
    let (a, b) = (key("a"), key("b"));
    assert_ne!(a.editing_dir(&root), b.editing_dir(&root));
    assert_ne!(a.target_dir(&root, "v-1"), b.target_dir(&root, "v-1"));
    assert_ne!(
        a.verification_dir(&root, "v-1"),
        a.verification_dir(&root, "v-2")
    );
    assert_ne!(a.target_dir(&root, "v-1"), a.target_dir(&root, "v-2"));
    let editing_a = prepare_editing(&repo, &root, &a, &h0, "host", at()).unwrap();
    let editing_b = prepare_editing(&repo, &root, &b, &h0, "host", at()).unwrap();
    assert_ne!(editing_a.path, editing_b.path);
    assert_eq!(editing_a.head_sha, h0);
    assert_eq!(editing_a.repository_identity, "itismyfield/AgentDesk");
    assert!(editing_a.clean());
    let again = prepare_editing(&repo, &root, &a, &h0, "host", at()).unwrap();
    assert_eq!(again.path, editing_a.path);
    assert!(matches!(
        prepare_editing(
            &repo,
            &root,
            &a,
            "0000000000000000000000000000000000000000",
            "host",
            at()
        ),
        Err(WorkspaceError::WrongHead(_))
    ));
    let mut bad = key("a");
    bad.series = 0;
    assert!(matches!(bad.validate(), Err(WorkspaceError::InvalidKey(_))));
}

#[test]
fn sealed_output_stays_verified_at_h1_while_editing_moves_on_to_h2() {
    let (temp, repo, h0) = repo(REMOTE);
    let root = temp.path().join("root");
    let node = key("a");
    prepare_editing(&repo, &root, &node, &h0, "host", at()).unwrap();
    let editing_dir = node.editing_dir(&root);
    let h1 = commit(&editing_dir, "work.rs", "h1");
    let editing = observe_checkout(Repository::Adk, "host", &editing_dir, at()).unwrap();
    assert_eq!(editing.head_sha, h1);
    let sealed = subject(&h0, &h1);
    let verification =
        prepare_verification(&repo, &root, &node, &sealed, "host", "v-1", at()).unwrap();
    assert_eq!(verification.head_sha, h1);
    assert_ne!(verification.path, editing.path);
    let candidate = Candidate {
        repository: Repository::Adk,
        base_sha: h0.clone(),
        head_sha: h1.clone(),
    };
    let workspace = seal_workspace(&candidate, &editing, &verification).unwrap();
    assert_eq!(
        workspace.verification_kind,
        VerificationCheckoutKind::Isolated
    );
    let h2 = commit(&editing_dir, "more.rs", "h2");
    let moved = observe_checkout(Repository::Adk, "host", &editing_dir, at()).unwrap();
    assert_eq!(moved.head_sha, h2);
    let still =
        observe_checkout(Repository::Adk, "host", Path::new(&verification.path), at()).unwrap();
    assert_eq!(still.head_sha, h1);
    verification_checkout_current(&sealed, &still, "itismyfield/AgentDesk").unwrap();
    assert!(matches!(
        seal_workspace(&candidate, &moved, &still),
        Err(WorkspaceError::Contract(ContractError::CandidateMismatch))
    ));
    let reprepared =
        prepare_verification(&repo, &root, &node, &sealed, "host", "v-1", at()).unwrap();
    assert_eq!(reprepared, still);
}

#[test]
fn other_remote_or_dirty_verification_checkout_is_rejected() {
    let (temp, repo, h0) = repo(REMOTE);
    let root = temp.path().join("root");
    let node = key("a");
    prepare_editing(&repo, &root, &node, &h0, "host", at()).unwrap();
    let editing_dir = node.editing_dir(&root);
    let h1 = commit(&editing_dir, "work.rs", "h1");
    let sealed = subject(&h0, &h1);
    let verification =
        prepare_verification(&repo, &root, &node, &sealed, "host", "v-1", at()).unwrap();
    std::fs::write(Path::new(&verification.path).join("scratch.txt"), "x").unwrap();
    let dirty =
        observe_checkout(Repository::Adk, "host", Path::new(&verification.path), at()).unwrap();
    assert!(dirty.untracked_dirty && !dirty.tracked_dirty);
    assert_eq!(
        verification_checkout_current(&sealed, &dirty, "itismyfield/AgentDesk"),
        Err(WorkspaceError::Dirty)
    );
    std::fs::write(Path::new(&verification.path).join("work.rs"), "edited").unwrap();
    let tracked =
        observe_checkout(Repository::Adk, "host", Path::new(&verification.path), at()).unwrap();
    assert!(tracked.tracked_dirty);
    let fork = temp.path().join("fork");
    sh_git(
        temp.path(),
        &[
            "clone",
            "-q",
            repo.to_str().unwrap(),
            fork.to_str().unwrap(),
        ],
    );
    sh_git(
        &fork,
        &[
            "remote",
            "set-url",
            "origin",
            "git@github.com:someone-else/AgentDesk.git",
        ],
    );
    let other = observe_checkout(Repository::Adk, "host", &fork, at()).unwrap();
    assert_eq!(other.head_sha, h0);
    assert!(matches!(
        verification_checkout_current(&subject(&h0, &h0), &other, "itismyfield/AgentDesk"),
        Err(WorkspaceError::RepositoryMismatch(_))
    ));
    let editing = observe_checkout(Repository::Adk, "host", &editing_dir, at()).unwrap();
    let candidate = Candidate {
        repository: Repository::Adk,
        base_sha: h0.clone(),
        head_sha: h1.clone(),
    };
    assert_eq!(
        seal_workspace(&candidate, &editing, &dirty),
        Err(WorkspaceError::Dirty)
    );
    let mut foreign = editing.clone();
    foreign.repository_identity = "someone-else/AgentDesk".into();
    assert!(matches!(
        seal_workspace(&candidate, &foreign, &editing),
        Err(WorkspaceError::RepositoryMismatch(_))
    ));
}
