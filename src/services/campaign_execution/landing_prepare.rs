use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use chrono::{DateTime, Utc};

use crate::services::process::{configure_child_process_group, wait_with_output_timeout};

use super::adk_workspace::{self, WorkspaceError};
use super::verification_run::digest_of;
use super::verification_spec::landing_prepush_mode;
use crate::domain::campaign::landing::{LandingPreflight, LandingTarget, PreflightMode};
use crate::domain::campaign::model::Repository;

#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("checkout did not match the clean pinned landing head")]
    Checkout,
    #[error("landing check did not complete successfully: {0}")]
    Check(&'static str),
    #[error(transparent)]
    Encoding(#[from] serde_json::Error),
}

/// A runner for the installed AgentDesk skill on the fixed host.
#[derive(Clone)]
pub struct LandingChecks {
    scripts: PathBuf,
}

impl LandingChecks {
    pub fn new(scripts: &Path) -> io::Result<Self> {
        if !scripts.is_absolute() {
            return Err(io::Error::other(
                "landing check tools must use an absolute path",
            ));
        }
        Ok(Self {
            scripts: scripts.to_path_buf(),
        })
    }

    pub fn run(
        &self,
        checkout: &Path,
        host: &str,
        target: &LandingTarget,
        mode: PreflightMode,
        deadline: DateTime<Utc>,
    ) -> Result<LandingPreflight, PreflightError> {
        require_checkout(checkout, host, target)?;

        let log = tempfile::NamedTempFile::new()?;
        let mut prepush = self.command(checkout, "/bin/bash")?;
        prepush
            .env("PREPUSH_BASE", &target.base_ref)
            .arg(self.scripts.join("prepush_gates.sh"))
            .arg(checkout)
            .args(landing_prepush_mode(mode));
        let output = run(&mut prepush, "prepush", deadline)?;
        let expected = match mode {
            PreflightMode::Full => "PREPUSH_OK".into(),
            PreflightMode::PreTlc => format!("PREPUSH_STATIC_OK {}", target.head_sha),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.lines().any(|line| line == expected)
            || mode == PreflightMode::Full
                && !stdout.lines().any(|line| {
                    line.strip_prefix("PREPUSH_TREE_OK ") == Some(target.head_sha.as_str())
                })
        {
            return Err(PreflightError::Check("prepush receipt"));
        }
        require_checkout(checkout, host, target)?;

        let source = run(
            self.command(checkout, "python3")?
                .arg(self.scripts.join("source_scan_readers.py"))
                .arg(checkout)
                .arg("--base")
                .arg(format!("origin/{}", target.base_ref))
                .arg("--run")
                .arg(log.path()),
            "source scan",
            deadline,
        )?;
        if !source_scan_passed(&source.stdout) {
            return Err(PreflightError::Check("source scan results"));
        }
        require_checkout(checkout, host, target)?;
        let compile = run(
            self.command(checkout, "cargo")?
                .args(["check", "--lib", "--tests"]),
            "compile",
            deadline,
        )?;
        require_checkout(checkout, host, target)?;
        let hashes = [output, source, compile].map(|result| (result.stdout, result.stderr));
        Ok(LandingPreflight {
            head_sha: target.head_sha.clone(),
            mode,
            transcript_digest: digest_of(&(hashes, std::fs::read(log.path())?))?,
        })
    }

    fn command(&self, checkout: &Path, program: &str) -> io::Result<Command> {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(self.scripts.join("cargo-shim"))
                .chain(std::env::split_paths(&inherited)),
        )
        .map_err(io::Error::other)?;
        let mut command = Command::new(program);
        command
            .current_dir(checkout)
            .env("PATH", path)
            .stdin(Stdio::null());
        for name in [
            "TMUX",
            "TMUX_PANE",
            "AGENTDESK_ROOT_DIR",
            "AGENTDESK_CONFIG",
        ] {
            command.env_remove(name);
        }
        Ok(command)
    }
}

fn run(
    command: &mut Command,
    label: &'static str,
    deadline: DateTime<Utc>,
) -> Result<Output, PreflightError> {
    let remaining = (deadline - Utc::now())
        .to_std()
        .map_err(|_| PreflightError::Check(label))?;
    configure_child_process_group(command);
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let output = wait_with_output_timeout(child, remaining, label)
        .map_err(|_| PreflightError::Check(label))?;
    if !output.status.success() {
        return Err(PreflightError::Check(label));
    }
    Ok(output)
}

fn source_scan_passed(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout).lines().any(|line| {
        line == "SOURCE_SCAN no module reads a changed file"
            || line.starts_with("SOURCE_SCAN modules=")
                && ["failed=0", "pg_not_run=0", "ledger_known=0", "cargo_rc=0"]
                    .iter()
                    .all(|word| line.split_whitespace().any(|actual| actual == *word))
                && line
                    .split_whitespace()
                    .find_map(|word| word.strip_prefix("passed="))
                    .and_then(|count| count.parse::<u64>().ok())
                    .is_some_and(|count| count > 0)
    })
}

pub(super) fn require_checkout(
    checkout: &Path,
    host: &str,
    target: &LandingTarget,
) -> Result<(), PreflightError> {
    if !checkout.is_absolute() {
        return Err(PreflightError::Checkout);
    }
    let state = adk_workspace::observe_checkout(Repository::Adk, host, checkout, Utc::now())?;
    if state.repository_identity != target.repository
        || state.head_sha != target.head_sha
        || !state.clean()
    {
        return Err(PreflightError::Checkout);
    }
    Ok(())
}

#[cfg(test)]
#[path = "landing_prepare_tests.rs"]
pub(super) mod tests;
