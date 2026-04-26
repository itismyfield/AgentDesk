use chrono::Utc;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::registry::{
    MigrationHistoryEntry, MigrationState, ProviderCliChannel, ProviderCliMigrationState,
    update_strategy_for,
};
use super::snapshot::snapshot_current_channel;

const UPGRADE_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const UPGRADE_OUTPUT_LIMIT_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub enum UpgradeError {
    NoStrategy,
    CurrentSnapshotRequired,
    PreviousPreservationRequired {
        reason: String,
    },
    UpgradeCommandFailed {
        exit_code: Option<i32>,
        stderr: String,
    },
    UpgradeCommandTimedOut {
        seconds: u64,
    },
    VersionUnknown {
        pre_version: String,
        post_version: String,
    },
    VersionUnchanged {
        version: String,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for UpgradeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpgradeError::NoStrategy => write!(f, "update_strategy_missing"),
            UpgradeError::CurrentSnapshotRequired => write!(f, "current_snapshot_required"),
            UpgradeError::PreviousPreservationRequired { reason } => {
                write!(f, "previous_preservation_required: {reason}")
            }
            UpgradeError::UpgradeCommandFailed { exit_code, stderr } => {
                write!(f, "upgrade_command_failed(exit={exit_code:?}): {stderr}")
            }
            UpgradeError::UpgradeCommandTimedOut { seconds } => {
                write!(f, "upgrade_command_timed_out_after_{seconds}s")
            }
            UpgradeError::VersionUnknown {
                pre_version,
                post_version,
            } => write!(
                f,
                "upgrade_version_unknown(pre={pre_version:?}, post={post_version:?})"
            ),
            UpgradeError::VersionUnchanged { version } => {
                write!(f, "upgrade_version_unchanged: {version}")
            }
            UpgradeError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl From<std::io::Error> for UpgradeError {
    fn from(e: std::io::Error) -> Self {
        UpgradeError::Io(e)
    }
}

pub struct UpgradeResult {
    pub pre_version: String,
    pub post_version: String,
    pub candidate_channel: ProviderCliChannel,
    pub evidence: HashMap<String, String>,
}

struct UpgradeCommandOutput {
    success: bool,
    exit_code: Option<i32>,
    stderr: String,
}

fn drain_limited_output<R>(mut reader: R) -> Vec<u8>
where
    R: Read + Send + 'static,
{
    let mut output = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = UPGRADE_OUTPUT_LIMIT_BYTES.saturating_sub(output.len());
                if remaining > 0 {
                    output.extend_from_slice(&buf[..n.min(remaining)]);
                }
            }
            Err(_) => break,
        }
    }
    output
}

