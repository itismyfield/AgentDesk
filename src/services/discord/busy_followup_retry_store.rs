//! Durable per-input busy-notice binding and aggregate retry budget.
use super::input_runtime::fence;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::SourceMessageQueuedGeneration;

use super::{
    InflightTurnState, Intervention, MailboxEnqueueOutcome, SharedData,
    mailbox_requeue_intervention_front,
};
use serenity::model::id::{ChannelId, MessageId, UserId};

mod policy;
pub(crate) use policy::{BacklogLayers, LayerSnapshot, RetryClass, parked_only};
use policy::{CheckedInputView, SourceRead, read_source_in_root};
pub(in crate::services::discord) use policy::{auto_pick, decode_checked};

pub(in crate::services::discord) const MAX_BUSY_RETRY_COUNT: u32 = 6;
pub(in crate::services::discord) const MAX_BUSY_RETRY_ELAPSED: Duration =
    Duration::from_secs(5 * 60);
const BUSY_RETRY_STORE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

static STORE_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// The persisted record: count, real attempt cost, cap latch and notice binding.
pub(in crate::services::discord) type BusyFollowupRetryState = policy::RetryBudget;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct BusyRetryDecision {
    pub state: BusyFollowupRetryState,
    pub capped: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[cfg(test)]
thread_local! {
    static TEST_CLOCK_MS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    static RELEASE_FAULTS: std::cell::RefCell<std::collections::VecDeque<bool>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
    static SAVE_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Pins this thread's store clock; `None` returns it to the wall clock.
#[cfg(test)]
pub(in crate::services::discord) fn set_clock_for_tests(now_ms: Option<u64>) {
    TEST_CLOCK_MS.with(|clock| clock.set(now_ms));
}

/// The fallible steps of this thread's next releases, in order (listing, then each save): `true`
/// fails that step once.
#[cfg(test)]
pub(in crate::services::discord) fn fail_release_steps_for_tests(steps: &[bool]) {
    RELEASE_FAULTS.with(|faults| faults.borrow_mut().extend(steps));
}

/// Fails every retry-record write on this thread while set.
#[cfg(test)]
pub(in crate::services::discord) fn fail_saves_for_tests(fail: bool) {
    SAVE_FAULT.with(|fault| fault.set(fail));
}

#[cfg(test)]
fn injected_release_fault() -> bool {
    RELEASE_FAULTS.with(|faults| faults.borrow_mut().pop_front().unwrap_or(false))
}

#[cfg(not(test))]
fn injected_release_fault() -> bool {
    false
}

fn now_ms() -> u64 {
    #[cfg(test)]
    if let Some(now) = TEST_CLOCK_MS.with(std::cell::Cell::get) {
        return now;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Runs one store write under the channel's input fence when the channel has one.
fn fenced<T>(
    provider: &ProviderKind,
    channel_id: u64,
    write: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    match fence::lookup(provider, channel_id) {
        None => write(),
        Some(_) => fence::write(provider, channel_id, write),
    }
}

fn locked_root() -> Result<(PathBuf, std::sync::MutexGuard<'static, ()>), String> {
    let root = runtime_store::discord_busy_followup_retries_root()
        .ok_or_else(|| "AgentDesk runtime root unavailable".to_string())?;
    let guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    Ok((root, guard))
}

fn refused(read: SourceRead) -> Result<Option<BusyFollowupRetryState>, String> {
    match read {
        SourceRead::Absent => Ok(None),
        SourceRead::Budget(budget) => Ok(Some(budget)),
        SourceRead::Unreadable(refusal) => Err(format!(
            "busy retry record unreadable ({:?}): {}",
            refusal.reason, refusal.detail
        )),
    }
}

fn load_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
) -> Option<BusyFollowupRetryState> {
    refused(read_source_in_root(root, provider, channel_id, user_msg_id))
        .ok()
        .flatten()
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

/// The age a record may expire by: only a readable binding with no retry spent. A budget, even a
/// spent one, or a record that cannot be read never ages out.
fn expiry_stamp_ms(path: &Path, bytes: &[u8]) -> Result<Option<u64>, String> {
    let Ok(budget) = decode_checked(bytes) else {
        return Ok(None);
    };
    if budget.has_budget() {
        return Ok(None);
    }
    if budget.first_busy_retry_at_ms != 0 {
        return Ok(Some(budget.first_busy_retry_at_ms));
    }
    let modified = fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map_err(|error| error.to_string())?
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?;
    let modified = u64::try_from(modified.as_millis())
        .map_err(|_| "busy retry mtime exceeds u64".to_owned())?;
    Ok(Some(modified))
}

fn expired(stamp: Option<u64>, now_ms: u64) -> bool {
    stamp.is_some_and(|stamp| {
        now_ms.saturating_sub(stamp) >= BUSY_RETRY_STORE_TTL.as_millis() as u64
    })
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
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let stamp = expiry_stamp_ms(&path, &bytes).ok().flatten();
        if expired(stamp, now_ms) && fs::remove_file(&path).is_ok() {
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
        if expired(expiry_stamp_ms(&path, &bytes)?, now_ms) {
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
    state: &BusyFollowupRetryState,
) -> Result<(), String> {
    if channel_id == 0 || user_msg_id == 0 || state.notice_message_id == 0 {
        return Err("busy follow-up retry ids must be non-zero".to_string());
    }
    #[cfg(test)]
    if SAVE_FAULT.with(std::cell::Cell::get) {
        return Err("injected retry save failure".to_string());
    }
    let json = serde_json::to_string_pretty(state).map_err(|error| error.to_string())?;
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
    let permit = match fence::effect::admit(provider, channel_id.get()) {
        Ok(permit) => permit,
        Err(failure) => {
            use crate::services::turn_orchestrator::EnqueueRefusalReason as R;
            return MailboxEnqueueOutcome {
                refusal_reason: Some(match failure {
                    fence::Failure::Mode(mode) => R::InputModeFenced(mode),
                    fence::Failure::ActorUnreachable => R::ActorUnreachable,
                    fence::Failure::LockTimeout => R::LockTimeout,
                    _ => R::InputPersistence,
                }),
                ..Default::default()
            };
        }
    };
    if permit.is_none() && fence::effect::current().is_none() {
        return Box::pin(requeue_admitted(
            shared,
            provider,
            channel_id,
            inflight_state,
        ))
        .await;
    }
    let (shared, provider, inflight_state) =
        (shared.clone(), provider.clone(), inflight_state.clone());
    fence::effect::run(permit, async move {
        Box::pin(requeue_admitted(
            &shared,
            &provider,
            channel_id,
            &inflight_state,
        ))
        .await
    })
    .await
}

async fn requeue_admitted(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    inflight_state: &InflightTurnState,
) -> MailboxEnqueueOutcome {
    #[cfg(test)]
    tests::pause_root_retry(provider, channel_id).await;
    let user_msg_id = inflight_state.user_msg_id;
    let retry_user_msg_id = inflight_state.effective_busy_followup_retry_user_msg_id();
    if user_msg_id == 0 || inflight_state.user_text.trim().is_empty() {
        return MailboxEnqueueOutcome::default();
    }
    let message_id = MessageId::new(user_msg_id);
    let retry_message_id = MessageId::new(retry_user_msg_id);
    let queued_generation = shared.restart.current_generation;
    // Every merged id rides along so catch-up never recovers one twice. Row order stays because
    // the text fallback maps lines to ids by position; a missing primary or retry goes last.
    let mut source_message_ids: Vec<MessageId> = inflight_state
        .source_message_ids
        .iter()
        .filter(|id| **id != 0)
        .map(|id| MessageId::new(*id))
        .collect();
    source_message_ids.extend([message_id, retry_message_id]);
    let mut seen = std::collections::HashSet::new();
    source_message_ids.retain(|id| seen.insert(*id));
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
    let (root, _guard) = locked_root().ok()?;
    load_in_root(&root, provider, channel_id, user_msg_id)
}

/// The id whose record carries this input's budget: the most spent one, the primary on a tie.
pub(in crate::services::discord) fn resolve_identity(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    source_message_ids: &[MessageId],
) -> BusyFollowupRetryIdentity {
    let view = CheckedInputView::read(provider, channel_id, user_msg_id, source_message_ids);
    let weight = |(id, budget): &(u64, &BusyFollowupRetryState)| {
        let spent = (
            budget.exhausted(),
            budget.busy_retry_count,
            budget.active_retry_ms,
        );
        (budget.has_budget(), spent, *id == user_msg_id)
    };
    let heaviest = view
        .reads()
        .iter()
        .filter_map(|(id, read)| match read {
            SourceRead::Budget(budget) => Some((*id, budget)),
            _ => None,
        })
        .max_by_key(weight);
    BusyFollowupRetryIdentity {
        user_msg_id: heaviest.map_or(user_msg_id, |(id, _)| id),
        state: heaviest.map(|(_, budget)| budget.clone()),
    }
}

/// How the automatic paths may treat one queued input, read from every id it carries.
pub(crate) fn input_class(
    provider: &ProviderKind,
    channel_id: ChannelId,
    item: &Intervention,
) -> RetryClass {
    let primary = item.message_id.get();
    CheckedInputView::read(
        provider,
        channel_id.get(),
        primary,
        &item.source_message_ids,
    )
    .classify()
}

/// Whether any id of this input has spent retries, or cannot be read; such an input never
/// absorbs a later one.
pub(crate) fn carries_retry_budget(
    provider: &ProviderKind,
    channel_id: ChannelId,
    item: &Intervention,
) -> bool {
    let primary = item.message_id.get();
    let view = CheckedInputView::read(
        provider,
        channel_id.get(),
        primary,
        &item.source_message_ids,
    );
    view.reads().iter().any(|(_, read)| match read {
        SourceRead::Absent => false,
        SourceRead::Budget(budget) => budget.has_budget(),
        SourceRead::Unreadable(_) => true,
    })
}

#[cfg(test)]
pub(in crate::services::discord) fn state_is_capped(state: Option<BusyFollowupRetryState>) -> bool {
    state.is_some_and(|state| state.exhausted())
}

#[cfg(test)]
pub(in crate::services::discord) fn is_capped(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
) -> bool {
    state_is_capped(load(provider, channel_id, user_msg_id))
}

/// Bind the first posted placeholder. A stale attempt cannot replace a live binding, so callers
/// edit the returned current message; a binding whose card was answered or lost adopts this one
/// and keeps its budget.
pub(in crate::services::discord) fn bind_notice_if_absent(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    notice_message_id: u64,
) -> Result<BusyFollowupRetryState, String> {
    fenced(provider, channel_id, || {
        bind_notice_if_absent_unfenced(provider, channel_id, user_msg_id, notice_message_id)
    })
}

fn bind_notice_if_absent_unfenced(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    notice_message_id: u64,
) -> Result<BusyFollowupRetryState, String> {
    let (root, _guard) = locked_root()?;
    let state = match refused(read_source_in_root(
        &root,
        provider,
        channel_id,
        user_msg_id,
    ))? {
        Some(current) if current.notice_bound => return Ok(current),
        Some(unbound) => BusyFollowupRetryState {
            notice_message_id,
            notice_bound: true,
            ..unbound
        },
        None => BusyFollowupRetryState {
            notice_message_id,
            busy_retry_count: 0,
            first_busy_retry_at_ms: 0,
            draft_hold: false,
            active_retry_ms: 0,
            capped: false,
            notice_bound: true,
            draft_session: None,
        },
    };
    save_in_root(&root, provider, channel_id, user_msg_id, &state)?;
    Ok(state)
}

/// One real pre-submit failure of `user_msg_id` that cost `attempt`. The spending of every other
/// id of the same input is carried, and a record that cannot be read refuses the write.
/// `draft_session` names the pane that refused it to protect a person's draft.
pub(in crate::services::discord) fn record_attempt(
    provider: &ProviderKind,
    (channel_id, user_msg_id, notice_message_id): (u64, u64, u64),
    sources: &[u64],
    attempt: Duration,
    draft_session: Option<&str>,
) -> Result<BusyRetryDecision, String> {
    let attempt_ms = u64::try_from(attempt.as_millis()).unwrap_or(u64::MAX);
    let ids = (channel_id, user_msg_id, notice_message_id);
    fenced(provider, channel_id, || {
        record_retry_at_unfenced(
            provider,
            ids,
            sources,
            (attempt_ms, now_ms()),
            draft_session,
        )
    })
}

#[cfg(test)]
pub(in crate::services::discord) fn record_busy_retry(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    notice_message_id: u64,
) -> Result<BusyRetryDecision, String> {
    let ids = (channel_id, user_msg_id, notice_message_id);
    record_attempt(provider, ids, &[], Duration::ZERO, None)
}

/// `(channel, user message, notice message)`; `(attempt cost, now)` in milliseconds.
fn record_retry_at_unfenced(
    provider: &ProviderKind,
    (channel_id, user_msg_id, notice_message_id): (u64, u64, u64),
    sources: &[u64],
    (attempt_ms, now_ms): (u64, u64),
    draft_session: Option<&str>,
) -> Result<BusyRetryDecision, String> {
    let (root, _guard) = locked_root()?;
    let mut own = refused(read_source_in_root(
        &root,
        provider,
        channel_id,
        user_msg_id,
    ))?;
    for source in sources.iter().copied().filter(|id| *id != user_msg_id) {
        let Some(carried) = refused(read_source_in_root(&root, provider, channel_id, source))?
        else {
            continue;
        };
        match own.as_mut() {
            Some(own) => own.absorb(&carried),
            None => {
                own = Some(BusyFollowupRetryState {
                    notice_message_id,
                    notice_bound: true,
                    draft_session: None,
                    ..carried
                })
            }
        }
    }
    let state =
        BusyFollowupRetryState::charge(own, notice_message_id, attempt_ms, now_ms, draft_session);
    save_in_root(&root, provider, channel_id, user_msg_id, &state)?;
    let capped = state.capped;
    Ok(BusyRetryDecision { state, capped })
}

/// Remove bindings that spent no retry once the retention window passes. The placeholder
/// sweeper calls this periodically; a budget is kept however old, so age never re-arms a retry.
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

/// One release pass: inputs given a fresh budget, and whether a failed step left any for a later
/// pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::services::discord) struct DraftRelease {
    pub released: usize,
    pub retry: bool,
}

/// Gives this channel's inputs whose last retry met a draft protection a fresh budget once that
/// protection is gone; inputs held by busy turns keep theirs. A failed step never stops the rest.
pub(in crate::services::discord) fn release_draft_holds(
    provider: &ProviderKind,
    channel_id: u64,
) -> DraftRelease {
    let release = || -> Result<DraftRelease, String> {
        let (root, _guard) = locked_root()?;
        let dir = root.join(provider.as_str()).join(channel_id.to_string());
        let listed = if injected_release_fault() {
            Err(std::io::Error::other("injected listing failure"))
        } else {
            fs::read_dir(dir)
        };
        let entries = match listed {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DraftRelease::default());
            }
            Err(error) => return Err(error.to_string()),
        };
        let mut pass = DraftRelease::default();
        for entry in entries {
            let Ok(path) = entry.map(|entry| entry.path()) else {
                pass.retry = true;
                continue;
            };
            let stem = path.file_stem().and_then(|stem| stem.to_str());
            let Some(user_msg_id) = stem.and_then(|stem| stem.parse().ok()) else {
                continue;
            };
            let Ok(bytes) = fs::read(&path) else {
                pass.retry = true;
                continue;
            };
            let Some(state) = decode_checked(&bytes).ok().filter(|state| state.draft_hold) else {
                continue;
            };
            let fresh = state.released_from_draft();
            let saved = if injected_release_fault() {
                Err("injected save failure".to_string())
            } else {
                save_in_root(&root, provider, channel_id, user_msg_id, &fresh)
            };
            match saved {
                Ok(()) => pass.released += 1,
                Err(_) => pass.retry = true,
            }
        }
        Ok(pass)
    };
    fenced(provider, channel_id, release).unwrap_or(DraftRelease {
        released: 0,
        retry: true,
    })
}

/// Each `(channel, input, pane)` whose last retry met a person's draft in that pane; read after a
/// restart, when no watch remains for them.
pub(in crate::services::discord) fn draft_held_inputs(
    provider: &ProviderKind,
) -> Vec<(u64, u64, String)> {
    let Ok((root, _guard)) = locked_root() else {
        return Vec::new();
    };
    let Ok(channels) = fs::read_dir(root.join(provider.as_str())) else {
        return Vec::new();
    };
    let mut held = Vec::new();
    for channel in channels.flatten() {
        let Some(channel_id) = channel.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        for entry in fs::read_dir(channel.path()).into_iter().flatten().flatten() {
            let path = entry.path();
            let Some(input) = path.file_stem().and_then(|s| s.to_str()?.parse().ok()) else {
                continue;
            };
            let budget = fs::read(&path).ok().and_then(|b| decode_checked(&b).ok());
            if let Some(session) = budget.and_then(|b| b.draft_session.filter(|_| b.draft_hold)) {
                held.push((channel_id, input, session));
            }
        }
    }
    held
}

/// Busy-notice bindings kept for one channel, counted per input file.
pub(in crate::services::discord) fn channel_presence(
    provider: &ProviderKind,
    channel_id: u64,
) -> super::health::transcript_turn::Presence {
    let dir = runtime_store::discord_busy_followup_retries_root()
        .map(|root| root.join(provider.as_str()).join(channel_id.to_string()));
    super::health::transcript_turn::Presence::of_dir(dir, |path| {
        Ok(usize::from(
            path.extension().is_some_and(|ext| ext == "json"),
        ))
    })
}

/// Ends the input's tie to its card once its turn reached a terminal answer. Spent retries stay
/// on an unbound record so the same input never starts a fresh budget; a bare binding is removed.
pub(in crate::services::discord) fn release_notice(
    provider: &ProviderKind,
    channel_id: ChannelId,
    inflight_state: &InflightTurnState,
) {
    let user_msg_id = inflight_state.effective_busy_followup_retry_user_msg_id();
    let Some(state) = load(provider, channel_id.get(), user_msg_id) else {
        return;
    };
    if state.has_budget() {
        let _ = unbind_notice_if_current(
            provider,
            channel_id.get(),
            user_msg_id,
            state.notice_message_id,
        );
    } else {
        let _ = clear_if_current(
            provider,
            channel_id.get(),
            user_msg_id,
            state.notice_message_id,
        );
    }
}

/// Marks the card `notice_message_id` as no longer this input's, keeping every spent retry; the
/// next posted placeholder binds in its place.
pub(in crate::services::discord) fn unbind_notice_if_current(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    notice_message_id: u64,
) -> bool {
    fenced(provider, channel_id, || {
        let (root, _guard) = locked_root()?;
        let read = read_source_in_root(&root, provider, channel_id, user_msg_id);
        let Some(current) =
            refused(read)?.filter(|current| current.notice_message_id == notice_message_id)
        else {
            return Ok(false);
        };
        let unbound = BusyFollowupRetryState {
            notice_bound: false,
            ..current
        };
        save_in_root(&root, provider, channel_id, user_msg_id, &unbound)?;
        Ok(true)
    })
    .unwrap_or(false)
}

/// Removes a binding for `notice_message_id` that spent no retry; spent retries are never
/// deleted here.
fn clear_if_current(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    notice_message_id: u64,
) -> bool {
    fenced(provider, channel_id, || {
        let (root, _guard) = locked_root()?;
        let read = read_source_in_root(&root, provider, channel_id, user_msg_id);
        let current = refused(read)?.filter(|current| {
            current.notice_message_id == notice_message_id && !current.has_budget()
        });
        if current.is_none() {
            return Ok(false);
        }
        let path = input_file_path_in_root(&root, provider, channel_id, user_msg_id);
        fs::remove_file(&path).map_err(|error| error.to_string())?;
        runtime_store::fsync_parent_dir(&path).map_err(|error| error.to_string())?;
        Ok(true)
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    static ROOT_RETRY_PROBE: Mutex<
        Option<(
            ChannelId,
            tokio::sync::oneshot::Sender<(bool, bool)>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    > = Mutex::new(None);
    pub(super) async fn pause_root_retry(provider: &ProviderKind, channel: ChannelId) {
        let probe = {
            let mut slot = ROOT_RETRY_PROBE.lock().unwrap();
            if slot.as_ref().is_some_and(|probe| probe.0 == channel) {
                slot.take()
            } else {
                None
            }
        };
        if let Some((_, entered, resume)) = probe {
            let named = fence::effect::current()
                .is_some_and(|permit| permit.validate(provider, channel.get()).is_ok());
            let _ = entered.send((named, fence::require_worker().is_ok()));
            let _ = resume.await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn c1b_retry_root_cancel_releases_effect_before_any_mailbox_readmission() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = shared.provider.clone();
        let channel = ChannelId::new(6_325_538);
        let state = InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            6_325_533,
            6_325_534,
            7,
            "cancel root retry".into(),
            None,
            None,
            None,
            None,
            0,
        );
        let gate = fence::Gate::protect(provider.clone(), channel.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let (entered, observed) = tokio::sync::oneshot::channel();
        let (_release, resume) = tokio::sync::oneshot::channel();
        *ROOT_RETRY_PROBE.lock().unwrap() = Some((channel, entered, resume));
        let mut retry = Box::pin(requeue_inflight_for_followup_retry(
            &shared, &provider, channel, &state,
        ));
        let observed = tokio::select! {
            observation = observed => observation.unwrap(),
            _ = &mut retry => panic!("root retry must reach the rendezvous before mailbox re-admission"),
        };
        let closing = gate.close().unwrap();
        assert!(
            closing.drain().now_or_never().is_none(),
            "root retry owns a preclose effect"
        );
        drop(retry);
        tokio::time::timeout(Duration::from_secs(10), closing.drain())
            .await
            .unwrap();
        assert_eq!(observed, (true, true));
        assert!(shared.mailbox_peek(channel).is_none());
        assert!(fence::effect::current().is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn c1b_retry_requeues_all_sources_and_updates_binding_after_closing() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = shared.provider.clone();
        let channel = ChannelId::new(6_325_603);
        let mut state = InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            6_325_504,
            6_325_505,
            7,
            "retry input".into(),
            None,
            None,
            None,
            None,
            0,
        );
        state.source_message_ids = vec![6_325_504, 6_325_506];
        state.busy_followup_retry_user_msg_id = 6_325_506;
        let gate = fence::Gate::protect(provider.clone(), channel.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let permit = gate.admit().unwrap();
        let closing = gate.close().unwrap();
        let refused =
            requeue_inflight_for_followup_retry(&shared, &provider, channel, &state).await;
        assert!(!refused.enqueued);
        assert_eq!(
            refused.refusal_reason,
            Some(
                crate::services::turn_orchestrator::EnqueueRefusalReason::InputModeFenced(
                    fence::Mode::Closing
                )
            )
        );
        let work_shared = shared.clone();
        let work_provider = provider.clone();
        let result = fence::effect::run(Some(permit), async move {
            bind_notice_if_absent(&work_provider, channel.get(), 6_325_506, 6_325_507).unwrap();
            let queued =
                requeue_inflight_for_followup_retry(&work_shared, &work_provider, channel, &state)
                    .await;
            assert!(queued.enqueued, "{queued:?}");
            record_busy_retry(&work_provider, channel.get(), 6_325_506, 6_325_507).unwrap();
            queued
        })
        .await;
        assert!(result.enqueued);
        closing.drain().await;
        let snapshot = shared.mailbox(channel).snapshot().await;
        assert_eq!(snapshot.intervention_queue.len(), 1);
        assert_eq!(
            snapshot.intervention_queue[0].source_message_ids,
            vec![
                MessageId::new(6_325_504),
                MessageId::new(6_325_506),
                MessageId::new(6_325_505)
            ]
        );
        let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
            &provider,
            &shared.token_hash,
            channel,
        );
        assert_eq!(disk.len(), 1);
        assert_eq!(
            disk[0].source_message_ids,
            snapshot.intervention_queue[0].source_message_ids
        );
        assert_eq!(
            load(&provider, channel.get(), 6_325_506)
                .unwrap()
                .busy_retry_count,
            1
        );
        shared.mailboxes.remove_fixture_for_test(channel);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn c1b_retry_root_admits_protected_open_without_scheduler_writes() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = shared.provider.clone();
        let channel = ChannelId::new(6_325_513);
        let state = InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            6_325_514,
            6_325_515,
            7,
            "root retry input".into(),
            None,
            None,
            None,
            None,
            0,
        );
        let gate = fence::Gate::protect(provider.clone(), channel.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        assert_eq!(fence::require_worker(), Err(fence::Failure::Busy));
        let result = requeue_inflight_for_followup_retry(&shared, &provider, channel, &state).await;
        assert!(
            result.enqueued,
            "root retry must run protected persistence on worker: {result:?}"
        );
        gate.close().unwrap().drain().await;
        let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
            &provider,
            &shared.token_hash,
            channel,
        );
        assert_eq!(disk.len(), 1);
        assert_eq!(disk[0].source_message_ids, vec![MessageId::new(6_325_515)]);
        assert!(fence::effect::current().is_none());
        assert_eq!(fence::require_worker(), Err(fence::Failure::Busy));
        shared.mailboxes.remove_fixture_for_test(channel);
    }

    fn record_busy_retry_at(
        provider: &ProviderKind,
        channel_id: u64,
        user_msg_id: u64,
        notice_message_id: u64,
        now_ms: u64,
    ) -> Result<BusyRetryDecision, String> {
        let ids = (channel_id, user_msg_id, notice_message_id);
        fenced(provider, channel_id, || {
            record_retry_at_unfenced(provider, ids, &[], (0, now_ms), None)
        })
    }

    fn legacy(notice_message_id: u64, count: u32, first_ms: u64) -> BusyFollowupRetryState {
        let json = serde_json::json!({
            "notice_message_id": notice_message_id,
            "busy_retry_count": count,
            "first_busy_retry_at_ms": first_ms,
        });
        serde_json::from_value(json).expect("legacy record")
    }

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
            let ids = (channel, 8, 108);
            let baseline = record_retry_at_unfenced(&provider, ids, &[], (0, 100), None).unwrap();
            assert_eq!(result.state, baseline.state);
            assert_eq!(result.capped, baseline.capped);
            assert_eq!(fs::read(&path).unwrap(), after);
            assert!(!clear_if_current(&provider, channel, 8, 999));
            assert!(
                !clear_if_current(&provider, channel, 8, 108),
                "spent retries stay"
            );
            assert_eq!(fs::read(&path).unwrap(), after);
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
            let state = legacy(108, 0, 1);
            save_in_root(&root, &provider, protected, 8, &state).unwrap();
            save_in_root(&root, &provider, channel, 8, &state).unwrap();
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
            let merged_state = merged.state.expect("source state");
            assert_eq!(merged_state.notice_message_id, 903);
            assert!(state_is_capped(Some(merged_state)));

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
            assert_eq!(
                head_owned.user_msg_id, source_id,
                "the head's own unspent binding must not hide a source's spent budget"
            );
            assert!(state_is_capped(head_owned.state));

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

    /// A terminal answer unties the card but keeps spent retries; age alone never deletes them or
    /// an unreadable record, on the fast or the protected sweep, while an unspent binding ages out.
    #[test]
    fn sweeps_delete_only_unspent_bindings_however_old() {
        with_root(|| {
            let provider = ProviderKind::Claude;
            let root = runtime_store::discord_busy_followup_retries_root().expect("store root");
            let now = 1_000 + 25 * 60 * 60 * 1_000;
            let channel = root.join("claude/48884");
            let seed = |id: u64, state: &BusyFollowupRetryState| {
                save_in_root(&root, &provider, 48_884, id, state).expect("seed");
            };
            seed(48_885, &legacy(900, 1, 1_000));
            seed(48_886, &legacy(901, 0, 1_000));
            let capped = BusyFollowupRetryState::charge(None, 902, 300_000, 1_000, None);
            seed(48_887, &capped);
            fs::write(channel.join("48888.json"), b"{").expect("unreadable record");
            let mut answered = InflightTurnState::new(
                provider.clone(),
                48_884,
                None,
                7,
                48_887,
                902,
                "answered".into(),
                None,
                None,
                None,
                None,
                0,
            );
            answered.busy_followup_retry_user_msg_id = 48_887;
            release_notice(&provider, ChannelId::new(48_884), &answered);
            let capped = BusyFollowupRetryState {
                notice_bound: false,
                ..capped
            };
            assert_eq!(sweep_expired_in_root_at(&root, now), 1);
            assert_eq!(sweep_channel_checked(&channel, &root, now), Ok(0));
            assert!(load_in_root(&root, &provider, 48_884, 48_885).is_some());
            assert!(load_in_root(&root, &provider, 48_884, 48_886).is_none());
            assert_eq!(load_in_root(&root, &provider, 48_884, 48_887), Some(capped));
            assert!(channel.join("48888.json").exists());
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
                &legacy(902, 0, 0),
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
}
