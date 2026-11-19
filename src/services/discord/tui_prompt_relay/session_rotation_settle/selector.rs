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

/// Moves a cache still naming the session the pane left onto the new one.
async fn follow_cached_selector(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    rotation: &crate::services::tui_prompt_dedupe::ClaudeSessionRotation,
) -> CacheStep {
    let (Some(old), new) = (rotation.old_session_id.as_deref(), &rotation.new_session_id) else {
        return CacheStep::Kept;
    };
    let mut data = shared.core.lock().await;
    let Some(session) = data.sessions.get_mut(&channel_id) else {
        return CacheStep::Empty;
    };
    match session.session_id.as_deref() {
        _ if session.cleared => CacheStep::Kept,
        None => CacheStep::Empty,
        Some(cached) if cached == new => CacheStep::New,
        Some(cached) if cached == old => {
            session.restore_provider_session(Some(new.clone()));
            tracing::info!(
                provider = "claude",
                channel_id = channel_id.get(),
                tmux_session_name = %rotation.tmux_session_name,
                old_session_id = old,
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

/// Moves the row's stored selector off the old session; a row naming anything else is settled.
async fn move_stored_selector(
    pool: &sqlx::PgPool,
    row: i64,
    rotation: &crate::services::tui_prompt_dedupe::ClaudeSessionRotation,
) -> Result<(), String> {
    let Some(old) = rotation.old_session_id.as_deref() else {
        return Ok(());
    };
    let new = rotation.new_session_id.as_str();
    let moved = crate::db::dispatched_sessions::rotation_selector::move_provider_selector_pg;
    moved(pool, row, old, new).await.map(drop)
}

/// Moves the cache and then the stored selector. Returns false while the channel row is missing
/// or could not be written, so the caller keeps the rotation and tries again on the next pass.
pub(super) async fn follow_rotation_selector(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    rotation: &crate::services::tui_prompt_dedupe::ClaudeSessionRotation,
) -> bool {
    if matches!(
        follow_cached_selector(shared, channel_id, rotation).await,
        CacheStep::Kept
    ) {
        return true;
    }
    let Some(pool) = shared.pg_pool.as_ref() else {
        return true;
    };
    let stored = match channel_row(pool, shared, channel_id).await {
        Ok(Some((row, _))) => move_stored_selector(pool, row, rotation).await,
        Ok(None) => Err("the channel has no sessions row".to_string()),
        Err(error) => Err(error),
    };
    let Err(error) = stored else {
        return true;
    };
    tracing::warn!(
        provider = "claude",
        channel_id = channel_id.get(),
        tmux_session_name = %rotation.tmux_session_name,
        new_session_id = %rotation.new_session_id,
        error,
        "stored provider selector did not move to the rotated Claude session; retrying"
    );
    false
}

/// A Herdr pane without a watcher keeps its rotation; only its channel's selector follows, and
/// only while the channel row names the execution the reconcile admitted for this pane.
pub(super) async fn follow_herdr_rotation_selector(
    shared: &Arc<SharedData>,
    rotation: &crate::services::tui_prompt_dedupe::ClaudeSessionRotation,
) {
    let tmux = rotation.tmux_session_name.as_str();
    let dedupe = crate::services::tui_prompt_dedupe::herdr_listing;
    let (false, Some((nonce, true)), Some(pool)) = (
        rotation.selector_followed,
        dedupe(tmux),
        shared.pg_pool.as_ref(),
    ) else {
        return;
    };
    let Some(channel) = crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(tmux)
    else {
        return;
    };
    let channel_id = ChannelId::new(channel);
    let Ok(Some((row, HostedRecord::Known(execution)))) =
        channel_row(pool, shared, channel_id).await
    else {
        return;
    };
    if execution.execution_nonce != nonce || execution.owner.logical_key != tmux {
        return;
    }
    let followed = match follow_cached_selector(shared, channel_id, rotation).await {
        CacheStep::Kept => true,
        CacheStep::New | CacheStep::Empty => {
            move_stored_selector(pool, row, rotation).await.is_ok()
        }
    };
    if followed {
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
