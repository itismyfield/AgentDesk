//! Durable per-input busy-notice binding and aggregate retry budget.
use super::input_runtime::fence;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::SourceMessageQueuedGeneration;

use super::{
    InflightTurnState, Intervention, MailboxEnqueueOutcome, SharedData,
    mailbox_requeue_intervention_front,
};
use serenity::model::id::{ChannelId, MessageId, UserId};

pub(in crate::services::discord) const MAX_BUSY_RETRY_COUNT: u32 = 6;
pub(in crate::services::discord) const MAX_BUSY_RETRY_ELAPSED: Duration =
    Duration::from_secs(5 * 60);
const BUSY_RETRY_STORE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

static STORE_WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct BusyFollowupRetryState {
    pub notice_message_id: u64,
    pub busy_retry_count: u32,
    pub first_busy_retry_at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct BusyRetryDecision {
    pub state: BusyFollowupRetryState,
    pub capped: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct BusyFollowupRetryIdentity {
    pub user_msg_id: u64,
    pub state: Option<BusyFollowupRetryState>,
}

fn input_file_path_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
) -> PathBuf {
    root.join(provider.as_str())
        .join(channel_id.to_string())
        .join(format!("{user_msg_id}.json"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn load_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
) -> Option<BusyFollowupRetryState> {
    let raw = fs::read_to_string(input_file_path_in_root(
        root,
        provider,
        channel_id,
        user_msg_id,
    ))
    .ok()?;
    serde_json::from_str(&raw).ok()
}

fn remove_empty_ancestors(path: &Path, root: &Path) {
    let mut current = path.parent();
    while let Some(directory) = current {
        if directory == root {
            break;
        }
        if fs::remove_dir(directory).is_err() {
            break;
        }
        current = directory.parent();
    }
}

fn sweep_expired_in_root_at(root: &Path, now_ms: u64) -> usize {
    let mut removed = 0usize;
    let Ok(providers) = fs::read_dir(root) else {
        return 0;
    };
    for provider in providers.flatten() {
        let Ok(channels) = fs::read_dir(provider.path()) else {
            continue;
        };
        for channel in channels.flatten() {
            removed += sweep_channel(&channel.path(), root, now_ms);
        }
    }
    removed
}

fn sweep_channel(channel: &Path, root: &Path, now_ms: u64) -> usize {
    let mut removed = 0usize;
    let Ok(entries) = fs::read_dir(channel) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let timestamp_ms = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<BusyFollowupRetryState>(&raw).ok())
            .map(|state| state.first_busy_retry_at_ms)
            .filter(|timestamp| *timestamp != 0)
            .or_else(|| {
                fs::metadata(&path)
                    .ok()?
                    .modified()
                    .ok()?
                    .duration_since(UNIX_EPOCH)
                    .ok()?
                    .as_millis()
                    .try_into()
                    .ok()
            });
        if timestamp_ms.is_some_and(|timestamp| {
            now_ms.saturating_sub(timestamp) >= BUSY_RETRY_STORE_TTL.as_millis() as u64
        }) && fs::remove_file(&path).is_ok()
        {
            removed = removed.saturating_add(1);
            remove_empty_ancestors(&path, root);
        }
    }
    removed
}

fn sweep_channel_checked(channel: &Path, root: &Path, now_ms: u64) -> Result<usize, String> {
    let entries = match fs::read_dir(channel) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.to_string()),
    };
    let mut removed = 0;
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let bytes = fs::read(&path).map_err(|error| error.to_string())?;
        let timestamp = serde_json::from_slice::<BusyFollowupRetryState>(&bytes)
            .ok()
            .map(|state| state.first_busy_retry_at_ms)
            .filter(|timestamp| *timestamp != 0);
        let timestamp = match timestamp {
            Some(timestamp) => timestamp,
            None => fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .map_err(|error| error.to_string())?
                .duration_since(UNIX_EPOCH)
                .map_err(|error| error.to_string())?
                .as_millis()
                .try_into()
                .map_err(|_| "busy retry mtime exceeds u64".to_owned())?,
        };
        if now_ms.saturating_sub(timestamp) >= BUSY_RETRY_STORE_TTL.as_millis() as u64 {
            fs::remove_file(&path).map_err(|error| error.to_string())?;
            runtime_store::fsync_parent_dir(&path).map_err(|error| error.to_string())?;
            removed += 1;
            remove_empty_ancestors(&path, root);
        }
    }
    Ok(removed)
}