fn run_upgrade_command(argv: &[&str]) -> Result<UpgradeCommandOutput, UpgradeError> {
    let (cmd, args) = argv.split_first().expect("command_argv is non-empty");
    let mut command = Command::new(cmd);
    command.args(args);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    crate::services::platform::binary_resolver::apply_runtime_path(&mut command);
    crate::services::process::configure_child_process_group(&mut command);

    let mut child = command.spawn().map_err(UpgradeError::Io)?;
    let stdout_reader = child
        .stdout
        .take()
        .map(|reader| std::thread::spawn(move || drain_limited_output(reader)));
    let stderr_reader = child
        .stderr
        .take()
        .map(|reader| std::thread::spawn(move || drain_limited_output(reader)));

    let deadline = Instant::now() + UPGRADE_COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                crate::services::process::kill_child_tree(&mut child);
                return Err(UpgradeError::UpgradeCommandTimedOut {
                    seconds: UPGRADE_COMMAND_TIMEOUT.as_secs(),
                });
            }
            Err(error) => {
                crate::services::process::kill_child_tree(&mut child);
                return Err(UpgradeError::Io(error));
            }
        }
    };

    let _stdout = stdout_reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    let stderr = stderr_reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();

    Ok(UpgradeCommandOutput {
        success: status.success(),
        exit_code: status.code(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

fn version_is_unknown(version: &str) -> bool {
    let value = version.trim();
    value.is_empty() || value.eq_ignore_ascii_case("unknown")
}

/// Run the allowlisted update strategy for `provider`.
///
/// Guards (in order):
/// 1. Update strategy must exist in `PROVIDER_UPDATE_STRATEGIES`.
/// 2. `current_snapshot` must be provided (caller must snapshot before calling).
/// 3. When `mutates_in_place`, a previous-preservation path must be supplied OR
///    `skip_previous_preservation` must be `true` (operator confirmed).
/// 4. Post-upgrade version must differ from pre-upgrade version.
pub fn run_upgrade(
    provider: &str,
    current_snapshot: &ProviderCliChannel,
    previous_preservation_path: Option<&Path>,
    skip_previous_preservation: bool,
) -> Result<UpgradeResult, UpgradeError> {
    let strategy = update_strategy_for(provider).ok_or(UpgradeError::NoStrategy)?;

    // Guard: mutates_in_place requires previous preservation.
    if strategy.mutates_in_place && !skip_previous_preservation {
        match previous_preservation_path {
            Some(dest) => {
                let src = std::path::Path::new(&current_snapshot.canonical_path);
                if src.exists() {
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::copy(src, dest)?;
                } else {
                    return Err(UpgradeError::PreviousPreservationRequired {
                        reason: format!(
                            "source binary not found at {}",
                            current_snapshot.canonical_path
                        ),
                    });
                }
            }
            None => {
                return Err(UpgradeError::PreviousPreservationRequired {
                    reason: "mutates_in_place=true but no preservation path provided".to_string(),
                });
            }
        }
    }

    let pre_version = current_snapshot.version.clone();

    // Run the update command.
    let argv = strategy.command_argv;
    let output = run_upgrade_command(argv)?;

    if !output.success {
        return Err(UpgradeError::UpgradeCommandFailed {
            exit_code: output.exit_code,
            stderr: output.stderr,
        });
    }

    // Re-snapshot after upgrade to get new version.
    let post_channel =
        snapshot_current_channel(provider).ok_or_else(|| UpgradeError::UpgradeCommandFailed {
            exit_code: None,
            stderr: "binary not found after upgrade".to_string(),
        })?;

    let post_version = post_channel.version.clone();

    // Guard: version must change (for mutates_in_place providers).
    if strategy.mutates_in_place {
        if version_is_unknown(&pre_version) || version_is_unknown(&post_version) {
            return Err(UpgradeError::VersionUnknown {
                pre_version,
                post_version,
            });
        }
        if pre_version == post_version {
            return Err(UpgradeError::VersionUnchanged {
                version: post_version,
            });
        }
    }

    let mut evidence = HashMap::new();
    evidence.insert("pre_version".to_string(), pre_version.clone());
    evidence.insert("post_version".to_string(), post_version.clone());
    evidence.insert("strategy".to_string(), strategy.install_source.to_string());
    evidence.insert("command".to_string(), strategy.command_argv.join(" "));

    Ok(UpgradeResult {
        pre_version,
        post_version,
        candidate_channel: post_channel,
        evidence,
    })
}

/// Build the initial `ProviderCliMigrationState` in `Planned` state.
pub fn new_migration_state(
    provider: &str,
    current: ProviderCliChannel,
) -> ProviderCliMigrationState {
    let now = Utc::now();
    ProviderCliMigrationState {
        schema_version: 1,
        provider: provider.to_string(),
        state: MigrationState::Planned,
        selected_agent_id: None,
        current_channel: Some(current),
        candidate_channel: None,
        rollback_target: None,
        started_at: now,
        updated_at: now,
        history: vec![],
    }
}

/// Transition `state` to `next`, recording history. Returns `Err` on invalid transition.
pub fn transition(
    state: &mut ProviderCliMigrationState,
    next: MigrationState,
    evidence: Option<String>,
) -> Result<(), String> {
    if !is_valid_transition(&state.state, &next) {
        return Err(format!(
            "invalid transition {:?} -> {:?}",
            state.state, next
        ));
    }
    let entry = MigrationHistoryEntry {
        from_state: state.state.clone(),
        to_state: next.clone(),
        transitioned_at: Utc::now(),
        evidence,
    };
    state.history.push(entry);
    state.state = next;
    state.updated_at = Utc::now();
    Ok(())
}

fn is_valid_transition(from: &MigrationState, to: &MigrationState) -> bool {
    use MigrationState::*;
    if matches!(from, RolledBack | Failed) {
        return from == to;
    }

    matches!(
        (from, to),
        (Planned, CurrentSnapshotted)
            | (CurrentSnapshotted, SmokeCurrentPassed)
            | (SmokeCurrentPassed, PreviousPreserved)
            | (PreviousPreserved, UpgradePlanned)
            | (UpgradePlanned, UpgradeSucceeded)
            | (UpgradeSucceeded, CandidateDiscovered)
            | (CandidateDiscovered, SmokeCandidatePassed)
            | (SmokeCandidatePassed, CanarySelected)
            | (CanarySelected, CanarySessionSafeEnding)
            | (CanarySessionSafeEnding, CanarySessionRecreated)
            | (CanarySessionRecreated, CanaryActive)
            | (CanaryActive, CanaryPassed)
            | (CanaryPassed, AwaitingOperatorPromote)
            | (AwaitingOperatorPromote, ProviderSessionsSafeEnding)
            | (ProviderSessionsSafeEnding, ProviderSessionsRecreated)
            | (ProviderSessionsRecreated, ProviderAgentsMigrated)
            // Rollback is allowed from most states
            | (_, RolledBack)
            // Failed is a terminal state reachable from anywhere
            | (_, Failed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider_cli::registry::MigrationState;

    fn make_channel() -> ProviderCliChannel {
        ProviderCliChannel {
            path: "/usr/local/bin/codex".to_string(),
            canonical_path: "/usr/local/bin/codex".to_string(),
            version: "1.0.0".to_string(),
            version_output: None,
            source: "current_path".to_string(),
            checked_at: chrono::Utc::now(),
            evidence: Default::default(),
        }
    }

    #[test]
    fn new_migration_state_is_planned() {
        let state = new_migration_state("codex", make_channel());
        assert_eq!(state.state, MigrationState::Planned);
        assert_eq!(state.provider, "codex");
    }

    #[test]
    fn valid_transition_succeeds() {
        let mut state = new_migration_state("codex", make_channel());
        transition(&mut state, MigrationState::CurrentSnapshotted, None).unwrap();
        assert_eq!(state.state, MigrationState::CurrentSnapshotted);
        assert_eq!(state.history.len(), 1);
    }

    #[test]
    fn invalid_transition_returns_error() {
        let mut state = new_migration_state("codex", make_channel());
        let result = transition(&mut state, MigrationState::CanaryPassed, None);
        assert!(result.is_err());
    }

    #[test]
    fn rollback_always_valid() {
        let mut state = new_migration_state("codex", make_channel());
        transition(&mut state, MigrationState::CurrentSnapshotted, None).unwrap();
        transition(&mut state, MigrationState::SmokeCurrentPassed, None).unwrap();
        transition(
            &mut state,
            MigrationState::RolledBack,
            Some("test".to_string()),
        )
        .unwrap();
        assert_eq!(state.state, MigrationState::RolledBack);
    }

    #[test]
    fn terminal_states_do_not_transition_forward() {
        let mut rolled_back = new_migration_state("codex", make_channel());
        transition(&mut rolled_back, MigrationState::RolledBack, None).unwrap();
        assert!(transition(&mut rolled_back, MigrationState::CurrentSnapshotted, None).is_err());
        assert!(transition(&mut rolled_back, MigrationState::Failed, None).is_err());

        let mut failed = new_migration_state("codex", make_channel());
        transition(&mut failed, MigrationState::Failed, None).unwrap();
        assert!(transition(&mut failed, MigrationState::RolledBack, None).is_err());
    }

    #[test]
    fn upgrade_error_no_strategy_for_unknown_provider() {
        let channel = make_channel();
        let result = run_upgrade("__unknown__", &channel, None, false);
        assert!(matches!(result, Err(UpgradeError::NoStrategy)));
    }

    #[test]
    fn upgrade_error_mutates_in_place_without_preservation() {
        // All 4 providers are mutates_in_place=true; codex is easiest to test.
        let channel = make_channel();
        // No preservation path, skip=false → should fail guard.
        let result = run_upgrade("codex", &channel, None, false);
        assert!(matches!(
            result,
            Err(UpgradeError::PreviousPreservationRequired { .. })
        ));
    }
}
