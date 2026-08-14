//! Schema capability versions required by intake delivery stamping and settlement.

/// #5071 T2-W schema contract shared by the capability probe and its tests.
///
/// T2-W adds no migration. Keep this list aligned with the existing journal,
/// dispatched-state, clock, and index migrations instead of raising the
/// forward-only binary floor.
pub(crate) const INTAKE_DELIVERY_REQUIRED_MIGRATIONS: [i64; 6] = [103, 105, 106, 107, 108, 109];
