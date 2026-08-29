pub(super) mod token;

use std::marker::PhantomData;
use std::num::NonZeroU64;
use std::sync::Arc;

use super::{DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder, LeaseOutcome, LeaseState};
use token::LeaseToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ConsumedCoordinate(NonZeroU64);

impl ConsumedCoordinate {
    pub(super) fn new(value: u64) -> Option<Self> {
        NonZeroU64::new(value).map(Self)
    }

    fn get(self) -> u64 {
        self.0.get()
    }
}

pub(super) struct Acquired;
pub(super) struct Committed;
pub(super) struct Released;

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

pub(super) struct PinnedLeaseCell<S: GuardState> {
    cell: Arc<DeliveryLeaseCell>,
    key: DeliveryLeaseKey,
    coordinate: ConsumedCoordinate,
    token: LeaseToken,
    armed: bool,
    state: PhantomData<S>,
}

impl PinnedLeaseCell<Acquired> {
    pub(super) fn acquire(
        cell: Arc<DeliveryLeaseCell>,
        key: DeliveryLeaseKey,
        coordinate: ConsumedCoordinate,
        deadline_ms: u64,
    ) -> Option<Self> {
        if cell.channel_id != key.channel_id {
            return None;
        }
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

    pub(super) fn commit(
        mut self,
        outcome: LeaseOutcome,
    ) -> Result<PinnedLeaseCell<Committed>, Self> {
        let key = self.key.clone();
        let coordinate = self.coordinate;
        let token = self.token;
        if !self
            .cell
            .commit_exact(&self.cell, &key, token, coordinate, outcome)
        {
            return Err(self);
        }
        let successor = PinnedLeaseCell {
            cell: Arc::clone(&self.cell),
            key: self.key.clone(),
            coordinate,
            token,
            armed: true,
            state: PhantomData,
        };
        self.armed = false;
        Ok(successor)
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
        if !self.release_owned_state() {
            return Err(self);
        }
        let successor = self.released_successor();
        self.armed = false;
        Ok(successor)
    }
}

impl PinnedLeaseCell<Committed> {
    pub(super) fn release(mut self) -> Result<PinnedLeaseCell<Released>, Self> {
        if !self.release_owned_state() {
            return Err(self);
        }
        let successor = self.released_successor();
        self.armed = false;
        Ok(successor)
    }
}

impl<S: GuardState> PinnedLeaseCell<S> {
    fn release_owned_state(&self) -> bool {
        self.cell
            .release_exact(&self.cell, &self.key, self.token, self.coordinate, |_| true)
    }

    fn released_successor(&self) -> PinnedLeaseCell<Released> {
        PinnedLeaseCell {
            cell: Arc::clone(&self.cell),
            key: self.key.clone(),
            coordinate: self.coordinate,
            token: self.token,
            armed: false,
            state: PhantomData,
        }
    }
}

impl<S: GuardState> Drop for PinnedLeaseCell<S> {
    fn drop(&mut self) {
        if self.armed && S::RELEASE_ON_DROP {
            self.release_owned_state();
        }
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
        let _marker = super::PayloadLockMarker::enter();
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
        guard.lease = LeaseState::Leased {
            holder: LeaseHolder::Sink,
            key,
            deadline_ms,
            start: coordinate.get(),
            end: coordinate.get(),
            exact_token: Some(token),
        };
        true
    }

    fn commit_exact(
        &self,
        pinned: &Arc<Self>,
        key: &DeliveryLeaseKey,
        token: LeaseToken,
        coordinate: ConsumedCoordinate,
        outcome: LeaseOutcome,
    ) -> bool {
        self.exact_mutate(
            pinned,
            key,
            token,
            coordinate,
            Some(super::TAG_COMMITTED),
            |state| match state {
                LeaseState::Leased { .. } => {
                    *state = LeaseState::Committed {
                        holder: LeaseHolder::Sink,
                        key: key.clone(),
                        start: coordinate.get(),
                        end: coordinate.get(),
                        outcome,
                        exact_token: Some(token),
                    };
                    true
                }
                _ => false,
            },
        )
    }

    fn matches_exact(
        &self,
        pinned: &Arc<Self>,
        key: &DeliveryLeaseKey,
        token: LeaseToken,
        coordinate: ConsumedCoordinate,
    ) -> bool {
        self.exact_mutate(pinned, key, token, coordinate, None, |_| true)
    }

    fn renew_exact(
        &self,
        pinned: &Arc<Self>,
        key: &DeliveryLeaseKey,
        token: LeaseToken,
        coordinate: ConsumedCoordinate,
        deadline_ms: u64,
    ) -> bool {
        self.exact_mutate(pinned, key, token, coordinate, None, |state| match state {
            LeaseState::Leased { deadline_ms: d, .. } => {
                *d = deadline_ms;
                true
            }
            _ => false,
        })
    }

    fn exact_mutate(
        &self,
        pinned: &Arc<Self>,
        key: &DeliveryLeaseKey,
        token: LeaseToken,
        coordinate: ConsumedCoordinate,
        success_tag: Option<u8>,
        mutate: impl FnOnce(&mut LeaseState) -> bool,
    ) -> bool {
        if !std::ptr::eq(self, Arc::as_ptr(pinned)) {
            return false;
        }
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _marker = super::PayloadLockMarker::enter();
        if !exact_matches(&guard.lease, key, token, coordinate) {
            return false;
        }
        if !mutate(&mut guard.lease) {
            return false;
        }
        if let Some(tag) = success_tag {
            self.state_tag
                .store(tag, std::sync::atomic::Ordering::Release);
        }
        true
    }

    fn reclaim_exact_if_expired(
        &self,
        pinned: &Arc<Self>,
        key: &DeliveryLeaseKey,
        token: LeaseToken,
        coordinate: ConsumedCoordinate,
        now_ms: u64,
    ) -> bool {
        self.release_exact(pinned, key, token, coordinate, |state| {
            matches!(state, LeaseState::Leased { deadline_ms, .. } if now_ms >= *deadline_ms)
        })
    }

    fn release_exact(
        &self,
        pinned: &Arc<Self>,
        key: &DeliveryLeaseKey,
        token: LeaseToken,
        coordinate: ConsumedCoordinate,
        predicate: impl FnOnce(&LeaseState) -> bool,
    ) -> bool {
        self.exact_mutate(
            pinned,
            key,
            token,
            coordinate,
            Some(super::TAG_UNLEASED),
            |state| {
                if !predicate(state) {
                    return false;
                }
                *state = LeaseState::Unleased;
                true
            },
        )
    }
}

fn exact_matches(
    state: &LeaseState,
    key: &DeliveryLeaseKey,
    token: LeaseToken,
    coordinate: ConsumedCoordinate,
) -> bool {
    match state {
        LeaseState::Leased {
            holder,
            key: current_key,
            start,
            end,
            exact_token,
            ..
        }
        | LeaseState::Committed {
            holder,
            key: current_key,
            start,
            end,
            exact_token,
            ..
        } => {
            *holder == LeaseHolder::Sink
                && current_key == key
                && *exact_token == Some(token)
                && *start == coordinate.get()
                && *end == coordinate.get()
        }
        LeaseState::Unleased => false,
    }
}

#[cfg(test)]
mod tests;
