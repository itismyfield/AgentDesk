//! Move-only effect permits: a start exists only behind a committed begin, and a permit runs one effect.

pub(crate) use super::RetryPermit;
use crate::db::replay_disposition::write::{BeginAck, ExactAttempt};

/// Where a permitted first input lands: a new provider start, or an existing busy pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EffectTarget {
    ProviderStart {
        provider: String,
        channel: String,
    },
    /// The busy-pane injection path, which is not connected yet.
    #[cfg(test)]
    BusyPane {
        owner: String,
        pane: String,
    },
}

impl EffectTarget {
    /// The stored projection key; a permit for one target never matches another.
    pub(crate) fn key(&self) -> String {
        match self {
            Self::ProviderStart { provider, channel } => {
                serde_json::json!(["provider_start", provider, channel])
            }
            #[cfg(test)]
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
    /// The token already carries an attempt; one token runs one attempt.
    Spent,
}

/// The one right to make the first provider input of an acknowledged attempt.
#[derive(Debug)]
pub(crate) struct StartPermit {
    attempt: ExactAttempt,
    target_key: String,
    input_hash: String,
    binding: serde_json::Value,
}

impl StartPermit {
    /// Seals a committed begin for the target its projection named.
    pub(crate) fn seal(ack: BeginAck, target: &EffectTarget) -> Result<Self, PermitRefusal> {
        let binding = ack.binding().clone();
        let (attempt, target_key, input_hash) = ack.into_parts();
        if target_key != target.key() {
            return Err(PermitRefusal::TargetMismatch);
        }
        Ok(Self {
            attempt,
            target_key,
            input_hash,
            binding,
        })
    }

    pub(crate) fn hold_proof(&self) -> crate::db::replay_disposition::write::HoldProof {
        self.attempt.hold_proof()
    }

    pub(crate) fn attempt(&self) -> &ExactAttempt {
        &self.attempt
    }

    pub(crate) fn matches_start_scope(&self, driver: &str, runtime: &str) -> bool {
        driver == "legacy-prompt"
            && [("driver_scope", driver), ("runtime_scope", runtime)]
                .iter()
                .all(|(key, expected)| {
                    self.binding
                        .get(key)
                        .is_some_and(|stored| stored.as_str() == Some(*expected))
                })
    }

    /// Spends the permit on its own target and prepared input; a mismatch drops it without the effect.
    pub(crate) fn consume<R>(
        self,
        target: &EffectTarget,
        input_hash: &str,
        effect: impl FnOnce(ExactAttempt) -> R,
    ) -> Result<R, PermitRefusal> {
        self.consume_projected(target, input_hash, |attempt, _binding, _hash| {
            effect(attempt)
        })
    }

    /// Moves the acknowledged binding and frame hash with the attempt into one effect callback.
    pub(crate) fn consume_projected<R>(
        self,
        target: &EffectTarget,
        input_hash: &str,
        effect: impl FnOnce(ExactAttempt, serde_json::Value, String) -> R,
    ) -> Result<R, PermitRefusal> {
        if self.target_key != target.key() {
            return Err(PermitRefusal::TargetMismatch);
        }
        if self.input_hash != input_hash {
            return Err(PermitRefusal::InputMismatch);
        }
        Ok(effect(self.attempt, self.binding, self.input_hash))
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
