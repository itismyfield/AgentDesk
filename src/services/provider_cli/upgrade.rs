use chrono::Utc;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::registry::{
    MigrationHistoryEntry, MigrationState, ProviderCliChannel, ProviderCliMigrationState,
    ProviderCliUpdateStrategy, update_strategy_for,
};
use super::snapshot::snapshot_current_channel;

const UPGRADE_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

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
    UnmanagedSnapshotSource {
        source: String,
        install_source: String,
    },
    CandidatePathChanged {
        pre_canonical_path: String,
        post_canonical_path: String,
    },
    VersionUnknown {
        pre_version: String,
        post_version: String,
    },
    VersionUnchanged {
        version: String,
    },
    EntrypointRestoreFailed {
        failure: String,
        restore_error: String,
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
            UpgradeError::UnmanagedSnapshotSource {
                source,
                install_source,
            } => write!(
                f,
                "unmanaged_snapshot_source(source={source:?}, install_source={install_source:?})"
            ),
            UpgradeError::CandidatePathChanged {
                pre_canonical_path,
                post_canonical_path,
            } => write!(
                f,
                "candidate_path_changed(pre={pre_canonical_path:?}, post={post_canonical_path:?})"
            ),
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
            UpgradeError::EntrypointRestoreFailed {
                failure,
                restore_error,
            } => write!(
                f,
                "entrypoint_restore_failed_after_upgrade_error({failure}): {restore_error}"
            ),
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

fn run_upgrade_command(argv: &[&str]) -> Result<UpgradeCommandOutput, UpgradeError> {
    let (cmd, args) = argv.split_first().expect("command_argv is non-empty");

    let mut command = Command::new(cmd);
    command.args(args);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    crate::services::platform::binary_resolver::apply_runtime_path(&mut command);
    crate::services::process::configure_child_process_group(&mut command);

    let mut child = command.spawn().map_err(UpgradeError::Io)?;

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

    Ok(UpgradeCommandOutput {
        success: status.success(),
        exit_code: status.code(),
        stderr: if status.success() {
            String::new()
        } else {
            "upgrade command output suppressed to avoid unbounded provider CLI logs".to_string()
        },
    })
}

fn version_is_unknown(version: &str) -> bool {
    let value = version.trim();
    value.is_empty() || value.eq_ignore_ascii_case("unknown")
}

fn validate_managed_snapshot_source(
    strategy: &ProviderCliUpdateStrategy,
    current_snapshot: &ProviderCliChannel,
) -> Result<(), UpgradeError> {
    if !matches!(
        current_snapshot.source.as_str(),
        "current_path" | "login_shell_path" | "fallback_path"
    ) {
        return Err(UpgradeError::UnmanagedSnapshotSource {
            source: current_snapshot.source.clone(),
            install_source: strategy.install_source.to_string(),
        });
    }
    Ok(())
}

fn validate_post_upgrade_channel(
    strategy: &ProviderCliUpdateStrategy,
    current_snapshot: &ProviderCliChannel,
    post_channel: &ProviderCliChannel,
    pre_version: &str,
) -> Result<(), UpgradeError> {
    if !strategy.mutates_in_place {
        return Ok(());
    }

    let post_version = post_channel.version.clone();
    if version_is_unknown(pre_version) || version_is_unknown(&post_version) {
        return Err(UpgradeError::VersionUnknown {
            pre_version: pre_version.to_string(),
            post_version,
        });
    }
    if !strategy.allow_candidate_path_change
        && current_snapshot.canonical_path != post_channel.canonical_path
    {
        return Err(UpgradeError::CandidatePathChanged {
            pre_canonical_path: current_snapshot.canonical_path.clone(),
            post_canonical_path: post_channel.canonical_path.clone(),
        });
    }
    if pre_version == post_version {
        return Err(UpgradeError::VersionUnchanged {
            version: post_version,
        });
    }
    Ok(())
}

fn remove_path_if_exists(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        let metadata = fs::symlink_metadata(&src_path)?;

        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&src_path)?;
            create_symlink_or_copy(&src_path, &target, &dest_path)?;
        } else if metadata.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else {
            fs::copy(&src_path, &dest_path)?;
            fs::set_permissions(&dest_path, metadata.permissions())?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink_or_copy(
    src_path: &Path,
    symlink_target: &Path,
    dest_path: &Path,
) -> std::io::Result<()> {
    let _ = src_path;
    std::os::unix::fs::symlink(symlink_target, dest_path)
}

#[cfg(not(unix))]
fn create_symlink_or_copy(
    src_path: &Path,
    _symlink_target: &Path,
    dest_path: &Path,
) -> std::io::Result<()> {
    fs::copy(src_path, dest_path).map(|_| ())
}

fn npm_package_from_strategy(strategy: &ProviderCliUpdateStrategy) -> Option<&'static str> {
    if strategy.install_source != "npm-global" {
        return None;
    }
    strategy
        .command_argv
        .iter()
        .rev()
        .copied()
        .find(|arg| !arg.starts_with('-'))
}

