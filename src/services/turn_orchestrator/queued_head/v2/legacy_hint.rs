//! Marker debt changes custody through the queue's existing byte CAS.
use super::super::v2_types::marker_control::{MarkerControl, MarkerRowExpectation, MarkerStage};
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct ClaimLife(Arc<AtomicBool>);
impl Drop for ClaimLife {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Owner {
    pub(crate) fn a_claim_liveness(
        &mut self,
        front: &Front,
        number: u64,
    ) -> Result<Arc<AtomicBool>, String> {
        if number != self.claim_number || !self.claimed(front.1.message_id, front) {
            return Err("marker admission claim changed".into());
        }
        let life = self
            .a_claim_live
            .get_or_insert_with(|| ClaimLife(Arc::new(AtomicBool::new(true))));
        Ok(life.0.clone())
    }

    pub(crate) fn a_controls(&self) -> Vec<MarkerControl> {
        self.snapshot.a_control.clone()
    }

    pub(crate) fn a_control_for(&self, front: &Front) -> Option<MarkerControl> {
        let identity = sources::identity(&front.1);
        let receipt = self.a_front_receipt(front).ok()?;
        self.snapshot
            .a_control
            .iter()
            .find(|c| c.identity == identity && c.front_receipt == receipt)
            .cloned()
    }

    pub(crate) fn a_front_receipt(&self, front: &Front) -> Result<Option<String>, String> {
        if !self
            .front()
            .as_ref()
            .is_some_and(|now| is_front(now, front))
        {
            return Err("marker original is no longer the front".into());
        }
        let digest = self
            .snapshot
            .queued_digest(0)
            .ok_or("missing marker front digest")?;
        Ok(self
            .snapshot
            .admission_binds
            .iter()
            .find_map(|bind| match &bind.at {
                BindAt::Queued { index: 0, item } if *item == digest => Some(bind.receipt.clone()),
                _ => None,
            }))
    }

    pub(crate) fn a_intent(
        &mut self,
        front: &Front,
        control: MarkerControl,
        epoch: &str,
    ) -> Result<MarkerControl, String> {
        self.recover()?;
        if control.identity != sources::identity(&front.1)
            || control.front_receipt != self.a_front_receipt(front)?
            || control.epoch != epoch
            || epoch.is_empty()
            || control.cleanup_id.is_empty()
            || control.paths.is_empty()
            || control.stage != MarkerStage::Intent
            || control.attempt.is_some()
            || matches!(&control.expected, MarkerRowExpectation::Missing { reserved_row_id, .. }
                if reserved_row_id.is_none_or(|id| id <= 0))
        {
            return Err("marker intent does not name the exact front".into());
        }
        if let Some(existing) = self.a_control_for(front) {
            if existing.cleanup_id != control.cleanup_id
                || existing.expected != control.expected
                || existing.witness != control.witness
                || existing.paths != control.paths
                || existing.epoch != control.epoch
                || existing.front_receipt != control.front_receipt
            {
                return Err("marker intent already belongs to another observation".into());
            }
            return Ok(existing);
        }
        if self
            .snapshot
            .a_control
            .iter()
            .any(|c| c.cleanup_id == control.cleanup_id)
        {
            return Err("marker cleanup id already names another original".into());
        }
        let mut next = self.snapshot.clone();
        next.version = 3;
        next.a_control.push(control.clone());
        self.commit(next)?;
        Ok(control)
    }

    pub(crate) fn a_bind_created_row(
        &mut self,
        id: &str,
        row_id: i64,
        exact_key: &str,
    ) -> Result<MarkerControl, String> {
        self.recover()?;
        if row_id <= 0 || exact_key.is_empty() {
            return Err("invalid created marker row".into());
        }
        let mut next = self.snapshot.clone();
        let control = next
            .a_control
            .iter_mut()
            .find(|c| c.cleanup_id == id)
            .ok_or("marker debt missing")?;
        if control.stage != MarkerStage::Intent {
            return Err("marker row changed after cleanup began".into());
        }
        let owner = match &control.expected {
            MarkerRowExpectation::Missing {
                owner,
                reserved_row_id: Some(reserved),
            } if *reserved == row_id => owner.clone(),
            MarkerRowExpectation::Legacy {
                row_id: previous,
                exact_session_key,
                ..
            } if *previous == row_id && exact_session_key == exact_key => {
                return Ok(control.clone());
            }
            _ => return Err("marker row already names another observation".into()),
        };
        control.expected = MarkerRowExpectation::Legacy {
            owner,
            row_id,
            exact_session_key: exact_key.into(),
        };
        let result = control.clone();
        self.commit(next)?;
        Ok(result)
    }

