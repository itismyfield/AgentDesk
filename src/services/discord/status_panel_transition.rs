//! Durable status-panel create and ownership transition authority (#4891).
//!
//! Every Discord create is preceded by a durable nonce-keyed intent. Discord's
//! enforced nonce makes an ACK-unknown retry idempotent: replay receives the same
//! message id, records it, and resumes ownership reconciliation. The journal is
//! authoritative until ownership or retirement reaches a terminal state.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::inflight::{self, InflightTurnIdentity};
use super::runtime_store;
use super::status_panel_orphan_store;
use super::status_panel_singleton_store::{self, StatusPanelSingletonBinding};
use crate::services::provider::ProviderKind;

static TRANSITION_LOCK: Mutex<()> = Mutex::new(());
const PREPARED_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const PREPARED_RETENTION: Duration = Duration::from_secs(15 * 60);

mod prepare_recovery;
pub(in crate::services::discord) use prepare_recovery::{
    classify_create_failure_status, prepare_candidate, record_create_failure,
    record_serenity_create_failure, recover_unreconciled_with_delete, recover_with_transport,
};
use prepare_recovery::{
    duration_millis, now_unix_ms, prepare_candidate_at, prepared_recovery_disposition_at,
    record_create_failure_at,
};
#[cfg(test)]
static FAIL_NEXT_INTENT_REMOVE: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static FAIL_NEXT_PENDING_BIND_WRITE: AtomicBool = AtomicBool::new(false);

struct TransitionFileLock {
    _file: fs::File,
}

