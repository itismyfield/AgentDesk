use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord::delivery_lease_cell) struct LeaseToken(NonZeroU64);

impl LeaseToken {
    pub(super) fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let value = NEXT
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .unwrap_or_else(|_| panic!("exact delivery lease token space exhausted"));
        Self(NonZeroU64::new(value).expect("token sequence starts nonzero"))
    }
}
