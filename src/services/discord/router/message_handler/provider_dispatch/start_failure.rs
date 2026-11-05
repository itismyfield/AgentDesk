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
    pub(super) async fn commit(
        self,
        kind: Kind,
        detail: String,
        watch: Option<row_removed::Watch>,
        shared: Option<Shared>,
    ) -> Result<Option<bool>, String> {
        let channel = shared_channel(&self.members);
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
                        watch.removed().await;
                        mailbox
                            .v2_request(move |slot| {
                                Ok(slot
                                    .as_mut()
                                    .is_some_and(|owner| owner.start_row_removed(&pin)))
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
        )) {
            Ok(None) => Err(error),
            Ok(Some(_)) | Err(_) => Err("NotSent(\"owner_start_failed\")".into()),
        }
    }
}
fn shared_channel(members: &[PendingQueueItem]) -> poise::serenity_prelude::ChannelId {
    poise::serenity_prelude::ChannelId::new(members[0].channel_id.expect("owner input channel"))
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
    failure
        .expect("owner failure context")
        .finish(&context, watch, result)
}
