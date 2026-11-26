//! The first delivery attempt of one approved Decision event: its PG row is reserved once, the
//! target is resolved now, and a wait, a confirmed refusal and an unknown write stay distinct.

use std::future::Future;

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    DeliveryPorts, HealthRegistry, HumanInputError, HumanInputRequest, ProviderKind, StartAttempt,
};
use crate::db::decisions::{self, AttemptState, DecisionProvenance};
use crate::services::discord::queue_io::input_owner::origin::InputOrigin;

/// How the first attempt of an event ended.
#[derive(Debug, PartialEq, Eq)]
pub enum FirstAttempt {
    /// PG holds no approved event under this id, or the text is not the one it approved.
    NotApproved,
    /// Another caller holds this event's workflow; nothing was sent here.
    AlreadyReserved,
    /// Real activity holds the channel: the one original waits for the turn boundary.
    NormalWait { turn_id: String },
    /// A start took the input as its first prompt and answered before the provider read it:
    /// the row stays `may_have_sent` until that turn's receipt settles it. No deliver follows.
    Started { turn_id: String },
    /// Confirmed not sent and closed with this reason; the input is not kept for a retry.
    Failed(String),
    /// A write may have happened: the row stays `may_have_sent` and nothing is sent again.
    Unknown(String),
}

/// What one normalization of an absent or refusing session reported.
pub enum Normalized {
    /// The session start carried this input as its first prompt, under this turn. Like any
    /// start it is no receipt: the event settles from the evidence that turn leaves.
    FirstPrompt(String),
    /// A session is up and the input still has to be delivered.
    Started,
    NotSentRefused(String),
    Unknown(String),
}

/// Brings up the current session of the event's agent and channel, at most once per event.
#[async_trait]
pub trait Normalizer: Send + Sync {
    async fn normalize_once(&self) -> Normalized;
}

/// The input as the delivery path sees it: the approved actor as author and the event as origin.
fn request_of(event: &DecisionProvenance, provider: ProviderKind, text: &str) -> HumanInputRequest {
    HumanInputRequest {
        channel_id: poise::serenity_prelude::ChannelId::new(event.channel_id()),
        provider,
        text: text.to_string(),
        author_id: event.actor_id(),
        source: decisions::SOURCE.to_string(),
        metadata: Some(metadata_of(event)),
        channel_name_hint: None,
    }
}

fn metadata_of(event: &DecisionProvenance) -> serde_json::Value {
    serde_json::json!({"human_input": {
        "source": decisions::SOURCE,
        "origin_id": event.origin(),
        "author_discord_user_id": event.actor_id().to_string(),
    }})
}

/// One try on resolved ports: either the attempt is over, or nothing was sent for this reason.
enum Tried {
    Done(FirstAttempt),
    NotSent(String),
}

struct Workflow<'a> {
    pool: &'a PgPool,
    event: Uuid,
    approved: &'a DecisionProvenance,
}

impl Workflow<'_> {
    /// `Ok(false)` means another actor moved the row; the caller then stops writing.
    async fn step(
        &self,
        states: (AttemptState, AttemptState),
        turn_id: Option<&str>,
        detail: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        decisions::advance(self.pool, self.event, states, turn_id, detail).await
    }

    async fn fail(&self, from: AttemptState, reason: String) -> Result<FirstAttempt, sqlx::Error> {
        self.step((from, AttemptState::Failed), None, Some(&reason))
            .await?;
        Ok(FirstAttempt::Failed(reason))
    }

    /// Ends an event whose original the owner may have admitted: the closing fence first, then
    /// exactly that original leaves the owner's queue, so no later wake or restart runs it.
    async fn close_admitted(&self, reason: String) -> FirstAttempt {
        let event = (
            self.event,
            self.approved.origin(),
            self.approved.channel_id(),
        );
        let closed = crate::services::discord::decision_drain::close_admitted;
        if let Err(error) = closed(self.pool, event, &reason).await {
            tracing::warn!(event_id = %self.event, %error, "decision close left for settlement");
        }
        FirstAttempt::Failed(reason)
    }

    async fn try_once<P: DeliveryPorts>(&self, ports: &P) -> Result<Tried, sqlx::Error> {
        use AttemptState::{MayHaveSent, NormalWait, Resolving};
        if ports.owner_takes().await {
            // The owner only admits the original under the event's ticket; its pump writes later.
            return Ok(Tried::Done(match ports.try_start().await {
                StartAttempt::Accepted(turn_id) => {
                    self.step((Resolving, NormalWait), Some(&turn_id), None)
                        .await?;
                    FirstAttempt::NormalWait { turn_id }
                }
                StartAttempt::Started(turn_id) => {
                    self.step((Resolving, MayHaveSent), Some(&turn_id), None)
                        .await?;
                    FirstAttempt::Started { turn_id }
                }
                // Pending and refused admissions answer alike, and an owner start's own errors
                // are not proof that nothing was admitted: neither continues into normalization.
                StartAttempt::Busy => self.close_admitted("owner_admission_pending".into()).await,
                StartAttempt::Unavailable(error) | StartAttempt::InvalidTarget(error) => {
                    self.close_admitted(format!("owner_start_refused: {error}"))
                        .await
                }
            }));
        }
        if ports.external_hold().await.is_none() {
            // A non-owner start writes inside the call, so the journal precedes it.
            if !self.step((Resolving, MayHaveSent), None, None).await? {
                return Ok(Tried::Done(FirstAttempt::AlreadyReserved));
            }
            let not_sent = match ports.try_start().await {
                // The start spawned its worker; whether the provider took the prompt is not
                // known here, so the journal only gains the turn its receipt will name.
                StartAttempt::Started(turn_id) | StartAttempt::Accepted(turn_id) => {
                    self.step((MayHaveSent, MayHaveSent), Some(&turn_id), None)
                        .await?;
                    return Ok(Tried::Done(FirstAttempt::Started { turn_id }));
                }
                // Each reserved-start error is returned before anything is spawned.
                StartAttempt::Busy => None,
                StartAttempt::Unavailable(error) => Some(Tried::NotSent(error)),
                StartAttempt::InvalidTarget(error) => Some(Tried::Done(FirstAttempt::Failed(
                    format!("invalid_target: {error}"),
                ))),
            };
            if !self.step((MayHaveSent, Resolving), None, None).await? {
                return Ok(Tried::Done(FirstAttempt::AlreadyReserved));
            }
            match not_sent {
                Some(Tried::Done(FirstAttempt::Failed(reason))) => {
                    return Ok(Tried::Done(self.fail(Resolving, reason).await?));
                }
                Some(tried) => return Ok(tried),
                None => {}
            }
        }
        Ok(match ports.enqueue().await {
            Ok(turn_id) => {
                self.step((Resolving, NormalWait), Some(&turn_id), None)
                    .await?;
                Tried::Done(FirstAttempt::NormalWait { turn_id })
            }
            Err(refusal) => Tried::NotSent(format!("queue_refused: {refusal}")),
        })
    }
}

