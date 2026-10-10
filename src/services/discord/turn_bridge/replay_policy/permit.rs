//! Move-only effect permits: a start exists only behind a committed begin, and a permit runs one effect.

use crate::db::replay_disposition::write::{BeginAck, ExactAttempt, NoEffectAck};

/// Where a permitted first input lands: a new provider start, or an existing busy pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EffectTarget {
    ProviderStart { provider: String, channel: String },
    BusyPane { owner: String, pane: String },
}

impl EffectTarget {
    /// The stored projection key; a permit for one target never matches another.
    pub(crate) fn key(&self) -> String {
        match self {
            Self::ProviderStart { provider, channel } => {
                serde_json::json!(["provider_start", provider, channel])
            }
            Self::BusyPane { owner, pane } => serde_json::json!(["busy_pane", owner, pane]),
        }
        .to_string()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PermitRefusal {
    TargetMismatch,
    InputMismatch,
    NotARetry,
}

/// The one right to make the first provider input of an acknowledged attempt.
#[derive(Debug)]
pub(crate) struct StartPermit {
    attempt: ExactAttempt,
    target_key: String,
    input_hash: String,
}

impl StartPermit {
    /// Seals a committed begin for the target its projection named.
    pub(crate) fn seal(ack: BeginAck, target: &EffectTarget) -> Result<Self, PermitRefusal> {
        let (attempt, target_key, input_hash) = ack.into_parts();
        if target_key != target.key() {
            return Err(PermitRefusal::TargetMismatch);
        }
        Ok(Self {
            attempt,
            target_key,
            input_hash,
        })
    }

    pub(crate) fn attempt(&self) -> &ExactAttempt {
        &self.attempt
    }

    /// Spends the permit on its own target and prepared input; a mismatch drops it without the effect.
    pub(crate) fn consume<R>(
        self,
        target: &EffectTarget,
        input_hash: &str,
        effect: impl FnOnce(ExactAttempt) -> R,
    ) -> Result<R, PermitRefusal> {
        if self.target_key != target.key() {
            return Err(PermitRefusal::TargetMismatch);
        }
        if self.input_hash != input_hash {
            return Err(PermitRefusal::InputMismatch);
        }
        Ok(effect(self.attempt))
    }
}

/// The right to prepare one exact retry of a committed no-effect nonce.
#[derive(Debug)]
pub(crate) struct RetryPermit {
    receipt_id: i64,
    previous_nonce: String,
    next_nonce: String,
}

impl RetryPermit {
    pub(crate) fn from_no_effect(ack: NoEffectAck) -> Self {
        let (receipt_id, previous_nonce) = ack.into_parts();
        Self {
            receipt_id,
            previous_nonce,
            next_nonce: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub(crate) fn receipt_id(&self) -> i64 {
        self.receipt_id
    }

    pub(crate) fn previous_nonce(&self) -> &str {
        &self.previous_nonce
    }

    pub(crate) fn next_nonce(&self) -> &str {
        &self.next_nonce
    }
}

/// Fresh launch values bound to the permit of an acknowledged exact retry.
#[derive(Debug)]
pub(crate) struct PreparedRetryStart<P> {
    prepared: P,
    permit: StartPermit,
}

impl<P> PreparedRetryStart<P> {
    pub(crate) fn new(prepared: P, permit: StartPermit) -> Result<Self, PermitRefusal> {
        if permit.attempt.retry_of().is_none() {
            return Err(PermitRefusal::NotARetry);
        }
        Ok(Self { prepared, permit })
    }

    /// Runs the approved local reset, then moves the same values and permit into the provider call.
    pub(crate) fn launch<R>(
        self,
        reset: impl FnOnce(&P),
        start: impl FnOnce(P, StartPermit) -> R,
    ) -> R {
        reset(&self.prepared);
        start(self.prepared, self.permit)
    }
}
