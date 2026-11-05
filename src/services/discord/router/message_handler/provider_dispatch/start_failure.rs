use super::owner_input::OwnerTurn;
use super::*;
use crate::services::discord::inflight::row_removed;
use crate::services::discord::queue_io::input_owner::drive::{Front, FrontClaim};
use crate::services::turn_orchestrator::{ChannelMailboxHandle, PendingQueueItem};

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum Kind {
    Opaque,
    ProviderUnsupported,
    EndpointNotLocal,
    ExecutorNotWired,
    HostedHerdrUnconfigured,
}
pub(in crate::services::discord) fn confirmed(refusal: &HerdrRefusal) -> Option<Kind> {
    use crate::db::dispatched_sessions::hosted_execution::HostedState;
    match refusal {
        HerdrRefusal::ProviderUnsupported { .. } => Some(Kind::ProviderUnsupported),
        HerdrRefusal::EndpointNotLocal { .. } => Some(Kind::EndpointNotLocal),
        HerdrRefusal::ExecutorNotWired => Some(Kind::ExecutorNotWired),
        HerdrRefusal::HostedHerdrUnconfigured {
            state: Some(HostedState::Pending | HostedState::Bound),
        } => Some(Kind::HostedHerdrUnconfigured),
        _ => None,
    }
}
type Shared = Arc<crate::services::discord::SharedData>;

pub(super) struct Failure {
    mailbox: ChannelMailboxHandle,
    front: Front,
    number: u64,
    members: Vec<PendingQueueItem>,
}
impl Failure {
    pub(super) fn claimed(mailbox: &ChannelMailboxHandle, claim: &FrontClaim) -> Self {
        Self {
            mailbox: mailbox.clone(),
            front: claim.front().clone(),
            number: claim.number(),
            members: std::iter::once(claim.front().1.clone())
                .chain(claim.followers().iter().cloned())
                .collect(),
        }
    }
    pub(super) fn owner(owner: &OwnerTurn) -> Self {
        Self::claimed(owner.mailbox(), owner.claim())
    }
    pub(super) fn guard_reply(
        &self,
        reply: super::owner_input::Reply,
    ) -> super::owner_input::Reply {
        let (mailbox, front, number) = (self.mailbox.clone(), self.front.clone(), self.number);
        Arc::new(move |result| {
            let front = front.clone();
            // Only an unreserved exact claim waits for the failure commit; EffectOwned settles now.
            let before_input = result.is_err()
                && tokio::runtime::Handle::current()
                    .block_on(mailbox.v2_request(move |slot| {
                        Ok(slot.as_ref().is_some_and(|owner| {
                            owner.claim_number() == number
                                && owner.claimed(front.1.message_id, &front)
                        }))
                    }))
                    .unwrap_or(true);
            if !before_input {
                reply(result);
            }
        })
    }
    pub(super) async fn commit(
        self,
        kind: Kind,
        detail: String,
        watch: Option<row_removed::Watch>,
        shared: Option<Shared>,
        channel: poise::serenity_prelude::ChannelId,
        token: Option<Arc<CancelToken>>,
    ) -> Result<Option<bool>, String> {
        if token.is_some() && watch.is_none() {
            return Err("wrapper failure lacks its fresh row identity".into());
        }
        if let Some(watch) = &watch {
            let token = token.as_ref().ok_or("wrapper failure lacks its token")?;
            let pin = watch.key();
            if pin["anchor"] != self.front.1.message_id
                || pin["channel"] != channel.get()
                || shared
                    .as_ref()
                    .is_some_and(|shared| pin["provider"] != shared.provider.as_str())
                || pin["nonce"].as_str() != token.turn_nonce()
            {
                return Err("failure row does not name the producer episode".into());
            }
        }
        let pin = watch.as_ref().map(row_removed::Watch::key);
        let mailbox = self.mailbox.clone();
        let decision = mailbox
            .v2_request(move |slot| {
                let kind =
                    serde_json::from_value(serde_json::to_value(kind).map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?;
                slot.as_mut().ok_or("missing owner")?.start_failed(
                    &self.front,
                    self.number,
                    &self.members,
                    pin,
                    kind,
                    &detail,
                )
            })
            .await?;
        if decision.is_some() {
            if let Some(shared) = shared {
                tokio::spawn(async move {
                    let wake = if let Some(watch) = watch {
                        let pin = watch.key();
                        #[cfg(test)]
                        let nonce = token
                            .as_ref()
                            .and_then(|token| token.turn_nonce())
                            .unwrap_or_default()
                            .to_string();
                        let row = async {
                            watch.removed().await;
                            #[cfg(test)]
                            fact(channel, &nonce, "row_removed");
                        };
                        let released = async {
                            let result = token_released(
                                &shared,
                                channel,
                                &mailbox,
                                token.as_ref().expect("captured wrapper token"),
                            )
                            .await;
                            #[cfg(test)]
                            if result.is_ok() {
                                fact(channel, &nonce, "token_released");
                            }
                            result
                        };
                        let ((), released) = tokio::join!(row, released);
                        if let Err(error) = released {
                            tracing::warn!(channel_id=channel.get(),%error,"failed episode remains closing");
                            return;
                        }
                        mailbox
                            .v2_request(move |slot| {
                                Ok(slot
                                    .as_mut()
                                    .is_some_and(|owner| owner.start_cleanup_completed(&pin)))
                            })
                            .await
                            .unwrap_or(false)
                    } else {
                        true
                    };
                    if wake {
                        super::owner_input::wake(&shared, channel);
                    }
                });
            }
        }
        #[cfg(test)]
        if decision.is_some() {
            let gate = {
                let mut slot = COMMIT_GATE.lock().unwrap();
                if slot.as_ref().is_some_and(|(id, _, _)| *id == channel.get()) {
                    slot.take()
                } else {
                    None
                }
            };
            if let Some((_, entered, release)) = gate {
                let _ = entered.send(());
                release.await.expect("release committed start failure");
            }
        }
        Ok(decision)
    }
    pub(super) fn finish(
        self,
        turn: &StreamingTurn<'_>,
        watch: Result<Option<row_removed::Watch>, String>,
        result: Result<(), String>,
    ) -> Result<(), String> {
        let Err(error) = result else { return Ok(()) };
        let bound = crate::services::claude::herdr_turn::take_owner_bound_failure();
        if turn
            .cancel
            .cancelled
            .load(std::sync::atomic::Ordering::Acquire)
            || turn.cancel.native_stop.receipt_sink().stopped_before_key()
            || (!bound && error.starts_with("NotSent("))
        {
            return Err(error);
        }
        let watch = match watch {
            Ok(watch) => watch,
            Err(_) => return Err("NotSent(\"start_failure_row_unreadable\")".into()),
        };
        let kind = match turn.host {
            TurnHost::Refused(refusal) => match confirmed(refusal) {
                Some(kind) => kind,
                None => return Err(error),
            },
            _ => Kind::Opaque,
        };
        let shared = turn.shared.cloned();
        match tokio::runtime::Handle::current().block_on(self.commit(
            kind,
            error.clone(),
            watch,
            shared,
            poise::serenity_prelude::ChannelId::new(turn.channel_id),
            Some(turn.cancel.clone()),
        )) {
            Ok(None) => Err(error),
            Ok(Some(_)) | Err(_) => Err("NotSent(\"owner_start_failed\")".into()),
        }
    }
}
pub(super) fn run(
    turn: StreamingTurn<'_>,
    start: ReplayStart,
    sender: Sender<StreamMessage>,
    owner: Option<OwnerTurn>,
) -> Result<(), String> {
    let failure = owner.as_ref().map(Failure::owner);
    if failure.is_none() {
        return super::execute_inner(turn, start, sender, owner);
    }
    crate::services::claude::herdr_turn::take_owner_bound_failure();
    let watch = row_removed::watch(turn.provider, turn.channel_id);
    let context = turn.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        super::execute_inner(turn, start, sender, owner)
    }));
    let result = match result {
        Ok(result) => result,
        Err(info) => match producer_panic("owner streaming", info) {
            StreamMessage::Error { message, .. } => Err(message),
            _ => unreachable!(),
        },
    };
    #[cfg(test)]
    {
        let gate = {
            let mut slot = BRIDGE_GATE.lock().unwrap();
            if slot
                .as_ref()
                .is_some_and(|(id, _, _)| *id == context.channel_id)
            {
                slot.take()
            } else {
                None
            }
        };
        if let Some((_, entered, release)) = gate {
            let _ = entered.send(());
            tokio::runtime::Handle::current()
                .block_on(release)
                .expect("release producer bridge frame");
        }
    }
    failure
        .expect("owner failure context")
        .finish(&context, watch, result)
}

