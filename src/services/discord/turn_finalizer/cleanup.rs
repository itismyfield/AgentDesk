use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use crate::services::provider::ProviderKind;

use super::{FinalizeContext, TerminalEvent, TurnKey};
use crate::services::discord::SharedData;
use crate::services::discord::inflight::RelayOwnerKind;

#[derive(Clone, Copy)]
struct ReactionCleanupRequest {
    channel_id: serenity::model::id::ChannelId,
    message_id: serenity::model::id::MessageId,
    #[cfg_attr(test, allow(dead_code))]
    generation: u64,
    add_checkmark: bool,
    source: &'static str,
}

/// Finalizer-owned reaction cleanup for terminal paths that skipped the normal
/// watcher/bridge `⏳ -> ✅` block.
///
/// Reachable production matrix:
/// * `watcher` / `bridge` / `monitor` submitters pass `clear_inflight = false`,
///   so they never run this helper; watcher owns the normal committed-output
///   reaction block when it is safe to claim completion.
/// * `gate_backstop()` is not a production `submit_terminal` context. It is
///   reached only by `run_backstop_finalize -> do_finalize` after a deferred
///   busy-pane gate, where no caller remains to clear inflight or the reaction.
/// * the no-owner restored-watcher path mutates watcher context into the same
///   `clear_inflight && kickoff_queue && !completion_cleanup && !voice` shape.
///   That path also skipped the normal watcher block, so it needs this fallback.
/// * `AlreadyFinalized` losers only inherit their submitter context, so they
///   cannot become the backstop reaction owner after someone else won the gate.
/// * `StandbyRelay` owns its output outside the bridge-owned delivery path, so
///   a real completion gets the same idempotent `⏳` removal / `✅` add here.
pub(super) fn finalized_reaction_lifecycle(
    key: TurnKey,
    event: &TerminalEvent,
    ctx: FinalizeContext,
    shared: &Arc<SharedData>,
    source: &'static str,
    skip_completion_reaction: bool,
    relay_owner_kind: RelayOwnerKind,
) {
    if key.user_msg_id == 0 || skip_completion_reaction {
        return;
    }
    let message_id = serenity::model::id::MessageId::new(key.user_msg_id);
    if !super::super::formatting::is_real_discord_message_id(message_id) {
        return;
    }
    let backstop_cleanup = ctx.clear_inflight
        && ctx.kickoff_queue
        && !ctx.allow_completion_cleanup
        && !ctx.drain_voice;
    let standby_completion_cleanup = relay_owner_kind == RelayOwnerKind::StandbyRelay
        && matches!(event, TerminalEvent::Complete);
    if !backstop_cleanup && !standby_completion_cleanup {
        return;
    }
    schedule_reaction_cleanup(
        shared.clone(),
        ReactionCleanupRequest {
            channel_id: key.channel_id,
            message_id,
            generation: key.generation,
            add_checkmark: standby_completion_cleanup || !matches!(event, TerminalEvent::Cancel),
            source,
        },
    );
}

/// #4024 E19: remove every dispatch-thread parent mapping whose thread points
/// at `channel_id`, returning the parent channels that lost their mapping.
///
/// The parent list is collected during `retain`, and callers must perform any
/// queue kickoffs only after this function returns. That avoids re-entering the
/// DashMap while a shard ref from the retain walk is live.
pub(in crate::services::discord) fn collect_and_clear_thread_parents(
    shared: &Arc<SharedData>,
    channel_id: serenity::model::id::ChannelId,
) -> Vec<serenity::model::id::ChannelId> {
    let mut parents = Vec::new();
    shared.dispatch.thread_parents.retain(|parent, thread| {
        let remove = *thread == channel_id;
        if remove {
            parents.push(*parent);
        }
        !remove
    });
    parents
}

/// #4024 E19: a thread-parent mapping removal is exactly the event that can
/// strand work queued on the parent channel by ThreadGuard, so pair every
/// removed parent with an immediate deferred idle-queue kickoff.
pub(in crate::services::discord) fn kickoff_thread_parents_after_finalize(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    parents: Vec<serenity::model::id::ChannelId>,
) {
    for parent in parents {
        crate::services::discord::schedule_deferred_idle_queue_kickoff_immediate(
            shared.clone(),
            provider.clone(),
            parent,
            "thread finalize parent queue kick",
        );
    }
}

/// #4198: callers snapshot before any yielding release cleanup, then remove
/// only this value so a same-channel follow-up's replacement survives.
pub(in crate::services::discord) fn snapshot_role_override(
    shared: &Arc<SharedData>,
    channel_id: serenity::model::id::ChannelId,
) -> Option<serenity::model::id::ChannelId> {
    shared
        .dispatch
        .role_overrides
        .get(&channel_id)
        .map(|entry| *entry.value())
}

pub(in crate::services::discord) async fn clear_watchdog_and_kick_thread_parents_after_turn_release(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::model::id::ChannelId,
) {
    super::super::clear_watchdog_deadline_override(channel_id.get()).await;
    let thread_parent_kickoffs = collect_and_clear_thread_parents(shared, channel_id);
    kickoff_thread_parents_after_finalize(shared, provider, thread_parent_kickoffs);
}

pub(in crate::services::discord) fn remove_owned_role_override(
    shared: &Arc<SharedData>,
    channel_id: serenity::model::id::ChannelId,
    owned_role_override: Option<serenity::model::id::ChannelId>,
) {
    if let Some(owned) = owned_role_override {
        shared
            .dispatch
            .role_overrides
            .remove_if(&channel_id, |_, current| *current == owned);
    }
}

