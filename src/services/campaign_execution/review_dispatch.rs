use std::io;
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use sqlx::PgPool;

use crate::db::server_campaigns::{self as store, StoreError};

use super::host_gate::{GateRefusal, Initiated, Initiation, JournalState};
use super::host_worker::{HostWorker, WorkerError};
use super::review_wait::{self, send};
use super::runtime_profile::RuntimeProfile;
use super::verification_spec::adk_verification_spec;
use crate::domain::campaign::execution_pin::{ExecutionPin, PinError, RequirementsSnapshot};
use crate::domain::campaign::model::{ActionState, Command, Role};
use crate::domain::campaign::profile::Observation;
use crate::domain::campaign::review_gate::{
    ActorIdentity, ReviewFact, ReviewRequest, ReviewStatus, ScopeTest,
};
use crate::domain::campaign::verification_run::digest_of;

pub enum WebGptAdmission<S> {
    Ready(S),
    Full,
    Unavailable,
}

pub enum WebGptReply {
    Pending,
    Unavailable,
    Refused,
    Output { receipt: String, body: String },
}

// Reservations acquire capacity only. Send and collect use the exact native review conversation.
pub trait WebGptPro: Send + Sync {
    type Slot: Send;

    fn reserve(&self) -> WebGptAdmission<Self::Slot>;
    fn send_pro(
        &self,
        slot: Self::Slot,
        request: &ReviewRequest,
        nonce: &str,
    ) -> io::Result<String>;
    fn collect_pro(&self, request: &ReviewRequest, receipt: &str) -> WebGptReply;
}