#[cfg(test)]
type CommitGate = (
    u64,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
#[cfg(test)]
pub(in crate::services::discord) static COMMIT_GATE: std::sync::Mutex<Option<CommitGate>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(in crate::services::discord) static BRIDGE_GATE: std::sync::Mutex<Option<CommitGate>> =
    std::sync::Mutex::new(None);

async fn token_released(
    shared: &Shared,
    channel: poise::serenity_prelude::ChannelId,
    mailbox: &ChannelMailboxHandle,
    expected: &Arc<CancelToken>,
) -> Result<(), String> {
    let finished = shared.mailboxes.turn_finished(channel);
    let wait = finished.wait();
    tokio::pin!(wait);
    let ready = futures::poll!(&mut wait).is_ready();
    let active = mailbox.cancel_token().await.map_err(|e| format!("{e:?}"))?;
    if active
        .as_ref()
        .is_some_and(|active| Arc::ptr_eq(active, expected))
    {
        if !ready {
            wait.await;
        }
        let active = mailbox.cancel_token().await.map_err(|e| format!("{e:?}"))?;
        if active
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, expected))
        {
            return Err("turn-finished signal did not release the failed token".into());
        }
    }
    Ok(())
}
#[cfg(test)]
type FactSender = (
    u64,
    tokio::sync::mpsc::UnboundedSender<(String, &'static str)>,
);
#[cfg(test)]
pub(in crate::services::discord) static FACTS: std::sync::Mutex<Option<FactSender>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
fn fact(channel: poise::serenity_prelude::ChannelId, nonce: &str, event: &'static str) {
    if let Some((_, sender)) = FACTS
        .lock()
        .unwrap()
        .as_ref()
        .filter(|(id, _)| *id == channel.get())
    {
        let _ = sender.send((nonce.to_string(), event));
    }
}
#[cfg(all(test, unix))]
#[path = "start_failure_state_tests.rs"]
mod tests;
