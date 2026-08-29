use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};

/// Opaque process-local identity for one exact lease acquisition.
///
/// Values are never reused during a process lifetime. Once the counter is
/// exhausted, acquisition fails closed by panicking instead of wrapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord::delivery_lease_cell) struct LeaseToken(NonZeroU64);

impl LeaseToken {
    pub(super) fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let value = NEXT
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .unwrap_or_else(|_| panic!("exact delivery lease token space exhausted"));
        Self(NonZeroU64::new(value).expect("exact lease token counter starts non-zero"))
    }
}
