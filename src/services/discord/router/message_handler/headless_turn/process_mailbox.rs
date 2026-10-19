//! Retire only the idle actor captured by a fresh routine process attempt.

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use poise::serenity_prelude::ChannelId;
use tokio::{sync::oneshot, task::JoinHandle, time::Instant};

use crate::services::discord::{SharedData, inflight, turn_bridge::BridgeCompletionSignal};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, ClaimObservation, registry_purge::MailboxPurgeOutcome,
};

const SETTLEMENT_WINDOW: Duration = Duration::from_secs(5);
const SETTLEMENT_INTERVAL: Duration = Duration::from_millis(100);

enum Settlement {
    Complete(&'static str),
    Retry(&'static str),
    Retain(&'static str),
}

pub(super) struct ProcessMailbox {
    shared: Arc<SharedData>,
    channel: ChannelId,
    token: Arc<CancelToken>,
    mailbox: ChannelMailboxHandle,
    observed: ClaimObservation,
    user_message_id: u64,
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
        user_message_id: snapshot.active_user_message_id?.get(),
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
        let reason = match completion.await {
            Ok(BridgeCompletionSignal::Finalized) => None,
            Ok(BridgeCompletionSignal::Unresolved) => Some("completion_unresolved"),
            Ok(BridgeCompletionSignal::DeferredToCustody) => Some("completion_deferred_to_custody"),
            Ok(BridgeCompletionSignal::DeferredToOwner) => Some("completion_deferred_to_owner"),
            Ok(BridgeCompletionSignal::EntryAborted) => Some("completion_entry_aborted"),
            Err(_) => Some("completion_signal_missing"),
        };
        if let Some(reason) = reason {
            self.observe("retained", reason, 0);
            return;
        }
        self.observe("producer_wait", "terminal_finalized", 0);
        if producer.await.is_err() {
            self.observe("retained", "producer_join_failed", 0);
            return;
        }
        self.observe("producer_joined", "join_ok", 0);
        let deadline = Instant::now() + SETTLEMENT_WINDOW;
        let mut attempts = 0;
        let mut last_refusal = None;
        loop {
            attempts += 1;
            match self.remove_settled().await {
                Settlement::Complete(reason) => {
                    self.observe("completed", reason, attempts);
                    return;
                }
                Settlement::Retain(reason) => {
                    self.observe("retained", reason, attempts);
                    return;
                }
                Settlement::Retry(reason) => {
                    if last_refusal != Some(reason) {
                        self.observe("settlement_retry", reason, attempts);
                        last_refusal = Some(reason);
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        self.observe("budget_exhausted", reason, attempts);
                        return;
                    }
                    tokio::time::sleep_until((now + SETTLEMENT_INTERVAL).min(deadline)).await;
                }
            }
        }
    }

    fn observe(&self, event: &'static str, reason: &'static str, attempts: u32) {
        tracing::info!(
            channel_id = self.channel.get(),
            provider = self.shared.provider.as_str(),
            event,
            reason,
            attempts,
            "routine process mailbox settlement"
        );
    }

    async fn remove_settled(&self) -> Settlement {
        // A retry never re-captures ownership, even if a successor has already
        // finished. Check history before temporary settlement evidence too.
        let Some(current) = self.shared.mailbox_peek(self.channel) else {
            return Settlement::Complete("registry_absent");
        };
        if !current.same_actor(&self.mailbox) {
            return Settlement::Retain("actor_changed");
        }
        let Ok(snapshot) = current.try_snapshot().await else {
            return Settlement::Retain("actor_unreachable");
        };
        if snapshot.claim_observation != self.observed {
            return Settlement::Retain("claim_changed");
        }
        if !snapshot.intervention_queue.is_empty() {
            return Settlement::Retain("queue_not_empty");
        }
        if snapshot.recovery_started_at.is_some() {
            return Settlement::Retain("recovery_in_progress");
        }
        if snapshot.pending_user_dispatch.is_some() {
            return Settlement::Retain("pending_user_dispatch");
        }
        if self.token.restart_mode().is_some() {
            return Settlement::Retain("restart_owned");
        }
        if self.token.tmux_session_name().is_some() {
            return Settlement::Retain("tmux_bound");
        }
        let transition = self
            .shared
            .session_transition_lock(self.channel)
            .try_lock_owned()
            .ok();
        let mut retry = transition.is_none().then_some("transition_busy");
        if !self.token.cancelled.load(Ordering::Acquire) {
            retry.get_or_insert("token_not_settled");
        }
        if let Some(reason) = process_refusal(&self.token) {
            retry.get_or_insert(reason);
        }
        match inflight::load_inflight_state_read_only_result(
            &self.shared.provider,
            self.channel.get(),
        ) {
            Ok(None) => {}
            Ok(Some(row)) => {
                let mine = row.provider == self.shared.provider.as_str()
                    && row.channel_id == self.channel.get()
                    && row.user_msg_id == self.user_message_id
                    && row.turn_nonce.as_deref() == self.token.turn_nonce();
                if !mine || row.rebind_origin || row.restart_mode.is_some() {
                    return Settlement::Retain("foreign_or_pinned_inflight");
                }
                retry.get_or_insert("inflight_not_settled");
            }
            Err(_) => {
                retry.get_or_insert("inflight_probe_failed");
            }
        }
        // Watcher admission has its own synchronous fence and need not claim a
        // turn. Keep its absence proof through actor close/unlink, off async
        // workers so no non-Send mutex guard crosses an await.
        let runtime = tokio::runtime::Handle::current();
        let shared = self.shared.clone();
        let channel = self.channel;
        let mailbox = self.mailbox.clone();
        let observed = self.observed;
        tokio::task::spawn_blocking(move || {
            let _watchers =
                crate::services::discord::tmux_watcher_registry::lock_tmux_watcher_registry();
            if shared.tmux_watchers.channel_binding(&channel).is_some()
                || !shared
                    .tmux_watchers
                    .authoritative_session_bindings(channel)
                    .is_empty()
            {
                return Settlement::Retain("watcher_bound");
            }
            // Observe permanent ownership changes even while another guard is
            // transiently unsettled; never retire without the transition fence.
            if let Some(reason) = retry {
                return Settlement::Retry(reason);
            }
            // Actor-side claim comparison closes races with a successor that
            // started and finished since the early snapshot. CloseIfIdle also
            // rechecks token, queue, recovery, dispatch and injection reservations.
            match runtime.block_on(
                shared
                    .mailboxes
                    .remove_idle_entry_if_current(channel, &mailbox, observed),
            ) {
                MailboxPurgeOutcome::Removed => Settlement::Complete("removed"),
                MailboxPurgeOutcome::NoEntry => Settlement::Complete("registry_absent"),
                MailboxPurgeOutcome::RefusedLiveWork("live_cancel_token") => {
                    Settlement::Retry("turn_not_released")
                }
                MailboxPurgeOutcome::RefusedLiveWork(reason) => Settlement::Retain(reason),
            }
        })
        .await
        .unwrap_or(Settlement::Retain("retirement_task_failed"))
    }
}

fn process_refusal(token: &CancelToken) -> Option<&'static str> {
    let Some(process) = token.captured_child_process() else {
        return None;
    };
    #[cfg(unix)]
    {
        use crate::services::process::{ProcessGroupProbe, process_group_probe};
        match process_group_probe(process.pid) {
            ProcessGroupProbe::Gone => None,
            ProcessGroupProbe::Live => Some("process_group_live"),
            ProcessGroupProbe::ProbeError => Some("process_group_probe_failed"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = process;
        Some("process_probe_unsupported")
    }
}

#[cfg(test)]
mod tests;