pub struct ReviewStart<'a> {
    pub action: &'a str,
    pub pin: &'a ExecutionPin,
    pub requirements: &'a RequirementsSnapshot,
    pub implementer: ActorIdentity,
    pub reviewer: ActorIdentity,
    // Unknown or changed contracts use a full review. An unchanged contract names the exact prior head.
    pub delta_base: Option<String>,
    pub required_scopes: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReviewProgress {
    WaitingForSlot,
    AwaitingReply,
    Failed,
    Recorded,
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Worker(#[from] WorkerError),
    #[error(transparent)]
    Pin(#[from] PinError),
    #[error("review request or receipt did not match the current binding")]
    Binding,
    #[error("{0:?}")]
    Rejected(crate::domain::campaign::model::Rejection),
}

pub struct ReviewDispatcher<'a, T> {
    pub campaign: &'a str,
    pub transport: &'a T,
    pub runtime: RuntimeProfile<'a>,
}

impl<T: WebGptPro> ReviewDispatcher<'_, T> {
    pub async fn prepare(
        &self,
        pool: &PgPool,
        worker: &HostWorker,
        start: ReviewStart<'_>,
    ) -> Result<(), ReviewError> {
        let mut tx = pool.begin().await.map_err(StoreError::from)?;
        let state = store::get_for_worker(&mut tx, self.campaign)
            .await?
            .ok_or(ReviewError::Binding)?;
        let action = state
            .run
            .as_ref()
            .and_then(|r| r.actions.get(start.action))
            .ok_or(ReviewError::Binding)?;
        if !worker.serves(&action.binding)
            || action.target != *worker.identity()
            || start.requirements.observation.issue_number != self.runtime.scope.issue_number
        {
            return Err(ReviewError::Binding);
        }
        let spec = adk_verification_spec();
        let profile_hash = start.pin.resolved_profile_hash.clone();
        let spec_hash = spec.hash().map_err(|_| ReviewError::Binding)?;
        start.pin.revalidate(
            &action.binding.hashes,
            start.requirements,
            &profile_hash,
            &spec_hash,
        )?;
        let request = review_wait::request(
            action,
            start.pin.hash()?,
            profile_hash,
            start.implementer,
            start.reviewer,
            start.delta_base,
            start.required_scopes,
        );
        send(
            tx,
            self.campaign,
            &state,
            Command::RecordReview {
                action_id: start.action.into(),
                fact: Box::new(ReviewFact::Prepare(Box::new(request))),
            },
        )
        .await
    }

    pub async fn step(
        &self,
        pool: &PgPool,
        worker: &mut HostWorker,
        action_id: &str,
        headroom: Duration,
    ) -> Result<ReviewProgress, ReviewError> {
        let state = store::get(pool, self.campaign)
            .await?
            .ok_or(ReviewError::Binding)?;
        let run = state.run.as_ref().ok_or(ReviewError::Binding)?;
        let action = run.actions.get(action_id).ok_or(ReviewError::Binding)?;
        let job = run.reviews.get(action_id).ok_or(ReviewError::Binding)?;
        if !worker.serves(&action.binding) || action.target != *worker.identity() {
            return Err(ReviewError::Binding);
        }
        match &job.status {
            ReviewStatus::Failed { .. } => return Ok(ReviewProgress::Failed),
            ReviewStatus::Received { .. } => return Ok(ReviewProgress::Recorded),
            ReviewStatus::Sent { receipt, deadline } => {
                let reply = if Utc::now() >= *deadline {
                    None
                } else {
                    Some(self.transport.collect_pro(&job.request, receipt))
                };
                let Some(fact) =
                    review_wait::answer(&job.request, reply, receipt, *deadline, Utc::now())
                else {
                    return Ok(ReviewProgress::AwaitingReply);
                };
                let failed = matches!(fact, ReviewFact::Failed { .. });
                review_wait::record(pool, self.campaign, action_id, fact).await?;
                return Ok(if failed {
                    ReviewProgress::Failed
                } else {
                    ReviewProgress::Recorded
                });
            }
            ReviewStatus::Planned | ReviewStatus::WaitingForSlot => {}
        }
        let digest = digest_of(&job.request).map_err(StoreError::from)?;
        if let Some(entry) = worker.effect_entry(action_id) {
            if entry.identity.payload_digest != digest
                || entry.identity.run_id != action.binding.run_id
                || entry.identity.origin_attempt_id != action.binding.attempt_id
            {
                return Err(ReviewError::Binding);
            }
            let fact = match &entry.state {
                JournalState::Started { receipt, .. } => ReviewFact::Sent {
                    receipt: receipt.clone(),
                    deadline: entry.updated_at + TimeDelta::minutes(45),
                },
                JournalState::NotSent { .. } => ReviewFact::Failed {
                    reason: "send was refused".into(),
                    uncertain: false,
                },
                JournalState::Initiating | JournalState::Indeterminate { .. } => {
                    ReviewFact::Failed {
                        reason: "send could not be confirmed".into(),
                        uncertain: true,
                    }
                }
                _ => return Err(ReviewError::Binding),
            };
            let sent = matches!(fact, ReviewFact::Sent { .. });
            review_wait::record(pool, self.campaign, action_id, fact).await?;
            return Ok(if sent {
                ReviewProgress::AwaitingReply
            } else {
                ReviewProgress::Failed
            });
        }
        super::control::validate_new_start(&state, &action.binding)
            .map_err(ReviewError::Rejected)?;
        let admission = self.transport.reserve();
        let slot = match admission {
            WebGptAdmission::Ready(slot) => slot,
            WebGptAdmission::Full => {
                review_wait::record(pool, self.campaign, action_id, ReviewFact::WaitingForSlot)
                    .await?;
                return Ok(ReviewProgress::WaitingForSlot);
            }
            WebGptAdmission::Unavailable => {
                review_wait::record(
                    pool,
                    self.campaign,
                    action_id,
                    ReviewFact::Failed {
                        reason: "WebGPT unavailable before send".into(),
                        uncertain: false,
                    },
                )
                .await?;
                return Ok(ReviewProgress::Failed);
            }
        };
        let transitions: &[ActionState] = match action.state {
            ActionState::Planned => &[ActionState::Claimed, ActionState::StartPending],
            ActionState::Claimed => &[ActionState::StartPending],
            ActionState::StartPending => &[],
            _ => return Err(ReviewError::Binding),
        };
        for &to in transitions {
            let tx = pool.begin().await.map_err(StoreError::from)?;
            send(
                tx,
                self.campaign,
                &state,
                Command::AdvanceAction {
                    action_id: action_id.into(),
                    binding: Box::new(action.binding.clone()),
                    state: to,
                },
            )
            .await?;
        }
        let now = Utc::now();
        let host = worker.identity().clone();
        let admitted = worker
            .start_action(pool, action_id, digest, headroom, now, |nonce| {
                let observed = self.runtime.observe(&host, &adk_verification_spec());
                let current = observed.is_ok_and(|runtime| {
                    runtime
                        .require_session(Role::Review, &job.request.reviewer.session)
                        .is_ok()
                        && runtime
                            .require_session(Role::Implementation, &job.request.implementer.session)
                            .is_ok()
                        && runtime
                            .profile()
                            .hash()
                            .is_ok_and(|hash| hash == job.request.profile_hash)
                });
                if !current {
                    return Ok(Initiated::NotSent {
                        evidence: "pinned WebGPT pro session was not observed".into(),
                    });
                }
                let receipt = self.transport.send_pro(slot, &job.request, nonce)?;
                if receipt.trim().is_empty() {
                    return Err(io::Error::other("empty WebGPT receipt"));
                }
                Ok(Initiated::Started {
                    receipt,
                    process: None,
                    handle: (),
                })
            })
            .await?;
        let (fact, progress) = match admitted.initiation {
            Initiation::Started { receipt, .. } => (
                ReviewFact::Sent {
                    receipt,
                    deadline: now + TimeDelta::minutes(45),
                },
                ReviewProgress::AwaitingReply,
            ),
            Initiation::Indeterminate { .. } => (
                ReviewFact::Failed {
                    reason: "send could not be confirmed".into(),
                    uncertain: true,
                },
                ReviewProgress::Failed,
            ),
            Initiation::NotSent {
                reason: GateRefusal::Initiator,
                evidence,
            } => (
                ReviewFact::Failed {
                    reason: evidence,
                    uncertain: false,
                },
                ReviewProgress::Failed,
            ),
            Initiation::NotSent { .. } => return Err(ReviewError::Binding),
        };
        review_wait::record(pool, self.campaign, action_id, fact).await?;
        Ok(progress)
    }

    // Range tests come from the server's separate test runner observation.
    pub async fn scope_test(
        &self,
        pool: &PgPool,
        action_id: &str,
        test: Observation<ScopeTest>,
    ) -> Result<(), ReviewError> {
        let Observation::Observed { evidence, .. } = test else {
            return Err(ReviewError::Binding);
        };
        review_wait::record(pool, self.campaign, action_id, ReviewFact::Scope(evidence)).await
    }

    pub async fn finalize(&self, pool: &PgPool, action_id: &str) -> Result<(), ReviewError> {
        review_wait::record(pool, self.campaign, action_id, ReviewFact::Finalize).await
    }
}
