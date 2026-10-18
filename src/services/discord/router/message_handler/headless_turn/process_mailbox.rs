//! Retire only the idle actor captured by a fresh routine process attempt.

use std::sync::{Arc, atomic::Ordering};

use poise::serenity_prelude::ChannelId;
use tokio::{sync::oneshot, task::JoinHandle};

use crate::services::discord::{SharedData, inflight, turn_bridge::BridgeCompletionSignal};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{ChannelMailboxHandle, ClaimObservation};

pub(super) struct ProcessMailbox {
    shared: Arc<SharedData>,
    channel: ChannelId,
    token: Arc<CancelToken>,
    mailbox: ChannelMailboxHandle,
    observed: ClaimObservation,
}

/// Called with the admission transition guard held, before a provider can finish.
pub(super) async fn capture(
    (shared, channel): (&Arc<SharedData>, ChannelId),
    token: &Arc<CancelToken>,
    metadata: Option<&serde_json::Value>,
) -> Option<ProcessMailbox> {
    if !super::routine_metadata::routine_process_turn(metadata)
        || !matches!(shared.provider, ProviderKind::Claude | ProviderKind::Codex)
    {
        return None;
    }
    let mailbox = shared.mailbox_peek(channel)?;
    let snapshot = mailbox.snapshot().await;
    if !snapshot
        .cancel_token
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, token))
    {
        return None;
    }
    Some(ProcessMailbox {
        shared: shared.clone(),
        channel,
        token: token.clone(),
        mailbox,
        observed: snapshot.claim_observation,
    })
}

impl ProcessMailbox {
    pub(super) fn watch(self, producer: JoinHandle<()>) -> oneshot::Sender<BridgeCompletionSignal> {
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move { self.after_completion(rx, producer).await });
        tx
    }

    async fn after_completion(
        self,
        completion: oneshot::Receiver<BridgeCompletionSignal>,
        producer: JoinHandle<()>,
    ) {
        // CompletionGuard notifies after the bridge's last queue/recovery epilogue.
        // A final stream frame can arrive before the CLI exits, so join it too.
        if completion.await != Ok(BridgeCompletionSignal::Finalized) || producer.await.is_err() {
            return;
        }
        self.remove_settled().await;
    }

    async fn remove_settled(self) {
        let Ok(_transition) = self
            .shared
            .session_transition_lock(self.channel)
            .try_lock_owned()
        else {
            return;
        };
        if !self.token.cancelled.load(Ordering::Acquire)
            || self.token.restart_mode().is_some()
            || self.token.tmux_session_name().is_some()
            || !process_stopped(&self.token)
            || !matches!(
                inflight::load_inflight_state_read_only_result(
                    &self.shared.provider,
                    self.channel.get(),
                ),
                Ok(None)
            )
        {
            return;
        }
        // Watcher admission has its own synchronous fence and need not claim a
        // turn. Keep its absence proof through actor close/unlink, off async
        // workers so no non-Send mutex guard crosses an await.
        let runtime = tokio::runtime::Handle::current();
        let _ = tokio::task::spawn_blocking(move || {
            let _watchers =
                crate::services::discord::tmux_watcher_registry::lock_tmux_watcher_registry();
            if self
                .shared
                .tmux_watchers
                .channel_binding(&self.channel)
                .is_some()
                || !self
                    .shared
                    .tmux_watchers
                    .authoritative_session_bindings(self.channel)
                    .is_empty()
            {
                return;
            }
            runtime.block_on(self.remove_owned_actor());
        })
        .await;
    }

    async fn remove_owned_actor(self) {
        // Actor-side claim comparison also protects a successor that has already
        // finished and become idle on this same actor. CloseIfIdle then proves no
        // token, queue, recovery, dispatch or injection reservation remains.
        let outcome = self
            .shared
            .mailboxes
            .remove_idle_entry_if_current(self.channel, &self.mailbox, self.observed)
            .await;
        tracing::debug!(
            channel_id = self.channel.get(),
            ?outcome,
            "routine process mailbox retirement"
        );
    }
}

fn process_stopped(token: &CancelToken) -> bool {
    let Some(process) = token.captured_child_process() else {
        return true;
    };
    #[cfg(unix)]
    {
        crate::services::process::process_group_probe(process.pid)
            == crate::services::process::ProcessGroupProbe::Gone
    }
    #[cfg(not(unix))]
    {
        let _ = process;
        false
    }
}

#[cfg(test)]
mod tests;
