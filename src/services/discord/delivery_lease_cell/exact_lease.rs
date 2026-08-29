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
    armed: bool,
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
                armed: true,
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
            armed: true,
            state: PhantomData,
        };
        self.armed = false;
        Ok(committed)
    }

    pub(super) fn reclaim_if_expired(&self, now_ms: u64) -> bool {
        self.cell.reclaim_exact_if_expired(
            &self.cell,
            &self.key,
            self.token,
            self.coordinate,
            now_ms,
        )
    }

    pub(super) fn release(mut self) -> Result<PinnedLeaseCell<Released>, Self> {
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
            armed: false,
            state: PhantomData,
        };
        self.armed = false;
        Ok(released)
    }
}

impl PinnedLeaseCell<Committed> {
    pub(super) fn release(mut self) -> Result<PinnedLeaseCell<Released>, Self> {
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
            armed: false,
            state: PhantomData,
        };
        self.armed = false;
        Ok(released)
    }
}

impl<State: GuardState> Drop for PinnedLeaseCell<State> {
    fn drop(&mut self) {
        if self.armed && State::RELEASE_ON_DROP {
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
        if !std::ptr::eq(self, Arc::as_ptr(pinned_cell)) {
            return false;
        }
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _payload_lock_marker = super::PayloadLockMarker::enter();
        if let super::LeaseState::Leased {
            holder,
            key: current_key,
            deadline_ms: current_deadline,
            start,
            end,
            exact_token,
        } = &mut guard.lease
        {
            if *holder == LeaseHolder::Sink
                && current_key == key
                && *exact_token == Some(token)
                && *start == coordinate.value()
                && *end == coordinate.value()
            {
                *current_deadline = deadline_ms;
                return true;
            }
        }
        false
    }

    fn reclaim_exact_if_expired(
        &self,
        pinned_cell: &Arc<Self>,
        key: &DeliveryLeaseKey,
        token: LeaseToken,
        coordinate: ConsumedCoordinate,
        now_ms: u64,
    ) -> bool {
        use std::sync::atomic::Ordering;
        if !std::ptr::eq(self, Arc::as_ptr(pinned_cell)) {
            return false;
        }
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _payload_lock_marker = super::PayloadLockMarker::enter();
        if !matches!(
            &guard.lease,
            super::LeaseState::Leased { deadline_ms, .. }
                if now_ms >= *deadline_ms
                    && exact_leased_fields_match(&guard.lease, key, token, coordinate.value())
        ) {
            return false;
        }
        guard.lease = super::LeaseState::Unleased;
        self.state_tag.store(super::TAG_UNLEASED, Ordering::Release);
        true
    }

    fn commit_exact(
        &self,
        pinned_cell: &Arc<Self>,
        key: &DeliveryLeaseKey,
        token: LeaseToken,
        coordinate: ConsumedCoordinate,
        outcome: LeaseOutcome,
    ) -> bool {
        use std::sync::atomic::Ordering;
        if !std::ptr::eq(self, Arc::as_ptr(pinned_cell)) {
            return false;
        }
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _payload_lock_marker = super::PayloadLockMarker::enter();
        if !exact_leased_fields_match(&guard.lease, key, token, coordinate.value()) {
            return false;
        }
        guard.lease = super::LeaseState::Committed {
            holder: LeaseHolder::Sink,
            key: key.clone(),
            start: coordinate.value(),
            end: coordinate.value(),
            outcome,
            exact_token: Some(token),
        };
        self.state_tag
            .store(super::TAG_COMMITTED, Ordering::Release);
        true
    }

    fn release_exact(
        &self,
        pinned_cell: &Arc<Self>,
        key: &DeliveryLeaseKey,
        token: LeaseToken,
        coordinate: ConsumedCoordinate,
    ) -> bool {
        use std::sync::atomic::Ordering;
        if !std::ptr::eq(self, Arc::as_ptr(pinned_cell)) {
            return false;
        }
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _payload_lock_marker = super::PayloadLockMarker::enter();
        if !exact_state_matches(&guard.lease, key, token, coordinate.value()) {
            return false;
        }
        guard.lease = super::LeaseState::Unleased;
        self.state_tag.store(super::TAG_UNLEASED, Ordering::Release);
        true
    }
}

fn exact_leased_fields_match(
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
        } => {
            *holder == LeaseHolder::Sink
                && *exact_token == Some(expected_token)
                && key == expected_key
                && *start == expected_coordinate
                && *end == expected_coordinate
        }
        super::LeaseState::Unleased | super::LeaseState::Committed { .. } => false,
    }
}

fn exact_state_matches(
    state: &super::LeaseState,
    expected_key: &DeliveryLeaseKey,
    expected_token: LeaseToken,
    expected_coordinate: u64,
) -> bool {
    exact_leased_fields_match(state, expected_key, expected_token, expected_coordinate)
        || matches!(
            state,
            super::LeaseState::Committed {
                holder: LeaseHolder::Sink,
                key,
                start,
                end,
                exact_token: Some(token),
                ..
            } if *token == expected_token
                && key == expected_key
                && *start == expected_coordinate
                && *end == expected_coordinate
        )
}

#[cfg(test)]
mod tests;