    pub(crate) fn a_stage(
        &mut self,
        id: &str,
        expected: &MarkerStage,
        next_stage: MarkerStage,
    ) -> Result<(), String> {
        self.recover()?;
        let mut next = self.snapshot.clone();
        let control = next
            .a_control
            .iter_mut()
            .find(|c| c.cleanup_id == id)
            .ok_or("marker debt missing")?;
        if control.stage == next_stage {
            return Ok(());
        }
        let allowed = matches!(
            (expected, &next_stage),
            (MarkerStage::Intent, MarkerStage::Receipt)
                | (MarkerStage::Receipt, MarkerStage::Outbox { .. })
        );
        if &control.stage != expected
            || !allowed
            || matches!(&next_stage, MarkerStage::Outbox { event_id } if event_id.is_empty())
        {
            return Err("marker stage changed".into());
        }
        control.stage = next_stage;
        self.commit(next)
    }

    pub(crate) fn a_attempt(&mut self, id: &str, attempt: AttemptIdentity) -> Result<(), String> {
        self.recover()?;
        let mut next = self.snapshot.clone();
        let control = next
            .a_control
            .iter_mut()
            .find(|c| c.cleanup_id == id)
            .ok_or("marker debt missing")?;
        if !matches!(control.stage, MarkerStage::Outbox { .. }) {
            return Err("marker attempt before wake outbox".into());
        }
        if let Some(previous) = &control.attempt {
            return if *previous == attempt {
                Ok(())
            } else {
                Err("marker execution changed".into())
            };
        }
        control.attempt = Some(attempt);
        self.commit(next)
    }

    pub(crate) fn a_ack_if_settled(&mut self, id: &str) -> Result<bool, String> {
        self.recover()?;
        let Some(control) = self.snapshot.a_control.iter().find(|c| c.cleanup_id == id) else {
            return Ok(true);
        };
        let settled = self.snapshot.terminal_identities.iter().any(|t| t.identity == control.identity)
            || self.snapshot.retained_inputs.iter().any(|entry|
                entry.input.identity == control.identity
                && matches!(entry.disposition, Some(RetainedDisposition::KnownNotSent))
                && control.attempt.as_ref().is_none_or(|a| entry.input.attempt.as_ref() == Some(a)))
            || control.attempt.as_ref().is_some_and(|attempt| self.snapshot.admission_binds.iter().any(|bind|
                control.front_receipt.as_deref() == Some(bind.receipt.as_str())
                && matches!(&bind.at, BindAt::Settled { attempt: nonce, .. } if nonce == &attempt.nonce)));
        if !settled {
            return Ok(false);
        }
        let mut next = self.snapshot.clone();
        next.a_control.retain(|c| c.cleanup_id != id);
        self.commit(next)?;
        Ok(true)
    }

    #[cfg(unix)]
    pub(crate) fn a_refuse(
        &mut self,
        front: &Front,
        id: &str,
        reason: &str,
    ) -> Result<bool, String> {
        self.a_finish(front, id, reason, false)
    }

    #[cfg(unix)]
    pub(crate) fn a_unproved(
        &mut self,
        front: &Front,
        id: &str,
        reason: &str,
    ) -> Result<bool, String> {
        self.recover()?;
        let Some(control) = self.a_control_for(front) else {
            return Ok(false);
        };
        if control.cleanup_id != id
            || self.snapshot.head_injection.is_some()
            || self.snapshot.retained_inputs.iter().any(|entry| {
                entry.input.identity == control.identity && entry.input.attempt.is_some()
            })
        {
            return Ok(false);
        }
        self.a_finish(front, id, reason, true)
    }

    #[cfg(unix)]
    fn a_finish(
        &mut self,
        front: &Front,
        id: &str,
        reason: &str,
        unproved: bool,
    ) -> Result<bool, String> {
        self.recover()?;
        if self.a_ack_if_settled(id)? {
            return Ok(true);
        }
        let control = self
            .a_control_for(front)
            .ok_or("marker debt does not name the refused front")?;
        if control.cleanup_id != id
            || !self
                .front()
                .as_ref()
                .is_some_and(|now| is_front(now, front))
        {
            return Err("refused marker front changed".into());
        }
        self.grant_front(front)?;
        self.claim_turn(front.1.message_id, front)
            .ok_or("refused marker claim changed")?;
        let result = self.start_failed_as(
            front,
            self.claim_number,
            std::slice::from_ref(&front.1),
            None,
            (StartFailureKind::MarkerAdmissionRefused, reason, unproved),
        );
        self.revoke_front();
        if result? != Some(true) {
            return Err("marker refusal was not retained".into());
        }
        self.a_ack_if_settled(id)
    }
}

#[cfg(all(test, unix))]
#[path = "legacy_hint_tests.rs"]
mod tests;
