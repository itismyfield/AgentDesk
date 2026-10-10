use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use poise::serenity_prelude::ChannelId;
use tokio::sync::Notify;

use crate::services::turn_orchestrator::TokenFinish;

#[derive(Default)]
struct ReleasePause {
    entered: Notify,
    resume: Notify,
}

#[derive(Default)]
struct SupportState {
    pauses: BTreeMap<u64, Arc<ReleasePause>>,
    finishes: BTreeMap<u64, &'static str>,
}

fn state() -> &'static Mutex<SupportState> {
    static STATE: OnceLock<Mutex<SupportState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(SupportState::default()))
}

pub(in crate::services::discord) struct PauseGuard {
    channel: ChannelId,
    pause: Arc<ReleasePause>,
}

pub(in crate::services::discord) fn install_release_pause(channel: ChannelId) -> PauseGuard {
    let pause = Arc::new(ReleasePause::default());
    let mut state = state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        !state.pauses.contains_key(&channel.get()),
        "only one release barrier may own a channel"
    );
    state.finishes.remove(&channel.get());
    state.pauses.insert(channel.get(), pause.clone());
    PauseGuard { channel, pause }
}

impl PauseGuard {
    pub(in crate::services::discord) async fn wait(&self, timeout: Duration) {
        tokio::time::timeout(timeout, self.pause.entered.notified())
            .await
            .expect("release must reach its evidence barrier");
    }

    pub(in crate::services::discord) fn release(&self) {
        self.pause.resume.notify_one();
    }
}

impl Drop for PauseGuard {
    fn drop(&mut self) {
        let mut state = state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let same = state
            .pauses
            .get(&self.channel.get())
            .is_some_and(|current| Arc::ptr_eq(current, &self.pause));
        if same {
            state.pauses.remove(&self.channel.get());
            state.finishes.remove(&self.channel.get());
        }
        self.pause.resume.notify_one();
    }
}

pub(in crate::services::discord) async fn pause_after_evidence(channel: ChannelId) {
    let pause = state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pauses
        .get(&channel.get())
        .cloned();
    if let Some(pause) = pause {
        pause.entered.notify_one();
        pause.resume.notified().await;
    }
}

pub(in crate::services::discord) fn record_finish(channel: ChannelId, finish: &TokenFinish) {
    let status = match finish {
        TokenFinish::Finished(_) => "finished",
        TokenFinish::NoActiveTurn(_) => "no_active_turn",
        TokenFinish::TokenMismatch { .. } => "token_mismatch",
        TokenFinish::Unavailable => "unavailable",
        TokenFinish::NoMailbox => "no_mailbox",
    };
    state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .finishes
        .insert(channel.get(), status);
}

pub(in crate::services::discord) fn finish_status(channel: ChannelId) -> Option<&'static str> {
    state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .finishes
        .get(&channel.get())
        .copied()
}