fn save_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    state: BusyFollowupRetryState,
) -> Result<(), String> {
    if channel_id == 0 || user_msg_id == 0 || state.notice_message_id == 0 {
        return Err("busy follow-up retry ids must be non-zero".to_string());
    }
    let json = serde_json::to_string_pretty(&state).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(
        &input_file_path_in_root(root, provider, channel_id, user_msg_id),
        &json,
    )
}

/// Front-restore an inflight Claude TUI follow-up that failed before submission;
/// it predates queued interventions, and the deferred kickoff prevents hot loops.
pub(in crate::services::discord) async fn requeue_inflight_for_followup_retry(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    inflight_state: &InflightTurnState,
) -> MailboxEnqueueOutcome {
    let user_msg_id = inflight_state.user_msg_id;
    let retry_user_msg_id = inflight_state.effective_busy_followup_retry_user_msg_id();
    if user_msg_id == 0 || inflight_state.user_text.trim().is_empty() {
        return MailboxEnqueueOutcome::default();
    }
    let message_id = MessageId::new(user_msg_id);
    let retry_message_id = MessageId::new(retry_user_msg_id);
    let queued_generation = shared.restart.current_generation;
    let source_message_ids = if retry_message_id == message_id {
        vec![message_id]
    } else {
        vec![message_id, retry_message_id]
    };
    let source_message_queued_generations = source_message_ids
        .iter()
        .map(|&source_id| {
            if source_id == message_id && inflight_state.followup_preserve_on_cancel {
                SourceMessageQueuedGeneration::user_instruction(source_id, queued_generation)
            } else {
                SourceMessageQueuedGeneration::new(source_id, queued_generation)
            }
        })
        .collect();
    let intervention = Intervention {
        author_id: UserId::new(inflight_state.request_owner_user_id),
        author_is_bot: false,
        message_id,
        queued_generation,
        source_message_ids,
        source_message_queued_generations,
        source_text_segments: Vec::new(),
        text: inflight_state.user_text.clone(),
        mode: crate::services::turn_orchestrator::InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: inflight_state.followup_reply_context.clone(),
        has_reply_boundary: inflight_state.followup_has_reply_boundary,
        merge_consecutive: inflight_state.followup_merge_consecutive,
        pending_uploads: inflight_state.followup_pending_uploads.clone(),
        voice_announcement: inflight_state.followup_voice_announcement.clone(),
    };
    mailbox_requeue_intervention_front(shared, provider, channel_id, intervention).await
}

pub(in crate::services::discord) fn load(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
) -> Option<BusyFollowupRetryState> {
    let root = runtime_store::discord_busy_followup_retries_root()?;
    let _guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    load_in_root(&root, provider, channel_id, user_msg_id)
}

pub(in crate::services::discord) fn resolve_identity(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    source_message_ids: &[serenity::model::id::MessageId],
) -> BusyFollowupRetryIdentity {
    let Some(root) = runtime_store::discord_busy_followup_retries_root() else {
        return BusyFollowupRetryIdentity {
            user_msg_id,
            state: None,
        };
    };
    let _guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(state) = load_in_root(&root, provider, channel_id, user_msg_id) {
        return BusyFollowupRetryIdentity {
            user_msg_id,
            state: Some(state),
        };
    }
    for source_message_id in source_message_ids {
        let source_message_id = source_message_id.get();
        if source_message_id == user_msg_id {
            continue;
        }
        if let Some(state) = load_in_root(&root, provider, channel_id, source_message_id) {
            return BusyFollowupRetryIdentity {
                user_msg_id: source_message_id,
                state: Some(state),
            };
        }
    }
    BusyFollowupRetryIdentity {
        user_msg_id,
        state: None,
    }
}

pub(in crate::services::discord) fn state_is_capped(state: Option<BusyFollowupRetryState>) -> bool {
    let Some(state) = state else {
        return false;
    };
    let elapsed_ms = now_ms().saturating_sub(state.first_busy_retry_at_ms);
    state.busy_retry_count >= MAX_BUSY_RETRY_COUNT
        || (state.first_busy_retry_at_ms != 0
            && elapsed_ms >= MAX_BUSY_RETRY_ELAPSED.as_millis() as u64)
}

