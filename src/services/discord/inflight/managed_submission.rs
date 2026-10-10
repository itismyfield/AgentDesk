//! Submission-boundary evidence of managed Claude TUI turns and its pre-payload fence.
//! An absent record is LegacyUnknown; only the two turn creators install `Waiting`.

use std::sync::{Arc, Mutex, PoisonError};

use super::*;
use crate::services::claude_tui::submission_fence::{PreSubmitPersistenceRefused, SubmissionFence};
use crate::services::turn_host::TurnHost;

const MANAGED_SUBMISSION_PROTOCOL: u32 = 1;
const FENCE_CALLER: &str = "managed_submission_fence";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum SubmissionPhase {
    /// The boundary is installed and no payload write was allowed yet.
    Waiting,
    /// The fence persisted before the first payload; resending is unsafe even if no key landed.
    MayHaveSubmitted,
    /// A phase a newer binary wrote; kept verbatim and never read as `Waiting`.
    Unrecognized(String),
}

impl SubmissionPhase {
    fn as_str(&self) -> &str {
        match self {
            Self::Waiting => "waiting",
            Self::MayHaveSubmitted => "may_have_submitted",
            Self::Unrecognized(raw) => raw,
        }
    }
}

impl Serialize for SubmissionPhase {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SubmissionPhase {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.as_str() {
            "waiting" => Self::Waiting,
            "may_have_submitted" => Self::MayHaveSubmitted,
            _ => Self::Unrecognized(raw),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct ManagedSubmissionRecord {
    pub(in crate::services::discord) protocol: u32,
    pub(in crate::services::discord) phase: SubmissionPhase,
}

impl ManagedSubmissionRecord {
    fn at(phase: SubmissionPhase) -> Self {
        Self {
            protocol: MANAGED_SUBMISSION_PROTOCOL,
            phase,
        }
    }

    pub(in crate::services::discord) fn is_waiting(&self) -> bool {
        self.protocol == MANAGED_SUBMISSION_PROTOCOL && self.phase == SubmissionPhase::Waiting
    }

    /// Unknown protocols and phases rank above every known one so they are never lowered.
    fn rank(&self) -> u8 {
        match (self.protocol == MANAGED_SUBMISSION_PROTOCOL, &self.phase) {
            (true, SubmissionPhase::Waiting) => 1,
            (true, SubmissionPhase::MayHaveSubmitted) => 2,
            _ => u8::MAX,
        }
    }
}

fn same_birth_episode(a: &InflightTurnState, b: &InflightTurnState) -> bool {
    InflightEpisodePin::from_state(a).is_same_episode_as(&InflightEpisodePin::from_state(b))
}

/// Every row write keeps the stronger record of the same birth episode: a stale snapshot
/// cannot lower it and no write can promote an unmarked (LegacyUnknown) row.
pub(super) fn carry_forward(
    existing: Option<&InflightTurnState>,
    outgoing: &mut InflightTurnState,
) {
    let Some(existing) = existing.filter(|existing| same_birth_episode(existing, outgoing)) else {
        return;
    };
    let next = outgoing.managed_submission.take();
    outgoing.managed_submission = match (&existing.managed_submission, next) {
        (None, _) => None,
        (Some(durable), Some(next)) if next.rank() > durable.rank() => Some(next),
        (Some(durable), _) => Some(durable.clone()),
    };
}

enum Gate {
    Open,
    Fenced,
    Refused(PreSubmitPersistenceRefused),
}

struct Attempt {
    provider: ProviderKind,
    channel_id: u64,
    birth: InflightEpisodePin,
    gate: Mutex<Gate>,
}

/// Live input capability of the attempt that installed the boundary; never persisted.
#[derive(Clone)]
pub(in crate::services::discord) struct ManagedSubmission(Arc<Attempt>);

impl std::fmt::Debug for ManagedSubmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedSubmission")
            .field("channel_id", &self.0.channel_id)
            .finish_non_exhaustive()
    }
}

/// How a provider `Err` of a managed attempt is reported.
pub(in crate::services::discord) enum SubmissionFailure {
    /// The fence refused before any payload; the request stays in its Waiting row.
    PreSubmitPersistenceRefused(PreSubmitPersistenceRefused),
    /// The fence passed, so the prompt may have reached the pane.
    SubmissionIndeterminate,
    /// The attempt failed before it reached the fence.
    ProviderExecutionFailed,
}

impl ManagedSubmission {
    pub(in crate::services::discord) fn fence(&self) -> Arc<dyn SubmissionFence> {
        self.0.clone()
    }

    pub(in crate::services::discord) fn refusal(&self) -> Option<PreSubmitPersistenceRefused> {
        self.0.refusal()
    }