fn npm_package_root(canonical_path: &Path, package_name: &str) -> Option<PathBuf> {
    let (scope, name) = package_name
        .strip_prefix('@')
        .and_then(|pkg| pkg.split_once('/'))
        .map(|(scope, name)| (Some(format!("@{scope}")), name.to_string()))
        .unwrap_or((None, package_name.to_string()));

    for ancestor in canonical_path.ancestors() {
        if ancestor.file_name().and_then(|v| v.to_str()) != Some(name.as_str()) {
            continue;
        }

        let Some(parent) = ancestor.parent() else {
            continue;
        };

        if let Some(scope) = &scope {
            if parent.file_name().and_then(|v| v.to_str()) != Some(scope.as_str()) {
                continue;
            }
            if parent
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|v| v.to_str())
                == Some("node_modules")
            {
                return Some(ancestor.to_path_buf());
            }
        } else if parent.file_name().and_then(|v| v.to_str()) == Some("node_modules") {
            return Some(ancestor.to_path_buf());
        }
    }

    None
}

fn previous_install_root(
    strategy: &ProviderCliUpdateStrategy,
    canonical_path: &Path,
) -> Option<PathBuf> {
    npm_package_from_strategy(strategy)
        .and_then(|package_name| npm_package_root(canonical_path, package_name))
        .filter(|root| root.is_dir())
}

fn preserve_previous_install(
    strategy: &ProviderCliUpdateStrategy,
    current_snapshot: &ProviderCliChannel,
    dest_entry: &Path,
) -> Result<(), UpgradeError> {
    let canonical_entry = Path::new(&current_snapshot.canonical_path);
    let source_entry = if canonical_entry.exists() {
        canonical_entry
    } else {
        Path::new(&current_snapshot.path)
    };

    if !source_entry.exists() {
        return Err(UpgradeError::PreviousPreservationRequired {
            reason: format!(
                "source binary not found at {}",
                current_snapshot.canonical_path
            ),
        });
    }

    if let Some(parent) = dest_entry.parent() {
        fs::create_dir_all(parent)?;
    }
    remove_path_if_exists(dest_entry)?;

    if let Some(source_root) = previous_install_root(strategy, source_entry) {
        let tree_dest = super::paths::preserved_previous_tree_path(dest_entry);
        remove_path_if_exists(&tree_dest)?;
        copy_dir_recursive(&source_root, &tree_dest)?;

        let relative_entry = source_entry.strip_prefix(&source_root).map_err(|_| {
            UpgradeError::PreviousPreservationRequired {
                reason: format!(
                    "source binary {} is outside preservation root {}",
                    source_entry.display(),
                    source_root.display()
                ),
            }
        })?;
        let preserved_entry = tree_dest.join(relative_entry);
        create_symlink_or_copy(&preserved_entry, &preserved_entry, dest_entry)?;
    } else {
        fs::copy(source_entry, dest_entry)?;
        let metadata = fs::symlink_metadata(source_entry)?;
        fs::set_permissions(dest_entry, metadata.permissions())?;
    }

    Ok(())
}

fn restore_npm_global_entrypoint(
    strategy: &ProviderCliUpdateStrategy,
    current_snapshot: &ProviderCliChannel,
    previous_preservation_path: Option<&Path>,
) -> Result<(), UpgradeError> {
    if strategy.install_source != "npm-global" {
        return Ok(());
    }

    let Some(previous_entry) = previous_preservation_path else {
        return Ok(());
    };

    match fs::symlink_metadata(previous_entry) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(UpgradeError::Io(error)),
    }

    let current_entry = Path::new(&current_snapshot.path);
    if let Some(parent) = current_entry.parent() {
        fs::create_dir_all(parent)?;
    }
    remove_path_if_exists(current_entry)?;
    create_symlink_or_copy(previous_entry, previous_entry, current_entry)?;
    Ok(())
}