/// Reserves the event and makes its first attempt on the ports `resolve` finds at that moment.
/// A confirmed refusal continues through one normalization; every other failure ends here.
pub(super) async fn first_attempt<P, R, RF>(
    pool: &PgPool,
    (event_id, text): (Uuid, &str),
    resolve: R,
    normalizer: &dyn Normalizer,
) -> Result<FirstAttempt, sqlx::Error>
where
    P: DeliveryPorts,
    R: Fn(&DecisionProvenance) -> RF,
    RF: Future<Output = Result<P, HumanInputError>>,
{
    use AttemptState::{MayHaveSent, Normalizing, Resolving};
    let Some(event) = decisions::load_event(pool, event_id).await? else {
        return Ok(FirstAttempt::NotApproved);
    };
    let ticket = InputOrigin::decision_identity(
        event.channel_id(),
        Some(decisions::SOURCE),
        Some(&metadata_of(&event)),
    );
    let Some((_, ticket)) = ticket.filter(|_| event.approves(text)) else {
        return Ok(FirstAttempt::NotApproved);
    };
    // The run's start is fixed before the row is moved, so boot recovery never reads it as old.
    crate::services::discord::decision_drain::run(pool).await?;
    if !decisions::reserve_first_attempt(pool, event_id, &ticket).await? {
        return Ok(FirstAttempt::AlreadyReserved);
    }
    crate::services::discord::decision_drain::note_open();
    let workflow = Workflow {
        pool,
        event: event_id,
        approved: &event,
    };
    loop {
        let refusal = match resolve(&event).await {
            Ok(ports) => match workflow.try_once(&ports).await? {
                Tried::Done(outcome) => return Ok(outcome),
                Tried::NotSent(reason) => reason,
            },
            // No runtime owns the channel: nothing was touched.
            Err(HumanInputError::RuntimeUnavailable(reason)) => reason,
            Err(error) => return workflow.fail(Resolving, error.kind().to_string()).await,
        };
        // The update refuses a second normalization of the same event.
        if !workflow.step((Resolving, Normalizing), None, None).await? {
            return workflow.fail(Resolving, refusal).await;
        }
        match normalizer.normalize_once().await {
            Normalized::FirstPrompt(turn_id) => {
                workflow
                    .step((Normalizing, MayHaveSent), Some(&turn_id), None)
                    .await?;
                return Ok(FirstAttempt::Started { turn_id });
            }
            Normalized::Started => {
                if !workflow.step((Normalizing, Resolving), None, None).await? {
                    return Ok(FirstAttempt::AlreadyReserved);
                }
            }
            Normalized::NotSentRefused(reason) => return workflow.fail(Normalizing, reason).await,
            Normalized::Unknown(detail) => {
                workflow
                    .step((Normalizing, MayHaveSent), None, Some(&detail))
                    .await?;
                return Ok(FirstAttempt::Unknown(detail));
            }
        }
    }
}

/// Delivers an approved event once on the runtime that owns its channel now. `provider` is the
/// channel's current binding as the caller just resolved it.
pub async fn deliver_once(
    registry: &HealthRegistry,
    pool: &PgPool,
    (event_id, text): (Uuid, &str),
    provider: &ProviderKind,
    normalizer: &dyn Normalizer,
) -> Result<FirstAttempt, sqlx::Error> {
    let resolve = |event: &DecisionProvenance| {
        registry.live_ports(request_of(event, provider.clone(), text), None, None)
    };
    first_attempt(pool, (event_id, text), resolve, normalizer).await
}

#[cfg(test)]
#[path = "decision_target_tests.rs"]
mod tests;