pub(in crate::services::discord) fn is_capped(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
) -> bool {
    state_is_capped(load(provider, channel_id, user_msg_id))
}

/// Bind the first posted placeholder. A stale attempt cannot replace an existing
/// input binding; callers edit the returned current message instead.
pub(in crate::services::discord) fn bind_notice_if_absent(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    notice_message_id: u64,
) -> Result<BusyFollowupRetryState, String> {
    if fence::lookup(provider, channel_id).is_none() {
        return bind_notice_if_absent_unfenced(
            provider,
            channel_id,
            user_msg_id,
            notice_message_id,
        );
    }
    fence::write(provider, channel_id, || {
        bind_notice_if_absent_unfenced(provider, channel_id, user_msg_id, notice_message_id)
    })
}

fn bind_notice_if_absent_unfenced(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    notice_message_id: u64,
) -> Result<BusyFollowupRetryState, String> {
    let root = runtime_store::discord_busy_followup_retries_root()
        .ok_or_else(|| "AgentDesk runtime root unavailable".to_string())?;
    let _guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(current) = load_in_root(&root, provider, channel_id, user_msg_id) {
        return Ok(current);
    }
    let state = BusyFollowupRetryState {
        notice_message_id,
        busy_retry_count: 0,
        first_busy_retry_at_ms: 0,
    };
    save_in_root(&root, provider, channel_id, user_msg_id, state)?;
    Ok(state)
}

pub(in crate::services::discord) fn record_busy_retry(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    notice_message_id: u64,
) -> Result<BusyRetryDecision, String> {
    record_busy_retry_at(
        provider,
        channel_id,
        user_msg_id,
        notice_message_id,
        now_ms(),
    )
}

fn record_busy_retry_at(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    notice_message_id: u64,
    now_ms: u64,
) -> Result<BusyRetryDecision, String> {
    if fence::lookup(provider, channel_id).is_none() {
        return record_busy_retry_at_unfenced(
            provider,
            channel_id,
            user_msg_id,
            notice_message_id,
            now_ms,
        );
    }
    fence::write(provider, channel_id, || {
        record_busy_retry_at_unfenced(provider, channel_id, user_msg_id, notice_message_id, now_ms)
    })
}

fn record_busy_retry_at_unfenced(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    notice_message_id: u64,
    now_ms: u64,
) -> Result<BusyRetryDecision, String> {
    let root = runtime_store::discord_busy_followup_retries_root()
        .ok_or_else(|| "AgentDesk runtime root unavailable".to_string())?;
    let _guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut state =
        load_in_root(&root, provider, channel_id, user_msg_id).unwrap_or(BusyFollowupRetryState {
            notice_message_id,
            busy_retry_count: 0,
            first_busy_retry_at_ms: now_ms,
        });
    if state.notice_message_id == 0 {
        state.notice_message_id = notice_message_id;
    }
    if state.first_busy_retry_at_ms == 0 {
        state.first_busy_retry_at_ms = now_ms;
    }
    state.busy_retry_count = state.busy_retry_count.saturating_add(1);
    let elapsed_ms = now_ms.saturating_sub(state.first_busy_retry_at_ms);
    let max_elapsed_ms = MAX_BUSY_RETRY_ELAPSED.as_millis() as u64;
    let capped = state.busy_retry_count >= MAX_BUSY_RETRY_COUNT || elapsed_ms >= max_elapsed_ms;
    save_in_root(&root, provider, channel_id, user_msg_id, state)?;
    Ok(BusyRetryDecision { state, capped })
}

/// Remove retry bindings older than the bounded retention window. The
/// placeholder sweeper calls this periodically; mtime covers bindings that were
/// created but never reached their first busy retry before a crash.
pub(in crate::services::discord) fn sweep_expired() -> usize {
    let Some(root) = runtime_store::discord_busy_followup_retries_root() else {
        return 0;
    };
    if !fence::any_protected() {
        let _guard = STORE_WRITE_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        return sweep_expired_in_root_at(&root, now_ms());
    }
    let mut removed = 0;
    let mut protected = Vec::new();
    let now;
    {
        let _guard = STORE_WRITE_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        now = now_ms();
        let Ok(providers) = fs::read_dir(&root) else {
            return 0;
        };
        for provider in providers.flatten() {
            let kind =
                ProviderKind::from_str_or_unsupported(&provider.file_name().to_string_lossy());
            let Ok(channels) = fs::read_dir(provider.path()) else {
                continue;
            };
            for channel in channels.flatten() {
                let id = channel
                    .file_name()
                    .to_str()
                    .and_then(|n| n.parse::<u64>().ok());
                if let Some(id) = id.filter(|id| fence::lookup(&kind, *id).is_some()) {
                    protected.push((kind.clone(), id, channel.path()));
                } else {
                    removed += sweep_channel(&channel.path(), &root, now);
                }
            }
        }
    }
    // Never wait for canonical admission while holding the global store lock.
    for (kind, id, path) in protected {
        removed += fence::write(&kind, id, || {
            let _guard = STORE_WRITE_LOCK
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            sweep_channel_checked(&path, &root, now)
        })
        .unwrap_or(0);
    }
    removed
}