/// #3350 ②: pure verdict — must this finalize ENSURE the #3303 DeferredClaim
/// marker for the row it is finalizing? (Same pure-gate pattern as the
/// `should_complete_…` helpers.) All six gates must hold:
///
/// * the terminal carries a real identity AND the row IS that turn (an id-0
///   orphan or a mismatched/newer row proves nothing about this anchor);
/// * the row is a TUI-direct synthetic turn (`turn_source == ExternalInput`);
/// * SC3: WATCHER-owned only — a bridge-owned turn finalizes Done with its own
///   `⏳` cleanup (`turn_bridge`), so a marker would contradict that normal
///   completion with a TTL `⚠`;
/// * I4: `injected_prompt_message_id` pins the row's OWN anchor
///   (`user_msg_id`), never a later injection's overwrite of the shared slot;
/// * a tmux session is present (the marker's reconcile scope needs it).
pub(super) fn should_ensure_synthetic_claim_marker(
    key_user_msg_id: u64,
    row_user_msg_id: u64,
    row_turn_source_external: bool,
    row_relay_owner_watcher: bool,
    row_injected_prompt_message_id: Option<u64>,
    row_tmux_session_present: bool,
) -> bool {
    key_user_msg_id != 0
        && row_user_msg_id == key_user_msg_id
        && row_turn_source_external
        && row_relay_owner_watcher
        && row_injected_prompt_message_id == Some(key_user_msg_id)
        && row_tmux_session_present
}

/// #3350 codex r1-1: submit-time snapshot of the inflight-row fields the
/// finalize-time marker ensure authenticates against. The production watcher
/// submitters clear the row BEFORE submitting the finalize (tmux.rs
/// `finish_restored_watcher_active_turn` docs), so a row re-load inside
/// `do_finalize` is a guaranteed no-op for exactly the turns the ensure
/// exists for — the snapshot, captured from the caller's pre-clear row pinned
/// to the submitted turn, closes that guarantee hole.
#[derive(Clone, Debug)]
pub(in crate::services::discord) struct SyntheticClaimSnapshot {
    pub(in crate::services::discord) user_msg_id: u64,
    pub(in crate::services::discord) turn_source_external: bool,
    pub(in crate::services::discord) relay_owner_watcher: bool,
    pub(in crate::services::discord) injected_prompt_message_id: Option<u64>,
    pub(in crate::services::discord) tmux_session_name: Option<String>,
    pub(in crate::services::discord) started_at: String,
    pub(in crate::services::discord) status_message_id: Option<u64>,
    pub(in crate::services::discord) status_panel_generation: u64,
    pub(in crate::services::discord) save_generation: u64,
    pub(in crate::services::discord) current_tool_line: Option<String>,
    pub(in crate::services::discord) turn_start_offset: Option<u64>,
    pub(in crate::services::discord) relay_ownership_only: bool,
    pub(in crate::services::discord) relay_owner_kind: RelayOwnerKind,
}

impl SyntheticClaimSnapshot {
    pub(in crate::services::discord) fn from_row(
        row: &crate::services::discord::inflight::InflightTurnState,
    ) -> Self {
        use crate::services::discord::inflight::{RelayOwnerKind, TurnSource};
        Self {
            user_msg_id: row.user_msg_id,
            turn_source_external: row.turn_source == TurnSource::ExternalInput,
            relay_owner_watcher: row.relay_owner_kind == RelayOwnerKind::Watcher,
            injected_prompt_message_id: row.injected_prompt_message_id,
            tmux_session_name: row.tmux_session_name.clone(),
            started_at: row.started_at.clone(),
            status_message_id: row.status_message_id,
            status_panel_generation: row.status_panel_generation,
            save_generation: row.save_generation,
            current_tool_line: row.current_tool_line.clone(),
            turn_start_offset: row.turn_start_offset,
            relay_ownership_only: row.relay_ownership_only,
            relay_owner_kind: row.effective_relay_owner_kind(),
        }
    }
}

pub(super) fn enqueue_terminal_status_panel_reconcile(
    key: TurnKey,
    provider: &ProviderKind,
    event: &TerminalEvent,
    submit_snapshot: Option<&SyntheticClaimSnapshot>,
    shared: &SharedData,
) {
    let snapshot = match submit_snapshot {
        Some(snapshot) if snapshot.user_msg_id == key.user_msg_id => snapshot.clone(),
        _ => {
            let Some(row) = crate::services::discord::inflight::load_inflight_state(
                provider,
                key.channel_id.get(),
            ) else {
                return;
            };
            if key.user_msg_id != 0 && !row.matches_finalizer_turn_id(key.user_msg_id) {
                return;
            }
            SyntheticClaimSnapshot::from_row(&row)
        }
    };
    let Some(panel_message_id) = snapshot.status_message_id else {
        return;
    };
    if crate::services::discord::turn_bridge::normalize_status_panel_message_id(Some(
        serenity::model::id::MessageId::new(panel_message_id),
    ))
    .is_none()
    {
        return;
    }
    let terminal_status = match event {
        TerminalEvent::Complete => {
            crate::services::discord::abandon_request_store::TerminalCardStatus::Completed
        }
        TerminalEvent::Cancel | TerminalEvent::GateTimeout { .. } | TerminalEvent::RelayMiss => {
            crate::services::discord::abandon_request_store::TerminalCardStatus::Aborted
        }
    };
    if let Err(error) = crate::services::discord::abandon_request_store::enqueue(
        provider,
        &shared.token_hash,
        key.channel_id.get(),
        crate::services::discord::abandon_request_store::AbandonRecord {
            msg_id: panel_message_id,
            started_at: snapshot.started_at.clone(),
            current_tool_line: snapshot.current_tool_line,
            terminal_status,
            episode: crate::services::discord::abandon_request_store::AbandonEpisodeIdentity {
                user_msg_id: snapshot.user_msg_id,
                started_at: snapshot.started_at,
                status_panel_generation: snapshot.status_panel_generation,
                save_generation: snapshot.save_generation,
            },
        },
    ) {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = key.channel_id.get(),
            panel_message_id,
            error = %error,
            "failed to persist terminal status-panel reconcile request"
        );
    }
}

pub(super) fn relay_ownership_only_for_finalize(
    key: TurnKey,
    provider: &ProviderKind,
    submit_snapshot: Option<&SyntheticClaimSnapshot>,
) -> bool {
    if key.user_msg_id == 0 {
        return false;
    }
    if let Some(snapshot) = submit_snapshot
        && snapshot.user_msg_id == key.user_msg_id
    {
        return snapshot.relay_ownership_only;
    }
    let Some(row) =
        crate::services::discord::inflight::load_inflight_state(provider, key.channel_id.get())
    else {
        return false;
    };
    row.user_msg_id == key.user_msg_id && row.relay_ownership_only
}

