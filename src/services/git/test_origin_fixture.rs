//! Temporary bare origin, a pusher clone and a desk clone for base-ref tests.

use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub(crate) fn commit(dir: &Path, message: &str) -> String {
    git(dir, &["commit", "--allow-empty", "-m", message]);
    git(dir, &["rev-parse", "HEAD"])
}

/// `seed` pushes to `origin`; `desk` is the AgentDesk-side clone taken at `main_tip`/`default_tip`.
pub(crate) struct OriginFixture {
    _tmp: tempfile::TempDir,
    pub(crate) origin: PathBuf,
    pub(crate) seed: PathBuf,
    pub(crate) desk: PathBuf,
    pub(crate) main_tip: String,
    pub(crate) default_tip: String,
}

impl OriginFixture {
    /// Origin whose HEAD is `default_branch`; a non-main default also gets a newer commit than main.
    pub(crate) fn new(default_branch: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let origin_str = origin.to_str().unwrap();
        git(
            tmp.path(),
            &["init", "--bare", "-b", default_branch, origin_str],
        );
        let seed = tmp.path().join("seed");
        git(tmp.path(), &["clone", origin_str, seed.to_str().unwrap()]);
        git(&seed, &["checkout", "-b", "main"]);
        let main_tip = commit(&seed, "base");
        git(&seed, &["push", "origin", "main"]);
        let mut default_tip = main_tip.clone();
        if default_branch != "main" {
            git(&seed, &["checkout", "-b", default_branch]);
            default_tip = commit(&seed, "default branch work");
            git(&seed, &["push", "origin", default_branch]);
        }
        let unique = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let desk = tmp.path().join(format!("desk{unique}"));
        git(tmp.path(), &["clone", origin_str, desk.to_str().unwrap()]);
        Self {
            _tmp: tmp,
            origin,
            seed,
            desk,
            main_tip,
            default_tip,
        }
    }

    pub(crate) fn desk_dir(&self) -> &str {
        self.desk.to_str().unwrap()
    }

    /// Commits on the seed's current branch and pushes it, returning the new tip.
    pub(crate) fn push_new_commit(&self, message: &str) -> String {
        let tip = commit(&self.seed, message);
        git(&self.seed, &["push", "origin", "HEAD"]);
        tip
    }

    /// Makes every desk fetch from origin wait `seconds` before it starts transferring.
    pub(crate) fn delay_desk_fetches(&self, seconds: u32) {
        let upload_pack = format!("sleep {seconds}; git-upload-pack");
        git(
            &self.desk,
            &["config", "remote.origin.uploadpack", &upload_pack],
        );
    }
}
