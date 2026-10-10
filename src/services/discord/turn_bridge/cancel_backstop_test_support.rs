//! Owns only a fixture's spawned bridge while it is paused before its first effect.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use poise::serenity_prelude::ChannelId;
use tokio::sync::Notify;
use tokio::task::AbortHandle;

static BRIDGES: LazyLock<dashmap::DashMap<ChannelId, Arc<OwnedBridge>>> =
    LazyLock::new(dashmap::DashMap::new);

#[derive(Default)]
struct OwnedBridge {
    handle: Mutex<Option<AbortHandle>>,
    entered: AtomicBool,
    changed: Notify,
    released: AtomicBool,
    release: Notify,
}

pub(super) fn record(channel: ChannelId, handle: AbortHandle) {
    let Some(owned) = BRIDGES.get(&channel).map(|slot| slot.value().clone()) else {
        return;
    };
    assert!(
        owned
            .handle
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .replace(handle)
            .is_none()
    );
    owned.changed.notify_waiters();
}

pub(super) async fn pause(channel: ChannelId) {
    let Some(owned) = BRIDGES.get(&channel).map(|slot| slot.value().clone()) else {
        return;
    };
    owned.entered.store(true, Ordering::Release);
    owned.changed.notify_waiters();
    loop {
        let released = owned.release.notified();
        tokio::pin!(released);
        released.as_mut().enable();
        if owned.released.load(Ordering::Acquire) {
            break;
        }
        released.await;
    }
}

pub(in crate::services::discord) struct BridgePause {
    channel: ChannelId,
    owned: Arc<OwnedBridge>,
}

impl BridgePause {
    pub(in crate::services::discord) fn install(channel: ChannelId) -> Self {
        let owned = Arc::new(OwnedBridge::default());
        assert!(BRIDGES.insert(channel, owned.clone()).is_none());
        Self { channel, owned }
    }

    pub(in crate::services::discord) async fn wait_for_entry(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, async {
            loop {
                let changed = self.owned.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.owned.entered.load(Ordering::Acquire)
                    && self
                        .owned
                        .handle
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .is_some()
                {
                    break;
                }
                changed.await;
            }
        })
        .await
        .is_ok()
    }

    pub(in crate::services::discord) async fn abort_and_wait(&self, timeout: Duration) -> bool {
        let handle = self
            .owned
            .handle
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .unwrap();
        handle.abort();
        tokio::time::timeout(timeout, async {
            while !handle.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok()
    }
}

impl Drop for BridgePause {
    fn drop(&mut self) {
        BRIDGES.remove_if(&self.channel, |_, owned| Arc::ptr_eq(owned, &self.owned));
        if let Some(handle) = self
            .owned
            .handle
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
        {
            handle.abort();
        }
        self.owned.released.store(true, Ordering::Release);
        self.owned.release.notify_waiters();
    }
}
