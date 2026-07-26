use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CleanupIdentityKind {
    DiscordChannel,
    ScheduledSnapshot,
}

impl CleanupIdentityKind {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::DiscordChannel => "discord_channel",
            Self::ScheduledSnapshot => "scheduled_snapshot",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CanonicalCleanupIdentity<'a> {
    pub(crate) kind: CleanupIdentityKind,
    pub(crate) provider: &'a str,
    pub(crate) discord_token_hash: &'a str,
    pub(crate) channel_id: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CleanupTarget {
    pub(crate) target_id: Uuid,
    pub(crate) operation_high_watermark: i64,
    pub(crate) retired: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LocatorClaim {
    pub(crate) target_id: Uuid,
    pub(crate) generation: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationState {
    Open,
    Committed,
    Completed,
    Aborted,
}

impl OperationState {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Committed => "committed",
            Self::Completed => "completed",
            Self::Aborted => "aborted",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CleanupOperation {
    pub(crate) operation_id: Uuid,
    pub(crate) target_id: Uuid,
    pub(crate) operation_epoch: i64,
    pub(crate) state: OperationState,
    pub(crate) claim_owner: Option<String>,
    pub(crate) attempt_epoch: i64,
    pub(crate) claim_expires_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CreatedOperation {
    pub(crate) operation_id: Uuid,
    pub(crate) operation_epoch: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClaimResult {
    Claimed {
        attempt_epoch: i64,
        expires_at: DateTime<Utc>,
    },
    Renewed {
        attempt_epoch: i64,
        expires_at: DateTime<Utc>,
    },
    Held {
        owner: String,
        attempt_epoch: i64,
        expires_at: DateTime<Utc>,
    },
    Stale,
    NotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReceiptState {
    Applied,
    NotApplied,
    Unknown,
}

impl ReceiptState {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::NotApplied => "not_applied",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CapabilityUse {
    Accepted {
        request_id: Uuid,
    },
    Replay {
        request_id: Uuid,
        state: ReceiptState,
    },
    FingerprintConflict,
    BindingMismatch,
    Expired,
    LostOwnership,
    NeedsReconcile {
        request_id: Uuid,
    },
    StaleAttempt,
    NotFound,
}

#[derive(Clone, Debug)]
pub(crate) struct CapabilityBinding<'a> {
    pub(crate) capability_id: Uuid,
    pub(crate) target_id: Uuid,
    pub(crate) operation_id: Uuid,
    pub(crate) intent_id: Uuid,
    pub(crate) attempt_epoch: i64,
    pub(crate) audience: &'a str,
    pub(crate) claim_owner: &'a str,
    pub(crate) expires_at: DateTime<Utc>,
    pub(crate) idempotency_identity: Uuid,
}