pub(super) fn relay_owner_kind_for_finalize(
    key: TurnKey,
    provider: &ProviderKind,
    submit_snapshot: Option<&SyntheticClaimSnapshot>,
) -> RelayOwnerKind {
    if key.user_msg_id == 0 {
        return RelayOwnerKind::None;
    }
    if let Some(snapshot) = submit_snapshot
        && snapshot.user_msg_id == key.user_msg_id
    {
        return snapshot.relay_owner_kind;
    }
    let Some(row) =
        crate::services::discord::inflight::load_inflight_state(provider, key.channel_id.get())
    else {
        return RelayOwnerKind::None;
    };
    if row.user_msg_id == key.user_msg_id {
        row.effective_relay_owner_kind()
    } else {
        RelayOwnerKind::None
    }
}

/// #3350 ②: `do_finalize` entry hook — whatever submitter (watcher / bridge /
/// monitor / backstop) finalizes a watcher-owned TUI-direct synthetic turn,
/// guarantee the durable #3303 DeferredClaim marker exists for its anchor.
/// Idempotent (an existing own-pin/covered marker is never touched; an
/// uncovered stale Abort pin is replaced per the #3303 contract — see
/// `ensure_marker_for_own_synthetic_turn`) and reaction-free: the `⏳`
/// verdict belongs exclusively to the #3303 reconcilers (drain `✅` / sweep
/// TTL `⚠`), so output that commits late after a Stopped event never races a
/// false-`⚠` here. Runs for Cancel too — a cancelled turn with no commit
/// converging to the TTL `⚠` is the honest signal.
///
/// Evidence source (codex r1-1): the SUBMIT-TIME snapshot wins when the
/// submitter carried one — the watcher clears the row before submitting, so
/// for its turns the re-load below proves nothing. The row re-load (which
/// must then run BEFORE the `do_finalize` (A) inflight clear) remains the
/// fallback for submitters that did not capture a snapshot.
pub(super) fn ensure_synthetic_claim_marker_before_clear(
    key: TurnKey,
    provider: &ProviderKind,
    submit_snapshot: Option<&SyntheticClaimSnapshot>,
) {
    if key.user_msg_id == 0 {
        return;
    }
    let snapshot = match submit_snapshot {
        Some(snapshot) => snapshot.clone(),
        None => {
            let Some(row) = crate::services::discord::inflight::load_inflight_state(
                provider,
                key.channel_id.get(),
            ) else {
                return;
            };
            SyntheticClaimSnapshot::from_row(&row)
        }
    };
    if !should_ensure_synthetic_claim_marker(
        key.user_msg_id,
        snapshot.user_msg_id,
        snapshot.turn_source_external,
        snapshot.relay_owner_watcher,
        snapshot.injected_prompt_message_id,
        snapshot.tmux_session_name.is_some(),
    ) {
        return;
    }
    let Some(tmux) = snapshot.tmux_session_name.as_deref() else {
        return;
    };
    let _ = crate::services::discord::tui_direct_abort_marker::ensure_marker_for_own_synthetic_turn(
        provider.as_str(),
        key.channel_id.get(),
        key.user_msg_id,
        tmux,
        &snapshot.started_at,
        snapshot.turn_start_offset,
    );
}

/// Late `AlreadyFinalized` losers still perform guarded active-state cleanup.
/// This is intentionally narrower than `do_finalize`: only the same real turn id
/// can lose mailbox/inflight state, so a newer active turn is preserved.
pub(super) async fn already_finalized_active_state(
    key: TurnKey,
    provider: &ProviderKind,
    event: &TerminalEvent,
    ctx: FinalizeContext,
    shared: &Arc<SharedData>,
) {
    if key.user_msg_id == 0 {
        return;
    }

    let owned_role_override = snapshot_role_override(shared, key.channel_id);
    let _ = crate::services::discord::inflight::clear_inflight_state_if_matches(
        provider,
        key.channel_id.get(),
        key.user_msg_id,
    );

    let finish = super::super::mailbox_finish_turn_if_matches(
        shared,
        provider,
        key.channel_id,
        serenity::model::id::MessageId::new(key.user_msg_id),
    )
    .await;
    let Some(token) = finish.removed_token.as_ref() else {
        return;
    };

    if ctx.allow_completion_cleanup && !matches!(event, TerminalEvent::Cancel) {
        token.mark_completion_cleanup();
    }
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    super::super::saturating_decrement_global_active(shared);
    clear_watchdog_and_kick_thread_parents_after_turn_release(shared, provider, key.channel_id)
        .await;
    if !finish.has_pending {
        remove_owned_role_override(shared, key.channel_id, owned_role_override);
    }
}

#[cfg(not(test))]
fn schedule_reaction_cleanup(shared: Arc<SharedData>, request: ReactionCleanupRequest) {
    super::super::task_supervisor::spawn_observed("turn_finalizer_reaction_cleanup", async move {
        if request.add_checkmark {
            let Some(http) = shared.serenity_http_or_token_fallback() else {
                return;
            };
            let _ = super::super::turn_view_reconciler::note_intake_turn_completed(
                &shared,
                &http,
                request.channel_id,
                request.message_id,
                request.generation,
                request.source,
            )
            .await;
        } else if let Some(http) = shared.serenity_http_or_token_fallback() {
            let _ = super::super::turn_view_reconciler::note_intake_turn_cleared(
                &shared,
                &http,
                request.channel_id,
                request.message_id,
                request.generation,
                request.source,
            )
            .await;
        }
        let target = super::super::turn_view_reconciler::TurnViewTarget::intake_user_message(
            request.channel_id,
            request.message_id,
        );
        let owner = super::super::turn_view_reconciler::turn_view_owner_for_message(
            request.channel_id,
            request.message_id,
            request.generation,
        );
        shared.turn_view_reconciler.evict_finalized(target, &owner);
    });
}

