//! The first output's identity and terminal choice: adopted, or rejected with one reason.
use super::FirstProof;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, Instant};

pub(crate) use crate::db::tui_output_disposition::{Identity, Reason};
impl Identity {
    pub(crate) fn from_proof(proof: &FirstProof, input: u64) -> Self {
        Self {
            channel: proof.channel,
            input,
            nonce: super::first_prompt_digest(&proof.nonce),
            execution: super::first_prompt_digest(&proof.context.execution_nonce),
            source: Self::source_digest(&proof.source),
        }
    }
    pub(crate) fn source_digest(source: &crate::services::tui_o::shadow::SourceId) -> String {
        super::first_prompt_digest(&serde_json::to_string(source).expect("source identity"))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Normalizing,
    Adopted,
    ExistingEffectPending,
    RejectRequested(Reason),
    RejectPendingNotice(Reason),
    Rejected(Reason),
}
/// One total budget from the admission covers every retry, wake and repair of a normalization.
pub(crate) const BUDGET: Duration = Duration::from_secs(25);
#[derive(Clone, Debug)]
pub(crate) struct Output {
    id: Identity,
    deadline: Instant,
    state: Arc<Mutex<(Phase,)>>,
    /// The one repair the admission may spend, shared by every view of that admission.
    repair: Arc<AtomicBool>,
}
impl Output {
    pub(crate) fn new(id: Identity, admitted_at: Instant) -> Self {
        Self::admitted(id, admitted_at, Arc::default())
    }
    /// A view of the admission that began at `admitted_at` and spends its `repair`; neither the
    /// deadline nor the repair is renewed by a later view.
    pub(crate) fn admitted(id: Identity, admitted_at: Instant, repair: Arc<AtomicBool>) -> Self {
        // Notice delivery has a separate metadata-only timeout.
        Self {
            id,
            deadline: admitted_at + BUDGET,
            state: Arc::new(Mutex::new((Phase::Normalizing,))),
            repair,
        }
    }
    pub(crate) async fn restore(
        pool: &sqlx::PgPool,
        id: Identity,
        admitted_at: Instant,
    ) -> Result<Self, String> {
        let recorded = crate::db::tui_output_disposition::record(pool, &id).await?;
        let output = Self::new(id, admitted_at);
        if let Some((reason, receipt)) = recorded {
            output.state.lock().unwrap().0 = Phase::RejectPendingNotice(reason);
            if let Some(receipt) = receipt {
                output.confirm(&receipt);
            }
        }
        Ok(output)
    }
    pub(crate) async fn persist_rejection(&self, pool: &sqlx::PgPool) -> Result<(), String> {
        let reason = match self.phase() {
            Phase::RejectPendingNotice(reason) | Phase::Rejected(reason) => reason,
            _ => return Err("only a terminal rejection may persist a tombstone".into()),
        };
        crate::db::tui_output_disposition::persist_intent(pool, &self.id, reason).await
    }
    pub(crate) fn id(&self) -> &Identity {
        &self.id
    }
    pub(crate) fn phase(&self) -> Phase {
        self.state.lock().unwrap().0
    }
    pub(crate) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
    pub(crate) fn expire(&self) -> bool {
        if Instant::now() < self.deadline {
            return false;
        }
        self.reject(Reason::Deadline)
    }
    pub(crate) fn reject(&self, reason: Reason) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.0 != Phase::Normalizing {
            return false;
        }
        state.0 = Phase::RejectRequested(reason);
        true
    }
    pub(crate) fn adopt(
        &self,
        proof: &FirstProof,
        validate: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        if Self::identity_mismatch(self.id(), proof) {
            return Err("output proof identity changed".into());
        }
        let mut state = self.state.lock().unwrap();
        if state.0 != Phase::Normalizing || Instant::now() >= self.deadline {
            return Err("output is terminal or expired".into());
        }
        proof.with_current(|| {
            validate()?;
            if Instant::now() >= self.deadline {
                return Err("normalization deadline expired".into());
            }
            state.0 = Phase::Adopted;
            Ok(())
        })
    }
    pub(crate) fn finish_first(
        &self,
        candidate: &super::Candidate,
        proof: &FirstProof,
        store: &crate::services::tui_o::store::OStore,
        reason: Reason,
    ) -> Result<FirstProof, Reason> {
        use super::Adoption;
        let mut adoption = candidate.lock();
        let mut state = self.state.lock().unwrap();
        if Self::identity_mismatch(self.id(), proof) || self.id.input != proof.mark.order {
            if matches!(state.0, Phase::Normalizing | Phase::RejectRequested(_)) {
                state.0 = Phase::ExistingEffectPending;
            }
            return Err(Reason::Invalidated);
        }
        let reason = match state.0 {
            Phase::RejectRequested(requested) => requested,
            _ => reason,
        };
        match state.0 {
            Phase::Adopted => return Ok(proof.clone()),
            Phase::RejectPendingNotice(reason) | Phase::Rejected(reason) => return Err(reason),
            Phase::ExistingEffectPending if *adoption != Adoption::Committed => {
                return Err(Reason::Invalidated);
            }
            Phase::Normalizing | Phase::RejectRequested(_) | Phase::ExistingEffectPending => {}
        }
        if *adoption == Adoption::Committed {
            if !candidate.first_names(proof) {
                state.0 = Phase::ExistingEffectPending;
                return Err(Reason::Invalidated);
            }
            let observed = || -> Option<bool> {
                use crate::services::tui_o::store::InitOrigin;
                use sha2::{Digest, Sha256};
                store.read_era().ok().flatten()?;
                let init = store.read_init(proof.channel).ok().flatten()?;
                Some(
                    store.read_init_origin(proof.channel).ok()? == InitOrigin::LateCodexTakeover
                        && init.sources.len() == 1
                        && init.sources[0].source_id == proof.source
                        && init.sources[0].delivery_start == 0
                        && init.sources[0].prefix_hash == hex::encode(Sha256::digest(b""))
                        && init.policy_parent
                            == candidate.child_surface().map(|child| child.parent()),
                )
            };
            state.0 = if observed() == Some(true) {
                Phase::Adopted
            } else {
                Phase::ExistingEffectPending
            };
            return (state.0 == Phase::Adopted)
                .then(|| proof.clone())
                .ok_or(Reason::Invalidated);
        }
        if matches!(*adoption, Adoption::Held | Adoption::Released)
            || store.has_channel_dir(proof.channel)
        {
            state.0 = Phase::ExistingEffectPending;
            return Err(Reason::Invalidated);
        }
        // The actor's final checks and init publication take this same candidate lock.
        let matched = candidate.invalidate_first_proved(proof);
        if matched {
            *adoption = Adoption::Refused;
        }
        let reason = if matched { reason } else { Reason::Invalidated };
        state.0 = Phase::RejectPendingNotice(reason);
        Err(reason)
    }

    fn identity_mismatch(id: &Identity, proof: &FirstProof) -> bool {
        *id != Identity::from_proof(proof, id.input)
    }
    /// Whether any view of this admission has spent its one repair.
    pub(crate) fn repair_spent(&self) -> bool {
        self.repair.load(Ordering::SeqCst)
    }
    pub(crate) fn take_repair(&self) -> bool {
        let state = self.state.lock().unwrap();
        if state.0 != Phase::Normalizing || Instant::now() >= self.deadline {
            return false;
        }
        !self.repair.swap(true, Ordering::SeqCst)
    }
    pub(crate) fn confirm(&self, receipt: &crate::db::tui_output_disposition::Receipt) -> bool {
        let mut state = self.state.lock().unwrap();
        let Phase::RejectPendingNotice(reason) = state.0 else {
            return false;
        };
        if receipt.key() != self.id.key()
            || receipt.reason() != reason
            || receipt.template_version() != 1
            || receipt.message() == 0
        {
            return false;
        }
        state.0 = Phase::Rejected(reason);
        true
    }
}

#[cfg(test)]
#[path = "output_disposition_tests.rs"]
pub(crate) mod tests;
