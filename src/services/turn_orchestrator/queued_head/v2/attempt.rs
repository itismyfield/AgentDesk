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

pub(crate) struct WriterLease(Arc<AtomicBool>);
impl Drop for WriterLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
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
    static WRITERS: OnceLock<Mutex<HashMap<String, Weak<AtomicBool>>>> = OnceLock::new();
    // Execution nonce and queue namespace do not separate writes to the same physical target.
    let name = serde_json::to_string(&(&target.host, &target.owner, &target.root))
        .map_err(|error| error.to_string())?;
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
    Ok(WriterLease(slot))
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
        head.confirmation_deadline_ms = Some(now.saturating_add(30_000));
        self.commit(next)?;
        Ok(EffectPermit {
            key: key.clone(),
            lease,
        })
    }
}
