//! Linear publication-attempt substrate for the turn-output controller.
//!
//! This slice does not route production delivery through these types. Later
//! slices may cut owners over only through the acquisition-backed issuers below.

#![allow(dead_code)] // S1 substrate; production cutover is explicitly deferred.

mod authority {
    use std::future::Future;

    use super::super::ControllerLeaseGuard;
    use crate::services::discord::delivery_lease_cell::PositiveByteRange;
    use crate::services::discord::{DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder};

    pub(super) struct HeldAttempt<'a> {
        key: DeliveryLeaseKey,
        holder: LeaseHolder,
        coordinate: HeldCoordinate,
        _guard: ControllerLeaseGuard<'a, DeliveryLeaseCell>,
    }

    enum HeldCoordinate {
        Range(PositiveByteRange),
        /// A real pseudo-range lease with no durable-frontier authority.
        FreshNoRange(PositiveByteRange),
    }

    pub(super) struct MarkerlessAttempt {
        coordinate: (u64, u64),
    }

    pub(super) enum PublicationAttempt<'a> {
        Held(HeldAttempt<'a>),
        Markerless(MarkerlessAttempt),
    }

    /// Opaque linear proof. Held variants own the non-Clone RAII guard created
    /// as the unique result of a successful lease acquisition.
    pub(super) struct IssuedAttempt<'a> {
        attempt: PublicationAttempt<'a>,
    }

    impl IssuedAttempt<'_> {
        fn held_identity(&self) -> Option<(&DeliveryLeaseKey, LeaseHolder)> {
            match &self.attempt {
                PublicationAttempt::Held(held) => Some((&held.key, held.holder)),
                PublicationAttempt::Markerless(_) => None,
            }
        }

        fn frontier_range(&self) -> Option<(u64, u64)> {
            match &self.attempt {
                PublicationAttempt::Held(HeldAttempt {
                    coordinate: HeldCoordinate::Range(range),
                    ..
                }) => Some((range.start(), range.end())),
                PublicationAttempt::Held(HeldAttempt {
                    coordinate: HeldCoordinate::FreshNoRange(_),
                    ..
                })
                | PublicationAttempt::Markerless(_) => None,
            }
        }

        fn lease_range(&self) -> Option<(u64, u64)> {
            match &self.attempt {
                PublicationAttempt::Held(HeldAttempt {
                    coordinate: HeldCoordinate::Range(range),
                    ..
                })
                | PublicationAttempt::Held(HeldAttempt {
                    coordinate: HeldCoordinate::FreshNoRange(range),
                    ..
                }) => Some((range.start(), range.end())),
                PublicationAttempt::Markerless(_) => None,
            }
        }
    }

    /// Acquire and issue one exact held range. Raw identity values cannot mint a
    /// proof: only the winning `try_acquire` path constructs and transfers the
    /// guard into `IssuedAttempt`.
    fn acquire_held(
        lease: &DeliveryLeaseCell,
        key: DeliveryLeaseKey,
        holder: LeaseHolder,
        start: u64,
        end: u64,
        deadline_ms: u64,
    ) -> Option<IssuedAttempt<'_>> {
        let range = PositiveByteRange::new(start, end)?;
        lease
            .try_acquire(key.clone(), holder, start, end, deadline_ms)
            .then(|| IssuedAttempt {
                attempt: PublicationAttempt::Held(HeldAttempt {
                    key: key.clone(),
                    holder,
                    coordinate: HeldCoordinate::Range(range),
                    _guard: ControllerLeaseGuard::arm(lease, holder, key, start, end),
                }),
            })
    }

    /// Recovery/standby authority deliberately owns no lease.
    fn issue_markerless() -> IssuedAttempt<'static> {
        IssuedAttempt {
            attempt: PublicationAttempt::Markerless(MarkerlessAttempt { coordinate: (0, 0) }),
        }
    }

    /// Acquire and issue fresh NoRange authority. Its positive pseudo-range is a
    /// real lease, but the coordinate deliberately exposes no frontier range.
    fn acquire_fresh_no_range(
        lease: &DeliveryLeaseCell,
        key: DeliveryLeaseKey,
        holder: LeaseHolder,
        lease_start: u64,
        lease_end: u64,
        deadline_ms: u64,
    ) -> Option<IssuedAttempt<'_>> {
        let range = PositiveByteRange::new(lease_start, lease_end)?;
        lease
            .try_acquire(key.clone(), holder, lease_start, lease_end, deadline_ms)
            .then(|| IssuedAttempt {
                attempt: PublicationAttempt::Held(HeldAttempt {
                    key: key.clone(),
                    holder,
                    coordinate: HeldCoordinate::FreshNoRange(range),
                    _guard: ControllerLeaseGuard::arm(lease, holder, key, lease_start, lease_end),
                }),
            })
    }

    /// Private logical transport verb. `FnOnce` and by-value proof ownership
    /// expose exactly one transport invocation.
    async fn transport_once<T, F, Fut>(proof: IssuedAttempt<'_>, transport: F) -> T
    where
        F: FnOnce(&PublicationAttempt<'_>) -> Fut + Send,
        Fut: Future<Output = T> + Send,
        T: Send,
    {
        transport(&proof.attempt).await
    }

    #[cfg(test)]
    mod tests {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use poise::serenity_prelude::ChannelId;

        use super::*;
        use crate::services::discord::LeaseSnapshot;

        fn key(user_msg_id: u64) -> DeliveryLeaseKey {
            DeliveryLeaseKey::new(ChannelId::new(72), 9, user_msg_id, None, None)
        }

        fn lease() -> DeliveryLeaseCell {
            DeliveryLeaseCell::new(ChannelId::new(72))
        }

        #[test]
        fn held_attempt_requires_winning_acquisition_and_preserves_identity() {
            let lease = lease();
            let key = key(7101);
            let proof = acquire_held(&lease, key.clone(), LeaseHolder::Sink, 11, 29, u64::MAX)
                .expect("first acquisition wins");
            assert!(
                acquire_held(&lease, key.clone(), LeaseHolder::Sink, 11, 29, u64::MAX,).is_none(),
                "a failed acquire cannot issue authority"
            );
            assert_eq!(proof.held_identity(), Some((&key, LeaseHolder::Sink)));
            assert_eq!(proof.lease_range(), Some((11, 29)));
            assert_eq!(proof.frontier_range(), Some((11, 29)));
            drop(proof);
            assert!(matches!(lease.read(), LeaseSnapshot::Unleased));
        }

        #[test]
        fn held_attempt_rejects_zero_and_reversed_ranges_before_acquire() {
            let lease = lease();
            assert!(acquire_held(&lease, key(7111), LeaseHolder::Sink, 9, 9, u64::MAX).is_none());
            assert!(acquire_held(&lease, key(7112), LeaseHolder::Sink, 10, 9, u64::MAX).is_none());
            assert!(matches!(lease.read(), LeaseSnapshot::Unleased));
        }

        #[test]
        fn markerless_cannot_present_held_lease_identity() {
            let proof = issue_markerless();
            assert_eq!(proof.held_identity(), None);
            assert_eq!(proof.lease_range(), None);
            assert_eq!(proof.frontier_range(), None);
            assert!(matches!(
                proof.attempt,
                PublicationAttempt::Markerless(MarkerlessAttempt { coordinate: (0, 0) })
            ));
        }

        #[test]
        fn fresh_no_range_is_acquired_held_and_frontierless() {
            let lease = lease();
            let key = key(7201);
            let holder = LeaseHolder::Watcher { instance_id: 44 };
            let proof = acquire_fresh_no_range(&lease, key.clone(), holder, 100, 117, u64::MAX)
                .expect("positive pseudo-range acquisition");
            assert_eq!(proof.held_identity(), Some((&key, holder)));
            assert_eq!(proof.lease_range(), Some((100, 117)));
            assert_eq!(proof.frontier_range(), None);
            assert!(matches!(proof.attempt, PublicationAttempt::Held(_)));
        }

        #[test]
        fn private_logical_verb_consumes_one_attempt_and_transports_once() {
            fn assert_send<T: Send>() {}
            assert_send::<IssuedAttempt<'_>>();

            let lease = lease();
            let calls = AtomicUsize::new(0);
            let proof = acquire_held(&lease, key(7301), LeaseHolder::Sink, 5, 8, u64::MAX)
                .expect("positive range acquisition");
            let observed = futures::executor::block_on(transport_once(proof, |attempt| {
                calls.fetch_add(1, Ordering::SeqCst);
                std::future::ready(matches!(attempt, PublicationAttempt::Held(_)))
            }));
            assert!(observed);
            assert_eq!(calls.load(Ordering::SeqCst), 1, "one logical transport");
            assert!(matches!(lease.read(), LeaseSnapshot::Unleased));
        }

        #[test]
        fn authority_surface_remains_closed_and_proof_non_clone() {
            let source = include_str!("publication_attempt.rs");
            let clone_impl_signature = ["impl Clone", " for IssuedAttempt"].concat();
            assert!(
                !source.contains(&clone_impl_signature),
                "linear proof must not gain a manual Clone implementation"
            );

            let authority_body = source
                .split_once("mod authority {")
                .expect("authority module")
                .1
                .split_once("#[cfg(test)]\n    mod tests")
                .expect("authority production body")
                .0;
            let exposed = authority_body
                .lines()
                .filter(|line| line.trim_start().starts_with("pub(super)"))
                .map(str::trim)
                .collect::<Vec<_>>();
            assert_eq!(
                exposed,
                [
                    "pub(super) struct HeldAttempt<'a> {",
                    "pub(super) struct MarkerlessAttempt {",
                    "pub(super) enum PublicationAttempt<'a> {",
                    "pub(super) struct IssuedAttempt<'a> {",
                ],
                "authority module must not expose a mint or reconstruction helper"
            );
        }
    }
}
