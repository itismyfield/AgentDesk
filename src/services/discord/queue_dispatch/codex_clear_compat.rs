//! A historical clear boundary may retire one ended canary without consuming its queued input.

use super::super::*;
use crate::services::codex_tui::canary::{self, CANARY_CHANNEL, CANARY_TMUX};
use crate::services::tui_prompt_dedupe::binding_context::SpawnNonceMarker;
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context, binding_events};
use crate::services::turn_orchestrator::QueueIdentity;
use std::sync::{
    Weak,
    atomic::{AtomicU64, Ordering},
};

const OWNER: &str = "codex_clear_compat";
static EPOCH: AtomicU64 = AtomicU64::new(0);
static FAILURE: tokio::sync::Mutex<Option<Failure>> = tokio::sync::Mutex::const_new(None);

struct Failure {
    runtime: Weak<SharedData>,
    epoch: u64,
    input: Vec<QueueIdentity>,
}

/// Explicit clear invalidates an in-flight observation without waiting for its database read.
pub(in crate::services::discord) fn reset(shared: &SharedData, channel: ChannelId) {
    if channel.get() == CANARY_CHANNEL {
        EPOCH.fetch_add(1, Ordering::AcqRel);
        shared.queue_park_ledger.clear_promote_hold(channel, OWNER);
    }
}

pub(super) async fn admit(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    _snapshot: &ChannelMailboxSnapshot,
) -> bool {
    if !matches!(provider, ProviderKind::Codex) || channel.get() != CANARY_CHANNEL {
        return true;
    }
    let mut cached = FAILURE.lock().await;
    // A delayed kickoff must not replace a newer input's failure with its older snapshot.
    let current = shared.mailbox(channel).snapshot().await;
    let input: Vec<_> = current
        .intervention_queue
        .iter()
        .map(QueueIdentity::of)
        .collect();
    let epoch = EPOCH.load(Ordering::Acquire);
    if cached.as_ref().is_some_and(|previous| {
        previous.epoch == epoch
            && previous.input == input
            && previous
                .runtime
                .upgrade()
                .is_some_and(|runtime| Arc::ptr_eq(&runtime, shared))
    }) {
        return false;
    }
    *cached = None;
    shared.queue_park_ledger.clear_promote_hold(channel, OWNER);
    // Legacy and healthy live incarnations keep their old path; historical pins require C1.
    if !dedupe::codex_verified_requires_proof(CANARY_TMUX)
        || dedupe::clear_relaunch::relaunch_open(CANARY_TMUX)
        || matches!(
            binding_context::observe_spawn_nonce_marker(CANARY_TMUX),
            SpawnNonceMarker::Known(_)
        )
    {
        return true;
    }
    let decision = evaluate(shared, provider, channel, epoch).await;
    if EPOCH.load(Ordering::Acquire) != epoch {
        return false;
    }
    if let Err(reason) = decision {
        shared
            .queue_park_ledger
            .note_promote_hold(provider, channel, OWNER, reason);
        *cached = Some(Failure {
            runtime: Arc::downgrade(shared),
            epoch,
            input,
        });
        return false;
    }
    true
}

async fn evaluate(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    epoch: u64,
) -> Result<(), &'static str> {
    if !canary::enabled_for(CANARY_TMUX)
        || crate::config::session_hosts::herdr_endpoint(channel.get()).is_some()
    {
        return Err("codex_clear_compat:C1");
    }
    let exact = {
        let core = shared.core.lock().await;
        core.sessions.get(&channel).is_some_and(|session| {
            session.remote_profile_name.is_none()
                && session
                    .channel_name
                    .as_deref()
                    .is_some_and(|name| provider.build_tmux_session_name(name) == CANARY_TMUX)
        })
    };
    if !exact {
        return Err("codex_clear_compat:C1");
    }
    if !dedupe::clear_relaunch::vacant(CANARY_TMUX) {
        return Err("codex_clear_compat:C2");
    }
    let captured = dedupe::clear_relaunch::capture(CANARY_TMUX)
        .map_err(|_| "codex_clear_compat:C3")?
        .ok_or("codex_clear_compat:C3")?;
    let fold = binding_events::codex::read_ownership(&captured.context)
        .map_err(|_| "codex_clear_compat:C3")?;
    if fold.conflicted || !fold.pending.is_empty() || fold.verified.is_none() {
        return Err("codex_clear_compat:C3");
    }
    let Some(pool) = shared.pg_pool.as_ref() else {
        return Err("codex_clear_compat:C4");
    };
    if !boundary_after(pool, captured.committed_at).await {
        return Err("codex_clear_compat:C4");
    }
    // A new explicit clear supersedes the read, including any result it would publish.
    if EPOCH.load(Ordering::Acquire) != epoch {
        return Ok(());
    }
    dedupe::clear_relaunch::commit(&captured, "legacy_clear_boundary")
        .map_err(|_| "codex_clear_compat:C5")
}

async fn boundary_after(pool: &sqlx::PgPool, recorded_at: chrono::DateTime<chrono::Utc>) -> bool {
    sqlx::query_scalar::<_, Option<bool>>(
        "SELECT cleared_at > $2 FROM channel_session_clear_boundaries WHERE channel_id = $1",
    )
    .bind(CANARY_CHANNEL.to_string())
    .bind(recorded_at)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten()
    .unwrap_or(false)
}

#[cfg(test)]
#[path = "codex_clear_compat_tests.rs"]
mod tests;