#[cfg(test)]
async fn retry_once_after_backoff<F, Fut, E>(
    mut operation: F,
    backoff: Duration,
) -> Result<(), (E, E)>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
{
    match operation().await {
        Ok(()) => Ok(()),
        Err(first_error) => {
            if !backoff.is_zero() {
                tokio::time::sleep(backoff).await;
            }
            operation()
                .await
                .map_err(|second_error| (first_error, second_error))
        }
    }
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
// #4370 R3-3: widened from `pub(super)` so the tui_prompt_relay stale-reclaim
// F3(b) test (in a sibling discord submodule) can observe that a stale-owner
// reclaim schedules NO reaction change. Test-only (`#[cfg(test)]`); no production
// surface. Both that test and the recorder-driving finalizer tests serialize on
// `crate::config::shared_test_env_lock`, so the global recorder never races.
pub(in crate::services::discord) struct ReactionCleanupRecord {
    pub(super) channel_id: u64,
    pub(super) message_id: u64,
    pub(super) emoji: char,
    pub(super) add: bool,
    pub(super) source: &'static str,
}

#[cfg(test)]
static REACTION_CLEANUP_RECORDS: std::sync::LazyLock<
    std::sync::Mutex<Option<Vec<ReactionCleanupRecord>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
static REACTION_CLEANUP_ATTEMPTS: std::sync::LazyLock<
    std::sync::Mutex<Option<Vec<ReactionCleanupRecord>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
static REACTION_CLEANUP_FAILED_CHANNELS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<u64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

// #4370 R3-3: widened to `pub(in crate::services::discord)` (test-only) so the
// stale-reclaim F3(b) test can drive/observe the recorder — see the
// `ReactionCleanupRecord` note above.
#[cfg(test)]
pub(in crate::services::discord) fn begin_reaction_cleanup_recording() {
    *REACTION_CLEANUP_RECORDS
        .lock()
        .expect("reaction cleanup recorder lock") = Some(Vec::new());
    *REACTION_CLEANUP_ATTEMPTS
        .lock()
        .expect("reaction cleanup attempt recorder lock") = Some(Vec::new());
    REACTION_CLEANUP_FAILED_CHANNELS
        .lock()
        .expect("reaction cleanup failed channel lock")
        .clear();
}

#[cfg(test)]
pub(in crate::services::discord) fn take_reaction_cleanup_records() -> Vec<ReactionCleanupRecord> {
    REACTION_CLEANUP_RECORDS
        .lock()
        .expect("reaction cleanup recorder lock")
        .take()
        .unwrap_or_default()
}

#[cfg(test)]
pub(super) fn take_reaction_cleanup_attempts() -> Vec<ReactionCleanupRecord> {
    REACTION_CLEANUP_ATTEMPTS
        .lock()
        .expect("reaction cleanup attempt recorder lock")
        .take()
        .unwrap_or_default()
}

#[cfg(test)]
pub(super) fn fail_reaction_cleanup_channel(channel_id: serenity::model::id::ChannelId) {
    REACTION_CLEANUP_FAILED_CHANNELS
        .lock()
        .expect("reaction cleanup failed channel lock")
        .insert(channel_id.get());
}

#[cfg(test)]
fn schedule_reaction_cleanup(_shared: Arc<SharedData>, request: ReactionCleanupRequest) {
    if !super::super::formatting::is_real_discord_message_id(request.message_id) {
        return;
    }
    record_reaction_with_dispatch_parent_retry(
        &_shared,
        request.channel_id,
        request.message_id,
        '⏳',
        false,
        request.source,
    );
    if request.add_checkmark {
        record_reaction_with_dispatch_parent_retry(
            &_shared,
            request.channel_id,
            request.message_id,
            '✅',
            true,
            request.source,
        );
    }
}

#[cfg(test)]
fn record_reaction_with_dispatch_parent_retry(
    shared: &SharedData,
    channel_id: serenity::model::id::ChannelId,
    message_id: serenity::model::id::MessageId,
    emoji: char,
    add: bool,
    source: &'static str,
) {
    if try_record_reaction(channel_id, message_id, emoji, add, source).is_ok() {
        return;
    }
    let target_channel_id =
        super::super::formatting::reaction_target_channel_for_shared(shared, channel_id);
    if target_channel_id != channel_id {
        let _ = try_record_reaction(target_channel_id, message_id, emoji, add, source);
    }
}

#[cfg(test)]
fn try_record_reaction(
    channel_id: serenity::model::id::ChannelId,
    message_id: serenity::model::id::MessageId,
    emoji: char,
    add: bool,
    source: &'static str,
) -> Result<(), ()> {
    record_reaction_attempt(channel_id, message_id, emoji, add, source);
    if REACTION_CLEANUP_FAILED_CHANNELS
        .lock()
        .expect("reaction cleanup failed channel lock")
        .contains(&channel_id.get())
    {
        return Err(());
    }
    record_reaction_success(channel_id, message_id, emoji, add, source);
    Ok(())
}

#[cfg(test)]
fn record_reaction_attempt(
    channel_id: serenity::model::id::ChannelId,
    message_id: serenity::model::id::MessageId,
    emoji: char,
    add: bool,
    source: &'static str,
) {
    if let Some(records) = REACTION_CLEANUP_ATTEMPTS
        .lock()
        .expect("reaction cleanup attempt recorder lock")
        .as_mut()
    {
        records.push(ReactionCleanupRecord {
            channel_id: channel_id.get(),
            message_id: message_id.get(),
            emoji,
            add,
            source,
        });
    }
}

#[cfg(test)]
fn record_reaction_success(
    channel_id: serenity::model::id::ChannelId,
    message_id: serenity::model::id::MessageId,
    emoji: char,
    add: bool,
    source: &'static str,
) {
    if let Some(records) = REACTION_CLEANUP_RECORDS
        .lock()
        .expect("reaction cleanup recorder lock")
        .as_mut()
    {
        records.push(ReactionCleanupRecord {
            channel_id: channel_id.get(),
            message_id: message_id.get(),
            emoji,
            add,
            source,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        FinalizeContext, FinalizeOutcome, GATE_BACKSTOP, RECONCILE_INTERVAL, TerminalEvent,
        TurnFinalizer, TurnKey,
    };
    use super::*;
    use crate::services::discord::inflight::{
        InflightTurnState, RelayOwnerKind, TurnSource, clear_inflight_state, save_inflight_state,
    };
    use crate::services::provider::{CancelToken, ProviderKind};
    use serenity::model::id::{ChannelId, MessageId, UserId};
    use std::time::Duration;

    fn test_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
    }

    fn real_message_id(offset: u64) -> u64 {
        940_000_000_000_000 + offset
    }

    fn with_isolated_runtime_root(f: impl FnOnce()) {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        let tmp = tempfile::tempdir().expect("create temp runtime dir for reaction cleanup test");
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap());
        }
        f();
        unsafe {
            match prev {
                Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
        }
    }

    async fn seed_active_turn(
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        user_msg_id: u64,
    ) -> Arc<CancelToken> {
        let token = Arc::new(CancelToken::new());
        shared
            .mailbox(channel_id)
            .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(user_msg_id))
            .await;
        token
    }

    #[test]
    fn complete_finalize_snapshot_queues_status_panel_reconcile() {
        with_isolated_runtime_root(|| {
            let shared = super::super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel_id = ChannelId::new(4_340_001);
            let user_msg_id = 4_340_002;
            let mut row = InflightTurnState::new(
                provider.clone(),
                channel_id.get(),
                None,
                7,
                user_msg_id,
                4_340_003,
                "prompt".to_string(),
                Some("session".to_string()),
                Some("AgentDesk-claude-4340".to_string()),
                Some("/tmp/issue-4340.jsonl".to_string()),
                None,
                0,
            );
            row.status_message_id = Some(4_340_004);
            let snapshot = SyntheticClaimSnapshot::from_row(&row);

            enqueue_terminal_status_panel_reconcile(
                TurnKey::new(channel_id, user_msg_id, 0),
                &provider,
                &TerminalEvent::Complete,
                Some(&snapshot),
                shared.as_ref(),
            );

            let pending = crate::services::discord::abandon_request_store::load_pending(
                &provider,
                &shared.token_hash,
            );
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].0, channel_id.get());
            assert_eq!(pending[0].1.msg_id, 4_340_004);
            assert_eq!(
                pending[0].1.terminal_status,
                crate::services::discord::abandon_request_store::TerminalCardStatus::Completed
            );
        });
    }

    #[test]
    fn cancel_finalize_snapshot_queues_aborted_status_panel_reconcile() {
        with_isolated_runtime_root(|| {
            let shared = super::super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Codex;
            let channel_id = ChannelId::new(4_340_011);
            let user_msg_id = 4_340_012;
            let mut row = InflightTurnState::new(
                provider.clone(),
                channel_id.get(),
                None,
                7,
                user_msg_id,
                4_340_013,
                "prompt".to_string(),
                Some("session".to_string()),
                Some("AgentDesk-codex-4340".to_string()),
                Some("/tmp/issue-4340-cancel.jsonl".to_string()),
                None,
                0,
            );
            row.status_message_id = Some(4_340_014);
            let snapshot = SyntheticClaimSnapshot::from_row(&row);

            enqueue_terminal_status_panel_reconcile(
                TurnKey::new(channel_id, user_msg_id, 0),
                &provider,
                &TerminalEvent::Cancel,
                Some(&snapshot),
                shared.as_ref(),
            );

            let pending = crate::services::discord::abandon_request_store::load_pending(
                &provider,
                &shared.token_hash,
            );
            assert_eq!(pending.len(), 1);
            assert_eq!(
                pending[0].1.terminal_status,
                crate::services::discord::abandon_request_store::TerminalCardStatus::Aborted
            );
        });
    }

    #[test]
    fn remove_owned_role_override_preserves_replacement_and_removes_owned_value() {
        let shared = super::super::super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_198_001);
        let owned = ChannelId::new(4_198_002);
        let replacement = ChannelId::new(4_198_003);

        shared.dispatch.role_overrides.insert(channel_id, owned);
        let owned_role_override = snapshot_role_override(&shared, channel_id);
        shared
            .dispatch
            .role_overrides
            .insert(channel_id, replacement);
        remove_owned_role_override(&shared, channel_id, owned_role_override);
        assert!(
            shared
                .dispatch
                .role_overrides
                .get(&channel_id)
                .is_some_and(|current| *current == replacement),
            "cleanup for turn A must not remove turn B's replacement override"
        );

        shared.dispatch.role_overrides.insert(channel_id, owned);
        let owned_role_override = snapshot_role_override(&shared, channel_id);
        remove_owned_role_override(&shared, channel_id, owned_role_override);
        assert!(
            !shared.dispatch.role_overrides.contains_key(&channel_id),
            "cleanup must remove the override when turn A's owned value is still current"
        );
    }

    fn recorded_actions(records: &[ReactionCleanupRecord]) -> Vec<(u64, u64, char, bool)> {
        records
            .iter()
            .map(|record| {
                (
                    record.channel_id,
                    record.message_id,
                    record.emoji,
                    record.add,
                )
            })
            .collect()
    }

    /// #3350 ②: `should_ensure_synthetic_claim_marker` truth table — each of
    /// the six gates flips the verdict alone (RED per gate), and the all-green
    /// row ensures (the `should_complete_…` truth-table pattern).
    #[test]
    fn should_ensure_synthetic_claim_marker_truth_table() {
        // GREEN: real identity + own row + ExternalInput + Watcher + own
        // injected pin + tmux present.
        assert!(should_ensure_synthetic_claim_marker(
            77,
            77,
            true,
            true,
            Some(77),
            true
        ));
        // RED: id-0 orphan terminal — no identity to authenticate against.
        assert!(!should_ensure_synthetic_claim_marker(
            0,
            0,
            true,
            true,
            Some(0),
            true
        ));
        // RED: the row is a different (newer) turn than the terminal's key.
        assert!(!should_ensure_synthetic_claim_marker(
            77,
            78,
            true,
            true,
            Some(77),
            true
        ));
        // RED: not a TUI-direct synthetic turn (Discord-origin / monitor row).
        assert!(!should_ensure_synthetic_claim_marker(
            77,
            77,
            false,
            true,
            Some(77),
            true
        ));
        // RED (SC3): bridge-owned rows complete their own ⏳ via turn_bridge —
        // a marker would contradict the normal completion with a TTL ⚠.
        assert!(!should_ensure_synthetic_claim_marker(
            77,
            77,
            true,
            false,
            Some(77),
            true
        ));
        // RED (I4): the injected ⏳ slot pins a LATER injection, not this
        // anchor — and an absent pin proves nothing.
        assert!(!should_ensure_synthetic_claim_marker(
            77,
            77,
            true,
            true,
            Some(78),
            true
        ));
        assert!(!should_ensure_synthetic_claim_marker(
            77, 77, true, true, None, true
        ));
        // RED: no tmux session — the marker's reconcile scope needs one.
        assert!(!should_ensure_synthetic_claim_marker(
            77,
            77,
            true,
            true,
            Some(77),
            false
        ));
    }

    /// #3350 ② integration: a terminal finalize over a watcher-owned
    /// TUI-direct synthetic row ensures the durable DeferredClaim marker
    /// pinned to the row's OWN identity — and sends NO reaction (delivery
    /// belongs exclusively to the #3303 reconcilers, so late-committing
    /// output after a Stopped event can never race a false-⚠ here). RED
    /// pre-#3350: a turn claimed before the inline-claim record existed (or
    /// whose record failed) finalized with no marker → eternal anchor ⏳.
    #[test]
    fn finalize_ensures_deferred_claim_marker_for_synthetic_watcher_row() {
        use crate::services::discord::inflight::{
            InflightTurnState, TurnSource, save_inflight_state,
        };

        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared = super::super::super::make_shared_data_for_tests_with_storage(None);
                let ch = ChannelId::new(3_350_100);
                let tid = 3_350_101_u64;
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                // The watcher-owned TUI-direct synthetic row the finalize reads
                // (the exact shape the inline claim persists).
                let mut row = InflightTurnState::new(
                    ProviderKind::Claude,
                    ch.get(),
                    None,
                    0,
                    tid,
                    0,
                    "/loop tick".to_string(),
                    None,
                    Some("tmux-3350".to_string()),
                    None,
                    None,
                    0,
                );
                row.turn_source = TurnSource::ExternalInput;
                row.set_relay_owner_kind(RelayOwnerKind::Watcher);
                row.injected_prompt_message_id = Some(tid);
                save_inflight_state(&row).expect("persist synthetic watcher row");

                begin_reaction_cleanup_recording();
                let outcome = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::bridge(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));

                let markers = crate::services::discord::tui_direct_abort_marker::load_for_channel(
                    "claude",
                    ch.get(),
                );
                assert_eq!(
                    markers.len(),
                    1,
                    "RED pre-#3350: finalize left no marker — the anchor ⏳ of a \
                     turn claimed before the inline record existed had no \
                     reconcile owner"
                );
                let marker = &markers[0];
                assert_eq!(
                    marker.origin,
                    crate::services::discord::tui_direct_abort_marker::MarkerOrigin::DeferredClaim
                );
                assert_eq!(marker.anchor_message_id, tid);
                assert_eq!(
                    marker.foreign_user_msg_id,
                    Some(tid),
                    "the pin is the row's OWN identity (SC1 — never a foreign turn)"
                );
                assert_eq!(
                    marker.foreign_started_at.as_deref(),
                    Some(row.started_at.as_str())
                );
                assert!(
                    take_reaction_cleanup_records().is_empty(),
                    "the ensure must never send reactions — delivery is owned by \
                     the #3303 drain ✅ / sweep TTL ⚠ (I1: zero new reaction sites)"
                );
            });
        });
    }

    /// #3350 codex r1-1 (the production watcher shape): the watcher clears the
    /// row BEFORE submitting the finalize, so the row re-load inside
    /// `do_finalize` proves nothing — the submit-time snapshot must carry the
    /// identity. RED pre-fix: with the row already gone the ensure was a
    /// guaranteed no-op on exactly the watcher path it was built for (a turn
    /// claimed before the inline record existed finalized with no marker —
    /// eternal anchor ⏳). Also pins the negative: a snapshot pinned to a
    /// DIFFERENT turn than the submitted key must ensure NOTHING.
    #[test]
    fn finalize_ensures_marker_from_submit_snapshot_when_watcher_precleared_row() {
        use crate::services::discord::inflight::{
            InflightTurnState, TurnSource, save_inflight_state,
        };

        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared = super::super::super::make_shared_data_for_tests_with_storage(None);
                let ch = ChannelId::new(3_350_200);
                let tid = 3_350_201_u64;
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                let mut row = InflightTurnState::new(
                    ProviderKind::Claude,
                    ch.get(),
                    None,
                    0,
                    tid,
                    0,
                    "/loop tick".to_string(),
                    None,
                    Some("tmux-3350-pre".to_string()),
                    None,
                    None,
                    0,
                );
                row.turn_source = TurnSource::ExternalInput;
                row.set_relay_owner_kind(RelayOwnerKind::Watcher);
                row.injected_prompt_message_id = Some(tid);
                save_inflight_state(&row).expect("persist synthetic watcher row");

                // The watcher's exact production sequence: snapshot, clear, submit.
                let snapshot = super::SyntheticClaimSnapshot::from_row(&row);
                assert!(snapshot.turn_source_external && snapshot.relay_owner_watcher);
                assert_eq!(snapshot.user_msg_id, tid);
                assert_eq!(snapshot.injected_prompt_message_id, Some(tid));
                crate::services::discord::inflight::clear_inflight_state(
                    &ProviderKind::Claude,
                    ch.get(),
                );

                begin_reaction_cleanup_recording();
                let outcome = fin
                    .submit_terminal_with_claim_snapshot(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::watcher(),
                        Some(snapshot),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));

                let markers = crate::services::discord::tui_direct_abort_marker::load_for_channel(
                    "claude",
                    ch.get(),
                );
                assert_eq!(
                    markers.len(),
                    1,
                    "RED pre-r1-1: the row-reload ensure no-op'd on the precleared watcher path"
                );
                assert_eq!(markers[0].anchor_message_id, tid);
                assert_eq!(markers[0].foreign_user_msg_id, Some(tid), "OWN pin (SC1)");
                assert_eq!(
                    markers[0].foreign_started_at.as_deref(),
                    Some(row.started_at.as_str()),
                    "the pin is the SUBMIT-TIME snapshot identity"
                );
                assert!(
                    take_reaction_cleanup_records().is_empty(),
                    "the ensure stays reaction-free (#3303 reconcilers own delivery)"
                );

                // Negative: a snapshot for a DIFFERENT turn (newer row captured
                // by mistake) fails the row-is-this-turn gate — no marker.
                let ch2 = ChannelId::new(3_350_300);
                let key2 = TurnKey::new(ch2, 3_350_301, 0);
                let mut foreign = snapshot_for_other_turn(&row, 3_350_999);
                foreign.tmux_session_name = Some("tmux-3350-pre".to_string());
                let outcome = fin
                    .submit_terminal_with_claim_snapshot(
                        key2,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::watcher(),
                        Some(foreign),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));
                assert!(
                    crate::services::discord::tui_direct_abort_marker::load_for_channel(
                        "claude",
                        ch2.get(),
                    )
                    .is_empty(),
                    "a mismatched snapshot must never pin a marker onto the submitted key"
                );
            });
        });
    }

    /// Helper for the negative leg: the same row's snapshot re-pinned to a
    /// different `user_msg_id` (what a buggy caller passing a newer row's
    /// snapshot would produce).
    fn snapshot_for_other_turn(
        row: &crate::services::discord::inflight::InflightTurnState,
        other_user_msg_id: u64,
    ) -> super::SyntheticClaimSnapshot {
        let mut snapshot = super::SyntheticClaimSnapshot::from_row(row);
        snapshot.user_msg_id = other_user_msg_id;
        snapshot.injected_prompt_message_id = Some(other_user_msg_id);
        snapshot
    }

    #[test]
    fn reconciler_backstop_finalize_removes_hourglass_and_marks_complete() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared = super::super::super::make_shared_data_for_tests_with_storage(None);
                let ch = ChannelId::new(3_334_100);
                let tid = real_message_id(101);
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                begin_reaction_cleanup_recording();
                let deferred = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(deferred, FinalizeOutcome::Deferred));

                tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 3).await;
                tokio::task::yield_now().await;

                let records = take_reaction_cleanup_records();
                assert_eq!(
                    recorded_actions(&records),
                    vec![(ch.get(), tid, '⏳', false), (ch.get(), tid, '✅', true)]
                );
                assert!(records.iter().all(|record| record.source == "finalized"));
            });
        });
    }

    #[test]
    fn backstop_reaction_cleanup_targets_dispatch_parent_channel() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared = super::super::super::make_shared_data_for_tests_with_storage(None);
                let parent = ChannelId::new(3_334_120);
                let thread = ChannelId::new(3_334_121);
                let tid = real_message_id(121);
                shared.dispatch.thread_parents.insert(parent, thread);
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, thread, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(thread, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                begin_reaction_cleanup_recording();
                fail_reaction_cleanup_channel(thread);
                let deferred = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(deferred, FinalizeOutcome::Deferred));

                tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 3).await;
                tokio::task::yield_now().await;

                let attempts = take_reaction_cleanup_attempts();
                assert_eq!(
                    recorded_actions(&attempts),
                    vec![
                        (thread.get(), tid, '⏳', false),
                        (parent.get(), tid, '⏳', false),
                        (thread.get(), tid, '✅', true),
                        (parent.get(), tid, '✅', true)
                    ],
                    "dispatch parent is a retry target only after the original thread channel fails"
                );
                let records = take_reaction_cleanup_records();
                assert_eq!(
                    recorded_actions(&records),
                    vec![
                        (parent.get(), tid, '⏳', false),
                        (parent.get(), tid, '✅', true)
                    ]
                );
            });
        });
    }

    #[test]
    fn backstop_reaction_cleanup_keeps_thread_origin_when_original_succeeds() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared =
                    super::super::super::make_shared_data_for_tests_with_storage(None);
                let parent = ChannelId::new(3_334_122);
                let thread = ChannelId::new(3_334_123);
                let tid = real_message_id(123);
                shared.dispatch.thread_parents.insert(parent, thread);
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, thread, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(thread, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                begin_reaction_cleanup_recording();
                let deferred = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(deferred, FinalizeOutcome::Deferred));

                tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 3).await;
                tokio::task::yield_now().await;

                let attempts = take_reaction_cleanup_attempts();
                assert_eq!(
                    recorded_actions(&attempts),
                    vec![
                        (thread.get(), tid, '⏳', false),
                        (thread.get(), tid, '✅', true)
                    ],
                    "a live dispatch parent mapping must not retarget a message posted in the thread"
                );
                let records = take_reaction_cleanup_records();
                assert_eq!(
                    recorded_actions(&records),
                    vec![
                        (thread.get(), tid, '⏳', false),
                        (thread.get(), tid, '✅', true)
                    ]
                );
            });
        });
    }

    #[test]
    fn backstop_reaction_cleanup_without_mapping_keeps_single_failed_attempt() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared = super::super::super::make_shared_data_for_tests_with_storage(None);
                let ch = ChannelId::new(3_334_124);
                let tid = real_message_id(124);
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                begin_reaction_cleanup_recording();
                fail_reaction_cleanup_channel(ch);
                let deferred = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(deferred, FinalizeOutcome::Deferred));

                tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 3).await;
                tokio::task::yield_now().await;

                let attempts = take_reaction_cleanup_attempts();
                assert_eq!(
                    recorded_actions(&attempts),
                    vec![(ch.get(), tid, '⏳', false), (ch.get(), tid, '✅', true)],
                    "unmapped shared cleanup must not invent a dispatch-parent retry"
                );
                assert!(take_reaction_cleanup_records().is_empty());
            });
        });
    }

    #[test]
    fn standby_relay_completion_finalizer_removes_hourglass_and_marks_complete() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared =
                    super::super::super::make_shared_data_for_tests_with_storage(None);
                let ch = ChannelId::new(3_334_170);
                let tid = real_message_id(171);
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(
                    key,
                    ProviderKind::Claude,
                    RelayOwnerKind::StandbyRelay,
                    &shared,
                );

                let mut row = InflightTurnState::new(
                    ProviderKind::Claude,
                    ch.get(),
                    None,
                    0,
                    tid,
                    0,
                    "standby relay completion".to_string(),
                    None,
                    Some("tmux-3334-standby".to_string()),
                    None,
                    None,
                    0,
                );
                row.set_relay_owner_kind(RelayOwnerKind::StandbyRelay);
                save_inflight_state(&row).expect("persist standby relay row");

                begin_reaction_cleanup_recording();
                let outcome = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));

                let records = take_reaction_cleanup_records();
                assert_eq!(
                    recorded_actions(&records),
                    vec![(ch.get(), tid, '⏳', false), (ch.get(), tid, '✅', true)],
                    "StandbyRelay-owned normal completions skipped bridge delivery, so finalizer cleanup must add the completion reaction"
                );
            });
        });
    }

    #[test]
    fn standby_relay_cancel_does_not_mark_complete() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared = super::super::super::make_shared_data_for_tests_with_storage(None);
                let ch = ChannelId::new(3_334_180);
                let tid = real_message_id(181);
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(
                    key,
                    ProviderKind::Claude,
                    RelayOwnerKind::StandbyRelay,
                    &shared,
                );

                let mut row = InflightTurnState::new(
                    ProviderKind::Claude,
                    ch.get(),
                    None,
                    0,
                    tid,
                    0,
                    "standby relay cancel".to_string(),
                    None,
                    Some("tmux-3334-standby-cancel".to_string()),
                    None,
                    None,
                    0,
                );
                row.set_relay_owner_kind(RelayOwnerKind::StandbyRelay);
                save_inflight_state(&row).expect("persist standby relay row");

                begin_reaction_cleanup_recording();
                let outcome = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Cancel,
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));
                assert!(
                    take_reaction_cleanup_records().is_empty(),
                    "StandbyRelay completion reactions are only for real Complete terminals"
                );
            });
        });
    }

    #[test]
    fn synthetic_message_ids_skip_backstop_reaction_cleanup() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared = super::super::super::make_shared_data_for_tests_with_storage(None);
                let ch = ChannelId::new(3_334_130);
                let tid = 99_u64;
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                begin_reaction_cleanup_recording();
                let deferred = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(deferred, FinalizeOutcome::Deferred));

                tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 3).await;
                tokio::task::yield_now().await;

                assert!(take_reaction_cleanup_records().is_empty());
            });
        });
    }

    #[test]
    fn reaction_cleanup_retries_once_after_failed_attempt() {
        test_rt().block_on(async {
            let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let attempts_for_op = attempts.clone();
            let result = retry_once_after_backoff(
                move || {
                    let attempts_for_op = attempts_for_op.clone();
                    async move {
                        let attempt =
                            attempts_for_op.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if attempt == 0 {
                            Err("first failure")
                        } else {
                            Ok(())
                        }
                    }
                },
                Duration::ZERO,
            )
            .await;

            assert!(result.is_ok());
            assert_eq!(
                attempts.load(std::sync::atomic::Ordering::SeqCst),
                2,
                "cleanup should make the initial attempt plus one retry"
            );
        });
    }

    #[test]
    fn relay_ownership_only_snapshot_skips_backstop_reaction_cleanup() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared =
                    super::super::super::make_shared_data_for_tests_with_storage(None);
                let ch = ChannelId::new(3_334_150);
                let tid = real_message_id(151);
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                let mut row = InflightTurnState::new(
                    ProviderKind::Claude,
                    ch.get(),
                    None,
                    0,
                    tid,
                    0,
                    "This session is being continued from a previous conversation".to_string(),
                    None,
                    Some("tmux-3334-relay-only".to_string()),
                    None,
                    None,
                    0,
                );
                row.turn_source = TurnSource::ExternalInput;
                row.set_relay_owner_kind(RelayOwnerKind::Watcher);
                row.injected_prompt_message_id = Some(tid);
                row.relay_ownership_only = true;
                save_inflight_state(&row).expect("persist relay-only synthetic row");
                let snapshot = SyntheticClaimSnapshot::from_row(&row);
                clear_inflight_state(&ProviderKind::Claude, ch.get());

                begin_reaction_cleanup_recording();
                let outcome = fin
                    .submit_terminal_with_claim_snapshot(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::gate_backstop(),
                        Some(snapshot),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));
                assert!(
                    take_reaction_cleanup_records().is_empty(),
                    "relay_ownership_only compact-note anchors must not receive the backstop ⏳ removal / ✅ add reaction lifecycle"
                );
            });
        });
    }

    #[test]
    fn already_finalized_loser_does_not_claim_reaction_cleanup() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared =
                    super::super::super::make_shared_data_for_tests_with_storage(None);
                let ch = ChannelId::new(3_334_200);
                let tid = real_message_id(201);
                shared
                    .restart.global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                begin_reaction_cleanup_recording();
                let first = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(first, FinalizeOutcome::Finalized { .. }));
                assert!(take_reaction_cleanup_records().is_empty());

                begin_reaction_cleanup_recording();
                let late = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::bridge(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
                assert!(
                    take_reaction_cleanup_records().is_empty(),
                    "reachable AlreadyFinalized losers inherit watcher/bridge/monitor context and must not masquerade as the backstop reaction owner"
                );
            });
        });
    }

    #[test]
    fn watcher_context_skips_extra_reaction_calls() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared = super::super::super::make_shared_data_for_tests_with_storage(None);
                let ch = ChannelId::new(3_334_400);
                let tid = real_message_id(401);
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                begin_reaction_cleanup_recording();
                let outcome = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));
                assert!(take_reaction_cleanup_records().is_empty());
            });
        });
    }

    #[test]
    fn cleanup_targets_turn_identity_and_skips_synthetic_id_zero() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared = super::super::super::make_shared_data_for_tests_with_storage(None);
                let ch = ChannelId::new(3_334_500);
                let old_tid = real_message_id(501);
                let newer_tid = real_message_id(502);
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _newer = seed_active_turn(&shared, ch, newer_tid).await;
                let fin = TurnFinalizer::spawn();

                begin_reaction_cleanup_recording();
                let outcome = fin
                    .submit_terminal(
                        TurnKey::new(ch, old_tid, 0),
                        ProviderKind::Claude,
                        TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(
                    outcome,
                    FinalizeOutcome::Finalized {
                        removed_token: None,
                        ..
                    }
                ));
                let records = take_reaction_cleanup_records();
                assert_eq!(recorded_actions(&records).len(), 2);
                assert!(records.iter().all(|record| record.message_id == old_tid));
                assert!(records.iter().all(|record| record.message_id != newer_tid));
                assert!(shared.mailbox(ch).has_active_turn().await);

                begin_reaction_cleanup_recording();
                let zero = fin
                    .submit_terminal(
                        TurnKey::new(ChannelId::new(3_334_600), 0, 0),
                        ProviderKind::Claude,
                        TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(zero, FinalizeOutcome::Finalized { .. }));
                assert!(take_reaction_cleanup_records().is_empty());
            });
        });
    }
}