impl Drop for TransitionFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn lock_intent_path(path: &Path) -> Result<TransitionFileLock, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "status panel transition path has no channel directory".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    // One stable lock per channel bounds lock artifacts and serializes scans with
    // nonce-specific updates across rolling processes.
    let lock_path = parent.join(".transitions.lock");
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_path)
        .map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(TransitionFileLock { _file: file })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum StatusPanelTransitionState {
    #[default]
    Prepared,
    CandidateAcknowledged,
    PendingBindDurable,
    // Legacy states. Replay reconciles them from current durable owner evidence.
    OwnershipCommitted,
    Retiring,
    CandidateSent,
    Unreconciled,
    // A committed owner has been durably recorded; replay only removes residue.
    Settled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum StatusPanelTransitionOperation {
    #[default]
    LiveBind,
    CompletionFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum StatusPanelTransitionAction {
    KeepCurrent { generation: Option<u64> },
    RetireCandidate,
    DeferDurability { error: String },
    RecoverUnreconciled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum StatusPanelRetirementOutcome {
    Removed,
    PermanentAbsent,
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum StatusPanelCreateFailureDisposition {
    PermanentRejected,
    RetrySameNonce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreparedRecoveryDisposition {
    RetryNow,
    Deferred,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct StatusPanelTransitionIntent {
    #[serde(default)]
    pub nonce: String,
    pub provider: String,
    pub token_hash: String,
    pub channel_id: u64,
    #[serde(default)]
    pub candidate_panel_id: Option<u64>,
    pub prior_panel_id: Option<u64>,
    #[serde(default)]
    pub prior_generation: Option<u64>,
    #[serde(default)]
    pub generation: Option<u64>,
    pub identity: Option<InflightTurnIdentity>,
    #[serde(default)]
    pub operation: StatusPanelTransitionOperation,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub state: StatusPanelTransitionState,
    #[serde(default)]
    pub prepared_at_unix_ms: u64,
    #[serde(default)]
    pub next_retry_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct PreparedStatusPanelTransition {
    pub nonce: String,
    pub content: String,
    pub send_now: bool,
}

fn root() -> Result<PathBuf, String> {
    runtime_store::discord_status_panel_transitions_root()
        .ok_or_else(|| "status panel transition root unavailable".to_string())
}

fn channel_dir_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> PathBuf {
    root.join(provider.as_str())
        .join(token_hash)
        .join(channel_id.to_string())
}

fn path_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    nonce: &str,
) -> PathBuf {
    channel_dir_in_root(root, provider, token_hash, channel_id).join(format!("{nonce}.json"))
}

fn canonical_nonce(path: &Path) -> Option<&str> {
    path.file_stem()
        .and_then(|value| value.to_str())
        .filter(|stem| {
            stem.parse::<u64>().is_ok()
                || (stem.starts_with("adksp")
                    && stem.len() <= 40
                    && stem.chars().all(|ch| ch.is_ascii_alphanumeric()))
        })
}

fn valid_canonical_name(path: &Path) -> bool {
    path.extension().and_then(|value| value.to_str()) == Some("json")
        && canonical_nonce(path).is_some()
}

fn load_in_root(path: &Path) -> Result<Option<StatusPanelTransitionIntent>, String> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let mut intent: StatusPanelTransitionIntent =
                serde_json::from_str(&raw).map_err(|error| error.to_string())?;
            let path_nonce = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if intent.nonce.is_empty() && path_nonce.parse::<u64>().is_ok() {
                intent.nonce = path_nonce.to_string();
            }
            Ok(Some(intent))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn validate_intent_scope(
    path: &Path,
    intent: &StatusPanelTransitionIntent,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> Result<(), String> {
    let nonce = canonical_nonce(path)
        .ok_or_else(|| "status panel transition path has no canonical nonce".to_string())?;
    if intent.provider != provider.as_str()
        || intent.token_hash != token_hash
        || intent.channel_id != channel_id
        || intent.nonce != nonce
    {
        return Err("status panel transition payload/path scope mismatch".to_string());
    }
    Ok(())
}

fn validate_intent_shape(intent: &StatusPanelTransitionIntent) -> Result<(), String> {
    if intent.channel_id == 0 || intent.content.is_empty() {
        return Err("status panel transition requires channel and content".to_string());
    }
    if intent.prior_panel_id.is_some() != intent.prior_generation.is_some() {
        return Err("status panel transition prior binding is incomplete".to_string());
    }
    if intent.candidate_panel_id == Some(0) {
        return Err("status panel transition candidate id must be non-zero".to_string());
    }
    match intent.state {
        StatusPanelTransitionState::Prepared => {
            if intent.candidate_panel_id.is_some() || intent.generation.is_some() {
                return Err("prepared status panel transition has resolved fields".to_string());
            }
        }
        StatusPanelTransitionState::CandidateAcknowledged
        | StatusPanelTransitionState::PendingBindDurable
        | StatusPanelTransitionState::OwnershipCommitted
        | StatusPanelTransitionState::Retiring
        | StatusPanelTransitionState::CandidateSent
        | StatusPanelTransitionState::Unreconciled => {
            if intent.candidate_panel_id.is_none() || intent.generation.is_some() {
                return Err(
                    "acknowledged status panel transition has impossible fields".to_string()
                );
            }
        }
        StatusPanelTransitionState::Settled => {
            if intent.candidate_panel_id.is_none() || intent.generation.is_none() {
                return Err("settled status panel transition is incomplete".to_string());
            }
        }
    }
    Ok(())
}

fn load_scoped_in_root(
    path: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> Result<Option<StatusPanelTransitionIntent>, String> {
    let Some(intent) = load_in_root(path)? else {
        return Ok(None);
    };
    validate_intent_scope(path, &intent, provider, token_hash, channel_id)?;
    validate_intent_shape(&intent)?;
    Ok(Some(intent))
}

fn save_in_root(path: &Path, intent: &StatusPanelTransitionIntent) -> Result<(), String> {
    let json = serde_json::to_string_pretty(intent).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(path, &json)
}

fn remove_in_root(path: &Path) -> Result<(), String> {
    #[cfg(test)]
    if FAIL_NEXT_INTENT_REMOVE.swap(false, Ordering::SeqCst) {
        return Err("injected status panel transition removal failure".to_string());
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn find_by_candidate_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    candidate_panel_id: u64,
) -> Result<Option<(PathBuf, StatusPanelTransitionIntent)>, String> {
    let _guard = TRANSITION_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let probe = path_in_root(
        root,
        provider,
        token_hash,
        channel_id,
        &format!("lookup-{candidate_panel_id}"),
    );
    let _file_guard = lock_intent_path(&probe)?;
    find_by_candidate_locked_in_root(root, provider, token_hash, channel_id, candidate_panel_id)
}

fn find_by_candidate_locked_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    candidate_panel_id: u64,
) -> Result<Option<(PathBuf, StatusPanelTransitionIntent)>, String> {
    let dir = channel_dir_in_root(root, provider, token_hash, channel_id);
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if !valid_canonical_name(&path) {
            continue;
        }
        match load_scoped_in_root(&path, provider, token_hash, channel_id) {
            Ok(Some(intent)) if intent.candidate_panel_id == Some(candidate_panel_id) => {
                return Ok(Some((path, intent)));
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "isolating malformed status-panel transition intent"
                );
                quarantine_malformed_strict(&path)?;
            }
        }
    }
    Ok(None)
}

fn update_intent_by_nonce<F>(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    nonce: &str,
    update: F,
) -> Result<StatusPanelTransitionIntent, String>
where
    F: FnOnce(&mut StatusPanelTransitionIntent) -> Result<(), String>,
{
    let _guard = TRANSITION_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let root = root()?;
    let path = path_in_root(&root, provider, token_hash, channel_id, nonce);
    let _file_guard = lock_intent_path(&path)?;
    let mut intent = match load_scoped_in_root(&path, provider, token_hash, channel_id) {
        Ok(Some(intent)) => intent,
        Ok(None) => return Err("status panel transition intent missing".to_string()),
        Err(error) => {
            quarantine_malformed_strict(&path)?;
            return Err(error);
        }
    };
    update(&mut intent)?;
    validate_intent_shape(&intent)?;
    save_in_root(&path, &intent)?;
    Ok(intent)
}

pub(in crate::services::discord) fn cancel_prepared_candidate(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    prepared: &PreparedStatusPanelTransition,
) -> Result<bool, String> {
    let _guard = TRANSITION_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let root = root()?;
    let path = path_in_root(&root, provider, token_hash, channel_id, &prepared.nonce);
    let _file_guard = lock_intent_path(&path)?;
    let intent = match load_scoped_in_root(&path, provider, token_hash, channel_id) {
        Ok(Some(intent)) => intent,
        Ok(None) => return Ok(false),
        Err(error) => {
            quarantine_malformed_strict(&path)?;
            return Err(error);
        }
    };
    if intent.state != StatusPanelTransitionState::Prepared || intent.candidate_panel_id.is_some() {
        return Ok(false);
    }
    remove_in_root(&path)?;
    Ok(true)
}

fn protect_acknowledged_candidate(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    nonce: &str,
    candidate_panel_id: u64,
) -> Result<StatusPanelTransitionIntent, String> {
    let _guard = TRANSITION_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let root = root()?;
    let path = path_in_root(&root, provider, token_hash, channel_id, nonce);
    let _file_guard = lock_intent_path(&path)?;
    let mut intent = match load_scoped_in_root(&path, provider, token_hash, channel_id) {
        Ok(Some(intent)) => intent,
        Ok(None) => return Err("status panel transition intent missing".to_string()),
        Err(error) => {
            quarantine_malformed_strict(&path)?;
            return Err(error);
        }
    };
    if intent
        .candidate_panel_id
        .is_some_and(|id| id != candidate_panel_id)
    {
        return Err("status panel nonce resolved to a different message".to_string());
    }
    intent.candidate_panel_id = Some(candidate_panel_id);
    intent.state = StatusPanelTransitionState::CandidateAcknowledged;
    validate_intent_shape(&intent)?;
    save_in_root(&path, &intent)?;
    #[cfg(test)]
    if FAIL_NEXT_PENDING_BIND_WRITE.swap(false, Ordering::SeqCst) {
        return Err("injected status panel pending-bind persistence failure".to_string());
    }
    status_panel_orphan_store::enqueue_pending_bind(
        provider,
        token_hash,
        channel_id,
        candidate_panel_id,
        intent.identity.clone(),
    )?;
    intent.state = StatusPanelTransitionState::PendingBindDurable;
    validate_intent_shape(&intent)?;
    save_in_root(&path, &intent)?;
    Ok(intent)
}

#[cfg(test)]
pub(in crate::services::discord) fn fail_next_pending_bind_write_for_test() {
    FAIL_NEXT_PENDING_BIND_WRITE.store(true, Ordering::SeqCst);
}

pub(in crate::services::discord) fn acknowledge_candidate(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    prepared: &PreparedStatusPanelTransition,
    candidate_panel_id: u64,
) -> StatusPanelTransitionAction {
    if candidate_panel_id == 0 {
        return StatusPanelTransitionAction::DeferDurability {
            error: "status panel candidate id must be non-zero".to_string(),
        };
    }
    match protect_acknowledged_candidate(
        provider,
        token_hash,
        channel_id,
        &prepared.nonce,
        candidate_panel_id,
    ) {
        Ok(_) => StatusPanelTransitionAction::KeepCurrent { generation: None },
        Err(error) => StatusPanelTransitionAction::DeferDurability { error },
    }
}

fn mark_intent_terminal_for_candidate(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    candidate_panel_id: u64,
    generation: u64,
) -> Result<(), String> {
    let _guard = TRANSITION_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let root = root()?;
    let probe = path_in_root(
        &root,
        provider,
        token_hash,
        channel_id,
        &format!("lookup-{candidate_panel_id}"),
    );
    let _file_guard = lock_intent_path(&probe)?;
    if let Some((path, mut intent)) = find_by_candidate_locked_in_root(
        &root,
        provider,
        token_hash,
        channel_id,
        candidate_panel_id,
    )? {
        intent.state = StatusPanelTransitionState::Settled;
        intent.generation = Some(generation);
        validate_intent_shape(&intent)?;
        save_in_root(&path, &intent)?;
    }
    Ok(())
}

fn remove_intent_for_candidate(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    candidate_panel_id: u64,
) -> Result<(), String> {
    let _guard = TRANSITION_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let root = root()?;
    let probe = path_in_root(
        &root,
        provider,
        token_hash,
        channel_id,
        &format!("lookup-{candidate_panel_id}"),
    );
    let _file_guard = lock_intent_path(&probe)?;
    if let Some((path, _)) = find_by_candidate_locked_in_root(
        &root,
        provider,
        token_hash,
        channel_id,
        candidate_panel_id,
    )? {
        remove_in_root(&path)?;
    }
    Ok(())
}

fn settle_committed_candidate(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    candidate_panel_id: u64,
    binding: StatusPanelSingletonBinding,
) -> StatusPanelTransitionAction {
    if let Err(error) = mark_intent_terminal_for_candidate(
        provider,
        token_hash,
        channel_id,
        candidate_panel_id,
        binding.generation,
    ) {
        return StatusPanelTransitionAction::DeferDurability { error };
    }
    if let Err(error) = status_panel_orphan_store::remove_pending_bind_checked(
        provider,
        token_hash,
        channel_id,
        candidate_panel_id,
    ) {
        return StatusPanelTransitionAction::DeferDurability { error };
    }
    if let Err(error) =
        remove_intent_for_candidate(provider, token_hash, channel_id, candidate_panel_id)
    {
        return StatusPanelTransitionAction::DeferDurability { error };
    }
    StatusPanelTransitionAction::KeepCurrent {
        generation: Some(binding.generation),
    }
}

pub(in crate::services::discord) fn commit_bound_candidate(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    candidate_panel_id: u64,
    identity: &InflightTurnIdentity,
    expected_generation: u64,
) -> StatusPanelTransitionAction {
    let expected_prior = match root().and_then(|root| {
        find_by_candidate_in_root(&root, provider, token_hash, channel_id, candidate_panel_id)
    }) {
        Ok(Some((_, intent))) => expected_prior(&intent),
        Ok(None) => None,
        Err(error) => return StatusPanelTransitionAction::DeferDurability { error },
    };
    let binding = match status_panel_singleton_store::bind_if_owned_guarded_with_prior(
        provider,
        token_hash,
        channel_id,
        candidate_panel_id,
        Some(expected_generation),
        Some(identity),
        Some(expected_generation),
        expected_prior,
        true,
    ) {
        status_panel_singleton_store::GuardedSingletonBindOutcome::Committed(binding) => binding,
        status_panel_singleton_store::GuardedSingletonBindOutcome::NotOwned => {
            match status_panel_singleton_store::load_typed(provider, token_hash, channel_id) {
                status_panel_singleton_store::StatusPanelSingletonLoadOutcome::Present(binding)
                    if binding.panel_message_id == candidate_panel_id =>
                {
                    return settle_committed_candidate(
                        provider,
                        token_hash,
                        channel_id,
                        candidate_panel_id,
                        binding,
                    );
                }
                status_panel_singleton_store::StatusPanelSingletonLoadOutcome::DurabilityFailure(
                    error,
                ) => return StatusPanelTransitionAction::DeferDurability { error },
                _ => return StatusPanelTransitionAction::RetireCandidate,
            }
        }
        status_panel_singleton_store::GuardedSingletonBindOutcome::DurabilityFailure(error) => {
            return StatusPanelTransitionAction::DeferDurability { error };
        }
    };
    settle_committed_candidate(
        provider,
        token_hash,
        channel_id,
        candidate_panel_id,
        binding,
    )
}

pub(in crate::services::discord) fn settle_completed_candidate(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    candidate_panel_id: u64,
) -> StatusPanelTransitionAction {
    let intent = match root().and_then(|root| {
        find_by_candidate_in_root(&root, provider, token_hash, channel_id, candidate_panel_id)
    }) {
        Ok(Some((_, intent))) => intent,
        Ok(None) => {
            return match status_panel_singleton_store::load_typed(
                provider,
                token_hash,
                channel_id,
            ) {
                status_panel_singleton_store::StatusPanelSingletonLoadOutcome::Present(binding)
                    if binding.panel_message_id == candidate_panel_id =>
                {
                    settle_committed_candidate(
                        provider,
                        token_hash,
                        channel_id,
                        candidate_panel_id,
                        binding,
                    )
                }
                status_panel_singleton_store::StatusPanelSingletonLoadOutcome::DurabilityFailure(
                    error,
                ) => StatusPanelTransitionAction::DeferDurability { error },
                _ => StatusPanelTransitionAction::RecoverUnreconciled,
            };
        }
        Err(error) => return StatusPanelTransitionAction::DeferDurability { error },
    };
    reconcile_acknowledged(&intent)
}

pub(in crate::services::discord) fn finalize_retirement(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    candidate_panel_id: u64,
    outcome: StatusPanelRetirementOutcome,
) -> Result<bool, String> {
    if outcome == StatusPanelRetirementOutcome::Deferred {
        return Ok(false);
    }
    status_panel_orphan_store::remove_checked(
        provider,
        token_hash,
        channel_id,
        candidate_panel_id,
    )?;
    remove_intent_for_candidate(provider, token_hash, channel_id, candidate_panel_id)?;
    Ok(true)
}

fn expected_prior(intent: &StatusPanelTransitionIntent) -> Option<StatusPanelSingletonBinding> {
    intent
        .prior_panel_id
        .zip(intent.prior_generation)
        .map(
            |(panel_message_id, generation)| StatusPanelSingletonBinding {
                panel_message_id,
                generation,
            },
        )
}

fn reconcile_acknowledged(intent: &StatusPanelTransitionIntent) -> StatusPanelTransitionAction {
    let Some(provider) = ProviderKind::from_str(&intent.provider) else {
        return StatusPanelTransitionAction::DeferDurability {
            error: format!(
                "unknown status panel transition provider: {}",
                intent.provider
            ),
        };
    };
    let Some(candidate) = intent.candidate_panel_id else {
        return StatusPanelTransitionAction::RecoverUnreconciled;
    };
    match status_panel_singleton_store::load_typed(&provider, &intent.token_hash, intent.channel_id)
    {
        status_panel_singleton_store::StatusPanelSingletonLoadOutcome::Present(binding)
            if binding.panel_message_id == candidate =>
        {
            return settle_committed_candidate(
                &provider,
                &intent.token_hash,
                intent.channel_id,
                candidate,
                binding,
            );
        }
        status_panel_singleton_store::StatusPanelSingletonLoadOutcome::DurabilityFailure(error) => {
            return StatusPanelTransitionAction::DeferDurability { error };
        }
        _ => {}
    }

    if let Some(identity) = intent.identity.as_ref()
        && let Some(state) = inflight::load_inflight_state(&provider, intent.channel_id)
        && identity.matches_state(&state)
    {
        let bound = if state.status_message_id == Some(candidate) {
            true
        } else {
            let guard = inflight::StatusPanelBindGuard {
                require_identity: Some(identity.clone()),
                skip_if_panel_already_set: intent.prior_panel_id.is_none(),
                require_current_status_message_id: intent.prior_panel_id,
                bump_status_panel_generation: true,
                ..Default::default()
            };
            inflight::bind_status_panel(&provider, intent.channel_id, candidate, &guard).is_bound()
        };
        if bound
            && let Some(state) = inflight::load_inflight_state(&provider, intent.channel_id)
            && state.status_message_id == Some(candidate)
        {
            return commit_bound_candidate(
                &provider,
                &intent.token_hash,
                intent.channel_id,
                candidate,
                identity,
                state.status_panel_generation,
            );
        }
    }

    if intent.operation == StatusPanelTransitionOperation::CompletionFallback {
        return match status_panel_singleton_store::commit_replacement_if_current(
            &provider,
            &intent.token_hash,
            intent.channel_id,
            candidate,
            expected_prior(intent),
        ) {
            status_panel_singleton_store::CompletedBindingCommitOutcome::CommittedCurrent(
                binding,
            ) => settle_committed_candidate(
                &provider,
                &intent.token_hash,
                intent.channel_id,
                candidate,
                binding,
            ),
            status_panel_singleton_store::CompletedBindingCommitOutcome::Superseded => {
                StatusPanelTransitionAction::RetireCandidate
            }
            status_panel_singleton_store::CompletedBindingCommitOutcome::DurabilityFailure(
                error,
            ) => StatusPanelTransitionAction::DeferDurability { error },
        };
    }
    StatusPanelTransitionAction::RetireCandidate
}

fn quarantine_malformed_strict(path: &Path) -> Result<(), String> {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("unknown.json");
    let quarantine = path.with_file_name(format!(
        "{file_name}.corrupt-{}",
        uuid::Uuid::new_v4().simple()
    ));
    match fs::rename(path, quarantine) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn quarantine_malformed_for_discovery(path: &Path) {
    if let Err(error) = quarantine_malformed_strict(path) {
        tracing::warn!(
            path = %path.display(),
            error = %error,
            "failed to quarantine malformed status-panel transition intent"
        );
    }
}

pub(in crate::services::discord) fn load_unreconciled(
    provider: &ProviderKind,
    token_hash: &str,
) -> Result<Vec<StatusPanelTransitionIntent>, String> {
    let root = root()?.join(provider.as_str()).join(token_hash);
    let channels = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut intents = HashMap::new();
    for channel in channels {
        let channel = channel.map_err(|error| error.to_string())?;
        let channel_path = channel.path();
        if !channel_path.is_dir() {
            continue;
        }
        let Some(channel_id) = channel_path
            .file_name()
            .and_then(|value| value.to_str())
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        let candidates = match fs::read_dir(&channel_path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        for candidate in candidates {
            let candidate = candidate.map_err(|error| error.to_string())?;
            let path = candidate.path();
            if !valid_canonical_name(&path) {
                continue;
            }
            match load_scoped_in_root(&path, provider, token_hash, channel_id) {
                Ok(Some(intent)) => {
                    intents.insert((intent.channel_id, intent.nonce.clone()), intent);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %error,
                        "isolating malformed status-panel transition intent"
                    );
                    quarantine_malformed_for_discovery(&path);
                }
            }
        }
    }
    Ok(intents.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_intent_exists_before_message_id_is_known_4891() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );
        let prepared = prepare_candidate(
            &ProviderKind::Claude,
            "tok",
            42,
            None,
            None,
            StatusPanelTransitionOperation::CompletionFallback,
            "complete",
        )
        .expect("prepare intent");
        let intents = load_unreconciled(&ProviderKind::Claude, "tok").expect("load intents");
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].nonce, prepared.nonce);
        assert_eq!(intents[0].candidate_panel_id, None);
        assert_eq!(intents[0].state, StatusPanelTransitionState::Prepared);
    }

    #[test]
    fn exact_prepared_authority_coalesces_and_bounds_retry_window_4891() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );
        let provider = ProviderKind::Claude;
        let token = "tok";
        let channel_id = 4_891;
        let identity = InflightTurnIdentity {
            user_msg_id: 77,
            started_at: "2026-07-26T00:00:00Z".to_string(),
            tmux_session_name: Some("AgentDesk-test".to_string()),
            turn_start_offset: Some(9),
        };
        let first = prepare_candidate_at(
            &provider,
            token,
            channel_id,
            None,
            Some(identity.clone()),
            StatusPanelTransitionOperation::LiveBind,
            "frame-1",
            1_000,
        )
        .expect("prepare first authority");
        assert!(first.send_now);
        record_create_failure_at(
            &provider,
            token,
            channel_id,
            &first,
            StatusPanelCreateFailureDisposition::RetrySameNonce,
            1_000,
        )
        .expect("record ambiguous failure");
        let retry_at = load_unreconciled(&provider, token).expect("load retry authority")[0]
            .next_retry_at_unix_ms;
        let coalesced = prepare_candidate_at(
            &provider,
            token,
            channel_id,
            None,
            Some(identity.clone()),
            StatusPanelTransitionOperation::LiveBind,
            "frame-2",
            retry_at.saturating_sub(1),
        )
        .expect("coalesce retry authority");
        assert_eq!(coalesced.nonce, first.nonce);
        assert_eq!(coalesced.content, "frame-2");
        assert!(
            !coalesced.send_now,
            "retry interval must suppress tick sends"
        );
        assert_eq!(
            load_unreconciled(&provider, token)
                .expect("one coalesced intent")
                .len(),
            1
        );
        let replacement = prepare_candidate_at(
            &provider,
            token,
            channel_id,
            None,
            Some(identity),
            StatusPanelTransitionOperation::LiveBind,
            "frame-3",
            1_000 + duration_millis(PREPARED_RETENTION),
        )
        .expect("replace expired authority");
        assert_ne!(replacement.nonce, first.nonce);
        assert_eq!(
            load_unreconciled(&provider, token)
                .expect("expired intent retired before replacement")
                .len(),
            1
        );
    }

    #[test]
    fn create_failure_classification_retains_ambiguous_and_retires_permanent_4891() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );
        let provider = ProviderKind::Claude;
        let token = "tok";
        let channel_id = 4_892;
        let prepared = prepare_candidate(
            &provider,
            token,
            channel_id,
            None,
            None,
            StatusPanelTransitionOperation::LiveBind,
            "frame",
        )
        .expect("prepare authority");
        assert_eq!(
            classify_create_failure_status(Some(500)),
            StatusPanelCreateFailureDisposition::RetrySameNonce
        );
        assert_eq!(
            classify_create_failure_status(None),
            StatusPanelCreateFailureDisposition::RetrySameNonce
        );
        record_create_failure(
            &provider,
            token,
            channel_id,
            &prepared,
            StatusPanelCreateFailureDisposition::RetrySameNonce,
        )
        .expect("retain ambiguous authority");
        assert_eq!(
            load_unreconciled(&provider, token).expect("retained").len(),
            1
        );
        for status in [403, 404, 410] {
            assert_eq!(
                classify_create_failure_status(Some(status)),
                StatusPanelCreateFailureDisposition::PermanentRejected
            );
        }
        record_create_failure(
            &provider,
            token,
            channel_id,
            &prepared,
            StatusPanelCreateFailureDisposition::PermanentRejected,
        )
        .expect("retire permanent rejection");
        assert!(
            load_unreconciled(&provider, token)
                .expect("retired")
                .is_empty()
        );
    }

    #[test]
    fn impossible_transition_shapes_are_quarantined_on_exact_and_discovery_paths_4891() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );
        let provider = ProviderKind::Claude;
        let token = "tok";
        let channel_id = 4_893;
        for state in [
            StatusPanelTransitionState::CandidateAcknowledged,
            StatusPanelTransitionState::Settled,
        ] {
            let prepared = prepare_candidate(
                &provider,
                token,
                channel_id,
                None,
                None,
                StatusPanelTransitionOperation::LiveBind,
                "frame",
            )
            .expect("prepare shape fixture");
            let path = path_in_root(
                &root().expect("root"),
                &provider,
                token,
                channel_id,
                &prepared.nonce,
            );
            let mut impossible = load_in_root(&path)
                .expect("load fixture")
                .expect("fixture exists");
            impossible.state = state;
            impossible.candidate_panel_id = None;
            impossible.generation = None;
            save_in_root(&path, &impossible).expect("write impossible fixture");
            let exact = cancel_prepared_candidate(&provider, token, channel_id, &prepared);
            assert!(exact.is_err(), "exact impossible mutation must fail closed");
            assert!(!path.exists(), "exact impossible path must be isolated");
        }

        let prepared = prepare_candidate(
            &provider,
            token,
            channel_id + 1,
            None,
            None,
            StatusPanelTransitionOperation::LiveBind,
            "discovery",
        )
        .expect("prepare discovery fixture");
        let path = path_in_root(
            &root().expect("root"),
            &provider,
            token,
            channel_id + 1,
            &prepared.nonce,
        );
        let mut impossible = load_in_root(&path)
            .expect("load discovery fixture")
            .expect("fixture exists");
        impossible.state = StatusPanelTransitionState::PendingBindDurable;
        impossible.candidate_panel_id = None;
        save_in_root(&path, &impossible).expect("write discovery poison");
        assert!(
            load_unreconciled(&provider, token)
                .expect("discovery isolates impossible intent")
                .is_empty()
        );
        assert!(!path.exists());
    }

    #[test]
    fn cancel_prepared_candidate_only_removes_unsent_intent_4891() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );
        let provider = ProviderKind::Claude;
        let token = "tok";
        let channel_id = 42;
        let unsent = prepare_candidate(
            &provider,
            token,
            channel_id,
            None,
            None,
            StatusPanelTransitionOperation::CompletionFallback,
            "unsent",
        )
        .expect("prepare unsent intent");
        assert!(
            cancel_prepared_candidate(&provider, token, channel_id, &unsent)
                .expect("cancel unsent intent")
        );
        assert!(
            load_unreconciled(&provider, token)
                .expect("load intents")
                .is_empty()
        );

        let acknowledged = prepare_candidate(
            &provider,
            token,
            channel_id,
            None,
            None,
            StatusPanelTransitionOperation::CompletionFallback,
            "acknowledged",
        )
        .expect("prepare acknowledged intent");
        assert!(matches!(
            acknowledge_candidate(&provider, token, channel_id, &acknowledged, 5000),
            StatusPanelTransitionAction::KeepCurrent { generation: None }
        ));
        assert!(
            !cancel_prepared_candidate(&provider, token, channel_id, &acknowledged)
                .expect("acknowledged intent is not cancellable")
        );
        assert_eq!(
            load_unreconciled(&provider, token)
                .expect("load acknowledged intent")
                .len(),
            1
        );
    }

    #[test]
    fn completed_current_singleton_without_inflight_settles_intent_4891() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );
        let provider = ProviderKind::Claude;
        let token = "tok";
        let channel_id = 43;
        let candidate = 5001;
        let prepared = prepare_candidate(
            &provider,
            token,
            channel_id,
            None,
            None,
            StatusPanelTransitionOperation::CompletionFallback,
            "complete",
        )
        .expect("prepare intent");
        assert!(matches!(
            acknowledge_candidate(&provider, token, channel_id, &prepared, candidate),
            StatusPanelTransitionAction::KeepCurrent { generation: None }
        ));
        assert!(matches!(
            status_panel_singleton_store::commit_replacement_if_current(
                &provider, token, channel_id, candidate, None,
            ),
            status_panel_singleton_store::CompletedBindingCommitOutcome::CommittedCurrent(_)
        ));

        assert!(matches!(
            settle_completed_candidate(&provider, token, channel_id, candidate),
            StatusPanelTransitionAction::KeepCurrent {
                generation: Some(1)
            }
        ));
        assert!(
            load_unreconciled(&provider, token)
                .expect("load intents")
                .is_empty(),
            "already-current completion must settle rather than retire on restart"
        );
        assert!(
            find_by_candidate_in_root(
                &root().expect("transition root"),
                &provider,
                token,
                channel_id,
                candidate,
            )
            .expect("terminal lookup")
            .is_none(),
            "terminal intent must be removed after pending-bind cleanup"
        );
        assert!(!status_panel_orphan_store::is_queued(
            &provider, token, channel_id, candidate
        ));
    }

    #[tokio::test]
    async fn identityless_prepared_intent_replays_same_nonce_and_terminates_4891() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );
        let provider = ProviderKind::Claude;
        let token = "tok";
        let channel_id = 44;
        let candidate = 5002;
        let prepared = prepare_candidate(
            &provider,
            token,
            channel_id,
            None,
            None,
            StatusPanelTransitionOperation::CompletionFallback,
            "complete",
        )
        .expect("prepare intent");
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sent = observed.clone();

        assert_eq!(
            recover_with_transport(
                &provider,
                token,
                move |observed_channel, content, nonce| {
                    let sent = sent.clone();
                    async move {
                        sent.lock().expect("send observations").push((
                            observed_channel,
                            content,
                            nonce,
                        ));
                        Ok(candidate)
                    }
                },
                |_, _| async { StatusPanelRetirementOutcome::Deferred },
            )
            .await,
            1
        );
        assert_eq!(
            observed.lock().expect("send observations").as_slice(),
            &[(channel_id, "complete".to_string(), prepared.nonce)]
        );
        assert!(
            load_unreconciled(&provider, token)
                .expect("load intents")
                .is_empty()
        );
        assert_eq!(
            status_panel_singleton_store::load(&provider, token, channel_id),
            Some(StatusPanelSingletonBinding {
                panel_message_id: candidate,
                generation: 1,
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn candidate_lookup_waits_for_cross_process_channel_lock_4891() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );
        let provider = ProviderKind::Claude;
        let token = "tok";
        let channel_id = 44;
        let candidate = 5_002;
        let prepared = prepare_candidate(
            &provider,
            token,
            channel_id,
            None,
            None,
            StatusPanelTransitionOperation::LiveBind,
            "live",
        )
        .expect("prepare intent");
        assert!(matches!(
            acknowledge_candidate(&provider, token, channel_id, &prepared, candidate),
            StatusPanelTransitionAction::KeepCurrent { generation: None }
        ));

        let transition_root = root().expect("root");
        let held_probe = path_in_root(
            &transition_root,
            &provider,
            token,
            channel_id,
            "held-by-peer",
        );
        let peer_lock = lock_intent_path(&held_probe).expect("peer channel lock");
        let (sender, receiver) = std::sync::mpsc::channel();
        let lookup_root = transition_root.clone();
        let lookup_provider = provider.clone();
        let lookup = std::thread::spawn(move || {
            sender
                .send(find_by_candidate_in_root(
                    &lookup_root,
                    &lookup_provider,
                    token,
                    channel_id,
                    candidate,
                ))
                .expect("send lookup result");
        });
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "candidate lookup must not scan while a peer owns the channel lock"
        );
        drop(peer_lock);
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("lookup resumes after unlock")
                .expect("lookup succeeds")
                .is_some()
        );
        lookup.join().expect("lookup thread");
    }

    #[test]
    fn live_candidate_lookup_isolates_poison_and_finds_valid_intent_4891() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );
        let provider = ProviderKind::Claude;
        let prepared = prepare_candidate(
            &provider,
            "tok",
            45,
            None,
            None,
            StatusPanelTransitionOperation::CompletionFallback,
            "complete",
        )
        .expect("prepare intent");
        assert!(matches!(
            acknowledge_candidate(&provider, "tok", 45, &prepared, 5003),
            StatusPanelTransitionAction::KeepCurrent { generation: None }
        ));
        let dir = root().expect("root").join("claude").join("tok").join("45");
        fs::write(dir.join("00000000000000000000.json"), "{").expect("poison");

        let found = find_by_candidate_in_root(&root().expect("root"), &provider, "tok", 45, 9999)
            .expect("lookup continues");
        assert!(found.is_none());
        assert!(
            find_by_candidate_in_root(&root().expect("root"), &provider, "tok", 45, 5003)
                .expect("valid lookup")
                .is_some()
        );
        assert!(
            fs::read_dir(dir)
                .expect("dir")
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"))
        );
    }

    #[test]
    fn replay_skips_non_channel_entries_and_quarantines_scope_mismatch_4891() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );
        let provider = ProviderKind::Claude;
        let token = "tok";
        let valid = prepare_candidate(
            &provider,
            token,
            42,
            None,
            None,
            StatusPanelTransitionOperation::LiveBind,
            "live",
        )
        .expect("valid intent");
        let token_root = root().expect("root").join("claude").join(token);
        fs::write(token_root.join("notes"), "not a channel").expect("provider-root file");
        fs::create_dir_all(token_root.join("not-a-channel")).expect("nonnumeric directory");

        let poison_nonce = "adkspbbbbbbbbbbbbbbbbbbbb";
        let poison_path = token_root.join("43").join(format!("{poison_nonce}.json"));
        fs::create_dir_all(poison_path.parent().expect("poison parent")).expect("poison dir");
        let poison = StatusPanelTransitionIntent {
            nonce: poison_nonce.to_string(),
            provider: provider.as_str().to_string(),
            token_hash: "other-token".to_string(),
            channel_id: 43,
            candidate_panel_id: None,
            prior_panel_id: None,
            prior_generation: None,
            generation: None,
            identity: None,
            operation: StatusPanelTransitionOperation::LiveBind,
            content: "poison".to_string(),
            state: StatusPanelTransitionState::Prepared,
            prepared_at_unix_ms: 0,
            next_retry_at_unix_ms: 0,
        };
        save_in_root(&poison_path, &poison).expect("scope poison");

        let intents = load_unreconciled(&provider, token).expect("valid replay continues");
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].nonce, valid.nonce);
        assert!(
            fs::read_dir(poison_path.parent().expect("poison parent"))
                .expect("poison dir")
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"))
        );
    }

    #[test]
    fn exact_nonce_mutations_quarantine_malformed_intent_4891() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );
        let provider = ProviderKind::Claude;
        let token = "tok";
        let channel_id = 46;

        for cancel in [false, true] {
            let prepared = prepare_candidate(
                &provider,
                token,
                channel_id,
                None,
                None,
                StatusPanelTransitionOperation::LiveBind,
                "live",
            )
            .expect("prepare intent");
            let path = path_in_root(
                &root().expect("root"),
                &provider,
                token,
                channel_id,
                &prepared.nonce,
            );
            fs::write(&path, "{").expect("corrupt exact intent");

            let result = if cancel {
                cancel_prepared_candidate(&provider, token, channel_id, &prepared).map(|_| ())
            } else {
                update_intent_by_nonce(&provider, token, channel_id, &prepared.nonce, |_| Ok(()))
                    .map(|_| ())
            };
            assert!(result.is_err(), "exact malformed mutation must fail closed");
            assert!(!path.exists(), "malformed canonical path must be isolated");
            assert!(
                fs::read_dir(path.parent().expect("intent parent"))
                    .expect("intent dir")
                    .filter_map(Result::ok)
                    .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-")),
                "malformed exact intent must be quarantined"
            );
        }
    }

    #[tokio::test]
    async fn retirement_partial_failure_retains_intent_for_idempotent_replay_4891() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );
        let provider = ProviderKind::Claude;
        let token = "tok";
        let channel_id = 47;
        let candidate = 5_004;
        let prepared = prepare_candidate(
            &provider,
            token,
            channel_id,
            None,
            None,
            StatusPanelTransitionOperation::LiveBind,
            "live",
        )
        .expect("prepare intent");
        assert!(matches!(
            acknowledge_candidate(&provider, token, channel_id, &prepared, candidate),
            StatusPanelTransitionAction::KeepCurrent { generation: None }
        ));
        FAIL_NEXT_INTENT_REMOVE.store(true, Ordering::SeqCst);
        assert!(
            finalize_retirement(
                &provider,
                token,
                channel_id,
                candidate,
                StatusPanelRetirementOutcome::Removed,
            )
            .is_err(),
            "orphan removal may succeed while intent removal reports failure"
        );
        assert!(
            !status_panel_orphan_store::is_queued(&provider, token, channel_id, candidate),
            "first retirement phase must have removed orphan protection"
        );
        assert_eq!(
            load_unreconciled(&provider, token)
                .expect("load crash residue")
                .len(),
            1,
            "intent must retain retry authority after the partial failure"
        );

        assert_eq!(
            recover_unreconciled_with_delete(&provider, token, |_, id| async move {
                assert_eq!(id, candidate);
                StatusPanelRetirementOutcome::PermanentAbsent
            })
            .await,
            1,
            "replay must converge through idempotent permanent absence"
        );
        assert!(
            load_unreconciled(&provider, token)
                .expect("load intents")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn legacy_states_converge_from_current_owner_evidence_4891() {
        for (index, state) in [
            StatusPanelTransitionState::CandidateSent,
            StatusPanelTransitionState::Unreconciled,
            StatusPanelTransitionState::OwnershipCommitted,
            StatusPanelTransitionState::Retiring,
        ]
        .into_iter()
        .enumerate()
        {
            let _env_lock = crate::config::shared_test_env_lock()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let runtime_root = tempfile::tempdir().expect("runtime root");
            let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                "AGENTDESK_ROOT_DIR",
                runtime_root.path(),
            );
            let provider = ProviderKind::Claude;
            let token = "tok";
            let channel_id = 50 + index as u64;
            let candidate = 5_100 + index as u64;
            let prepared = prepare_candidate(
                &provider,
                token,
                channel_id,
                None,
                None,
                StatusPanelTransitionOperation::CompletionFallback,
                "complete",
            )
            .expect("prepare intent");
            assert!(matches!(
                acknowledge_candidate(&provider, token, channel_id, &prepared, candidate),
                StatusPanelTransitionAction::KeepCurrent { generation: None }
            ));
            update_intent_by_nonce(&provider, token, channel_id, &prepared.nonce, |intent| {
                intent.state = state;
                Ok(())
            })
            .expect("set legacy state");
            status_panel_singleton_store::commit_replacement_if_current(
                &provider, token, channel_id, candidate, None,
            );

            assert_eq!(
                recover_unreconciled_with_delete(&provider, token, |_, _| async {
                    panic!("current singleton must not be retired")
                })
                .await,
                1,
                "legacy state {state:?} must settle"
            );
            assert!(
                load_unreconciled(&provider, token)
                    .expect("load intents")
                    .is_empty()
            );
            assert!(
                find_by_candidate_in_root(
                    &root().expect("transition root"),
                    &provider,
                    token,
                    channel_id,
                    candidate,
                )
                .expect("terminal lookup")
                .is_none(),
                "legacy state {state:?} must reach bounded terminal cleanup"
            );
        }
    }

    #[tokio::test]
    async fn settled_residue_is_removed_after_newer_singleton_wins_4891() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );
        let provider = ProviderKind::Claude;
        let token = "tok";
        let channel_id = 60;
        let candidate = 5_200;
        let newer = 5_201;
        let prepared = prepare_candidate(
            &provider,
            token,
            channel_id,
            None,
            None,
            StatusPanelTransitionOperation::CompletionFallback,
            "complete",
        )
        .expect("prepare intent");
        assert!(matches!(
            acknowledge_candidate(&provider, token, channel_id, &prepared, candidate),
            StatusPanelTransitionAction::KeepCurrent { generation: None }
        ));
        status_panel_singleton_store::commit_replacement_if_current(
            &provider, token, channel_id, candidate, None,
        );
        update_intent_by_nonce(&provider, token, channel_id, &prepared.nonce, |intent| {
            intent.state = StatusPanelTransitionState::Settled;
            intent.generation = Some(1);
            Ok(())
        })
        .expect("mark settled residue");
        status_panel_singleton_store::commit_replacement_if_current(
            &provider,
            token,
            channel_id,
            newer,
            Some(StatusPanelSingletonBinding {
                panel_message_id: candidate,
                generation: 1,
            }),
        );

        assert_eq!(
            recover_unreconciled_with_delete(&provider, token, |_, _| async {
                panic!("settled residue must never delete a panel")
            })
            .await,
            1
        );
        assert!(
            load_unreconciled(&provider, token)
                .expect("load intents")
                .is_empty()
        );
        assert_eq!(
            status_panel_singleton_store::load(&provider, token, channel_id),
            Some(StatusPanelSingletonBinding {
                panel_message_id: newer,
                generation: 2,
            })
        );
    }

    #[test]
    fn replay_ignores_temp_and_nonnumeric_and_isolates_poison_4891() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );
        prepare_candidate(
            &ProviderKind::Claude,
            "tok",
            42,
            None,
            None,
            StatusPanelTransitionOperation::LiveBind,
            "live",
        )
        .expect("valid intent");
        let dir = root().expect("root").join("claude").join("tok").join("42");
        fs::write(dir.join("adkspaaaaaaaaaaaaaaaaaaaa.json"), "{").expect("poison");
        fs::write(dir.join("write.tmp"), "{").expect("tmp");
        fs::write(dir.join("notes.json"), "{").expect("nonnumeric");

        assert_eq!(
            load_unreconciled(&ProviderKind::Claude, "tok")
                .expect("valid replay continues")
                .len(),
            1
        );
        assert!(
            fs::read_dir(dir)
                .expect("dir")
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"))
        );
    }
}