    pub(in crate::services::discord) fn classify_failure(&self) -> SubmissionFailure {
        match &*self.0.gate.lock().unwrap_or_else(PoisonError::into_inner) {
            Gate::Refused(refused) => {
                SubmissionFailure::PreSubmitPersistenceRefused(refused.clone())
            }
            Gate::Fenced => SubmissionFailure::SubmissionIndeterminate,
            Gate::Open => SubmissionFailure::ProviderExecutionFailed,
        }
    }
}

impl SubmissionFence for Attempt {
    fn before_first_payload(&self) -> Result<(), PreSubmitPersistenceRefused> {
        let mut gate = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        match &*gate {
            Gate::Fenced => return Ok(()),
            Gate::Refused(refused) => return Err(refused.clone()),
            Gate::Open => {}
        }
        match mark_may_have_submitted(&self.provider, self.channel_id, &self.birth) {
            Ok(()) => {
                *gate = Gate::Fenced;
                Ok(())
            }
            Err(reason) => {
                let refused = PreSubmitPersistenceRefused { reason };
                tracing::warn!(
                    provider = self.provider.as_str(),
                    channel_id = self.channel_id,
                    reason = %refused.reason,
                    "managed claude prompt held: submission fence not persisted"
                );
                *gate = Gate::Refused(refused.clone());
                Err(refused)
            }
        }
    }

    fn refusal(&self) -> Option<PreSubmitPersistenceRefused> {
        match &*self.gate.lock().unwrap_or_else(PoisonError::into_inner) {
            Gate::Refused(refused) => Some(refused.clone()),
            _ => None,
        }
    }
}

/// Waiting -> MayHaveSubmitted on the same birth episode, confirmed by reading the row back.
fn mark_may_have_submitted(
    provider: &ProviderKind,
    channel_id: u64,
    birth: &InflightEpisodePin,
) -> Result<(), String> {
    let mut guard = super::episode_guard::lock_inflight_birth_episode(provider, channel_id, birth)
        .map_err(|error| format!("inflight row unavailable: {error:?}"))?;
    if !guard
        .state()
        .managed_submission
        .as_ref()
        .is_some_and(ManagedSubmissionRecord::is_waiting)
    {
        let found = guard.state().managed_submission.clone();
        return Err(format!("submission record is not waiting: {found:?}"));
    }
    let mut updated = guard.state().clone();
    updated.managed_submission = Some(ManagedSubmissionRecord::at(
        SubmissionPhase::MayHaveSubmitted,
    ));
    match guard.persist_verified_under_guard(&updated, FENCE_CALLER) {
        GuardedSaveOutcome::Saved
            if guard.state().managed_submission == updated.managed_submission =>
        {
            Ok(())
        }
        outcome => Err(format!("fence write not confirmed: {outcome:?}")),
    }
}

/// Installs `Waiting` before the row is created when the Claude TUI on tmux will type the
/// prompt; any other route stays LegacyUnknown and gets no capability.
pub(in crate::services::discord) fn install_managed_submission_boundary(
    state: &mut InflightTurnState,
    host: &TurnHost,
) -> Option<ManagedSubmission> {
    let provider = state.provider_kind()?;
    let managed = provider == ProviderKind::Claude
        && state.runtime_kind == Some(RuntimeHandoffKind::ClaudeTui)
        && state.tmux_session_name.is_some()
        && matches!(host, TurnHost::Tmux);
    if !managed {
        return None;
    }
    state.managed_submission = Some(ManagedSubmissionRecord::at(SubmissionPhase::Waiting));
    let submission = ManagedSubmission(Arc::new(Attempt {
        provider,
        channel_id: state.channel_id,
        birth: InflightEpisodePin::from_state(state),
        gate: Mutex::new(Gate::Open),
    }));
    state.bind_managed_submission(submission.clone());
    Some(submission)
}

#[cfg(test)]
#[path = "managed_submission_tests.rs"]
mod tests;

/// Fault injection for the fence write, keyed by channel so parallel tests stay apart.
#[cfg(test)]
pub(in crate::services::discord) mod fault {
    use std::sync::Mutex;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(in crate::services::discord) enum FenceWriteFault {
        /// The temp file cannot be written; nothing reaches the directory.
        Write,
        /// The temp file is written but never renamed over the row.
        Rename,
    }

    static FAULTS: Mutex<Vec<(u64, FenceWriteFault)>> = Mutex::new(Vec::new());

    pub(in crate::services::discord) fn arm(channel_id: u64, fault: FenceWriteFault) {
        FAULTS.lock().unwrap().push((channel_id, fault));
    }

    /// Fails the fence write of an armed channel before `atomic_write` publishes it.
    pub(in crate::services::discord::inflight) fn check(
        path: &std::path::Path,
        channel_id: u64,
        caller: &str,
        json: &str,
    ) -> Result<(), String> {
        if caller != super::FENCE_CALLER {
            return Ok(());
        }
        let fault = {
            let mut faults = FAULTS.lock().unwrap();
            let index = faults
                .iter()
                .position(|(channel, _)| *channel == channel_id);
            index.map(|index| faults.remove(index).1)
        };
        match fault {
            None => Ok(()),
            Some(FenceWriteFault::Write) => Err("write_all: injected fence write fault".into()),
            Some(FenceWriteFault::Rename) => {
                std::fs::write(path.with_extension("json.fence.tmp"), json)
                    .map_err(|error| error.to_string())?;
                Err("rename: injected fence rename fault".into())
            }
        }
    }
}
