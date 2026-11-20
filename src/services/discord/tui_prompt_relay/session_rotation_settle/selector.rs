//! The channel selector follows a Claude session rotation: the cached provider session and the
//! stored selector move off the session the pane left onto the one it runs now.

use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, HostedRecord, load_hosted_execution_pg,
};

/// The channel's cache after following a rotation.
enum CacheStep {
    /// The cache names the new session.
    New,
    /// No cached session: the stored selector decides the next turn.
    Empty,
    /// A user clear or another chosen session, which the rotation leaves alone.
    Kept,
}

/// Whether the channel selector followed the rotation on this pass.
pub(super) enum Followed {
    /// The cache and the stored selector follow the pane, or a user choice keeps them.
    Done,
    /// The channel row is missing, not this pane's, or could not be written.
    Retry,
    /// A turn holds the channel's session transition; nothing changed.
    Busy,
}

/// Moves a cache still naming a session the pane left onto the new one.
async fn follow_cached_selector(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    rotation: &crate::services::tui_prompt_dedupe::ClaudeSessionRotation,
) -> CacheStep {
    let (left, new) = (&rotation.left_session_ids, &rotation.new_session_id);
    let mut data = shared.core.lock().await;
    let Some(session) = data.sessions.get_mut(&channel_id) else {
        return CacheStep::Empty;
    };
    match session.session_id.clone() {
        _ if session.cleared => CacheStep::Kept,
        None => CacheStep::Empty,
        Some(cached) if cached == *new => CacheStep::New,
        Some(cached) if left.contains(&cached) => {
            session.restore_provider_session(Some(new.clone()));
            tracing::info!(
                provider = "claude",
                channel_id = channel_id.get(),
                tmux_session_name = %rotation.tmux_session_name,
                old_session_id = %cached,
                new_session_id = %new,
                "moved the channel's cached provider session to the rotated Claude session"
            );
            CacheStep::New
        }
        Some(_) => CacheStep::Kept,
    }
}

/// The channel's canonical sessions row, the one a turn start restores from, and its record.
async fn channel_row(
    pool: &sqlx::PgPool,
    shared: &SharedData,
    channel_id: ChannelId,
) -> Result<Option<(i64, HostedRecord)>, String> {
    let channel = channel_id.get().to_string();
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: &shared.token_hash,
        channel_id: &channel,
    };
    let key = HostedLookupKey::Canonical {
        provider: ProviderKind::Claude.as_str(),
        identity,
    };
    match load_hosted_execution_pg(pool, key).await {
        HostedLookup::Found(found) => Ok(Some((found.session_id(), found.record))),
        HostedLookup::Missing => Ok(None),
        HostedLookup::Unknown(error) => Err(error),
        HostedLookup::Conflict(kind) => Err(format!("channel row conflict: {kind:?}")),
    }
}

/// Follows the rotation under the channel's turn-start transition, so no turn sees it half done;
/// `admitted` names a Herdr execution the channel row must hold before anything moves.
async fn follow_channel_selector(
    runtime: &Arc<SharedData>,
    channel_id: ChannelId,
    rotation: &crate::services::tui_prompt_dedupe::ClaudeSessionRotation,
    admitted: Option<&str>,
) -> Followed {
    let Ok(_transition) = runtime.session_transition_lock(channel_id).try_lock_owned() else {
        return Followed::Busy;
    };
    let pool = runtime.pg_pool.as_ref();
    let row = match pool {
        Some(pool) => channel_row(pool, runtime, channel_id).await,
        None => Ok(None),
    };
    if let Some(nonce) = admitted {
        let tmux = rotation.tmux_session_name.as_str();
        let owned = matches!(&row, Ok(Some((_, HostedRecord::Known(execution))))
            if execution.execution_nonce == nonce && execution.owner.logical_key == tmux);
        if !owned {
            return Followed::Retry;
        }
    }
    if matches!(
        follow_cached_selector(runtime, channel_id, rotation).await,
        CacheStep::Kept
    ) {
        return Followed::Done;
    }
    let Some(pool) = pool else {
        return Followed::Done;
    };
    let new = rotation.new_session_id.as_str();
    let moved = crate::db::dispatched_sessions::rotation_selector::move_provider_selector_pg;
    let stored = match row {
        Ok(Some((row, _))) => moved(pool, row, &rotation.left_session_ids, new).await,
        Ok(None) => Err("the channel has no sessions row".to_string()),
        Err(error) => Err(error),
    };
    let Err(error) = stored else {
        return Followed::Done;
    };
    tracing::warn!(
        provider = "claude",
        channel_id = channel_id.get(),
        tmux_session_name = %rotation.tmux_session_name,
        new_session_id = new,
        error,
        "stored provider selector did not move to the rotated Claude session; retrying"
    );
    Followed::Retry
}

/// The bot runtime that owns `channel_id`: the idle loop runs on one bot's runtime, while the
/// channel's own bot holds its row, cache and transition. None when no runtime owns it alone.
async fn channel_runtime(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
) -> Option<Arc<SharedData>> {
    match shared.health_registry.upgrade() {
        Some(registry) => {
            registry
                .shared_for_provider_on_channel(&ProviderKind::Claude, channel_id)
                .await
        }
        None => Some(shared.clone()),
    }
}

/// Follows the rotation on the settle pass's channel, on the bot runtime that owns it.
pub(super) async fn follow_rotation_selector(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    rotation: &crate::services::tui_prompt_dedupe::ClaudeSessionRotation,
) -> Followed {
    let Some(runtime) = channel_runtime(shared, channel_id).await else {
        return Followed::Retry;
    };
    follow_channel_selector(&runtime, channel_id, rotation, None).await
}

/// A Herdr pane without a watcher keeps its rotation; only its channel's selector follows, on the
/// bot runtime that owns the channel and only while its row names the admitted execution.
pub(super) async fn follow_herdr_rotation_selector(
    shared: &Arc<SharedData>,
    rotation: &crate::services::tui_prompt_dedupe::ClaudeSessionRotation,
) {
    let tmux = rotation.tmux_session_name.as_str();
    let dedupe = crate::services::tui_prompt_dedupe::herdr_listing;
    let (false, Some((nonce, true))) = (rotation.selector_followed, dedupe(tmux)) else {
        return;
    };
    let Some(channel) = crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(tmux)
    else {
        return;
    };
    let channel_id = ChannelId::new(channel);
    let Some(runtime) = channel_runtime(shared, channel_id).await else {
        return;
    };
    let followed = follow_channel_selector(&runtime, channel_id, rotation, Some(&nonce)).await;
    if matches!(followed, Followed::Done) {
        let mark = crate::services::tui_prompt_dedupe::mark_claude_rotation_selector_followed;
        mark(tmux, &rotation.new_session_id);
    }
}

/// Applies a rotation still waiting for its settle pass to the channel's cache, so a turn that
/// starts before that pass resumes the session the pane runs now. Returns the cached session.
pub(in crate::services::discord) async fn follow_pending_rotation_selector(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    tmux_session_name: &str,
) -> Option<String> {
    let rotation =
        crate::services::tui_prompt_dedupe::claude_session_rotation_for_tmux(tmux_session_name)?;
    let step = follow_cached_selector(shared, channel_id, &rotation).await;
    matches!(step, CacheStep::New).then_some(rotation.new_session_id)
}
