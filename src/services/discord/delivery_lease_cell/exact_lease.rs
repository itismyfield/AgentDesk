pub(super) mod token;

use std::marker::PhantomData;
use std::num::NonZeroU64;
use std::sync::Arc;

use super::{DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder, LeaseOutcome};
use token::LeaseToken;

/// Canonical positive consumed coordinate for one dormant exact lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ConsumedCoordinate(NonZeroU64);

impl ConsumedCoordinate {
    pub(super) fn new(value: u64) -> Option<Self> {
        NonZeroU64::new(value).map(Self)
    }

    fn value(self) -> u64 {
        self.0.get()
    }
}

#[derive(Debug)]
pub(super) struct Acquired;
#[derive(Debug)]
pub(super) struct Committed;
#[derive(Debug)]
pub(super) struct Released;

/// An exact, pinned lease guard. It never re-searches a registry: every
/// transition addresses the `Arc<DeliveryLeaseCell>` acquired here and proves
/// cell identity, full key, token, and consumed coordinate.
pub(super) trait GuardState {
    const RELEASE_ON_DROP: bool;
}

impl GuardState for Acquired {
    const RELEASE_ON_DROP: bool = true;
}
impl GuardState for Committed {
    const RELEASE_ON_DROP: bool = true;
}
impl GuardState for Released {
    const RELEASE_ON_DROP: bool = false;
}

pub(super) struct PinnedLeaseCell<State: GuardState> {
    cell: Arc<DeliveryLeaseCell>,
    key: DeliveryLeaseKey,
    coordinate: ConsumedCoordinate,
    token: LeaseToken,
    state: PhantomData<State>,
}

impl PinnedLeaseCell<Acquired> {
    pub(super) fn acquire(
        cell: Arc<DeliveryLeaseCell>,
        key: DeliveryLeaseKey,
        coordinate: ConsumedCoordinate,
        deadline_ms: u64,
    ) -> Option<Self> {
        let token = LeaseToken::next();
        cell.try_acquire_exact(key.clone(), token, coordinate, deadline_ms)
            .then_some(Self {
                cell,
                key,
                coordinate,
                token,
                state: PhantomData,
            })
    }

    pub(super) fn renew(&self, deadline_ms: u64) -> bool {
        self.cell.renew_exact(
            &self.cell,
            &self.key,
            self.token,
            self.coordinate,
            deadline_ms,
        )
    }

    pub(super) fn matches(&self) -> bool {
        self.cell
            .matches_exact(&self.cell, &self.key, self.token, self.coordinate)
    }

    #[must_use]
    pub(super) fn commit(
        mut self,
        outcome: LeaseOutcome,
    ) -> Result<PinnedLeaseCell<Committed>, Self> {
        if !self
            .cell
            .commit_exact(&self.cell, &self.key, self.token, self.coordinate, outcome)
        {
            return Err(self);
        }
        let committed = PinnedLeaseCell {
            cell: Arc::clone(&self.cell),
            key: self.key.clone(),
            coordinate: self.coordinate,
            token: self.token,
            state: PhantomData,
        };
        std::mem::forget(self);
        Ok(committed)
    }

    pub(super) fn release(self) -> Result<PinnedLeaseCell<Released>, Self> {
        if !self
            .cell
            .release_exact(&self.cell, &self.key, self.token, self.coordinate)
        {
            return Err(self);
        }
        let released = PinnedLeaseCell {
            cell: Arc::clone(&self.cell),
            key: self.key.clone(),
            coordinate: self.coordinate,
            token: self.token,
            state: PhantomData,
        };
        std::mem::forget(self);
        Ok(released)
    }
}

impl PinnedLeaseCell<Committed> {
    pub(super) fn release(self) -> Result<PinnedLeaseCell<Released>, Self> {
        if !self
            .cell
            .release_exact(&self.cell, &self.key, self.token, self.coordinate)
        {
            return Err(self);
        }
        let released = PinnedLeaseCell {
            cell: Arc::clone(&self.cell),
            key: self.key.clone(),
            coordinate: self.coordinate,
            token: self.token,
            state: PhantomData,
        };
        std::mem::forget(self);
        Ok(released)
    }
}