pub(in crate::services::discord) fn clear_for_input(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
) -> bool {
    let Some(state) = load(provider, channel_id, user_msg_id) else {
        return false;
    };
    clear_if_current(provider, channel_id, user_msg_id, state.notice_message_id)
}

pub(in crate::services::discord) fn clear_if_current(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    notice_message_id: u64,
) -> bool {
    if fence::lookup(provider, channel_id).is_none() {
        return clear_if_current_unfenced(provider, channel_id, user_msg_id, notice_message_id);
    }
    fence::write(provider, channel_id, || {
        let root = runtime_store::discord_busy_followup_retries_root()
            .ok_or_else(|| "AgentDesk runtime root unavailable".to_owned())?;
        let _guard = STORE_WRITE_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = input_file_path_in_root(&root, provider, channel_id, user_msg_id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.to_string()),
        };
        let current: BusyFollowupRetryState =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        if current.notice_message_id != notice_message_id {
            return Ok(false);
        }
        fs::remove_file(&path).map_err(|error| error.to_string())?;
        runtime_store::fsync_parent_dir(&path).map_err(|error| error.to_string())?;
        Ok(true)
    })
    .unwrap_or(false)
}

fn clear_if_current_unfenced(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    notice_message_id: u64,
) -> bool {
    let Some(root) = runtime_store::discord_busy_followup_retries_root() else {
        return false;
    };
    let _guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some(current) = load_in_root(&root, provider, channel_id, user_msg_id) else {
        return false;
    };
    if current.notice_message_id != notice_message_id {
        return false;
    }
    fs::remove_file(input_file_path_in_root(
        &root,
        provider,
        channel_id,
        user_msg_id,
    ))
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_root(test: impl FnOnce()) {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        test();
    }

    #[test]
    fn b2_busy_off_delta_zero_and_frozen_sweep_preserves_population() {
        with_root(|| {
            let provider = ProviderKind::Claude;
            let channel = 6_325_309;
            let root = runtime_store::discord_busy_followup_retries_root().unwrap();
            let path = input_file_path_in_root(&root, &provider, channel, 8);
            let first = bind_notice_if_absent(&provider, channel, 8, 108).unwrap();
            let bytes = fs::read(&path).unwrap();
            fs::remove_file(&path).unwrap();
            assert_eq!(
                bind_notice_if_absent_unfenced(&provider, channel, 8, 108).unwrap(),
                first
            );
            assert_eq!(fs::read(&path).unwrap(), bytes);
            let result = record_busy_retry_at(&provider, channel, 8, 108, 100).unwrap();
            let after = fs::read(&path).unwrap();
            fs::write(&path, &bytes).unwrap();
            let baseline = record_busy_retry_at_unfenced(&provider, channel, 8, 108, 100).unwrap();
            assert_eq!(result.state, baseline.state);
            assert_eq!(result.capped, baseline.capped);
            assert_eq!(fs::read(&path).unwrap(), after);
            assert!(!clear_if_current(&provider, channel, 8, 999));
            assert!(!clear_if_current_unfenced(&provider, channel, 8, 999));
            assert_eq!(fs::read(&path).unwrap(), after);
            assert!(clear_if_current(&provider, channel, 8, 108));
            fs::write(&path, &after).unwrap();
            assert!(clear_if_current_unfenced(&provider, channel, 8, 108));
            assert_eq!(
                bind_notice_if_absent(&provider, channel, 0, 0).unwrap_err(),
                bind_notice_if_absent_unfenced(&provider, channel, 0, 0).unwrap_err()
            );
            assert!(
                !runtime_store::runtime_root()
                    .unwrap()
                    .join("discord_inflight")
                    .exists()
            );
            let protected = 6_325_310;
            let gate = fence::Gate::protect(provider.clone(), protected).unwrap();
            let _health =
                crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
            let protected_path = input_file_path_in_root(&root, &provider, protected, 8);
            bind_notice_if_absent(&provider, protected, 8, 108).unwrap();
            assert!(!clear_if_current(&provider, protected, 8, 999));
            assert!(clear_if_current(&provider, protected, 8, 108));
            fs::create_dir(&protected_path).unwrap();
            assert!(!clear_if_current(&provider, protected, 8, 108));
            assert!(
                fence::health_reasons()
                    .iter()
                    .any(|reason| reason.contains("channel=6325310")
                        && reason.contains("Persistence"))
            );
            fs::remove_dir(&protected_path).unwrap();
            gate.clear_failure_for_test();
            let _closing = gate.close().unwrap();
            let state = BusyFollowupRetryState {
                notice_message_id: 108,
                busy_retry_count: 1,
                first_busy_retry_at_ms: 1,
            };
            save_in_root(&root, &provider, protected, 8, state).unwrap();
            save_in_root(&root, &provider, channel, 8, state).unwrap();
            let invalid = root.join("claude/non-numeric/8.json");
            fs::create_dir_all(invalid.parent().unwrap()).unwrap();
            fs::write(&invalid, serde_json::to_vec(&state).unwrap()).unwrap();
            assert_eq!(
                sweep_expired(),
                2,
                "unrelated off and invalid-name legacy entries keep original cleanup"
            );
            assert!(load_in_root(&root, &provider, protected, 8).is_some());
            assert!(record_busy_retry_at(&provider, protected, 8, 108, 200).is_err());
            assert!(!clear_if_current(&provider, protected, 8, 108));
            gate.clear_failure_for_test();
        });
    }

    #[test]
    fn repeated_busy_attempts_keep_one_notice_binding_and_cap_4888() {
        with_root(|| {
            let provider = ProviderKind::Claude;
            let channel_id = 48_880;
            let user_msg_id = 48_881;
            assert_eq!(
                bind_notice_if_absent(&provider, channel_id, user_msg_id, 700)
                    .expect("first bind")
                    .notice_message_id,
                700
            );
            assert_eq!(
                bind_notice_if_absent(&provider, channel_id, user_msg_id, 701)
                    .expect("second bind")
                    .notice_message_id,
                700,
                "retry must edit the existing card instead of binding a new POST"
            );
            for count in 1..=MAX_BUSY_RETRY_COUNT {
                let decision = record_busy_retry_at(
                    &provider,
                    channel_id,
                    user_msg_id,
                    701,
                    1_000 + u64::from(count),
                )
                .expect("record retry");
                assert_eq!(decision.state.notice_message_id, 700);
                assert_eq!(decision.capped, count == MAX_BUSY_RETRY_COUNT);
            }
            let persisted = load(&provider, channel_id, user_msg_id).expect("persisted state");
            assert_eq!(persisted.busy_retry_count, MAX_BUSY_RETRY_COUNT);
        });
    }

    #[test]
    fn merged_head_inherits_source_retry_identity_and_unrelated_head_does_not_4888() {
        with_root(|| {
            let provider = ProviderKind::Claude;
            let channel_id = 48_889;
            let source_id = 48_890;
            let merged_head_id = 48_891;
            let unrelated_head_id = 48_892;
            bind_notice_if_absent(&provider, channel_id, source_id, 903).expect("bind source");
            for count in 1..=MAX_BUSY_RETRY_COUNT {
                record_busy_retry_at(
                    &provider,
                    channel_id,
                    source_id,
                    903,
                    1_000 + u64::from(count),
                )
                .expect("record source retry");
            }

            let merged = resolve_identity(
                &provider,
                channel_id,
                merged_head_id,
                &[
                    serenity::model::id::MessageId::new(merged_head_id),
                    serenity::model::id::MessageId::new(source_id),
                ],
            );
            assert_eq!(merged.user_msg_id, source_id);
            assert_eq!(merged.state.expect("source state").notice_message_id, 903);
            assert!(state_is_capped(merged.state));

            bind_notice_if_absent(&provider, channel_id, merged_head_id, 904)
                .expect("bind merged head");
            let head_owned = resolve_identity(
                &provider,
                channel_id,
                merged_head_id,
                &[
                    serenity::model::id::MessageId::new(merged_head_id),
                    serenity::model::id::MessageId::new(source_id),
                ],
            );
            assert_eq!(head_owned.user_msg_id, merged_head_id);
            assert_eq!(
                head_owned.state.expect("head state").notice_message_id,
                904,
                "an existing head state must not inherit an older source binding"
            );

            let unrelated = resolve_identity(
                &provider,
                channel_id,
                unrelated_head_id,
                &[serenity::model::id::MessageId::new(unrelated_head_id)],
            );
            assert_eq!(unrelated.user_msg_id, unrelated_head_id);
            assert!(unrelated.state.is_none());
            assert!(!state_is_capped(unrelated.state));
        });
    }

    #[test]
    fn stale_bindings_are_swept_by_retry_timestamp_4888() {
        with_root(|| {
            let provider = ProviderKind::Claude;
            let root = runtime_store::discord_busy_followup_retries_root().expect("store root");
            let stale_at = 1_000;
            save_in_root(
                &root,
                &provider,
                48_884,
                48_885,
                BusyFollowupRetryState {
                    notice_message_id: 900,
                    busy_retry_count: 1,
                    first_busy_retry_at_ms: stale_at,
                },
            )
            .expect("save stale retry");
            save_in_root(
                &root,
                &provider,
                48_884,
                48_886,
                BusyFollowupRetryState {
                    notice_message_id: 901,
                    busy_retry_count: 1,
                    first_busy_retry_at_ms: stale_at + BUSY_RETRY_STORE_TTL.as_millis() as u64 + 1,
                },
            )
            .expect("save current retry");

            assert_eq!(
                sweep_expired_in_root_at(&root, stale_at + BUSY_RETRY_STORE_TTL.as_millis() as u64,),
                1
            );
            assert!(load_in_root(&root, &provider, 48_884, 48_885).is_none());
            assert!(load_in_root(&root, &provider, 48_884, 48_886).is_some());
        });
    }

    #[test]
    fn crash_before_first_retry_binding_is_swept_by_file_mtime_4888() {
        with_root(|| {
            let provider = ProviderKind::Claude;
            let root = runtime_store::discord_busy_followup_retries_root().expect("store root");
            let channel_id = 48_887;
            let user_msg_id = 48_888;
            save_in_root(
                &root,
                &provider,
                channel_id,
                user_msg_id,
                BusyFollowupRetryState {
                    notice_message_id: 902,
                    busy_retry_count: 0,
                    first_busy_retry_at_ms: 0,
                },
            )
            .expect("save crash-before-retry binding");
            let path = input_file_path_in_root(&root, &provider, channel_id, user_msg_id);
            let stale_mtime_ms = 2_000_u64;
            filetime::set_file_mtime(
                &path,
                filetime::FileTime::from_unix_time((stale_mtime_ms / 1_000) as i64, 0),
            )
            .expect("age crash-before-retry binding");

            assert_eq!(
                sweep_expired_in_root_at(
                    &root,
                    stale_mtime_ms + BUSY_RETRY_STORE_TTL.as_millis() as u64,
                ),
                1
            );
            assert!(load_in_root(&root, &provider, channel_id, user_msg_id).is_none());
        });
    }

    #[test]
    fn elapsed_cap_and_identity_guarded_clear_preserve_current_binding_4888() {
        with_root(|| {
            let provider = ProviderKind::Claude;
            let channel_id = 48_882;
            let user_msg_id = 48_883;
            bind_notice_if_absent(&provider, channel_id, user_msg_id, 800).expect("bind");
            record_busy_retry_at(&provider, channel_id, user_msg_id, 800, 1_000)
                .expect("first retry");
            let decision = record_busy_retry_at(
                &provider,
                channel_id,
                user_msg_id,
                800,
                1_000 + MAX_BUSY_RETRY_ELAPSED.as_millis() as u64,
            )
            .expect("elapsed retry");
            assert!(decision.capped);
            assert!(!clear_if_current(&provider, channel_id, user_msg_id, 801));
            assert!(load(&provider, channel_id, user_msg_id).is_some());
            assert!(clear_if_current(&provider, channel_id, user_msg_id, 800));
            assert!(load(&provider, channel_id, user_msg_id).is_none());
        });
    }
}
