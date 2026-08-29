//! Linear publication-attempt substrate for the turn-output controller.
//!
//! This slice does not route production delivery through these types. Later
//! slices may cut owners over only through the private issuers below.

#![allow(dead_code)] // S1 substrate; production cutover is explicitly deferred.

use std::future::Future;

use crate::services::discord::{DeliveryLeaseKey, LeaseHolder};

const CONTROLLER_ISSUER_STAMP: u64 = 0x5191_0072;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct HeldAttempt {
    key: DeliveryLeaseKey,
    holder: LeaseHolder,
    coordinate: HeldCoordinate,
}

#[derive(Debug, PartialEq, Eq)]
enum HeldCoordinate {
    Range {
        start: u64,
        end: u64,
    },
    /// A real pseudo-range lease with no durable-frontier authority.
    FreshNoRange {
        lease_start: u64,
        lease_end: u64,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct MarkerlessAttempt {
    _private: (),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PublicationAttempt {
    Held(HeldAttempt),
    Markerless(MarkerlessAttempt),
}

/// Opaque linear proof. It is intentionally neither `Clone` nor `Copy`.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct IssuedAttempt {
    attempt: PublicationAttempt,
    issuer_stamp: u64,
}

impl IssuedAttempt {
    fn held_identity(&self) -> Option<(&DeliveryLeaseKey, LeaseHolder)> {
        match &self.attempt {
            PublicationAttempt::Held(held) => Some((&held.key, held.holder)),
            PublicationAttempt::Markerless(_) => None,
        }
    }

    fn frontier_range(&self) -> Option<(u64, u64)> {
        match &self.attempt {
            PublicationAttempt::Held(HeldAttempt {
                coordinate: HeldCoordinate::Range { start, end },
                ..
            }) => Some((*start, *end)),
            PublicationAttempt::Held(HeldAttempt {
                coordinate: HeldCoordinate::FreshNoRange { .. },
                ..
            })
            | PublicationAttempt::Markerless(_) => None,
        }
    }

    fn lease_range(&self) -> Option<(u64, u64)> {
        match &self.attempt {
            PublicationAttempt::Held(HeldAttempt {
                coordinate: HeldCoordinate::Range { start, end },
                ..
            }) => Some((*start, *end)),
            PublicationAttempt::Held(HeldAttempt {
                coordinate:
                    HeldCoordinate::FreshNoRange {
                        lease_start,
                        lease_end,
                    },
                ..
            }) => Some((*lease_start, *lease_end)),
            PublicationAttempt::Markerless(_) => None,
        }
    }
}

/// Private controller seam for an exact acquired lease identity.
fn issue_held(
    key: DeliveryLeaseKey,
    holder: LeaseHolder,
    start: u64,
    end: u64,
) -> Option<IssuedAttempt> {
    (end > start).then_some(IssuedAttempt {
        attempt: PublicationAttempt::Held(HeldAttempt {
            key,
            holder,
            coordinate: HeldCoordinate::Range { start, end },
        }),
        issuer_stamp: CONTROLLER_ISSUER_STAMP,
    })
}

/// Private controller seam for recovery/standby authority with no lease.
fn issue_markerless() -> IssuedAttempt {
    IssuedAttempt {
        attempt: PublicationAttempt::Markerless(MarkerlessAttempt { _private: () }),
        issuer_stamp: CONTROLLER_ISSUER_STAMP,
    }
}

/// Private controller seam for fresh NoRange after its pseudo-range lease wins.
fn issue_fresh_no_range(
    key: DeliveryLeaseKey,
    holder: LeaseHolder,
    lease_start: u64,
    lease_end: u64,
) -> Option<IssuedAttempt> {
    (lease_end > lease_start).then_some(IssuedAttempt {
        attempt: PublicationAttempt::Held(HeldAttempt {
            key,
            holder,
            coordinate: HeldCoordinate::FreshNoRange {
                lease_start,
                lease_end,
            },
        }),
        issuer_stamp: CONTROLLER_ISSUER_STAMP,
    })
}

/// Private logical transport verb. Taking the proof by value makes one call the
/// available API use; the runtime witness also detects an implementation that
/// invokes the transport closure twice.
async fn transport_once<T, F, Fut>(
    proof: IssuedAttempt,
    mut transport: F,
) -> Result<T, IssuedAttempt>
where
    F: FnMut(&PublicationAttempt) -> Fut + Send,
    Fut: Future<Output = T> + Send,
    T: Send,
{
    if proof.issuer_stamp != CONTROLLER_ISSUER_STAMP {
        return Err(proof);
    }
    Ok(transport(&proof.attempt).await)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use poise::serenity_prelude::ChannelId;

    use super::*;

    fn key(user_msg_id: u64) -> DeliveryLeaseKey {
        DeliveryLeaseKey::new(ChannelId::new(72), 9, user_msg_id, None, None)
    }

    #[test]
    fn held_attempt_preserves_pinned_key_holder_and_range() {
        let key = key(7101);
        let proof = issue_held(key.clone(), LeaseHolder::Sink, 11, 29).expect("positive range");
        assert_eq!(proof.held_identity(), Some((&key, LeaseHolder::Sink)));
        assert_eq!(proof.lease_range(), Some((11, 29)));
        assert_eq!(proof.frontier_range(), Some((11, 29)));
    }

    #[test]
    fn markerless_cannot_present_held_lease_identity() {
        let proof = issue_markerless();
        assert_eq!(proof.held_identity(), None);
        assert_eq!(proof.lease_range(), None);
        assert_eq!(proof.frontier_range(), None);
        assert!(matches!(proof.attempt, PublicationAttempt::Markerless(_)));
    }

    #[test]
    fn fresh_no_range_is_held_and_frontierless_not_markerless() {
        let key = key(7201);
        let proof = issue_fresh_no_range(
            key.clone(),
            LeaseHolder::Watcher { instance_id: 44 },
            100,
            117,
        )
        .expect("positive pseudo-range");
        assert_eq!(
            proof.held_identity(),
            Some((&key, LeaseHolder::Watcher { instance_id: 44 }))
        );
        assert_eq!(proof.lease_range(), Some((100, 117)));
        assert_eq!(proof.frontier_range(), None);
        assert!(matches!(proof.attempt, PublicationAttempt::Held(_)));
    }

    #[test]
    fn private_logical_verb_consumes_one_issued_attempt_and_transports_once() {
        fn assert_send<T: Send>() {}
        assert_send::<IssuedAttempt>();

        let calls = AtomicUsize::new(0);
        let proof = issue_held(key(7301), LeaseHolder::Sink, 5, 8).expect("positive range");
        let observed = futures::executor::block_on(transport_once(proof, |attempt| {
            calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(matches!(attempt, PublicationAttempt::Held(_)))
        }))
        .expect("controller-issued proof");
        assert!(observed);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one logical transport");
    }

    #[test]
    fn logical_verb_rejects_a_proof_not_minted_by_private_issuer() {
        let calls = AtomicUsize::new(0);
        let bypass = IssuedAttempt {
            attempt: PublicationAttempt::Markerless(MarkerlessAttempt { _private: () }),
            issuer_stamp: 0,
        };
        let rejected = futures::executor::block_on(transport_once(bypass, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(())
        }));
        assert!(rejected.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn issuer_and_proof_visibility_remain_controller_private() {
        let source = include_str!("publication_attempt.rs");
        for signature in [
            "fn issue_held(\n    key:",
            "fn issue_markerless() ->",
            "fn issue_fresh_no_range(\n    key:",
        ] {
            let prefix = source.split_once(signature).expect("issuer signature").0;
            let declaration_line = prefix.rsplit_once('\n').map_or(prefix, |(_, line)| line);
            assert_eq!(declaration_line.trim(), "");
        }
        let proof_decl = source
            .split_once("pub(super) struct IssuedAttempt")
            .expect("proof declaration")
            .0;
        let derive_line = proof_decl.lines().next_back().expect("derive line");
        assert_eq!(derive_line.trim(), "#[derive(Debug, PartialEq, Eq)]");
    }
}