impl<State: GuardState> Drop for PinnedLeaseCell<State> {
    fn drop(&mut self) {
        if State::RELEASE_ON_DROP {
            let _released = self.release_owned_state();
        }
    }
}

impl<State: GuardState> PinnedLeaseCell<State> {
    fn release_owned_state(&self) -> bool {
        self.cell
            .release_exact(&self.cell, &self.key, self.token, self.coordinate)
    }
}

impl DeliveryLeaseCell {
    fn try_acquire_exact(
        &self,
        key: DeliveryLeaseKey,
        token: LeaseToken,
        coordinate: ConsumedCoordinate,
        deadline_ms: u64,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _payload_lock_marker = super::PayloadLockMarker::enter();
        if self
            .state_tag
            .compare_exchange(
                super::TAG_UNLEASED,
                super::TAG_LEASED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        guard.lease = super::LeaseState::Leased {
            holder: LeaseHolder::Sink,
            key,
            deadline_ms,
            start: coordinate.value(),
            end: coordinate.value(),
            exact_token: Some(token),
        };
        true
    }

    fn exact_identity_matches(
        &self,
        pinned_cell: &Arc<Self>,
        key: &DeliveryLeaseKey,
        token: LeaseToken,
        coordinate: ConsumedCoordinate,
    ) -> bool {
        if !std::ptr::eq(self, Arc::as_ptr(pinned_cell)) {
            return false;
        }
        let guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _payload_lock_marker = super::PayloadLockMarker::enter();
        exact_state_matches(&guard.lease, key, token, coordinate.value())
    }

    fn matches_exact(
        &self,
        pinned_cell: &Arc<Self>,
        key: &DeliveryLeaseKey,
        token: LeaseToken,
        coordinate: ConsumedCoordinate,
    ) -> bool {
        self.exact_identity_matches(pinned_cell, key, token, coordinate)
    }

    fn renew_exact(
        &self,
        pinned_cell: &Arc<Self>,
        key: &DeliveryLeaseKey,
        token: LeaseToken,
        coordinate: ConsumedCoordinate,
        deadline_ms: u64,
    ) -> bool {
        self.exact_identity_matches(pinned_cell, key, token, coordinate)
            && self.renew(LeaseHolder::Sink, key.clone(), deadline_ms)
    }

    fn commit_exact(
        &self,
        pinned_cell: &Arc<Self>,
        key: &DeliveryLeaseKey,
        token: LeaseToken,
        coordinate: ConsumedCoordinate,
        outcome: LeaseOutcome,
    ) -> bool {
        self.exact_identity_matches(pinned_cell, key, token, coordinate)
            && self.commit(
                LeaseHolder::Sink,
                key.clone(),
                coordinate.value(),
                coordinate.value(),
                outcome,
            )
    }

    fn release_exact(
        &self,
        pinned_cell: &Arc<Self>,
        key: &DeliveryLeaseKey,
        token: LeaseToken,
        coordinate: ConsumedCoordinate,
    ) -> bool {
        self.exact_identity_matches(pinned_cell, key, token, coordinate)
            && self.release(
                LeaseHolder::Sink,
                key.clone(),
                coordinate.value(),
                coordinate.value(),
            )
    }
}

fn exact_state_matches(
    state: &super::LeaseState,
    expected_key: &DeliveryLeaseKey,
    expected_token: LeaseToken,
    expected_coordinate: u64,
) -> bool {
    match state {
        super::LeaseState::Leased {
            holder,
            key,
            start,
            end,
            exact_token,
            ..
        }
        | super::LeaseState::Committed {
            holder,
            key,
            start,
            end,
            exact_token,
            ..
        } => {
            *holder == LeaseHolder::Sink
                && *exact_token == Some(expected_token)
                && key == expected_key
                && *start == expected_coordinate
                && *end == expected_coordinate
        }
        super::LeaseState::Unleased => false,
    }
}

#[cfg(test)]
mod tests;
