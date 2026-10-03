use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

use super::SharedData;
use super::inflight::InflightTurnState;
use crate::services::provider::{CancelToken, ProviderKind};
use poise::serenity_prelude::ChannelId;

type Key = (PathBuf, String, u64);
static SLOTS: LazyLock<Mutex<HashMap<Key, Weak<Slot>>>> = LazyLock::new(Mutex::default);
const START_WAIT: Duration = Duration::from_secs(5);

struct Slot {
    gate: Arc<RwLock<()>>,
    originals: Mutex<Vec<Weak<OriginalRegistration>>>,
}

pub(super) struct OriginalRegistration {
    _permit: OwnedRwLockReadGuard<()>,
    _slot: Arc<Slot>,
    cancel: Weak<CancelToken>,
    waited_for_recovery: bool,
}

#[derive(Clone)]
pub(super) struct RecoveryRegistration {
    slot: Option<Arc<Slot>>,
    _permit: Option<Arc<OwnedRwLockWriteGuard<()>>>,
}

tokio::task_local! {
    static RECOVERY: RecoveryRegistration;
}

fn enabled() -> bool {
    #[cfg(not(test))]
    {
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ENABLED.get_or_init(configured_enabled)
    }
    #[cfg(test)]
    configured_enabled()
}

fn configured_enabled() -> bool {
    std::env::var("AGENTDESK_CODEX_LIVE_BRIDGE_GUARD").as_deref() != Ok("0")
}

fn key(provider: &ProviderKind, channel_id: u64) -> Key {
    let root = super::inflight::inflight_runtime_root().unwrap_or_default();
    let mut ancestor = root.as_path();
    let mut missing = Vec::new();
    let root = loop {
        if let Ok(mut canonical) = ancestor.canonicalize() {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            break canonical;
        }
        let Some(component) = ancestor.file_name() else {
            break root.clone();
        };
        missing.push(component.to_owned());
        let Some(parent) = ancestor.parent() else {
            break root.clone();
        };
        ancestor = parent;
    };
    (root, provider.as_str().to_owned(), channel_id)
}

fn slot(provider: &ProviderKind, channel_id: u64) -> Arc<Slot> {
    let key = key(provider, channel_id);
    let mut slots = SLOTS.lock().unwrap_or_else(|error| error.into_inner());
    slots.retain(|_, slot| slot.strong_count() > 0);
    let slot = slots.entry(key).or_default();
    if let Some(slot) = slot.upgrade() {
        return slot;
    }
    let new = Arc::new(Slot {
        gate: Arc::new(RwLock::new(())),
        originals: Mutex::default(),
    });
    *slot = Arc::downgrade(&new);
    new
}

pub(super) fn is_live(provider: &ProviderKind, channel_id: u64) -> bool {
    if !matches!(provider, ProviderKind::Codex) || !enabled() {
        return false;
    }
    let slot = slot(provider, channel_id);
    let mut originals = slot
        .originals
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    originals.retain(|original| original.strong_count() > 0);
    !originals.is_empty()
}

pub(super) fn retain_original(
    provider: &ProviderKind,
    channel_id: u64,
    cancel: &Arc<CancelToken>,
) -> Option<Arc<OriginalRegistration>> {
    if !matches!(provider, ProviderKind::Codex) || !enabled() {
        return None;
    }
    let slot = slot(provider, channel_id);
    let originals = slot
        .originals
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    originals.iter().filter_map(Weak::upgrade).find(|original| {
        original
            .cancel
            .upgrade()
            .is_some_and(|owner| Arc::ptr_eq(&owner, cancel))
    })
}

async fn register_original(
    provider: &ProviderKind,
    channel_id: u64,
    cancel: &Arc<CancelToken>,
) -> Result<Option<Arc<OriginalRegistration>>, ()> {
    if !matches!(provider, ProviderKind::Codex) || !enabled() {
        return Ok(None);
    }
    let slot = slot(provider, channel_id);
    let (permit, waited_for_recovery) = match slot.gate.clone().try_read_owned() {
        Ok(permit) => (permit, false),
        Err(_) => (
            tokio::time::timeout(START_WAIT, slot.gate.clone().read_owned())
                .await
                .map_err(|_| ())?,
            true,
        ),
    };
    let original = Arc::new(OriginalRegistration {
        _permit: permit,
        _slot: slot.clone(),
        cancel: Arc::downgrade(cancel),
        waited_for_recovery,
    });
    slot.originals
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(Arc::downgrade(&original));
    Ok(Some(original))
}

