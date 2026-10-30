use super::*;
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct AttemptRef {
    pub(crate) identity: QueueIdentity,
    pub(crate) attempt: AttemptIdentity,
}

impl AttemptRef {
    pub(super) fn matches(&self, input: &InputAttempt) -> bool {
        self.identity == input.identity && input.attempt.as_ref() == Some(&self.attempt)
    }
}

pub(crate) struct WriterLease(Vec<Arc<AtomicBool>>);
impl Drop for WriterLease {
    fn drop(&mut self) {
        for slot in &self.0 {
            slot.store(false, Ordering::Release);
        }
    }
}

/// Neither permit nor writer lease is clonable. Moving the logical head never drops this lease.
pub(crate) struct EffectPermit {
    key: AttemptRef,
    lease: WriterLease,
}

impl EffectPermit {
    pub(crate) fn consume(self, key: &AttemptRef) -> Result<WriterLease, String> {
        if self.key != *key {
            return Err("stale effect permit".into());
        }
        Ok(self.lease)
    }
}

fn writer(target: &TargetIdentity) -> Result<WriterLease, String> {
    // Execution nonce and queue namespace do not separate writes to the same logical owner.
    let name = serde_json::to_string(&(&target.host, &target.owner, &target.root))
        .map_err(|error| error.to_string())?;
    writer_named(name)
}
fn writer_named(name: String) -> Result<WriterLease, String> {
    static WRITERS: OnceLock<Mutex<HashMap<String, Weak<AtomicBool>>>> = OnceLock::new();
    let mut writers = WRITERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let slot = writers
        .get(&name)
        .and_then(Weak::upgrade)
        .unwrap_or_else(|| {
            let slot = Arc::new(AtomicBool::new(false));
            writers.insert(name, Arc::downgrade(&slot));
            slot
        });
    slot.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| "physical writer busy")?;
    Ok(WriterLease(vec![slot]))
}

impl Owner {
    pub(crate) fn active_attempt(&self) -> Result<AttemptRef, String> {
        let head = self
            .snapshot
            .head_injection
            .as_ref()
            .ok_or("no active head")?;
        Ok(AttemptRef {
            identity: head.identity.clone(),
            attempt: head.attempt.clone().ok_or("unnamed attempt")?,
        })
    }
    pub(crate) fn arm(&mut self, key: &AttemptRef, now: u64) -> Result<EffectPermit, String> {
        self.arm_phase(key, now, false)
    }
    pub(crate) fn arm_bootstrap(
        &mut self,
        key: &AttemptRef,
        now: u64,
    ) -> Result<EffectPermit, String> {
        self.arm_phase(key, now, true)
    }
    fn arm_phase(
        &mut self,
        key: &AttemptRef,
        now: u64,
        bootstrap: bool,
    ) -> Result<EffectPermit, String> {
        let head = self
            .snapshot
            .head_injection
            .as_ref()
            .ok_or("no active head")?;
        if !key.matches(head) || head.phase != HeadPhase::Reserved {
            return Err("stale arm".into());
        }
        let lease = writer(&key.attempt.target)?;
        let mut next = self.snapshot.clone();
        let head = next
            .head_injection
            .as_mut()
            .ok_or("active head disappeared")?;
        head.phase = HeadPhase::MayHaveSent;
        head.confirmation_deadline_ms = (!bootstrap).then(|| now.saturating_add(30_000));
        head.payload_window = bootstrap.then_some(PayloadWindow::Bootstrap);
        self.commit(next)?;
        Ok(EffectPermit {
            key: key.clone(),
            lease,
        })
    }
}

impl Owner {
    /// Whether the head attempt `nonce` is still in its bootstrap window, which a modal may yet
    /// end unsent; no observer binds such an attempt.
    pub(crate) fn bootstrapping(&self, nonce: &str) -> bool {
        self.snapshot.head_injection.as_ref().is_some_and(|head| {
            head.attempt.as_ref().is_some_and(|a| a.nonce == nonce)
                && matches!(head.payload_window, Some(PayloadWindow::Bootstrap))
        })
    }

    pub(crate) fn payload_started(&mut self, key: &AttemptRef, now: u64) -> Result<(), String> {
        let head = self
            .snapshot
            .head_injection
            .as_ref()
            .ok_or("no active payload")?;
        if !key.matches(head) || head.phase != HeadPhase::MayHaveSent {
            return Err("stale payload attempt".into());
        }
        match head.payload_window {
            Some(PayloadWindow::Started { .. }) => return Ok(()),
            None if head.confirmation_deadline_ms.is_some() => return Ok(()),
            Some(PayloadWindow::Bootstrap) => {}
            None => return Err("unknown payload phase".into()),
        }
        let mut next = self.snapshot.clone();
        let head = next
            .head_injection
            .as_mut()
            .ok_or("active payload disappeared")?;
        head.payload_window = Some(PayloadWindow::Started { at_ms: now });
        head.confirmation_deadline_ms = Some(now.saturating_add(30_000));
        self.commit(next)
    }
    pub(crate) fn bind_target(
        &mut self,
        key: &AttemptRef,
        epoch: crate::services::tui_input::target_epoch::PaneEpoch,
    ) -> Result<(), String> {
        let mut next = self.snapshot.clone();
        let input = next
            .head_injection
            .iter_mut()
            .chain(next.retained_inputs.iter_mut().map(|r| &mut r.input))
            .find(|h| key.matches(h))
            .ok_or("stale target attempt")?;
        if epoch.host != key.attempt.target.host
            || epoch.owner != key.attempt.target.owner
            || epoch.root != key.attempt.target.root
            || epoch.execution_nonce != key.attempt.target.execution_nonce
        {
            return Err("target epoch authority mismatch".into());
        }
        if let Some(old) = &input.resolved_target {
            return (old == &epoch)
                .then_some(())
                .ok_or("target epoch already bound".into());
        }
        input.resolved_target = Some(epoch);
        self.commit(next)
    }
    pub(crate) fn record_preparation_hold(
        &mut self,
        key: &AttemptRef,
        hold: serde_json::Value,
    ) -> Result<(), String> {
        let mut next = self.snapshot.clone();
        let input = next
            .head_injection
            .iter_mut()
            .chain(next.retained_inputs.iter_mut().map(|r| &mut r.input))
            .find(|h| key.matches(h))
            .ok_or("stale preparation hold")?;
        input.preparation_hold = Some(hold);
        self.commit(next)
    }
}

impl WriterLease {
    pub(crate) fn add_physical(&mut self, physical: String) -> Result<(), String> {
        let mut physical = writer_named(format!("physical:{physical}"))?;
        self.0.append(&mut physical.0);
        Ok(())
    }
}