fn restore_npm_global_entrypoint_after_failure(
    strategy: &ProviderCliUpdateStrategy,
    current_snapshot: &ProviderCliChannel,
    previous_preservation_path: Option<&Path>,
    failure: UpgradeError,
) -> UpgradeError {
    match restore_npm_global_entrypoint(strategy, current_snapshot, previous_preservation_path) {
        Ok(()) => failure,
        Err(restore_error) => UpgradeError::EntrypointRestoreFailed {
            failure: failure.to_string(),
            restore_error: restore_error.to_string(),
        },
    }
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
    let pre_version = current_snapshot.version.clone();

    if strategy.mutates_in_place {
        validate_managed_snapshot_source(strategy, current_snapshot)?;
        if version_is_unknown(&pre_version) {
            return Err(UpgradeError::VersionUnknown {
                pre_version,
                post_version: String::new(),
            });
        }
    }

    // Guard: mutates_in_place requires previous preservation.
    if strategy.mutates_in_place && !skip_previous_preservation {
        match previous_preservation_path {
            Some(dest) => preserve_previous_install(strategy, current_snapshot, dest)?,
            None => {
                return Err(UpgradeError::PreviousPreservationRequired {
                    reason: "mutates_in_place=true but no preservation path provided".to_string(),
                });
            }
        }
    }

    // npm-global: the existing entry-point symlink blocks `npm install -g` from creating
    // its own (EEXIST). Remove it now that the previous install is preserved (or the
    // operator explicitly skipped preservation).
    if strategy.install_source == "npm-global" {
        remove_path_if_exists(Path::new(&current_snapshot.path))?;
    }

    // Run the update command.
    let argv = strategy.command_argv;
    let output = match run_upgrade_command(argv) {
        Ok(output) => output,
        Err(error) => {
            return Err(restore_npm_global_entrypoint_after_failure(
                strategy,
                current_snapshot,
                previous_preservation_path,
                error,
            ));
        }
    };

    if !output.success {
        return Err(restore_npm_global_entrypoint_after_failure(
            strategy,
            current_snapshot,
            previous_preservation_path,
            UpgradeError::UpgradeCommandFailed {
                exit_code: output.exit_code,
                stderr: output.stderr,
            },
        ));
    }

    // Re-snapshot after upgrade to get new version.
    let post_channel = match snapshot_current_channel(provider) {
        Some(channel) => channel,
        None => {
            return Err(restore_npm_global_entrypoint_after_failure(
                strategy,
                current_snapshot,
                previous_preservation_path,
                UpgradeError::UpgradeCommandFailed {
                    exit_code: None,
                    stderr: "binary not found after upgrade".to_string(),
                },
            ));
        }
    };

    let post_version = post_channel.version.clone();
    if let Err(error) =
        validate_post_upgrade_channel(strategy, current_snapshot, &post_channel, &pre_version)
    {
        return Err(restore_npm_global_entrypoint_after_failure(
            strategy,
            current_snapshot,
            previous_preservation_path,
            error,
        ));
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

pub fn migration_state_rank(state: &MigrationState) -> Option<u8> {
    use MigrationState::*;
    Some(match state {
        Planned => 0,
        CurrentSnapshotted => 1,
        SmokeCurrentPassed => 2,
        PreviousPreserved => 3,
        UpgradePlanned => 4,
        UpgradeSucceeded => 5,
        CandidateDiscovered => 6,
        SmokeCandidatePassed => 7,
        CanarySelected => 8,
        CanarySessionSafeEnding => 9,
        CanarySessionRecreated => 10,
        CanaryActive => 11,
        CanaryPassed => 12,
        AwaitingOperatorPromote => 13,
        ProviderSessionsSafeEnding => 14,
        ProviderSessionsRecreated => 15,
        ProviderAgentsMigrated => 16,
        RolledBack | Failed => return None,
    })
}

fn is_valid_transition(from: &MigrationState, to: &MigrationState) -> bool {
    use MigrationState::*;
    if matches!(from, RolledBack) {
        return from == to;
    }
    if matches!(from, Failed) {
        return matches!(to, Failed | RolledBack);
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