// Register before exposing the row; a recovery already in progress gets a bounded wait.
pub(super) async fn register_or_requeue(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
    cancel: &Arc<CancelToken>,
) -> Result<Option<Arc<OriginalRegistration>>, bool> {
    let registration = register_original(provider, state.channel_id, cancel).await;
    let registration = match registration {
        Ok(Some(original)) if original.waited_for_recovery => {
            let snapshot = super::mailbox_snapshot(shared, ChannelId::new(state.channel_id)).await;
            if snapshot
                .cancel_token
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, cancel))
                && snapshot.active_user_message_id.map(|id| id.get()) == Some(state.user_msg_id)
                && super::inflight::load_inflight_state(provider, state.channel_id).is_none_or(
                    |current| {
                        current.user_msg_id == state.user_msg_id
                            && current.turn_nonce == state.turn_nonce
                    },
                )
            {
                Ok(Some(original))
            } else {
                Err(())
            }
        }
        other => other,
    };
    match registration {
        Ok(registration) => Ok(registration),
        Err(()) => {
            let channel = ChannelId::new(state.channel_id);
            let finish = super::mailbox_finish::mailbox_finish_turn_if_matches_episode_started_before_with_actor_without_completion(
                shared, provider, channel, poise::serenity_prelude::MessageId::new(state.user_msg_id),
                state.turn_nonce.clone(), std::time::Instant::now(), Some(cancel.clone()),
            ).await;
            if let Some(removed) = finish.removed_token {
                removed.mark_completion_cleanup();
                removed
                    .cancelled
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                super::saturating_decrement_global_active(shared);
            }
            let queued = super::mailbox_requeue_inflight_for_followup_retry(
                shared, provider, channel, state,
            )
            .await;
            tracing::warn!(
                channel_id = state.channel_id,
                provider = provider.as_str(),
                accepted = queued.enqueued,
                "original_bridge_start_deferred"
            );
            let accepted = queued.enqueued
                || queued.merged
                || matches!(queued.refusal_reason,
                Some(crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdAlreadyQueued
                    | crate::services::turn_orchestrator::EnqueueRefusalReason::LastItemDedup));
            if accepted {
                super::arm_slow_idle_queue_backstop_if_queue_nonempty(
                    shared,
                    provider,
                    channel,
                    "original_bridge_start_deferred",
                )
                .await;
            }
            Err(accepted)
        }
    }
}

pub(super) fn try_recovery(
    provider: &ProviderKind,
    channel_id: u64,
) -> Result<RecoveryRegistration, ()> {
    if !matches!(provider, ProviderKind::Codex) || !enabled() {
        return Ok(RecoveryRegistration {
            slot: None,
            _permit: None,
        });
    }
    let slot = slot(provider, channel_id);
    if let Ok(Some(registration)) = RECOVERY.try_with(|held| {
        held.slot
            .as_ref()
            .filter(|held_slot| Arc::ptr_eq(held_slot, &slot))
            .map(|_| held.clone())
    }) {
        return Ok(registration);
    }
    let permit = slot.gate.clone().try_write_owned().map_err(|_| {
        tracing::info!(
            channel_id,
            provider = provider.as_str(),
            "live_original_bridge_deferred"
        );
    })?;
    Ok(RecoveryRegistration {
        slot: Some(slot),
        _permit: Some(Arc::new(permit)),
    })
}

impl RecoveryRegistration {
    pub(super) fn is_guarded(&self) -> bool {
        self.slot.is_some()
    }

    pub(super) async fn run<T>(&self, future: impl Future<Output = T>) -> T {
        if self.slot.is_none() {
            future.await
        } else {
            RECOVERY.scope(self.clone(), future).await
        }
    }
}

#[cfg(all(test, unix))]
#[path = "live_bridge/guard_tests.rs"]
mod tests;
