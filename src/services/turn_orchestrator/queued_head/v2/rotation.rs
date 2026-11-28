use super::super::v2_types::rotation::{Receipt, RotationIntent, StoredHandoff};
use super::{Owner, actor};
use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, QueuePersistenceContext,
    queued_head::compat::{self, SnapshotRead},
};
use chrono::{DateTime, Utc};
use poise::serenity_prelude::ChannelId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Handoff {
    pub(crate) op_id: Uuid,
    pub(crate) native_session_id: Uuid,
    pub(crate) ticket: Uuid,
    pub(crate) body: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HandoffError {
    Conflict,
    Gone,
    Unavailable,
}

pub(crate) struct Target {
    pub(crate) session_row_id: i64,
    pub(crate) channel: ChannelId,
    pub(crate) persistence: QueuePersistenceContext,
}

#[derive(Serialize)]
pub(crate) struct Status {
    pub(crate) op_id: Uuid,
    pub(crate) native_session_id: Uuid,
    pub(crate) requested_at: DateTime<Utc>,
    pub(crate) expires_at: DateTime<Utc>,
    pub(crate) state: &'static str,
    pub(crate) receipt: Option<Receipt>,
    pub(crate) body: Option<String>,
}

impl Target {
    pub(crate) async fn status(&self) -> Result<Option<Status>, HandoffError> {
        let (path, namespace) = self.queue()?;
        let row_id = self.session_row_id;
        if let Some(mailbox) = ChannelMailboxRegistry::global_handle(self.channel) {
            let (owner_path, owner_namespace) = (path.clone(), namespace.clone());
            let observed = mailbox
                .v2_request(move |slot| {
                    Ok(slot.as_ref().map(|owner| {
                        if !owner.owns_rotation_path(&owner_path, &owner_namespace) {
                            return Err(HandoffError::Conflict);
                        }
                        Ok(owner.snapshot.rotation.clone())
                    }))
                })
                .await
                .map_err(|_| HandoffError::Unavailable)?;
            if let Some(rotation) = observed {
                return Ok(rotation?
                    .filter(|intent| intent.session_row_id == row_id)
                    .map(|intent| intent.into_status(Utc::now())));
            }
        }
        Ok(Self::read_rotation(path, &namespace)
            .await?
            .filter(|intent| intent.session_row_id == row_id)
            .map(|intent| intent.into_status(Utc::now())))
    }

    fn queue(&self) -> Result<(std::path::PathBuf, String), HandoffError> {
        actor::v2_queue(&self.persistence, self.channel).map_err(|_| HandoffError::Unavailable)
    }

    async fn read_rotation(
        path: std::path::PathBuf,
        namespace: &str,
    ) -> Result<Option<RotationIntent>, HandoffError> {
        let raw = match tokio::fs::read_to_string(path).await {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(HandoffError::Unavailable),
        };
        let SnapshotRead::ReadOnlyV2(snapshot) =
            compat::inspect(&raw).map_err(|_| HandoffError::Unavailable)?
        else {
            return Ok(None);
        };
        if snapshot.rotation.is_some() && snapshot.namespace.as_deref() != Some(namespace) {
            return Err(HandoffError::Conflict);
        }
        Ok(snapshot.rotation)
    }

    pub(crate) async fn stage(&self, handoff: Handoff) -> Result<Receipt, HandoffError> {
        let (path, namespace) = self.queue()?;
        let row_id = self.session_row_id;
        let handoff = if let Some(mailbox) = ChannelMailboxRegistry::global_handle(self.channel) {
            let (owner_path, owner_namespace) = (path.clone(), namespace.clone());
            let outcome = mailbox
                .v2_request(move |slot| {
                    let Some(owner) = slot.as_mut() else {
                        return Ok(Err(handoff));
                    };
                    let staged = if owner.owns_rotation_path(&owner_path, &owner_namespace) {
                        owner.stage_rotation(row_id, handoff, Utc::now())
                    } else {
                        Err(HandoffError::Conflict)
                    };
                    Ok(Ok(staged))
                })
                .await
                .map_err(|_| HandoffError::Unavailable)?;
            match outcome {
                Ok(staged) => return staged,
                Err(handoff) => handoff,
            }
        } else {
            handoff
        };
        // A restarted node can replay a persisted receipt before its mailbox adopts the queue.
        let intent = Self::read_rotation(path, &namespace)
            .await?
            .ok_or(HandoffError::Conflict)?;
        intent
            .check_handoff(
                row_id,
                &handoff,
                &sha256(handoff.body.as_bytes()),
                Utc::now(),
            )?
            .ok_or(HandoffError::Unavailable)
    }
}

impl Owner {
    pub(crate) fn stage_rotation(
        &mut self,
        session_row_id: i64,
        handoff: Handoff,
        now: DateTime<Utc>,
    ) -> Result<Receipt, HandoffError> {
        let digest = sha256(handoff.body.as_bytes());
        let intent = self
            .snapshot
            .rotation
            .as_ref()
            .ok_or(HandoffError::Conflict)?;
        if let Some(receipt) = intent.check_handoff(session_row_id, &handoff, &digest, now)? {
            return Ok(receipt);
        }
        let mut next = self.snapshot.clone();
        let intent = next.rotation.as_mut().ok_or(HandoffError::Conflict)?;
        let receipt = Receipt {
            op_id: intent.op_id,
            digest,
            stored_at: now,
        };
        intent.submission = Some(StoredHandoff {
            receipt: receipt.clone(),
            body: handoff.body,
        });
        self.commit(next).map_err(|_| HandoffError::Unavailable)?;
        Ok(receipt)
    }

    fn owns_rotation_path(&self, path: &std::path::Path, namespace: &str) -> bool {
        self.path == path && self.snapshot.namespace.as_deref() == Some(namespace)
    }

    #[cfg(test)]
    pub(crate) async fn reserve_rotation<'a>(
        &mut self,
        soft_bytes: Option<u64>,
        evidence: &crate::services::transcript_rotation::eligibility::Eligibility<'_>,
        request: RotationIntent,
        pending_schedule: impl FnOnce() -> bool,
        latch: impl FnOnce() -> Option<(&'a sqlx::PgPool, &'a str, String)>,
    ) -> Result<bool, String> {
        use crate::{db::transcript_rotation_requests, services::transcript_rotation::eligibility};
        let Some(soft_bytes) = soft_bytes else {
            return Ok(false);
        };
        if self.snapshot.rotation.is_some()
            || self.snapshot.holds_input()
            || !eligibility::eligible(soft_bytes, evidence)
            || pending_schedule()
        {
            return Ok(false);
        }
        let Some((pool, provider, channel_id)) = latch() else {
            return Ok(false);
        };
        let inserted = transcript_rotation_requests::insert(
            pool,
            provider,
            request.native_session_id,
            &transcript_rotation_requests::Request {
                op_id: request.op_id,
                channel_id,
                requested_at: request.requested_at,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        if !inserted {
            return Ok(false);
        }
        let mut next = self.snapshot.clone();
        next.rotation = Some(request);
        self.commit(next)?;
        Ok(true)
    }
}

impl RotationIntent {
    fn check_handoff(
        &self,
        session_row_id: i64,
        handoff: &Handoff,
        digest: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Receipt>, HandoffError> {
        if self.session_row_id != session_row_id
            || self.op_id != handoff.op_id
            || self.native_session_id != handoff.native_session_id
            || !bool::from(
                self.ticket
                    .digest
                    .as_bytes()
                    .ct_eq(sha256(handoff.ticket.as_bytes()).as_bytes()),
            )
        {
            return Err(HandoffError::Conflict);
        }
        if let Some(stored) = &self.submission {
            return if stored.receipt.digest == digest {
                Ok(Some(stored.receipt.clone()))
            } else {
                Err(HandoffError::Conflict)
            };
        }
        if self.closed(now) {
            return Err(HandoffError::Gone);
        }
        Ok(None)
    }

    fn closed(&self, now: DateTime<Utc>) -> bool {
        self.ticket.revoked || now >= self.ticket.expires_at
    }

    fn into_status(self, now: DateTime<Utc>) -> Status {
        let state = if self.closed(now) {
            "closed"
        } else if self.submission.is_some() {
            "staged"
        } else {
            "requested"
        };
        let (receipt, body) = self.submission.map_or((None, None), |stored| {
            (Some(stored.receipt), Some(stored.body))
        });
        Status {
            op_id: self.op_id,
            native_session_id: self.native_session_id,
            requested_at: self.requested_at,
            expires_at: self.ticket.expires_at,
            state,
            receipt,
            body,
        }
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
