//! The owner's retry wait: a front refused for a cause a later look may clear waits unclaimed for
//! its due or for a read of that cause; until then the drive grants it nothing.
use super::*;

/// The first wait; each due the front consumes doubles it, up to `MAX_WAIT_MS`.
#[cfg(unix)]
const FIRST_WAIT_MS: u64 = 30_000;
#[cfg(unix)]
const MAX_WAIT_MS: u64 = 300_000;
#[cfg(unix)]
pub(super) use super::super::v2_types::retry_wait::RetryCause;
pub(super) use super::super::v2_types::retry_wait::RetryWait;

/// Whether `wait` is the wait of exactly this front: its slot's ticket and original.
fn holds(wait: &RetryWait, front: &Front) -> bool {
    wait.ticket == front.0 && wait.front == refused::refused_member(&front.1)
}

impl Owner {
    /// Whether `front` sits in an armed wait, which its grant waits out.
    pub(crate) fn retry_waiting(&self, front: &Front) -> bool {
        let wait = self.snapshot.retry.wait.as_ref();
        wait.is_some_and(|wait| wait.armed && holds(wait, front))
    }

    /// The grant `grant` of `front`, refused before any claim, waits as `cause`; it is spent.
    #[cfg(unix)]
    pub(crate) fn enter_retry_wait_granted(
        &mut self,
        (front, grant): (&Front, u64),
        cause: RetryCause,
        now: u64,
    ) -> Result<Option<u64>, String> {
        let granted = grant == self.grant_number
            && self.front_claim.is_none()
            && (self.front_grant.as_ref()).is_some_and(|g| is_front(g, front))
            && self
                .front()
                .is_some_and(|current| is_front(&current, front));
        if !granted {
            return Ok(None);
        }
        self.enter_retry_wait(front, cause, now).map(Some)
    }

    /// The current claim `number` of `front`, refused before any reserve, waits as `cause`; the
    /// claim is withdrawn.
    #[cfg(unix)]
    pub(crate) fn enter_retry_wait_claimed(
        &mut self,
        (front, number): (&Front, u64),
        cause: RetryCause,
        now: u64,
    ) -> Result<Option<u64>, String> {
        if number != self.claim_number || !self.claimed(front.1.message_id, front) {
            return Ok(None);
        }
        self.enter_retry_wait(front, cause, now).map(Some)
    }

    #[cfg(unix)]
    fn enter_retry_wait(
        &mut self,
        front: &Front,
        cause: RetryCause,
        now: u64,
    ) -> Result<u64, String> {
        let mut next = self.snapshot.clone();
        // The same execution keeps its consumed dues; another one starts over.
        let attempts = (next.retry.wait.as_ref())
            .filter(|wait| holds(wait, front) && wait.cause.nonce() == cause.nonce())
            .map_or(0, |wait| wait.attempts);
        next.retry.seq += 1;
        let wait_seq = next.retry.seq;
        let backoff = FIRST_WAIT_MS.saturating_mul(1u64 << attempts.min(8));
        next.retry.wait = Some(RetryWait {
            ticket: front.0.clone(),
            front: refused::refused_member(&front.1),
            wait_seq,
            cause,
            due_at_ms: now.saturating_add(backoff.min(MAX_WAIT_MS)),
            attempts,
            armed: true,
        });
        self.commit(next)?;
        self.revoke_front();
        self.held = Some(front.1.message_id);
        Ok(wait_seq)
    }

    /// A read that cleared the cause front `message` waits on ends that armed wait `wait_seq`
    /// early; a read left by another wait, or of a held root recovery, changes nothing.
    #[cfg(unix)]
    pub(crate) fn release_retry_wait(
        &mut self,
        message: u64,
        wait_seq: u64,
    ) -> Result<bool, String> {
        self.disarm(|wait| {
            wait.wait_seq == wait_seq
                && wait.front.identity.primary == message
                && wait.cause != RetryCause::RootRecoveryHeld
        })
    }

    /// The restart reader retired the held root recovery: only a wait held on it ends.
    #[cfg(unix)]
    pub(crate) fn release_root_recovery_wait(&mut self) -> Result<bool, String> {
        self.disarm(|wait| wait.cause == RetryCause::RootRecoveryHeld)
    }

    #[cfg(unix)]
    fn disarm(&mut self, releases: impl Fn(&RetryWait) -> bool) -> Result<bool, String> {
        let wait = self.snapshot.retry.wait.as_ref();
        if !wait.is_some_and(|wait| wait.armed && releases(wait)) {
            return Ok(false);
        }
        let mut next = self.snapshot.clone();
        if let Some(wait) = next.retry.wait.as_mut() {
            wait.armed = false;
        }
        self.commit(next)?;
        Ok(true)
    }

    /// Whether execution `nonce` of `front`, refused as never started, consumed a due: only then
    /// may its pane be closed automatically.
    #[cfg(all(test, unix))]
    pub(crate) fn never_started_close_qualified(&self, front: &Front, nonce: &str) -> bool {
        self.snapshot.retry.wait.as_ref().is_some_and(|wait| {
            holds(wait, front)
                && wait.attempts >= 1
                && matches!(&wait.cause, RetryCause::PendingUnstarted { nonce: n }
                    | RetryCause::OwnLaunchTimeout { nonce: n } if n == nonce)
        })
    }

    /// When `tick` next consumes the armed wait's due.
    pub(super) fn retry_due(&self) -> Option<u64> {
        let wait = self.snapshot.retry.wait.as_ref();
        wait.filter(|wait| wait.armed).map(|wait| wait.due_at_ms)
    }
}

impl SnapshotV2 {
    /// The ticket bound to the queue front's slot; a migrated or partly admitted front has none.
    pub(super) fn front_ticket(&self) -> Option<String> {
        let item = self.queued_digest(0)?;
        self.admission_binds.iter().find_map(|b| match &b.at {
            BindAt::Queued { index: 0, item: at } if *at == item => Some(
                b.receipt
                    .strip_prefix("tk:")?
                    .rsplit_once(':')?
                    .0
                    .to_string(),
            ),
            _ => None,
        })
    }

    /// Consumes the armed wait's due once it passed: one more attempt, and the front may start.
    pub(super) fn consume_retry_due(&mut self, now: u64) {
        let due = self.retry.wait.as_mut();
        if let Some(wait) = due.filter(|wait| wait.armed && now >= wait.due_at_ms) {
            wait.attempts = wait.attempts.saturating_add(1);
            wait.armed = false;
        }
    }

    /// Drops the wait once its front left the queue front or was attempted; its number stays spent.
    pub(super) fn prune_retry_wait(&mut self) {
        let front = self.items.first().filter(|_| self.head_injection.is_none());
        let ticket = self.front_ticket();
        let kept = (self.retry.wait.as_ref()).is_some_and(|wait| {
            front.is_some_and(|item| {
                wait.ticket == ticket && wait.front == refused::refused_member(item)
            })
        });
        if !kept {
            self.retry.wait = None;
        }
    }
}
