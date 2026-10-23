//! Channel runtime ownership, pending queue observations, and recovery API entry points.

use super::*;

use super::super::rebind_request::{ParsedRebindRequest, parse_rebind_body};
use crate::services::turn_orchestrator::HydratePendingQueueResult;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderMailboxState {
    pub channel_id: u64,
    pub has_cancel_token: bool,
    pub queue_depth: usize,
    pub recovery_started: bool,
}

/// Resolve the runtime that owns `channel_id` for `provider`. Channel-aware
/// so multi-bot deployments (several runtimes under one provider name) stop,
/// drain, and snapshot the runtime that actually handles the channel rather
/// than whichever registered first. Single-bot deployments are unaffected.
pub(super) async fn shared_for_provider(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<Arc<SharedData>> {
    registry
        .shared_for_provider_on_channel(provider, channel_id)
        .await
}

pub(crate) async fn resume_runtime_for_channel(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<Arc<SharedData>> {
    shared_for_provider(registry, provider, channel_id).await
}

pub(crate) fn retain_resume_runtime_owner_before_teardown(
    shared: &SharedData,
    channel_id: ChannelId,
    tmux_session_name: &str,
) -> bool {
    shared
        .tmux_watchers
        .retain_owner_during_session_rebind(tmux_session_name, channel_id)
}

pub(crate) fn clear_resume_runtime_owner_after_death(shared: &SharedData, tmux_session_name: &str) {
    shared
        .tmux_watchers
        .clear_restored_owner_for_tmux_session(tmux_session_name);
}

/// Mirror a successful durable `/resume` target into the owning Discord runtime.
///
/// `retain_runtime_owner` means teardown left the tmux pane alive. In that case
/// the authoritative owner-only registry entry is re-established before the
/// channel session is changed, so direct-pane output remains delayed/routable
/// without promoting the diagnostic dedupe mirror to routing authority.
pub(crate) async fn rebind_channel_provider_session(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    cwd: &str,
    session_id: &str,
    tmux_session_name: &str,
    retain_runtime_owner: bool,
) {
    let retained_runtime_owner = retain_runtime_owner
        && shared
            .tmux_watchers
            .retain_owner_during_session_rebind(tmux_session_name, channel_id);
    let (previous_path, previous_session_id) =
        discord::rebind_channel_session(shared, provider, channel_id, cwd, session_id).await;
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        target: "agentdesk::services::discord::health::recovery",
        provider = provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session_name,
        retained_runtime_owner,
        previous_path = previous_path.as_deref().unwrap_or("<none>"),
        previous_session_id = previous_session_id.as_deref().unwrap_or("<none>"),
        target_cwd = cwd,
        target_session_id = session_id,
        "  [{ts}] ↻ /resume: rebound in-memory session to previous provider session",
    );
}

/// #4790 `/resume` guard: report whether `channel_id` has an active or
/// recovery-blocking turn for `provider_name`. The rebind path refuses to
/// repoint a channel that is mid-turn (its running tmux/provider process would
/// keep writing to the OLD transcript). Returns `false` when no runtime owns the
/// channel (nothing in flight here).
pub(crate) async fn channel_has_active_turn(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
) -> bool {
    let Some(provider) = ProviderKind::from_str(provider_name) else {
        return false;
    };
    let Some(shared) = shared_for_provider(registry, &provider, channel_id).await else {
        return false;
    };
    discord::mailbox_has_blocking_active_turn_or_unreachable(&shared, channel_id).await
}

pub(super) async fn owning_runtime_http_for_channel(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<Arc<serenity::http::Http>> {
    shared_for_provider(registry, provider, channel_id)
        .await
        .and_then(|shared| shared.serenity_http_or_token_fallback())
}

/// Observe the mailbox and its disk queue; an unanswered mailbox has no snapshot.
pub async fn snapshot_pending_queue_state(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
) -> Option<PendingQueueSnapshot> {
    let provider = ProviderKind::from_str(provider_name)?;
    let shared = shared_for_provider(registry, &provider, channel_id).await?;
    snapshot_pending_queue_state_for_shared(&shared, &provider, channel_id).await
}

async fn snapshot_pending_queue_state_for_shared(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<PendingQueueSnapshot> {
    let queued = shared
        .mailbox(channel_id)
        .try_snapshot()
        .await
        .ok()?
        .intervention_queue;
    let queue_depth = queued.len();
    let message_ids = queued
        .iter()
        .map(|intervention| intervention.message_id.get())
        .collect();
    let disk_path = discord::runtime_store::discord_pending_queue_root().map(|root| {
        root.join(provider.as_str())
            .join(&shared.token_hash)
            .join(format!("{}.json", channel_id.get()))
    });
    let disk_present = disk_path.as_ref().and_then(|path| path.try_exists().ok());
    Some(PendingQueueSnapshot {
        queue_depth,
        disk_present,
        disk_path,
        message_ids,
    })
}

/// Hydrate the disk queue when needed, then schedule the surviving work for an idle channel.
/// No queue depth is published without an actor observation after the attempted hydration.
pub async fn schedule_pending_queue_drain_after_cancel(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    reason: &'static str,
) -> PostCancelDrainOutcome {
    let Some(provider) = ProviderKind::from_str(provider_name) else {
        return PostCancelDrainOutcome::skipped();
    };
    let Some(shared) = shared_for_provider(registry, &provider, channel_id).await else {
        return PostCancelDrainOutcome::skipped();
    };
    let Some(snapshot) =
        snapshot_pending_queue_state_for_shared(&shared, &provider, channel_id).await
    else {
        return PostCancelDrainOutcome::skipped();
    };
    // codex review round-4 P2-1 (#1672): hydrate from disk *whenever*
    // the disk file is present, not just when the in-memory queue is
    // empty. If a concurrent `mailbox_enqueue_intervention` slipped a
    // fresh message in between the cancel and this helper running, we
    // still need to merge whatever the disk holds. Actor-local hydrate
    // dedupes by `message_id` and prepends disk items so neither the
    // surviving disk payload nor the live racer is dropped.
    let post_depth = if snapshot.disk_present != Some(false) {
        match hydrate_pending_queue_from_disk(&shared, &provider, channel_id).await {
            Ok(result) => result.queue_len_after,
            Err(refusal) => {
                let diagnostic = legacy_restitution_refusal(refusal).persistence_error;
                tracing::warn!(
                    target: "agentdesk::services::discord::health::recovery",
                    channel_id = channel_id.get(),
                    ?refusal,
                    ?diagnostic,
                    "pending queue hydration refused"
                );
                return PostCancelDrainOutcome::skipped();
            }
        }
    } else {
        snapshot.queue_depth
    };
    if post_depth == 0 {
        return PostCancelDrainOutcome {
            scheduled: false,
            queue_depth_after: Some(0),
        };
    }
    discord::schedule_deferred_idle_queue_kickoff(shared.clone(), provider, channel_id, reason);
    PostCancelDrainOutcome {
        scheduled: true,
        queue_depth_after: Some(post_depth),
    }
}

/// Merge the disk queue inside the actor and restore its role override after an actual response.
pub(super) async fn hydrate_pending_queue_from_disk(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Result<HydratePendingQueueResult, MailboxRefusal> {
    let result =
        discord::mailbox_hydrate_pending_queue_from_disk(shared, provider, channel_id).await?;
    if let Some(alt_channel_id) = result.restored_override {
        shared
            .dispatch
            .role_overrides
            .insert(channel_id, alt_channel_id);
    }
    Ok(result)
}

/// #1672: Resolve a usable tmux session name for cancel observability.
///
/// Order: live tmux watcher binding → persistent inflight state file →
/// `discord_session.channel_name` rendered through the provider's tmux
/// naming convention. Returns `None` when none of those sources knows
/// about the channel — at which point cancel observability falls back
/// to whatever the caller passed in (typically empty).
pub async fn resolve_tmux_session_for_cancel(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
) -> Option<String> {
    let provider = ProviderKind::from_str(provider_name)?;
    let shared = shared_for_provider(registry, &provider, channel_id).await?;
    if let Some(binding) = shared.tmux_watchers.channel_binding(&channel_id) {
        return Some(binding.tmux_session_name);
    }
    if let Some(state) = discord::inflight::load_inflight_state(&provider, channel_id.get())
        && let Some(session) = state.tmux_session_name
    {
        return Some(session);
    }
    let data = shared.core.lock().await;
    data.sessions
        .get(&channel_id)
        .and_then(|session| session.channel_name.as_ref())
        .map(|channel_name| provider.build_tmux_session_name(channel_name))
}

/// #896: Handle `POST /api/inflight/rebind` — rebind a live tmux session to
/// a freshly-created inflight state and respawn the watcher. Used to recover
/// orphan states where tmux is still running but turn_bridge has no inflight
/// to track against (e.g. after a prior turn's cleanup cleared the state and
/// the agent continued work via internal auto-triggers).
///
/// Body shape:
/// ```json
/// { "provider": "claude" | "codex" | "gemini" | "opencode" | "qwen",
///   "channel_id": "1234567890",
///   "tmux_session": "AgentDesk-codex-foo",  // optional — derived otherwise
///   "output_path": "/path/to/live.jsonl",   // optional operator override
///   "session_id": "provider-session-uuid"   // optional operator override
/// }
/// ```
pub async fn handle_rebind_inflight<'a>(
    registry: &HealthRegistry,
    body: &str,
) -> (&'a str, String) {
    let parsed = match parse_rebind_body(body) {
        Ok(p) => p,
        Err((status, body)) => return (status, body),
    };
    let ParsedRebindRequest {
        provider,
        channel_id,
        tmux_session: tmux_override,
        overrides,
    } = parsed;

    let Some(result) = registry
        .rebind_inflight(&provider, channel_id, tmux_override, overrides, None)
        .await
    else {
        // #897 counter-model review: dcserver bootstrap registers the
        // `ProviderEntry` before the provider's Discord HTTP client, so a
        // lookup miss here can mean EITHER permanent misconfiguration OR a
        // transient warmup window. The error text now tells operators to
        // retry instead of assuming the provider is permanently absent.
        return (
            "503 Service Unavailable",
            format!(
                r#"{{"ok":false,"error":"provider {} is not yet available in this dcserver (still warming up or not registered) — retry in a few seconds"}}"#,
                provider.as_str()
            ),
        );
    };

    match result {
        Ok(outcome) => (
            "200 OK",
            serde_json::json!({
                "ok": true,
                "tmux_session": outcome.tmux_session,
                "channel_id": outcome.channel_id.to_string(),
                "initial_offset": outcome.initial_offset,
                "watcher_spawned": outcome.watcher_spawned,
                "watcher_replaced": outcome.watcher_replaced,
            })
            .to_string(),
        ),
        Err(err) => {
            let (status, message) = rebind_error_status_and_message(&err);
            (
                status,
                serde_json::json!({ "ok": false, "error": message }).to_string(),
            )
        }
    }
}

/// #1462: Handle relay recovery dry-run / bounded auto-heal for one channel.
///
/// `apply=false` is the default and only returns the proposed action with
/// evidence. `apply=true` is intentionally conservative: only local,
/// idempotent cleanup paths marked eligible by the recovery planner can run.
pub async fn handle_relay_recovery<'a>(
    registry: &HealthRegistry,
    provider: Option<&str>,
    channel_id: u64,
    apply: bool,
) -> (&'a str, String) {
    match discord::relay_recovery::run_relay_recovery(registry, provider, channel_id, apply).await {
        Ok(response) => (
            "200 OK",
            serde_json::to_string(&response).unwrap_or_else(|error| {
                serde_json::json!({
                    "ok": false,
                    "error": format!("failed to serialize relay recovery response: {error}")
                })
                .to_string()
            }),
        ),
        Err(error) => (error.status_str(), error.body().to_string()),
    }
}
