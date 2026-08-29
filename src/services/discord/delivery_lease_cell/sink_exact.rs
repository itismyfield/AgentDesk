use super::{
    DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder, LeaseOutcome, LeaseState, LeaseToken,
    PayloadLockMarker, TAG_COMMITTED, TAG_LEASED, TAG_UNLEASED,
};

impl DeliveryLeaseCell {
    /// Sink-only exact acquisition. The returned token is bound to this concrete
    /// cell acquisition and must accompany every sink renew/commit/release.
    pub(in crate::services::discord) fn try_acquire_sink_exact(
        &self,
        key: DeliveryLeaseKey,
        coordinate: u64,
        deadline_ms: u64,
    ) -> Option<LeaseToken> {
        use std::sync::atomic::Ordering;
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _payload_lock_marker = PayloadLockMarker::enter();
        self.state_tag
            .compare_exchange(
                TAG_UNLEASED,
                TAG_LEASED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()?;
        let token = LeaseToken(self.next_token.fetch_add(1, Ordering::AcqRel));
        guard.lease = LeaseState::Leased {
            holder: LeaseHolder::Sink,
            key,
            token,
            deadline_ms,
            start: coordinate,
            end: coordinate,
        };
        Some(token)
    }

    pub(in crate::services::discord) fn commit_sink_exact(
        &self,
        key: &DeliveryLeaseKey,
        coordinate: u64,
        token: LeaseToken,
        outcome: LeaseOutcome,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _payload_lock_marker = PayloadLockMarker::enter();
        let matches = matches!(
            &guard.lease,
            LeaseState::Leased { holder: LeaseHolder::Sink, key: current, token: current_token, start, end, .. }
                if current == key && *current_token == token && *start == coordinate && *end == coordinate
        );
        if !matches {
            return false;
        }
        guard.lease = LeaseState::Committed {
            holder: LeaseHolder::Sink,
            key: key.clone(),
            token,
            start: coordinate,
            end: coordinate,
            outcome,
        };
        self.state_tag.store(TAG_COMMITTED, Ordering::Release);
        true
    }

    pub(in crate::services::discord) fn release_sink_exact(
        &self,
        key: &DeliveryLeaseKey,
        coordinate: u64,
        token: LeaseToken,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _payload_lock_marker = PayloadLockMarker::enter();
        let matches = match &guard.lease {
            LeaseState::Leased {
                holder: LeaseHolder::Sink,
                key: current,
                token: current_token,
                start,
                end,
                ..
            }
            | LeaseState::Committed {
                holder: LeaseHolder::Sink,
                key: current,
                token: current_token,
                start,
                end,
                ..
            } => {
                current == key
                    && *current_token == token
                    && *start == coordinate
                    && *end == coordinate
            }
            _ => false,
        };
        if !matches {
            return false;
        }
        guard.lease = LeaseState::Unleased;
        self.state_tag.store(TAG_UNLEASED, Ordering::Release);
        true
    }

    pub(in crate::services::discord) fn sink_exact_matches(
        &self,
        key: &DeliveryLeaseKey,
        coordinate: u64,
        token: LeaseToken,
    ) -> bool {
        let guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _payload_lock_marker = PayloadLockMarker::enter();
        matches!(
            &guard.lease,
            LeaseState::Leased { holder: LeaseHolder::Sink, key: current, token: current_token, start, end, .. }
            | LeaseState::Committed { holder: LeaseHolder::Sink, key: current, token: current_token, start, end, .. }
                if current == key && *current_token == token && *start == coordinate && *end == coordinate
        )
    }

    pub(in crate::services::discord) fn renew_sink_exact(
        &self,
        key: &DeliveryLeaseKey,
        coordinate: u64,
        token: LeaseToken,
        new_deadline_ms: u64,
    ) -> bool {
        let mut guard = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _payload_lock_marker = PayloadLockMarker::enter();
        if let LeaseState::Leased {
            holder: LeaseHolder::Sink,
            key: current,
            token: current_token,
            start,
            end,
            deadline_ms,
        } = &mut guard.lease
            && current == key
            && *current_token == token
            && *start == coordinate
            && *end == coordinate
        {
            *deadline_ms = new_deadline_ms;
            return true;
        }
        false
    }
}
